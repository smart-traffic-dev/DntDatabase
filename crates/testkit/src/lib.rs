//! Integration-test support: the Docker target matrix, fixture paths and SSH helpers.
//! Only used as a dev-dependency by other crates.

use dnt_core::config::{ResolvedConfig, SshAuth, SshConfig};
use dnt_core::server::DbKind;
use dnt_store::repo::NewSecrets;
use secrecy::SecretString;
use std::path::PathBuf;
use std::time::Duration;

/// One database server of the Docker test matrix (`testdata/docker-compose.test.yml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    MySql57,
    MySql8,
    MariaDb106,
    MariaDb114,
    Pg12,
    Pg17,
}

pub const TEST_PASSWORD: &str = "dnt_test";
pub const FIXTURE_DB: &str = "dnt_fixture";
pub const SSH_PORT: u16 = 2222;

impl Target {
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

    pub fn kind(&self) -> DbKind {
        match self {
            Target::Pg12 | Target::Pg17 => DbKind::Postgres,
            _ => DbKind::MySql,
        }
    }

    /// Port published on the Docker host.
    pub fn host_port(&self) -> u16 {
        match self {
            Target::MySql57 => 33057,
            Target::MySql8 => 33080,
            Target::MariaDb106 => 33106,
            Target::MariaDb114 => 33114,
            Target::Pg12 => 54312,
            Target::Pg17 => 54317,
        }
    }

    /// Superuser name (`root` for MySQL family, `postgres` for PG).
    pub fn admin_user(&self) -> &'static str {
        match self.kind() {
            DbKind::MySql => "root",
            DbKind::Postgres => "postgres",
        }
    }

    /// Config for connecting directly from the host as the superuser.
    pub fn resolved(&self, db: Option<&str>) -> ResolvedConfig {
        self.resolved_as(self.admin_user(), TEST_PASSWORD, db)
    }

    /// Same as [`Target::resolved`] but with explicit credentials (e.g. `dnt_limited`).
    pub fn resolved_as(&self, user: &str, password: &str, db: Option<&str>) -> ResolvedConfig {
        ResolvedConfig {
            kind: self.kind(),
            host: "127.0.0.1".into(),
            port: self.host_port(),
            tls_server_name: "localhost".into(),
            username: user.into(),
            password: Some(SecretString::from(password.to_string())),
            database: db.map(str::to_string),
            ssl: None,
            connect_timeout: Duration::from_secs(10),
            read_only: false,
            application_name: "DntDatabase".into(),
        }
    }

    /// Compose service name and container port, as seen from inside the docker network
    /// (what an SSH tunnel through the `ssh` service forwards to).
    pub fn ssh_target_host(&self) -> (&'static str, u16) {
        match self {
            Target::MySql57 => ("mysql57", 3306),
            Target::MySql8 => ("mysql8", 3306),
            Target::MariaDb106 => ("mariadb106", 3306),
            Target::MariaDb114 => ("mariadb114", 3306),
            Target::Pg12 => ("pg12", 5432),
            Target::Pg17 => ("pg17", 5432),
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
