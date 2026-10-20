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

/// How long one completed stage of the connect pipeline took.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct StageTiming {
    pub stage: ConnectStage,
    /// Exported as a JS number (milliseconds).
    #[specta(type = f64)]
    pub ms: u64,
}

/// `Clone` so one result can be shared by several waiters (e.g. a failed SchemaCache load).
#[derive(thiserror::Error, Debug, Clone)]
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
    /// An operation ran past its time limit.
    ///
    /// `outcome_unknown` is only ever true for an executed statement (`stage: Execute`)
    /// that may have changed data: the limit expired, the statement was told to stop,
    /// but its completion was never observed, so it may or may not have been applied
    /// (and committed). When false the statement is known not to have taken effect, or
    /// it could not change anything.
    ///
    /// `was_in_tx`: the statement was sent inside a transaction (still open on MySQL,
    /// aborted on PostgreSQL), so running it again is not a plain retry. It is `false`
    /// when no statement was sent (a cancel before sending, or a non-statement stage),
    /// even if the session has a transaction open: it describes the stopped statement,
    /// not the session's transaction state (that is `AppError::tx_state`).
    #[error("timed out during {}{}", .stage.as_str(), outcome_note(.outcome_unknown))]
    Timeout {
        stage: ConnectStage,
        outcome_unknown: bool,
        was_in_tx: bool,
    },
    #[error("authentication failed: {message}")]
    Auth { message: String },
    #[error("server error: {message}")]
    Server {
        code: Option<String>,
        sqlstate: Option<String>,
        message: String,
        position: Option<u32>,
    },
    /// The connection to the server dropped (or had to be abandoned) while working.
    ///
    /// `was_in_tx`: a transaction was open on it, which the server has rolled back; the
    /// session's transaction state is reset. `outcome_unknown`: the statement that was
    /// running may have committed before the connection went away (same meaning as on
    /// [`DbError::Timeout`]). `message`: what happened, for diagnostics (the driver
    /// error, why replacing the connection failed, which session state was reset).
    /// Never holds secrets.
    #[error("connection lost{}: {message}", outcome_note(.outcome_unknown))]
    ConnectionLost {
        was_in_tx: bool,
        outcome_unknown: bool,
        message: String,
    },
    /// The user (or a superseding call) cancelled the operation. `outcome_unknown`: the
    /// statement was killed while it may already have applied part of its work (for
    /// example a `CALL` whose inner statements autocommit, or a non-transactional table
    /// outside a transaction).
    /// `was_in_tx`: as on [`DbError::Timeout`]; `false` for a cancel that stopped the
    /// script before the statement was sent, whatever the session's transaction state.
    #[error("cancelled{}", outcome_note(.outcome_unknown))]
    Cancelled {
        outcome_unknown: bool,
        was_in_tx: bool,
    },
    /// The query tab already has a run in progress.
    #[error("the tab is already running a query")]
    Busy,
    /// The action needs the tab's open transaction to be committed or rolled back first.
    #[error("a transaction is open")]
    TransactionOpen,
    /// The run plan the UI confirmed is gone (expired or replaced).
    #[error("the run plan expired")]
    PlanExpired,
    /// The guard blocked `count` statements that change data on a read-only connection.
    #[error("{count} statement(s) blocked on a read-only connection")]
    Blocked { count: u32 },
    /// The guard needs the user to confirm `count` statements on a Production connection.
    #[error("{count} statement(s) need confirmation on Production")]
    ConfirmationRequired { count: u32 },
    #[error("{feature} is not supported on server version {version}")]
    Unsupported { feature: String, version: String },
    #[error("internal error: {message}")]
    Internal { message: String },
    /// A browse call (or similar) for a connection that has no runtime.
    #[error("the connection is not open")]
    NotOpen,
    /// An error of the connect pipeline, with the stage that failed and the timings of
    /// the stages that completed before it. `AppError` keeps the inner error's kind.
    #[error("{source} (during {})", .stage.as_str())]
    AtStage {
        stage: ConnectStage,
        timings: Vec<StageTiming>,
        source: Box<DbError>,
    },
}

