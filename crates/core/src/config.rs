use crate::error::DbError;
use crate::server::DbKind;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// Current schema version of the nested JSON config blobs (`ssh_json`, `ssl_json`, `options_json`).
fn config_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub group_id: Option<Uuid>,
    pub name: String,
    /// Exported as a JS number (positions are small).
    #[specta(type = f64)]
    pub sort_order: i64,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub default_db: Option<String>,
    pub env_tag: Option<EnvTag>,
    pub read_only: bool,
    pub secret_mode: SecretMode,
    pub ssh: Option<SshConfig>,
    pub ssl: Option<SslConfig>,
    pub options: ConnOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum EnvTag {
    Production,
    Staging,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum SecretMode {
    Saved,
    Ask,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SshConfig {
    #[serde(default = "config_version")]
    pub version: u32,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub key_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    Password,
    Key,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SslConfig {
    #[serde(default = "config_version")]
    pub version: u32,
    pub mode: SslMode,
    pub ca_path: Option<PathBuf>,
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// Advanced options. Missing fields fall back to defaults so `options_json = '{}'` parses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(default)]
pub struct ConnOptions {
    pub version: u32,
    pub connect_timeout_s: u32,
    pub ssh_timeout_s: u32,
    pub charset: Option<String>,
    pub application_name: Option<String>,
    /// Per-statement time limit of query tabs, in seconds; 0 = no limit.
    pub query_timeout_s: u32,
    /// Engine-specific options; `None` for SQL engines and for pre-R3 `options_json`.
    pub driver: DriverOptions,
}

/// One `host:port` pair (extra Mongo seed hosts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

/// Options that only one engine family understands.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverOptions {
    #[default]
    None,
    Mongo {
        srv: bool,
        auth_source: Option<String>,
        replica_set: Option<String>,
        direct_connection: bool,
        extra_hosts: Vec<HostPort>,
    },
    Redis {
        db: u16,
    },
}

impl Default for ConnOptions {
    fn default() -> Self {
        ConnOptions {
            version: config_version(),
            connect_timeout_s: 10,
            ssh_timeout_s: 10,
            charset: None,
            application_name: None,
            query_timeout_s: 0,
            driver: DriverOptions::None,
        }
    }
}

/// Config with secrets merged in and the (possibly tunnelled) address to dial.
/// Never serialized; `Debug` redacts the password via `SecretString`.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub tls_server_name: String,
    pub username: String,
    pub password: Option<SecretString>,
    pub database: Option<String>,
    pub ssl: Option<SslConfig>,
    pub connect_timeout: Duration,
    pub read_only: bool,
    pub application_name: String,
    pub driver: DriverOptions,
}

impl ConnectionConfig {
    /// Checks required fields; the error names the offending field (e.g. `ssh.key_path`).
    pub fn validate(&self) -> Result<(), DbError> {
        match self.kind {
            DbKind::MySql | DbKind::Postgres => {
                self.validate_sql()?;
                self.require_driver_match()
            }
            DbKind::Mongo => self.validate_mongo(),
            DbKind::Redis => self.validate_redis(),
        }
    }

    /// `options.driver` must belong to `kind` (SQL engines carry `None`).
    fn require_driver_match(&self) -> Result<(), DbError> {
        let matches = match (self.kind, &self.options.driver) {
            (DbKind::MySql | DbKind::Postgres, DriverOptions::None) => true,
            (DbKind::Mongo, DriverOptions::Mongo { .. }) => true,
            (DbKind::Redis, DriverOptions::Redis { .. }) => true,
            (DbKind::MySql | DbKind::Postgres | DbKind::Mongo | DbKind::Redis, _) => false,
        };
        require(
            matches,
            "options.driver",
            "Driver options do not match the engine",
        )
    }

    /// Spec §6.2, MongoDB: port optional with SRV, SSH only for a single host.
    fn validate_mongo(&self) -> Result<(), DbError> {
        self.require_name()?;
        self.require_driver_match()?;
        let (srv, extra_hosts) = match &self.options.driver {
            DriverOptions::Mongo {
                srv, extra_hosts, ..
            } => (srv, extra_hosts),
            // Already refused by `require_driver_match`; kept fail-closed.
            DriverOptions::None | DriverOptions::Redis { .. } => {
                return require(
                    false,
                    "options.driver",
                    "Driver options do not match the engine",
                )
            }
        };
        self.require_host()?;
        if !srv {
            self.require_port()?;
        }
        require(
            !(*srv && !extra_hosts.is_empty()),
            "options.driver",
            "An SRV connection string cannot list extra hosts",
        )?;
        require(
            extra_hosts
                .iter()
                .all(|h| !h.host.trim().is_empty() && h.port != 0),
            "options.driver.extra_hosts",
            "Every additional host needs a host and a port",
        )?;
        if let Some(ssh) = &self.ssh {
            require(
                !*srv && extra_hosts.is_empty(),
                "ssh",
                "SSH tunnel needs a single host",
            )?;
            validate_ssh(ssh)?;
        }
        self.validate_ssl()
    }

    /// Spec §6.2, Redis: `db` is left to the server.
    fn validate_redis(&self) -> Result<(), DbError> {
        self.require_name()?;
        self.require_driver_match()?;
        self.require_host()?;
        self.require_port()?;
        if let Some(ssh) = &self.ssh {
            validate_ssh(ssh)?;
        }
        self.validate_ssl()
    }

    fn require_name(&self) -> Result<(), DbError> {
        require(
            !self.name.trim().is_empty(),
            "name",
            "name must not be empty",
        )
    }

    fn require_host(&self) -> Result<(), DbError> {
        require(
            !self.host.trim().is_empty(),
            "host",
            "host must not be empty",
        )
    }

    fn require_port(&self) -> Result<(), DbError> {
        require(self.port != 0, "port", "port must be between 1 and 65535")
    }

    fn validate_ssl(&self) -> Result<(), DbError> {
        if let Some(ssl) = &self.ssl {
            let verifies = matches!(ssl.mode, SslMode::VerifyCa | SslMode::VerifyFull);
            require(
                !verifies || ssl.ca_path.is_some(),
                "ssl.ca_path",
                "SSL verification requires a CA certificate file",
            )?;
        }
        Ok(())
    }

    fn validate_sql(&self) -> Result<(), DbError> {
        require(
            !self.name.trim().is_empty(),
            "name",
            "name must not be empty",
        )?;
        require(
            !self.host.trim().is_empty(),
            "host",
            "host must not be empty",
        )?;
        require(self.port != 0, "port", "port must be between 1 and 65535")?;
        if let Some(ssh) = &self.ssh {
            require(
                !ssh.host.trim().is_empty(),
                "ssh.host",
                "SSH host must not be empty",
            )?;
            require(
                !ssh.user.trim().is_empty(),
                "ssh.user",
                "SSH user must not be empty",
            )?;
            require(
                ssh.auth != SshAuth::Key || ssh.key_path.is_some(),
                "ssh.key_path",
                "SSH key authentication requires a private key file",
            )?;
        }
        if let Some(ssl) = &self.ssl {
            let verifies = matches!(ssl.mode, SslMode::VerifyCa | SslMode::VerifyFull);
            require(
                !verifies || ssl.ca_path.is_some(),
                "ssl.ca_path",
                "SSL verification requires a CA certificate file",
            )?;
        }
        Ok(())
    }
}

/// The `ssh.*` field checks, same texts as the SQL rules.
fn validate_ssh(ssh: &SshConfig) -> Result<(), DbError> {
    require(
        !ssh.host.trim().is_empty(),
        "ssh.host",
        "SSH host must not be empty",
    )?;
    require(
        !ssh.user.trim().is_empty(),
        "ssh.user",
        "SSH user must not be empty",
    )?;
    require(
        ssh.auth != SshAuth::Key || ssh.key_path.is_some(),
        "ssh.key_path",
        "SSH key authentication requires a private key file",
    )
}

fn require(ok: bool, field: &str, message: &str) -> Result<(), DbError> {
    if ok {
        Ok(())
    } else {
        Err(DbError::Config {
            field: Some(field.into()),
            message: message.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{sample_config, sample_resolved, sample_ssh};

    #[test]
    fn conn_options_query_timeout_defaults_to_zero() {
        let o: ConnOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o.query_timeout_s, 0);
        assert_eq!(ConnOptions::default().query_timeout_s, 0);
    }

    #[test]
    fn old_options_json_has_no_driver_options() {
        let o: ConnOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o.driver, DriverOptions::None);
        let o: ConnOptions = serde_json::from_str(r#"{"version":1,"connect_timeout_s":10,"ssh_timeout_s":10,"charset":null,"application_name":null,"query_timeout_s":0}"#).unwrap();
        assert_eq!(o.driver, DriverOptions::None);
    }

    #[test]
    fn driver_options_serde_shape() {
        let r = DriverOptions::Redis { db: 3 };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"kind":"redis","db":3})
        );
        let m: DriverOptions = serde_json::from_value(serde_json::json!({"kind":"mongo","srv":true,"auth_source":"admin","replica_set":null,"direct_connection":false,"extra_hosts":[]})).unwrap();
        assert!(matches!(m, DriverOptions::Mongo { srv: true, .. }));
        assert_eq!(
            serde_json::to_value(DriverOptions::None).unwrap(),
            serde_json::json!({"kind":"none"})
        );
        let m = DriverOptions::Mongo {
            srv: false,
            auth_source: Some("admin".into()),
            replica_set: Some("rs0".into()),
            direct_connection: true,
            extra_hosts: vec![HostPort {
                host: "db2.example.com".into(),
                port: 27018,
            }],
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(
            j,
            serde_json::json!({"kind":"mongo","srv":false,"auth_source":"admin","replica_set":"rs0",
                "direct_connection":true,"extra_hosts":[{"host":"db2.example.com","port":27018}]})
        );
        assert_eq!(serde_json::from_value::<DriverOptions>(j).unwrap(), m);
    }

    #[test]
    fn validate_rejects_bad_port_and_missing_key() {
        let mut c = sample_config();
        c.port = 0;
        assert!(matches!(c.validate(), Err(DbError::Config { field: Some(f), .. }) if f == "port"));
        let mut c = sample_config();
        c.ssh = Some(SshConfig {
            auth: SshAuth::Key,
            key_path: None,
            ..sample_ssh()
        });
        assert!(
            matches!(c.validate(), Err(DbError::Config { field: Some(f), .. }) if f == "ssh.key_path")
        );
    }

    fn mongo_options(srv: bool, extra_hosts: Vec<HostPort>) -> DriverOptions {
        DriverOptions::Mongo {
            srv,
            auth_source: None,
            replica_set: None,
            direct_connection: false,
            extra_hosts,
        }
    }

    fn mongo_config() -> ConnectionConfig {
        let mut c = sample_config();
        c.kind = DbKind::Mongo;
        c.name = "Local Mongo".into();
        c.port = 27017;
        c.options.driver = mongo_options(false, vec![]);
        c
    }

    fn redis_config() -> ConnectionConfig {
        let mut c = sample_config();
        c.kind = DbKind::Redis;
        c.name = "Local Redis".into();
        c.port = 6379;
        c.options.driver = DriverOptions::Redis { db: 0 };
        c
    }

    fn error_of(c: &ConnectionConfig) -> (Option<String>, String) {
        match c.validate() {
            Err(DbError::Config { field, message }) => (field, message),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    fn err(field: &str, message: &str) -> (Option<String>, String) {
        (Some(field.into()), message.into())
    }

    const MISMATCH: &str = "Driver options do not match the engine";

    #[test]
    fn mongo_and_redis_validate_now() {
        assert!(mongo_config().validate().is_ok());
        assert!(redis_config().validate().is_ok());
    }

    #[test]
    fn mongo_rules() {
        let mut c = mongo_config();
        c.port = 0;
        assert_eq!(
            error_of(&c),
            err("port", "port must be between 1 and 65535")
        );
        c.options.driver = mongo_options(true, vec![]);
        assert!(c.validate().is_ok(), "SRV needs no port");

        let mut c = mongo_config();
        c.username = String::new();
        assert!(c.validate().is_ok(), "username is optional");

        let mut c = mongo_config();
        c.name = " ".into();
        assert_eq!(error_of(&c), err("name", "name must not be empty"));
        let mut c = mongo_config();
        c.host = String::new();
        assert_eq!(error_of(&c), err("host", "host must not be empty"));

        for driver in [DriverOptions::None, DriverOptions::Redis { db: 0 }] {
            let mut c = mongo_config();
            c.options.driver = driver;
            assert_eq!(error_of(&c), err("options.driver", MISMATCH));
        }

        let mut c = mongo_config();
        c.ssl = Some(SslConfig {
            version: 1,
            mode: SslMode::VerifyFull,
            ca_path: None,
            cert_path: None,
            key_path: None,
        });
        assert_eq!(
            error_of(&c),
            err(
                "ssl.ca_path",
                "SSL verification requires a CA certificate file"
            )
        );
        let mut c = mongo_config();
        c.ssh = Some(SshConfig {
            user: String::new(),
            ..sample_ssh()
        });
        assert_eq!(error_of(&c), err("ssh.user", "SSH user must not be empty"));
        let mut c = mongo_config();
        c.ssh = Some(sample_ssh());
        assert!(c.validate().is_ok(), "single host over SSH is fine");
    }

    #[test]
    fn redis_rules() {
        let mut c = redis_config();
        c.username = String::new();
        assert!(c.validate().is_ok(), "username is optional");
        c.options.driver = DriverOptions::Redis { db: 999 };
        assert!(c.validate().is_ok(), "db index is checked by the server");

        let mut c = redis_config();
        c.port = 0;
        assert_eq!(
            error_of(&c),
            err("port", "port must be between 1 and 65535")
        );
        let mut c = redis_config();
        c.host = "  ".into();
        assert_eq!(error_of(&c), err("host", "host must not be empty"));
        let mut c = redis_config();
        c.name = String::new();
        assert_eq!(error_of(&c), err("name", "name must not be empty"));

        for driver in [DriverOptions::None, mongo_options(false, vec![])] {
            let mut c = redis_config();
            c.options.driver = driver;
            assert_eq!(error_of(&c), err("options.driver", MISMATCH));
        }

        let mut c = redis_config();
        c.ssl = Some(SslConfig {
            version: 1,
            mode: SslMode::VerifyCa,
            ca_path: None,
            cert_path: None,
            key_path: None,
        });
        assert_eq!(
            error_of(&c),
            err(
                "ssl.ca_path",
                "SSL verification requires a CA certificate file"
            )
        );
        let mut c = redis_config();
        c.ssh = Some(sample_ssh());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ssh_needs_a_single_host() {
        let extra = || HostPort {
            host: "db2".into(),
            port: 27018,
        };
        let mut c = mongo_config();
        c.ssh = Some(sample_ssh());
        c.options.driver = mongo_options(true, vec![]);
        assert_eq!(error_of(&c), err("ssh", "SSH tunnel needs a single host"));
        c.options.driver = mongo_options(false, vec![extra()]);
        assert_eq!(error_of(&c), err("ssh", "SSH tunnel needs a single host"));
    }

    #[test]
    fn srv_with_extra_hosts_is_refused() {
        let mut c = mongo_config();
        c.options.driver = mongo_options(
            true,
            vec![HostPort {
                host: "db2".into(),
                port: 27018,
            }],
        );
        assert_eq!(
            error_of(&c),
            err(
                "options.driver",
                "An SRV connection string cannot list extra hosts"
            )
        );
    }

    #[test]
    fn extra_host_needs_host_and_port() {
        let ok = HostPort {
            host: "db2".into(),
            port: 27018,
        };
        let mut c = mongo_config();
        c.options.driver = mongo_options(false, vec![ok.clone()]);
        assert!(c.validate().is_ok());
        for bad in [
            HostPort {
                host: " ".into(),
                port: 27018,
            },
            HostPort {
                host: "db3".into(),
                port: 0,
            },
        ] {
            c.options.driver = mongo_options(false, vec![ok.clone(), bad]);
            assert_eq!(
                error_of(&c),
                err(
                    "options.driver.extra_hosts",
                    "Every additional host needs a host and a port"
                )
            );
        }
    }

    #[test]
    fn mysql_and_pg_rules_unchanged() {
        for kind in [DbKind::MySql, DbKind::Postgres] {
            let mut c = sample_config();
            c.kind = kind;
            assert!(c.validate().is_ok());
            c.name = String::new();
            assert_eq!(error_of(&c), err("name", "name must not be empty"));
            c.name = "x".into();
            c.port = 0;
            assert_eq!(
                error_of(&c),
                err("port", "port must be between 1 and 65535")
            );
            c.port = 3306;
            c.options.driver = DriverOptions::Redis { db: 0 };
            assert_eq!(error_of(&c), err("options.driver", MISMATCH));
        }
        // The R1 field-by-field cases still hold.
        validate_checks_every_field();
    }

    fn field_of(c: &ConnectionConfig) -> Option<String> {
        match c.validate() {
            Err(DbError::Config { field, .. }) => field,
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn validate_checks_every_field() {
        assert!(sample_config().validate().is_ok());
        let mut c = sample_config();
        c.ssh = Some(sample_ssh());
        assert!(c.validate().is_ok());

        let mut c = sample_config();
        c.name = "  ".into();
        assert_eq!(field_of(&c).as_deref(), Some("name"));
        let mut c = sample_config();
        c.host = String::new();
        assert_eq!(field_of(&c).as_deref(), Some("host"));
        let mut c = sample_config();
        c.ssh = Some(SshConfig {
            host: String::new(),
            ..sample_ssh()
        });
        assert_eq!(field_of(&c).as_deref(), Some("ssh.host"));
        let mut c = sample_config();
        c.ssh = Some(SshConfig {
            user: String::new(),
            ..sample_ssh()
        });
        assert_eq!(field_of(&c).as_deref(), Some("ssh.user"));

        let ssl = |mode, ca: Option<&str>| SslConfig {
            version: 1,
            mode,
            ca_path: ca.map(PathBuf::from),
            cert_path: None,
            key_path: None,
        };
        let mut c = sample_config();
        c.ssl = Some(ssl(SslMode::VerifyFull, None));
        assert_eq!(field_of(&c).as_deref(), Some("ssl.ca_path"));
        c.ssl = Some(ssl(SslMode::VerifyCa, None));
        assert_eq!(field_of(&c).as_deref(), Some("ssl.ca_path"));
        c.ssl = Some(ssl(SslMode::VerifyCa, Some("C:\\certs\\ca.pem")));
        assert!(c.validate().is_ok());
        c.ssl = Some(ssl(SslMode::Require, None));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn resolved_config_debug_redacts() {
        let r = sample_resolved(Some("hunter2"));
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    #[test]
    fn serde_shapes() {
        let mut c = sample_config();
        c.env_tag = Some(EnvTag::Production);
        c.ssh = Some(sample_ssh());
        c.ssl = Some(SslConfig {
            version: 1,
            mode: SslMode::VerifyCa,
            ca_path: Some(PathBuf::from("ca.pem")),
            cert_path: None,
            key_path: None,
        });
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["kind"], "mysql");
        assert_eq!(j["env_tag"], "production");
        assert_eq!(j["secret_mode"], "saved");
        assert_eq!(j["ssh"]["auth"], "password");
        assert_eq!(j["ssl"]["mode"], "verify_ca");
        assert_eq!(j["ssl"]["ca_path"], "ca.pem");
        let back: ConnectionConfig = serde_json::from_value(j).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn nested_json_defaults() {
        let o: ConnOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(o, ConnOptions::default());
        assert_eq!(
            (o.version, o.connect_timeout_s, o.ssh_timeout_s),
            (1, 10, 10)
        );
        let s: SshConfig = serde_json::from_str(
            r#"{"host":"h","port":22,"user":"u","auth":"key","key_path":"k"}"#,
        )
        .unwrap();
        assert_eq!(s.version, 1);
        let s: SslConfig = serde_json::from_str(
            r#"{"mode":"verify_full","ca_path":null,"cert_path":null,"key_path":null}"#,
        )
        .unwrap();
        assert_eq!((s.version, s.mode), (1, SslMode::VerifyFull));
    }
}
