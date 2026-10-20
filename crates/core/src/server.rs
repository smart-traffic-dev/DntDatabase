use crate::error::DbError;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    MySql,
    Postgres,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum ServerVariant {
    MySql57,
    MySql8,
    MariaDb,
    Postgres,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ServerInfo {
    pub kind: DbKind,
    pub variant: ServerVariant,
    pub version: (u32, u32, u32),
    pub version_string: String,
}

/// Prefix MariaDB servers send in the handshake for old-client compatibility.
const MARIADB_COMPAT_PREFIX: &str = "5.5.5-";

fn unsupported(version_string: &str) -> DbError {
    DbError::Unsupported {
        feature: "server version".into(),
        version: version_string.into(),
    }
}

/// Parses a leading `major.minor[.patch]`, ignoring any suffix such as `-log`.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let mut parts = s[..end].split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) if !p.is_empty() => p.parse().ok()?,
        _ => 0,
    };
    Some((major, minor, patch))
}

/// Classifies a MySQL-family server from `SELECT VERSION()`.
/// MySQL < 5.7, MariaDB < 10.3 and unparseable strings are `Unsupported`.
pub fn detect_mysql(version_string: &str) -> Result<ServerInfo, DbError> {
    let s = version_string.trim();
    let is_maria = s.to_ascii_lowercase().contains("mariadb");
    let numeric = match s.strip_prefix(MARIADB_COMPAT_PREFIX) {
        Some(rest) if is_maria => rest,
        _ => s,
    };
    let version = parse_version(numeric).ok_or_else(|| unsupported(s))?;
    let variant = if is_maria {
        if version < (10, 3, 0) {
            return Err(unsupported(s));
        }
        ServerVariant::MariaDb
    } else {
        match version {
            (5, 7, _) => ServerVariant::MySql57,
            (major, _, _) if major >= 8 => ServerVariant::MySql8,
            _ => return Err(unsupported(s)),
        }
    };
    Ok(ServerInfo {
        kind: DbKind::MySql,
        variant,
        version,
        version_string: s.into(),
    })
}

/// Builds `ServerInfo` from `SHOW server_version_num` (e.g. 170002 → 17.0.2).
/// Minimum-version enforcement is the driver's job.
pub fn detect_pg(server_version_num: u32, version_string: &str) -> ServerInfo {
    let n = server_version_num;
    ServerInfo {
        kind: DbKind::Postgres,
        variant: ServerVariant::Postgres,
        version: (n / 10_000, (n / 100) % 100, n % 100),
        version_string: version_string.into(),
    }
}

/// Feature flags that differ across the MySQL family (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MySqlCapabilities {
    pub check_constraints: bool,
    pub functional_index: bool,
    pub json_is_alias: bool,
    pub sequences: bool,
    pub invisible_columns: bool,
    pub system_versioned: bool,
}

impl MySqlCapabilities {
    pub fn for_server(info: &ServerInfo) -> Self {
        let v = info.version;
        match info.variant {
            ServerVariant::MySql8 => MySqlCapabilities {
                check_constraints: v >= (8, 0, 16),
                functional_index: v >= (8, 0, 13),
                invisible_columns: v >= (8, 0, 23),
                ..Self::default()
            },
            ServerVariant::MariaDb => MySqlCapabilities {
                check_constraints: true,
                functional_index: false,
                json_is_alias: true,
                sequences: true,
                invisible_columns: v >= (10, 3, 3),
                system_versioned: true,
            },
            ServerVariant::MySql57 | ServerVariant::Postgres => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServerVariant::*;
    use super::*;

    #[test]
    fn detects_variants() {
        let c = |s| detect_mysql(s).unwrap();
        assert_eq!(c("5.7.44-log").variant, MySql57);
        assert_eq!(c("5.7.44-log").version, (5, 7, 44));
        assert_eq!(c("8.0.36").variant, MySql8);
        assert_eq!(c("8.4.2").variant, MySql8);
        assert_eq!(c("9.1.0").variant, MySql8);
        assert_eq!(
            c("10.6.16-MariaDB-1:10.6.16+maria~ubu2004").variant,
            MariaDb
        );
        assert_eq!(c("5.5.5-10.11.6-MariaDB").variant, MariaDb);
        assert_eq!(c("5.5.5-10.11.6-MariaDB").version, (10, 11, 6));
        assert!(matches!(
            detect_mysql("5.6.51"),
            Err(DbError::Unsupported { .. })
        ));
    }

    #[test]
    fn rejects_unparseable_and_old_versions() {
        for s in ["", "garbage", "MariaDB", "7.0.1", "10.2.9-MariaDB"] {
            match detect_mysql(s) {
                Err(DbError::Unsupported { feature, version }) => {
                    assert_eq!(feature, "server version");
                    assert_eq!(version, s);
                }
                other => panic!("{s:?}: expected Unsupported, got {other:?}"),
            }
        }
        let i = detect_mysql("8.0").unwrap();
        assert_eq!((i.kind, i.version), (DbKind::MySql, (8, 0, 0)));
        assert_eq!(
            detect_mysql("5.5.5-10.11.6-MariaDB")
                .unwrap()
                .version_string,
            "5.5.5-10.11.6-MariaDB"
        );
    }

    #[test]
    fn pg_version() {
        assert_eq!(
            detect_pg(170002, "PostgreSQL 17.2 on x86_64").version,
            (17, 0, 2)
        );
        assert_eq!(detect_pg(110000, "").version, (11, 0, 0));
    }

    #[test]
    fn capabilities_table() {
        let caps = |s| MySqlCapabilities::for_server(&detect_mysql(s).unwrap());
        assert!(
            !caps("5.7.44").check_constraints
                && !caps("8.0.15").check_constraints
                && caps("8.0.16").check_constraints
        );
        assert!(caps("8.0.13").functional_index && !caps("10.6.1-MariaDB").functional_index);
        assert!(
            caps("10.6.1-MariaDB").json_is_alias
                && caps("10.6.1-MariaDB").sequences
                && caps("10.6.1-MariaDB").system_versioned
        );
        assert!(
            caps("8.0.23").invisible_columns
                && !caps("8.0.22").invisible_columns
                && caps("10.3.3-MariaDB").invisible_columns
        );
        assert!(matches!(
            detect_mysql("10.2.44-MariaDB"),
            Err(DbError::Unsupported { .. })
        ));
    }
}