fn outcome_note(outcome_unknown: &bool) -> &'static str {
    if *outcome_unknown {
        " (the statement may have been applied)"
    } else {
        ""
    }
}

impl DbError {
    /// A [`DbError::Timeout`] at `stage` that applied nothing (`outcome_unknown: false`):
    /// every timeout except one of a statement that may have changed data.
    pub fn timeout(stage: ConnectStage) -> DbError {
        DbError::Timeout {
            stage,
            outcome_unknown: false,
            was_in_tx: false,
        }
    }

    /// A [`DbError::Cancelled`] that applied nothing (`outcome_unknown: false`).
    pub fn cancelled() -> DbError {
        DbError::Cancelled {
            outcome_unknown: false,
            was_in_tx: false,
        }
    }

    /// Wraps `err` with the connect stage it happened in. An error that already carries
    /// a stage keeps it (the innermost stage is the one that failed).
    pub fn at_stage(stage: ConnectStage, timings: Vec<StageTiming>, err: DbError) -> DbError {
        match err {
            DbError::AtStage { .. } => err,
            err => DbError::AtStage {
                stage,
                timings,
                source: Box::new(err),
            },
        }
    }

    /// The error without its connect-stage wrapper.
    pub fn unstaged(&self) -> &DbError {
        match self {
            DbError::AtStage { source, .. } => source.unstaged(),
            other => other,
        }
    }

    /// Owned [`DbError::unstaged`].
    pub fn into_unstaged(self) -> DbError {
        match self {
            DbError::AtStage { source, .. } => source.into_unstaged(),
            other => other,
        }
    }

    /// The connect stage this error was reported at, if any.
    pub fn stage(&self) -> Option<ConnectStage> {
        match self {
            DbError::AtStage { stage, .. } => Some(*stage),
            _ => None,
        }
    }
}

/// `DbError::Secret` message telling the UI to prompt for the missing secret(s) and call
/// again; it becomes `AppError { kind: "password_required" }`.
pub const PASSWORD_REQUIRED: &str = "password_required";

/// Start of the message of [`ssh_channel_error`]; [`hint`] reads the target back from it.
const CHANNEL_PREFIX: &str = "the SSH server could not open a channel to ";

