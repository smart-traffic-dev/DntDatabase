use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SshStage {
    Connect,
    Auth,
    HostKeyMismatch,
    Channel,
}

impl SshStage {
    fn as_str(self) -> &'static str {
        match self {
            SshStage::Connect => "connect",
            SshStage::Auth => "auth",
            SshStage::HostKeyMismatch => "host_key_mismatch",
            SshStage::Channel => "channel",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConnectStage {
    ResolveSecrets,
    OpenTunnel,
    ConnectDb,
    DetectServer,
    Execute,
}

impl ConnectStage {
    fn as_str(self) -> &'static str {
        match self {
            ConnectStage::ResolveSecrets => "resolve_secrets",
            ConnectStage::OpenTunnel => "open_tunnel",
            ConnectStage::ConnectDb => "connect_db",
            ConnectStage::DetectServer => "detect_server",
            ConnectStage::Execute => "execute",
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum DbError {
    #[error("invalid configuration: {message}")]
    Config {
        field: Option<String>,
        message: String,
    },
    #[error("secret store error: {message}")]
    Secret { message: String },
    #[error("SSH error ({}): {message}", .stage.as_str())]
    Ssh { stage: SshStage, message: String },
    #[error("unknown SSH host key for {host}:{port} ({fingerprint})")]
    SshHostKeyUnknown {
        host: String,
        port: u16,
        fingerprint: String,
        key_openssh: String,
    },
    #[error("TLS error: {message}")]
    Tls { message: String },
    #[error("connection failed: {message}")]
    Connect { message: String },
    #[error("timed out during {}", .stage.as_str())]
    Timeout { stage: ConnectStage },
    #[error("authentication failed: {message}")]
    Auth { message: String },
    #[error("server error: {message}")]
    Server {
        code: Option<String>,
        sqlstate: Option<String>,
        message: String,
        position: Option<u32>,
    },
    #[error("connection lost")]
    ConnectionLost { in_transaction: bool },
    #[error("cancelled")]
    Cancelled,
    #[error("{feature} is not supported on server version {version}")]
    Unsupported { feature: String, version: String },
    #[error("internal error: {message}")]
    Internal { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct AppError {
    pub kind: String,
    pub message: String,
    pub detail: Option<String>,
    pub stage: Option<String>,
    pub server_code: Option<String>,
    pub sqlstate: Option<String>,
    pub position: Option<u32>,
    pub retryable: bool,
    pub hint: Option<String>,
    pub fingerprint: Option<String>,
    pub key_openssh: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub field: Option<String>,
}

impl AppError {
    fn base(kind: &str, message: impl Into<String>) -> Self {
        AppError {
            kind: kind.into(),
            message: message.into(),
            detail: None,
            stage: None,
            server_code: None,
            sqlstate: None,
            position: None,
            retryable: false,
            hint: None,
            fingerprint: None,
            key_openssh: None,
            host: None,
            port: None,
            field: None,
        }
    }
}

impl From<DbError> for AppError {
    fn from(err: DbError) -> Self {
        let hint = hint(&err).map(String::from);
        let mut a = match err {
            DbError::Config { field, message } => AppError {
                detail: Some(message),
                field,
                ..AppError::base("config", "Invalid connection settings.")
            },
            DbError::Secret { message } => AppError {
                detail: Some(message),
                ..AppError::base("secret", "Could not access the secret store.")
            },
            DbError::Ssh { stage, message } => AppError {
                detail: Some(message),
                stage: Some(stage.as_str().into()),
                ..AppError::base("ssh", "SSH connection failed.")
            },
            DbError::SshHostKeyUnknown {
                host,
                port,
                fingerprint,
                key_openssh,
            } => AppError {
                fingerprint: Some(fingerprint),
                key_openssh: Some(key_openssh),
                host: Some(host),
                port: Some(port),
                ..AppError::base(
                    "ssh_host_key_unknown",
                    "The SSH host key is not trusted yet.",
                )
            },
            DbError::Tls { message } => AppError {
                detail: Some(message),
                ..AppError::base("tls", "TLS handshake failed.")
            },
            DbError::Connect { message } => AppError {
                detail: Some(message),
                retryable: true,
                ..AppError::base("connect", "Could not connect to the database server.")
            },
            DbError::Timeout { stage } => AppError {
                stage: Some(stage.as_str().into()),
                retryable: true,
                ..AppError::base("timeout", "The operation timed out.")
            },
            DbError::Auth { message } => AppError {
                detail: Some(message),
                ..AppError::base("auth", "Authentication failed.")
            },
            DbError::Server {
                code,
                sqlstate,
                message,
                position,
            } => AppError {
                detail: Some(message.clone()),
                server_code: code,
                sqlstate,
                position,
                ..AppError::base("server", message)
            },
            DbError::ConnectionLost { in_transaction } => AppError {
                retryable: !in_transaction,
                ..AppError::base(
                    "connection_lost",
                    if in_transaction {
                        "The connection was lost during a transaction; it was rolled back."
                    } else {
                        "The connection to the server was lost."
                    },
                )
            },
            DbError::Cancelled => AppError::base("cancelled", "The operation was cancelled."),
            DbError::Unsupported { feature, version } => AppError {
                detail: Some(format!("{feature} (server version {version})")),
                ..AppError::base(
                    "unsupported",
                    "This feature is not supported by the server.",
                )
            },
            DbError::Internal { message } => AppError {
                detail: Some(message),
                ..AppError::base("internal", "An internal error occurred.")
            },
        };
        a.hint = hint;
        a
    }
}

const HINT_DB_MISSING: &str = "Database does not exist.";
const HINT_ACCESS_DENIED: &str = "Wrong user/password, or this user may not log in from your host.";
const HINT_PG_PASSWORD: &str = "Wrong user/password.";
const HINT_PG_HBA: &str = "pg_hba.conf rejects connections from this host/user.";
const HINT_REFUSED: &str =
    "Connection refused: check host, port, firewall, or whether an SSH tunnel is needed.";

fn is_access_denied(msg: &str) -> bool {
    msg.contains("(1045)") || msg.contains("[1045]")
}

/// Human-friendly hint for common failures. Matching on messages is case-insensitive.
pub fn hint(err: &DbError) -> Option<&'static str> {
    match err {
        DbError::Server {
            code,
            sqlstate,
            message,
            ..
        } => {
            match code.as_deref() {
                Some("1049") => return Some(HINT_DB_MISSING),
                Some("1045") => return Some(HINT_ACCESS_DENIED),
                _ => {}
            }
            match sqlstate.as_deref() {
                Some("3D000") => return Some(HINT_DB_MISSING),
                Some("28000") => return Some(HINT_PG_HBA),
                Some("28P01") => return Some(HINT_PG_PASSWORD),
                _ => {}
            }
            if is_access_denied(&message.to_lowercase()) {
                return Some(HINT_ACCESS_DENIED);
            }
            None
        }
        DbError::Auth { message } => {
            let m = message.to_lowercase();
            if is_access_denied(&m) || m.contains("access denied") {
                Some(HINT_ACCESS_DENIED)
            } else if m.contains("[28p01]") {
                // PostgreSQL auth failures carry their SQLSTATE in brackets (driver-pg).
                Some(HINT_PG_PASSWORD)
            } else if m.contains("[28000]") {
                Some(HINT_PG_HBA)
            } else {
                None
            }
        }
        DbError::Connect { message } => {
            let m = message.to_lowercase();
            if m.contains("10061")
                || m.contains("econnrefused")
                || m.contains("refused")
                || m.contains("(2003)")
            {
                Some(HINT_REFUSED)
            } else {
                None
            }
        }
        DbError::Ssh {
            stage: SshStage::Auth,
            ..
        } => Some("SSH authentication failed. If the key has a passphrase, enter it."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_access_denied_mysql() {
        let e = DbError::Auth {
            message: "Access denied for user 'x'@'1.2.3.4' (using password: YES) [1045]".into(),
        };
        assert_eq!(
            hint(&e),
            Some("Wrong user/password, or this user may not log in from your host.")
        );
    }
    #[test]
    fn hint_by_server_code() {
        let mk = |code: &str, st: &str| DbError::Server {
            code: Some(code.into()),
            sqlstate: Some(st.into()),
            message: "m".into(),
            position: None,
        };
        assert_eq!(hint(&mk("1049", "42000")), Some("Database does not exist."));
        assert_eq!(hint(&mk("", "3D000")), Some("Database does not exist."));
        assert_eq!(
            hint(&mk("", "28000")),
            Some("pg_hba.conf rejects connections from this host/user.")
        );
        assert_eq!(hint(&mk("", "28P01")), Some("Wrong user/password."));
    }
    #[test]
    fn hint_pg_auth_errors() {
        let auth = |m: &str| DbError::Auth { message: m.into() };
        assert_eq!(
            hint(&auth(
                "password authentication failed for user \"postgres\" [28P01]"
            )),
            Some("Wrong user/password.")
        );
        assert_eq!(
            hint(&auth("no pg_hba.conf entry for host \"1.2.3.4\" [28000]")),
            Some("pg_hba.conf rejects connections from this host/user.")
        );
        assert_eq!(hint(&auth("something else")), None);
    }
    #[test]
    fn hint_conn_refused() {
        let e = DbError::Connect {
            message: "No connection could be made because the target machine actively refused it. (os error 10061)".into(),
        };
        assert_eq!(
            hint(&e),
            Some("Connection refused: check host, port, firewall, or whether an SSH tunnel is needed.")
        );
    }
    #[test]
    fn hint_mysql_2003() {
        let e = DbError::Connect {
            message: "Can't connect to MySQL server (2003)".into(),
        };
        assert!(hint(&e).unwrap().starts_with("Connection refused"));
    }
    #[test]
    fn hint_ssh_auth_mentions_passphrase() {
        let e = DbError::Ssh {
            stage: SshStage::Auth,
            message: "key is encrypted".into(),
        };
        assert_eq!(
            hint(&e),
            Some("SSH authentication failed. If the key has a passphrase, enter it.")
        );
    }
    #[test]
    fn app_error_keeps_raw_detail_and_kind() {
        let a: AppError = DbError::Server {
            code: Some("1064".into()),
            sqlstate: Some("42000".into()),
            message: "You have an error…".into(),
            position: Some(17),
        }
        .into();
        assert_eq!(a.kind, "server");
        assert_eq!(a.detail.as_deref(), Some("You have an error…"));
        assert_eq!(a.position, Some(17));
        assert!(!a.retryable);
    }
    #[test]
    fn host_key_unknown_carries_fingerprint() {
        let a: AppError = DbError::SshHostKeyUnknown {
            host: "h".into(),
            port: 22,
            fingerprint: "SHA256:abc".into(),
            key_openssh: "ssh-ed25519 AAAA".into(),
        }
        .into();
        assert_eq!(a.kind, "ssh_host_key_unknown");
        assert_eq!(a.fingerprint.as_deref(), Some("SHA256:abc"));
        assert_eq!(a.key_openssh.as_deref(), Some("ssh-ed25519 AAAA"));
        assert_eq!(a.port, Some(22));
    }
    #[test]
    fn retryable_and_stage() {
        let t: AppError = DbError::Timeout {
            stage: ConnectStage::ConnectDb,
        }
        .into();
        assert!(t.retryable);
        assert_eq!(t.stage.as_deref(), Some("connect_db"));
        let l: AppError = DbError::ConnectionLost {
            in_transaction: true,
        }
        .into();
        assert!(!l.retryable);
        let l: AppError = DbError::ConnectionLost {
            in_transaction: false,
        }
        .into();
        assert!(l.retryable);
        let s: AppError = DbError::Ssh {
            stage: SshStage::HostKeyMismatch,
            message: "x".into(),
        }
        .into();
        assert_eq!(s.stage.as_deref(), Some("host_key_mismatch"));
    }
}
