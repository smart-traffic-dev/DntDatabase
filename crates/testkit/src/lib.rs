//! Integration-test support: the Docker target matrix, fixture paths, SSH helpers and
//! an in-memory Command driver ([`fake_command`]). Only used as a dev-dependency by
//! other crates.

pub mod fake_command;

use dnt_core::config::{DriverOptions, ResolvedConfig, SshAuth, SshConfig};
use dnt_core::server::{DbKind, SqlKind};
use dnt_store::repo::NewSecrets;
use secrecy::SecretString;
use std::path::PathBuf;
use std::time::Duration;

/// One database server of the Docker test matrix (`testdata/docker-compose.test.yml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    MySql57,
    MySql8,
    /// MySQL 8.4 LTS: explain fixtures only, so not in [`Target::all`] or
    /// [`Target::mysql_family`].
    MySql84,
    MariaDb106,
    MariaDb114,
    Pg12,
    Pg17,
    /// MongoDB 6.0 standalone. Not part of the SQL matrix ([`Target::all`]); see [`Target::mongo`].
    Mongo60,
    /// MongoDB 8.0, single-node replica set `rs0` with TLS allowed (not required).
    Mongo80,
}

pub const TEST_PASSWORD: &str = "dnt_test";
pub const FIXTURE_DB: &str = "dnt_fixture";
pub const SSH_PORT: u16 = 2222;

impl Target {
    /// The SQL matrix (MySQL family and PostgreSQL).
    pub fn all() -> [Target; 6] {
        [
            Target::MySql57,
            Target::MySql8,
            Target::MariaDb106,
            Target::MariaDb114,
            Target::Pg12,
            Target::Pg17,
        ]
    }

    pub fn mysql_family() -> [Target; 4] {
        [
            Target::MySql57,
            Target::MySql8,
            Target::MariaDb106,
            Target::MariaDb114,
        ]
    }

    pub fn pg() -> [Target; 2] {
        [Target::Pg12, Target::Pg17]
    }

    pub fn mongo() -> [Target; 2] {
        [Target::Mongo60, Target::Mongo80]
    }

    pub fn kind(&self) -> DbKind {
        match self {
            Target::MySql57
            | Target::MySql8
            | Target::MySql84
            | Target::MariaDb106
            | Target::MariaDb114 => DbKind::MySql,
            Target::Pg12 | Target::Pg17 => DbKind::Postgres,
            Target::Mongo60 | Target::Mongo80 => DbKind::Mongo,
        }
    }

    /// The SQL engine of the server. Panics for a MongoDB target (a test bug).
    pub fn sql_kind(&self) -> SqlKind {
        match self.kind() {
            DbKind::MySql => SqlKind::MySql,
            DbKind::Postgres => SqlKind::Postgres,
            DbKind::Mongo | DbKind::Redis => panic!("{self:?} is not a SQL target"),
        }
    }

    /// Port published on the Docker host.
    pub fn host_port(&self) -> u16 {
        match self {
            Target::MySql57 => 33057,
            Target::MySql8 => 33080,
            Target::MySql84 => 33084,
            Target::MariaDb106 => 33106,
            Target::MariaDb114 => 33114,
            Target::Pg12 => 54312,
            Target::Pg17 => 54317,
            Target::Mongo60 => 27060,
            Target::Mongo80 => 27080,
        }
    }

    /// Superuser name (`root` for MySQL family and MongoDB, `postgres` for PG).
    pub fn admin_user(&self) -> &'static str {
        match self.kind() {
            DbKind::MySql | DbKind::Mongo | DbKind::Redis => "root",
            DbKind::Postgres => "postgres",
        }
    }

    /// Config for connecting directly from the host as the superuser.
    pub fn resolved(&self, db: Option<&str>) -> ResolvedConfig {
        self.resolved_as(self.admin_user(), TEST_PASSWORD, db)
    }

    /// Same as [`Target::resolved`] but with explicit credentials (e.g. `dnt_limited`).
    /// MongoDB targets get `DriverOptions::Mongo` for one host (no SRV, no replica set) with
    /// auth source `admin`, where the compose services create their users.
    pub fn resolved_as(&self, user: &str, password: &str, db: Option<&str>) -> ResolvedConfig {
        let driver = match self.kind() {
            DbKind::MySql | DbKind::Postgres => DriverOptions::None,
            DbKind::Mongo => DriverOptions::Mongo {
                srv: false,
                auth_source: Some("admin".into()),
                replica_set: None,
                direct_connection: false,
                extra_hosts: vec![],
            },
            DbKind::Redis => DriverOptions::Redis { db: 0 },
        };
        ResolvedConfig {
            kind: self.kind(),
            host: "127.0.0.1".into(),
            port: self.host_port(),
            tls_server_name: "localhost".into(),
            via_tunnel: false,
            username: user.into(),
            password: Some(SecretString::from(password.to_string())),
            database: db.map(str::to_string),
            ssl: None,
            connect_timeout: Duration::from_secs(10),
            read_only: false,
            application_name: "DntDatabase".into(),
            driver,
        }
    }

    /// Compose service name and container port, as seen from inside the docker network
    /// (what an SSH tunnel through the `ssh` service forwards to).
    pub fn ssh_target_host(&self) -> (&'static str, u16) {
        match self {
            Target::MySql57 => ("mysql57", 3306),
            Target::MySql8 => ("mysql8", 3306),
            Target::MySql84 => ("mysql84", 3306),
            Target::MariaDb106 => ("mariadb106", 3306),
            Target::MariaDb114 => ("mariadb114", 3306),
            Target::Pg12 => ("pg12", 5432),
            Target::Pg17 => ("pg17", 5432),
            Target::Mongo60 => ("mongo60", 27017),
            Target::Mongo80 => ("mongo80", 27080),
        }
    }
}

