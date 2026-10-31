use crate::error::DbError;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    MySql,
    Postgres,
    Mongo,
    Redis,
}

impl DbKind {
    /// How a driver of this kind talks to its engine.
    pub fn family(self) -> EngineFamily {
        match self {
            DbKind::MySql | DbKind::Postgres => EngineFamily::Sql,
            DbKind::Mongo | DbKind::Redis => EngineFamily::Command,
        }
    }

    /// The engine's display name, used in messages.
    pub fn label(self) -> &'static str {
        match self {
            DbKind::MySql => "MySQL",
            DbKind::Postgres => "PostgreSQL",
            DbKind::Mongo => "MongoDB",
            DbKind::Redis => "Redis",
        }
    }

    /// The SQL dialect of this kind, if it is a SQL engine.
    pub fn sql(self) -> Option<SqlKind> {
        match self {
            DbKind::MySql => Some(SqlKind::MySql),
            DbKind::Postgres => Some(SqlKind::Postgres),
            DbKind::Mongo | DbKind::Redis => None,
        }
    }

    /// The SQL dialect of this kind, or `Unsupported` for a non-SQL engine.
    pub fn require_sql(self) -> Result<SqlKind, DbError> {
        self.sql().ok_or_else(|| DbError::Unsupported {
            feature: format!("SQL on {}", self.label()),
            version: String::new(),
        })
    }
}

/// The SQL engines, for code that only makes sense for SQL drivers.
/// Serializes exactly like the matching `DbKind` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum SqlKind {
    MySql,
    Postgres,
}

impl From<SqlKind> for DbKind {
    fn from(kind: SqlKind) -> Self {
        match kind {
            SqlKind::MySql => DbKind::MySql,
            SqlKind::Postgres => DbKind::Postgres,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum ServerVariant {
    MySql57,
    MySql8,
    MariaDb,
    Postgres,
    Mongo,
    Redis,
}

/// How a driver talks to its engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum EngineFamily {
    Sql,
    Command,
}

/// How an engine nests its objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceModel {
    /// database → schema → object (PostgreSQL).
    DbSchema,
    /// database → object (MySQL, Mongo).
    Db,
    /// db0…dbN → keyspace (Redis).
    NumberedDb,
}

/// What a connected driver supports; the UI and commands gate features on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Capabilities {
    pub family: EngineFamily,
    pub namespaces: NamespaceModel,
    pub transactions: bool,
    pub data_edit: bool,
    pub designer: bool,
    pub ddl_edit: bool,
    pub object_actions: bool,
    pub import: bool,
    pub dump: bool,
    pub seed: bool,
    pub explain: bool,
    /// Objects are browsed page by page instead of listed in full.
    pub paged_objects: bool,
}

/// A gateable feature flag of [`Capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cap {
    DataEdit,
    Designer,
    DdlEdit,
    ObjectActions,
    Import,
    Dump,
    Seed,
    Explain,
}

impl Cap {
    pub const ALL: [Cap; 8] = [
        Cap::DataEdit,
        Cap::Designer,
        Cap::DdlEdit,
        Cap::ObjectActions,
        Cap::Import,
        Cap::Dump,
        Cap::Seed,
        Cap::Explain,
    ];

    /// The feature name used in `DbError::Unsupported`.
    pub fn label(self) -> &'static str {
        match self {
            Cap::DataEdit => "data editing",
            Cap::Designer => "table designer",
            Cap::DdlEdit => "DDL editing",
            Cap::ObjectActions => "object actions",
            Cap::Import => "import",
            Cap::Dump => "dump and restore",
            Cap::Seed => "data generation",
            Cap::Explain => "explain",
        }
    }
}

impl Capabilities {
    /// A SQL engine with every feature; objects are listed in full.
    pub fn sql_full(namespaces: NamespaceModel) -> Capabilities {
        Capabilities {
            family: EngineFamily::Sql,
            namespaces,
            transactions: true,
            data_edit: true,
            designer: true,
            ddl_edit: true,
            object_actions: true,
            import: true,
            dump: true,
            seed: true,
            explain: true,
            paged_objects: false,
        }
    }

    /// The capabilities a driver of `kind` declares.
    pub fn for_kind(kind: DbKind) -> Capabilities {
        match kind {
            DbKind::MySql => Capabilities::sql_full(NamespaceModel::Db),
            DbKind::Postgres => Capabilities::sql_full(NamespaceModel::DbSchema),
            DbKind::Mongo => Capabilities {
                data_edit: true,
                ..Capabilities::command(NamespaceModel::Db)
            },
            DbKind::Redis => Capabilities {
                data_edit: true,
                paged_objects: true,
                ..Capabilities::command(NamespaceModel::NumberedDb)
            },
        }
    }

    /// A command engine with every flag off.
    fn command(namespaces: NamespaceModel) -> Capabilities {
        Capabilities {
            family: EngineFamily::Command,
            namespaces,
            transactions: false,
            data_edit: false,
            designer: false,
            ddl_edit: false,
            object_actions: false,
            import: false,
            dump: false,
            seed: false,
            explain: false,
            paged_objects: false,
        }
    }

