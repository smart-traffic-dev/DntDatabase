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
}

impl ConnectionConfig {
    /// Checks required fields; the error names the offending field (e.g. `ssh.key_path`).
    pub fn validate(&self) -> Result<(), DbError> {
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