/// True when integration tests against Docker are enabled (`DNT_TEST_DB=1`).
pub fn db_enabled() -> bool {
    std::env::var("DNT_TEST_DB").is_ok_and(|v| v == "1")
}

/// Early-returns from the calling test (printing `skipped`) unless `DNT_TEST_DB=1`.
#[macro_export]
macro_rules! require_db {
    () => {
        if !$crate::db_enabled() {
            eprintln!("skipped: DNT_TEST_DB!=1");
            return;
        }
    };
}

/// Absolute path of a file under the repository's `testdata` directory.
pub fn testdata(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

/// SSH config for the compose `ssh` service plus the matching secrets.
pub fn ssh_config(auth: SshAuth) -> (SshConfig, NewSecrets) {
    let key_path = match auth {
        SshAuth::Key => Some(testdata("ssh/id_ed25519")),
        SshAuth::Password => None,
    };
    let ssh_password = match auth {
        SshAuth::Password => Some(SecretString::from(TEST_PASSWORD.to_string())),
        SshAuth::Key => None,
    };
    (
        SshConfig {
            version: 1,
            host: "127.0.0.1".into(),
            port: SSH_PORT,
            user: "dnt".into(),
            auth,
            key_path,
        },
        NewSecrets {
            db_password: None,
            ssh_password,
            ssh_key_passphrase: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_admin_user_per_target() {
        let mysql = [
            Target::MySql57,
            Target::MySql8,
            Target::MySql84,
            Target::MariaDb106,
            Target::MariaDb114,
        ];
        for t in mysql {
            assert_eq!((t.sql_kind(), t.kind()), (SqlKind::MySql, DbKind::MySql));
            assert_eq!(t.admin_user(), "root");
        }
        for t in Target::pg() {
            assert_eq!(
                (t.sql_kind(), t.kind()),
                (SqlKind::Postgres, DbKind::Postgres)
            );
            assert_eq!(t.admin_user(), "postgres");
        }
    }

    #[test]
    fn mongo_targets() {
        let ports = Target::mongo().map(|t| t.host_port());
        assert_eq!(ports, [27060, 27080]);
        for t in Target::mongo() {
            assert_eq!((t.kind(), t.admin_user()), (DbKind::Mongo, "root"));
            assert!(!Target::all().contains(&t), "the SQL matrix stays SQL");
            let cfg = t.resolved_as("dnt", TEST_PASSWORD, Some(FIXTURE_DB));
            assert_eq!(
                (cfg.kind, cfg.port, cfg.via_tunnel),
                (DbKind::Mongo, t.host_port(), false)
            );
            assert_eq!(cfg.username, "dnt");
            assert!(matches!(
                cfg.driver,
                DriverOptions::Mongo { srv: false, direct_connection: false, ref extra_hosts,
                    ref auth_source, ref replica_set }
                    if extra_hosts.is_empty() && auth_source.as_deref() == Some("admin")
                        && replica_set.is_none()
            ));
        }
        assert_eq!(Target::Mongo80.ssh_target_host(), ("mongo80", 27080));
        assert!(matches!(
            Target::Pg17.resolved(None).driver,
            DriverOptions::None
        ));
    }

    #[test]
    #[should_panic(expected = "is not a SQL target")]
    fn mongo_has_no_sql_kind() {
        Target::Mongo60.sql_kind();
    }
}