    pub fn has(&self, cap: Cap) -> bool {
        match cap {
            Cap::DataEdit => self.data_edit,
            Cap::Designer => self.designer,
            Cap::DdlEdit => self.ddl_edit,
            Cap::ObjectActions => self.object_actions,
            Cap::Import => self.import,
            Cap::Dump => self.dump,
            Cap::Seed => self.seed,
            Cap::Explain => self.explain,
        }
    }
}

/// `Unsupported` naming the feature when `caps` lacks `cap`.
pub fn check_cap(caps: &Capabilities, cap: Cap) -> Result<(), DbError> {
    if caps.has(cap) {
        Ok(())
    } else {
        Err(DbError::Unsupported {
            feature: cap.label().into(),
            version: String::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ServerInfo {
    pub kind: DbKind,
    pub variant: ServerVariant,
    pub version: (u32, u32, u32),
    pub version_string: String,
    pub capabilities: Capabilities,
}

impl ServerInfo {
    /// A server of `kind` with that kind's default capabilities.
    pub fn new(
        kind: DbKind,
        variant: ServerVariant,
        version: (u32, u32, u32),
        version_string: impl Into<String>,
    ) -> ServerInfo {
        ServerInfo {
            kind,
            variant,
            version,
            version_string: version_string.into(),
            capabilities: Capabilities::for_kind(kind),
        }
    }
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
    Ok(ServerInfo::new(DbKind::MySql, variant, version, s))
}

/// Builds `ServerInfo` from `SHOW server_version_num` (e.g. 170002 → 17.0.2).
/// Minimum-version enforcement is the driver's job.
pub fn detect_pg(server_version_num: u32, version_string: &str) -> ServerInfo {
    let n = server_version_num;
    ServerInfo::new(
        DbKind::Postgres,
        ServerVariant::Postgres,
        (n / 10_000, (n / 100) % 100, n % 100),
        version_string,
    )
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
            ServerVariant::MySql57
            | ServerVariant::Postgres
            | ServerVariant::Mongo
            | ServerVariant::Redis => Self::default(),
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

    #[test]
    fn sql_kind_serializes_like_db_kind() {
        assert_eq!(serde_json::to_value(SqlKind::MySql).unwrap(), "mysql");
        assert_eq!(serde_json::to_value(SqlKind::Postgres).unwrap(), "postgres");
        assert_eq!(DbKind::from(SqlKind::Postgres), DbKind::Postgres);
        assert_eq!(DbKind::MySql.sql(), Some(SqlKind::MySql));
        assert_eq!(DbKind::Postgres.sql(), Some(SqlKind::Postgres));
    }

    #[test]
    fn sql_capabilities() {
        let c = Capabilities::sql_full(NamespaceModel::Db);
        assert_eq!(
            (c.family, c.namespaces, c.paged_objects),
            (EngineFamily::Sql, NamespaceModel::Db, false)
        );
        assert!(
            c.transactions
                && c.data_edit
                && c.designer
                && c.ddl_edit
                && c.object_actions
                && c.import
                && c.dump
                && c.seed
                && c.explain
        );
        assert_eq!(
            Capabilities::for_kind(DbKind::Postgres),
            Capabilities::sql_full(NamespaceModel::DbSchema)
        );
        assert_eq!(
            detect_mysql("8.0.36").unwrap().capabilities.namespaces,
            NamespaceModel::Db
        );
        assert_eq!(
            detect_pg(170002, "PostgreSQL 17.2").capabilities.namespaces,
            NamespaceModel::DbSchema
        );
        let j = serde_json::to_value(Capabilities::sql_full(NamespaceModel::DbSchema)).unwrap();
        assert_eq!(
            (j["family"].as_str(), j["namespaces"].as_str()),
            (Some("sql"), Some("db_schema"))
        );
    }

    #[test]
    fn new_kinds() {
        for (k, s) in [(DbKind::Mongo, "mongo"), (DbKind::Redis, "redis")] {
            assert_eq!(serde_json::to_value(k).unwrap(), s);
            assert_eq!(serde_json::from_value::<DbKind>(s.into()).unwrap(), k);
            assert_eq!((k.family(), k.sql()), (EngineFamily::Command, None));
        }
        assert_eq!(DbKind::Postgres.require_sql().unwrap(), SqlKind::Postgres);
        assert!(matches!(DbKind::Mongo.require_sql(),
            Err(DbError::Unsupported { feature, .. }) if feature == "SQL on MongoDB"));
        assert_eq!(
            [
                DbKind::MySql,
                DbKind::Postgres,
                DbKind::Mongo,
                DbKind::Redis
            ]
            .map(DbKind::label),
            ["MySQL", "PostgreSQL", "MongoDB", "Redis"]
        );
        let r = Capabilities::for_kind(DbKind::Redis);
        assert!(r.paged_objects && r.data_edit && !r.designer && !r.transactions);
        assert_eq!(r.namespaces, NamespaceModel::NumberedDb);
    }

    #[test]
    fn check_cap_names_the_feature() {
        let mut c = Capabilities::sql_full(NamespaceModel::Db);
        assert!(Cap::ALL
            .iter()
            .all(|&cap| c.has(cap) && check_cap(&c, cap).is_ok()));
        c.designer = false;
        assert!(matches!(check_cap(&c, Cap::Designer),
            Err(DbError::Unsupported { feature, version }) if feature == "table designer" && version.is_empty()));
    }
}