/// The SSH server could not open a `direct-tcpip` channel to `host:port` (the database
/// behind the bastion is unreachable from it). `cause` is the server's reason.
pub fn ssh_channel_error(host: &str, port: u16, cause: &str) -> DbError {
    DbError::Ssh {
        stage: SshStage::Channel,
        message: format!("{CHANNEL_PREFIX}{host}:{port}: {cause}"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct AppError {
    pub kind: String,
    pub message: String,
    pub detail: Option<String>,
    pub stage: Option<String>,
    /// Connect errors: the stages that completed before the failing one.
    pub timings: Option<Vec<StageTiming>>,
    pub server_code: Option<String>,
    pub sqlstate: Option<String>,
    pub position: Option<u32>,
    pub retryable: bool,
    /// `timeout` / `connection_lost` / `cancelled`: the statement may have been applied
    /// (the server's outcome is unknown). The UI must show such an error to the user,
    /// never treat it as a silent cancel.
    pub outcome_unknown: bool,
    pub hint: Option<String>,
    pub fingerprint: Option<String>,
    pub key_openssh: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub field: Option<String>,
    /// Query errors: the tab's transaction state after the failure (`idle` / `active` /
    /// `aborted`, see `TxState::as_str`), so the status bar updates at once. Filled by
    /// the query layer; `None` from `From<DbError>`.
    pub tx_state: Option<String>,
}

impl AppError {
    fn base(kind: &str, message: impl Into<String>) -> Self {
        AppError {
            kind: kind.into(),
            message: message.into(),
            detail: None,
            stage: None,
            timings: None,
            server_code: None,
            sqlstate: None,
            position: None,
            retryable: false,
            outcome_unknown: false,
            hint: None,
            fingerprint: None,
            key_openssh: None,
            host: None,
            port: None,
            field: None,
            tx_state: None,
        }
    }
}

/// "1 statement" / "`n` statements".
fn statements(n: u32) -> String {
    if n == 1 {
        "1 statement".into()
    } else {
        format!("{n} statements")
    }
}

/// `message` as a sentence: capital first letter, final punctuation.
fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let mut out = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => return "Invalid settings.".into(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

impl From<DbError> for AppError {
    fn from(err: DbError) -> Self {
        let hint = hint(&err);
        let mut a = match err {
            DbError::AtStage {
                stage,
                timings,
                source,
            } => {
                let mut inner = AppError::from(*source);
                // SSH and timeout errors name their own (more precise) stage.
                inner.stage.get_or_insert_with(|| stage.as_str().into());
                inner.timings = Some(timings);
                return inner;
            }
            // Config messages are written for users (they name the field and the problem).
            DbError::Config { field, message } => AppError {
                field,
                ..AppError::base("config", sentence(&message))
            },
            DbError::Secret { message } if message == PASSWORD_REQUIRED => {
                AppError::base("password_required", "A password is required.")
            }
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
            // Re-running a statement that may have been applied could apply it twice;
            // inside a transaction (aborted on PG) it is not a plain retry either.
            DbError::Timeout {
                stage,
                outcome_unknown,
                was_in_tx,
            } => AppError {
                stage: Some(stage.as_str().into()),
                retryable: !outcome_unknown && !was_in_tx,
                outcome_unknown,
                ..AppError::base(
                    "timeout",
                    if outcome_unknown {
                        "The statement timed out and may have been applied."
                    } else {
                        "The operation timed out."
                    },
                )
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
            DbError::ConnectionLost {
                was_in_tx,
                outcome_unknown,
                message,
            } => AppError {
                retryable: !was_in_tx && !outcome_unknown,
                outcome_unknown,
                detail: (!message.is_empty()).then_some(message),
                ..AppError::base(
                    "connection_lost",
                    if outcome_unknown {
                        "The connection was lost and the statement may have been applied."
                    } else if was_in_tx {
                        "The connection was lost during a transaction; it was rolled back."
                    } else {
                        "The connection to the server was lost."
                    },
                )
            },
            DbError::Cancelled {
                outcome_unknown,
                was_in_tx,
            } => AppError {
                retryable: !outcome_unknown && !was_in_tx,
                outcome_unknown,
                ..AppError::base(
                    "cancelled",
                    if outcome_unknown {
                        "The statement was cancelled and may have been applied."
                    } else {
                        "The operation was cancelled."
                    },
                )
            },
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
            DbError::NotOpen => AppError {
                retryable: true,
                ..AppError::base("not_open", "This connection is not open.")
            },
            DbError::Busy => AppError::base("busy", "This tab is already running a query."),
            DbError::TransactionOpen => AppError::base(
                "transaction_open",
                "Commit or roll back the open transaction first.",
            ),
            DbError::PlanExpired => {
                AppError::base("plan_expired", "The run plan expired; run it again.")
            }
            DbError::Blocked { count } => AppError {
                detail: Some(statements(count)),
                ..AppError::base(
                    "blocked",
                    "Statements that change data are blocked on this read-only connection.",
                )
            },
            DbError::ConfirmationRequired { count } => AppError {
                detail: Some(statements(count)),
                ..AppError::base(
                    "confirmation_required",
                    "Running these statements on Production needs confirmation.",
                )
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
pub fn hint(err: &DbError) -> Option<String> {
    match err {
        DbError::AtStage { source, .. } => hint(source),
        DbError::Ssh {
            stage: SshStage::Channel,
            message,
        } => {
            let rest = message.strip_prefix(CHANNEL_PREFIX)?;
            let (target, _cause) = rest.split_once(": ")?;
            Some(format!("The SSH server could not reach {target}."))
        }
        other => static_hint(other).map(String::from),
    }
}

fn static_hint(err: &DbError) -> Option<&'static str> {
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
            static_hint(&e),
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
        assert_eq!(
            static_hint(&mk("1049", "42000")),
            Some("Database does not exist.")
        );
        assert_eq!(
            static_hint(&mk("", "3D000")),
            Some("Database does not exist.")
        );
        assert_eq!(
            static_hint(&mk("", "28000")),
            Some("pg_hba.conf rejects connections from this host/user.")
        );
        assert_eq!(static_hint(&mk("", "28P01")), Some("Wrong user/password."));
    }
    #[test]
    fn hint_pg_auth_errors() {
        let auth = |m: &str| DbError::Auth { message: m.into() };
        assert_eq!(
            static_hint(&auth(
                "password authentication failed for user \"postgres\" [28P01]"
            )),
            Some("Wrong user/password.")
        );
        assert_eq!(
            static_hint(&auth("no pg_hba.conf entry for host \"1.2.3.4\" [28000]")),
            Some("pg_hba.conf rejects connections from this host/user.")
        );
        assert_eq!(static_hint(&auth("something else")), None);
    }
    #[test]
    fn hint_conn_refused() {
        let e = DbError::Connect {
            message: "No connection could be made because the target machine actively refused it. (os error 10061)".into(),
        };
        assert_eq!(
            static_hint(&e),
            Some("Connection refused: check host, port, firewall, or whether an SSH tunnel is needed.")
        );
    }
    #[test]
    fn hint_mysql_2003() {
        let e = DbError::Connect {
            message: "Can't connect to MySQL server (2003)".into(),
        };
        assert!(static_hint(&e).unwrap().starts_with("Connection refused"));
    }
    #[test]
    fn hint_ssh_auth_mentions_passphrase() {
        let e = DbError::Ssh {
            stage: SshStage::Auth,
            message: "key is encrypted".into(),
        };
        assert_eq!(
            static_hint(&e),
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
    fn config_message_is_the_specific_text() {
        let a: AppError = DbError::Config {
            field: Some("port".into()),
            message: "port must be between 1 and 65535".into(),
        }
        .into();
        assert_eq!(a.kind, "config");
        assert_eq!(a.field.as_deref(), Some("port"));
        assert_eq!(a.message, "Port must be between 1 and 65535.");
        let a: AppError = DbError::Config {
            field: Some("passphrase".into()),
            message: "Passphrase is incorrect.".into(),
        }
        .into();
        assert_eq!(a.message, "Passphrase is incorrect.");
    }

    #[test]
    fn password_required_has_its_own_kind() {
        let a: AppError = DbError::Secret {
            message: PASSWORD_REQUIRED.into(),
        }
        .into();
        assert_eq!(a.kind, "password_required");
        assert_eq!(a.message, "A password is required.");
        let other: AppError = DbError::Secret {
            message: "read db_password failed: denied".into(),
        }
        .into();
        assert_eq!(other.kind, "secret");
    }

    #[test]
    fn not_open_has_its_own_kind() {
        let a: AppError = DbError::NotOpen.into();
        assert_eq!(a.kind, "not_open");
        assert_eq!(a.message, "This connection is not open.");
        assert!(a.retryable);
    }

    fn timings() -> Vec<StageTiming> {
        vec![
            StageTiming {
                stage: ConnectStage::ResolveSecrets,
                ms: 1,
            },
            StageTiming {
                stage: ConnectStage::OpenTunnel,
                ms: 40,
            },
        ]
    }

    #[test]
    fn at_stage_sets_stage_and_timings_and_keeps_kind_and_hint() {
        let e = DbError::at_stage(
            ConnectStage::ConnectDb,
            timings(),
            DbError::Auth {
                message: "Access denied for user 'x'@'h' (using password: YES) [1045]".into(),
            },
        );
        assert!(matches!(e.unstaged(), DbError::Auth { .. }));
        let a: AppError = e.into();
        assert_eq!(a.kind, "auth");
        assert_eq!(a.stage.as_deref(), Some("connect_db"));
        assert_eq!(a.hint.as_deref(), Some(HINT_ACCESS_DENIED));
        assert_eq!(a.timings, Some(timings()));
    }

    #[test]
    fn at_stage_keeps_the_ssh_stage_of_the_error() {
        let a: AppError = DbError::at_stage(
            ConnectStage::OpenTunnel,
            vec![],
            DbError::Ssh {
                stage: SshStage::Auth,
                message: "rejected".into(),
            },
        )
        .into();
        assert_eq!(a.kind, "ssh");
        assert_eq!(a.stage.as_deref(), Some("auth"));
        assert_eq!(a.timings, Some(vec![]));
        // Wrapping twice keeps the innermost (first) stage.
        let twice = DbError::at_stage(
            ConnectStage::DetectServer,
            vec![],
            DbError::at_stage(ConnectStage::ConnectDb, vec![], DbError::cancelled()),
        );
        assert!(matches!(twice.unstaged(), DbError::Cancelled { .. }));
        assert_eq!(twice.stage(), Some(ConnectStage::ConnectDb));
    }

    #[test]
    fn ssh_channel_error_names_the_unreachable_target() {
        let e = ssh_channel_error("db.internal", 5432, "Connection refused");
        let a: AppError = e.into();
        assert_eq!(a.kind, "ssh");
        assert_eq!(a.stage.as_deref(), Some("channel"));
        assert_eq!(
            a.hint.as_deref(),
            Some("The SSH server could not reach db.internal:5432.")
        );
        assert!(a.detail.unwrap().contains("Connection refused"));
        let other: AppError = DbError::Ssh {
            stage: SshStage::Channel,
            message: "cannot listen on a local port".into(),
        }
        .into();
        assert_eq!(other.hint, None);
    }

    #[test]
    fn timeout_with_unknown_outcome_says_it_may_have_been_applied() {
        let a: AppError = DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: true,
            was_in_tx: false,
        }
        .into();
        assert_eq!(a.kind, "timeout");
        assert_eq!(a.stage.as_deref(), Some("execute"));
        assert_eq!(
            a.message,
            "The statement timed out and may have been applied."
        );
        // Running it again could apply it twice.
        assert!(!a.retryable);
        let known: AppError = DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: false,
            was_in_tx: false,
        }
        .into();
        assert_eq!(known.message, "The operation timed out.");
        assert!(known.retryable);
        assert!(DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: true,
            was_in_tx: false,
        }
        .to_string()
        .contains("may have been applied"));
    }

    #[test]
    fn connection_lost_keeps_its_message_as_detail() {
        let a: AppError = DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message: "Lost connection to MySQL server during query".into(),
        }
        .into();
        assert_eq!(a.kind, "connection_lost");
        assert_eq!(a.message, "The connection to the server was lost.");
        assert_eq!(
            a.detail.as_deref(),
            Some("Lost connection to MySQL server during query")
        );
        let empty: AppError = DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message: String::new(),
        }
        .into();
        assert_eq!(empty.detail, None);
    }

    #[test]
    fn connection_lost_with_unknown_outcome_is_not_retryable() {
        for was_in_tx in [false, true] {
            let a: AppError = DbError::ConnectionLost {
                was_in_tx,
                outcome_unknown: true,
                message: "gone".into(),
            }
            .into();
            assert_eq!(a.kind, "connection_lost");
            assert_eq!(
                a.message,
                "The connection was lost and the statement may have been applied."
            );
            assert!(!a.retryable);
        }
        assert!(DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: true,
            message: "gone".into(),
        }
        .to_string()
        .contains("may have been applied"));
    }

    #[test]
    fn app_error_carries_outcome_unknown_as_data() {
        for unknown in [false, true] {
            let errors = [
                DbError::Timeout {
                    stage: ConnectStage::Execute,
                    outcome_unknown: unknown,
                    was_in_tx: false,
                },
                DbError::ConnectionLost {
                    was_in_tx: false,
                    outcome_unknown: unknown,
                    message: "gone".into(),
                },
                DbError::Cancelled {
                    outcome_unknown: unknown,
                    was_in_tx: false,
                },
            ];
            for e in errors {
                let a: AppError = e.into();
                assert_eq!(a.outcome_unknown, unknown, "{}", a.kind);
            }
        }
        let other: AppError = DbError::NotOpen.into();
        assert!(!other.outcome_unknown);
    }

    #[test]
    fn cancelled_with_unknown_outcome_says_it_may_have_been_applied() {
        let a: AppError = DbError::Cancelled {
            outcome_unknown: true,
            was_in_tx: false,
        }
        .into();
        assert_eq!(a.kind, "cancelled");
        assert_eq!(
            a.message,
            "The statement was cancelled and may have been applied."
        );
        assert!(!a.retryable);
        let plain: AppError = DbError::cancelled().into();
        assert_eq!(plain.message, "The operation was cancelled.");
    }

    #[test]
    fn timeout_in_tx_is_not_retryable() {
        let a: AppError = DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: false,
            was_in_tx: true,
        }
        .into();
        assert!(!a.retryable);
        assert!(!a.outcome_unknown);
        assert_eq!(a.tx_state, None);
        let outside: AppError = DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: false,
            was_in_tx: false,
        }
        .into();
        assert!(outside.retryable);
    }

    #[test]
    fn cancelled_in_tx_is_not_retryable() {
        let a: AppError = DbError::Cancelled {
            outcome_unknown: false,
            was_in_tx: true,
        }
        .into();
        assert!(!a.retryable);
        assert_eq!(a.tx_state, None);
        let b: AppError = DbError::Cancelled {
            outcome_unknown: false,
            was_in_tx: false,
        }
        .into();
        assert!(b.retryable);
        let c: AppError = DbError::Cancelled {
            outcome_unknown: true,
            was_in_tx: false,
        }
        .into();
        assert!(!c.retryable);
    }

    #[test]
    fn helpers_are_outside_a_transaction() {
        assert!(matches!(
            DbError::timeout(ConnectStage::ConnectDb),
            DbError::Timeout {
                stage: ConnectStage::ConnectDb,
                outcome_unknown: false,
                was_in_tx: false,
            }
        ));
        assert!(matches!(
            DbError::cancelled(),
            DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            }
        ));
    }

    #[test]
    fn query_layer_errors_have_their_own_kind_and_message() {
        let cases = [
            (
                DbError::Busy,
                "busy",
                "This tab is already running a query.",
            ),
            (
                DbError::TransactionOpen,
                "transaction_open",
                "Commit or roll back the open transaction first.",
            ),
            (
                DbError::PlanExpired,
                "plan_expired",
                "The run plan expired; run it again.",
            ),
            (
                DbError::Blocked { count: 2 },
                "blocked",
                "Statements that change data are blocked on this read-only connection.",
            ),
            (
                DbError::ConfirmationRequired { count: 3 },
                "confirmation_required",
                "Running these statements on Production needs confirmation.",
            ),
        ];
        for (err, kind, message) in cases {
            let a: AppError = err.into();
            assert_eq!(a.kind, kind);
            assert_eq!(a.message, message);
            assert!(!a.retryable, "{kind}");
            assert!(!a.outcome_unknown, "{kind}");
            assert_eq!(a.tx_state, None, "{kind}");
        }
    }

    #[test]
    fn guard_errors_carry_the_count_as_detail() {
        let a: AppError = DbError::Blocked { count: 2 }.into();
        assert_eq!(a.detail.as_deref(), Some("2 statements"));
        let a: AppError = DbError::ConfirmationRequired { count: 1 }.into();
        assert_eq!(a.detail.as_deref(), Some("1 statement"));
        let a: AppError = DbError::Busy.into();
        assert_eq!(a.detail, None);
    }

    #[test]
    fn retryable_and_stage() {
        let t: AppError = DbError::Timeout {
            stage: ConnectStage::ConnectDb,
            outcome_unknown: false,
            was_in_tx: false,
        }
        .into();
        assert!(t.retryable);
        assert_eq!(t.stage.as_deref(), Some("connect_db"));
        let l: AppError = DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: false,
            message: "gone".into(),
        }
        .into();
        assert!(!l.retryable);
        assert_eq!(
            l.message,
            "The connection was lost during a transaction; it was rolled back."
        );
        let l: AppError = DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message: "gone".into(),
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
