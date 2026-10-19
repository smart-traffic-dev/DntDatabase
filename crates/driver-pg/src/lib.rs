//! PostgreSQL driver (12–17) built on `tokio-postgres` + rustls (`tokio-postgres-rustls`).

mod cancel;
pub mod ddl;
mod introspect;
mod numeric;
mod queries;
mod session;
mod tls;
mod values;

pub use session::PgSession;

use async_trait::async_trait;
use dnt_core::config::{ResolvedConfig, SslMode};
use dnt_core::driver::{Driver, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::{detect_pg, DbKind, ServerInfo};
use secrecy::ExposeSecret;
use std::error::Error as _;
use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode as PgSslMode;
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::{Client, SimpleQueryMessage};

/// Run on every new physical connection (also on reconnect) when the connection is read-only.
const SET_READ_ONLY: &str = "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY";
/// Used when the config carries no application name.
const DEFAULT_APPLICATION_NAME: &str = "DntDatabase";
/// PostgreSQL 12.0 as `server_version_num`.
const MIN_SERVER_VERSION_NUM: u32 = 120_000;

/// SQLSTATEs meaning the credentials or the login itself were rejected:
/// `invalid_password` and `invalid_authorization_specification` (e.g. no pg_hba.conf entry).
const AUTH_STATES: [&str; 2] = ["28P01", "28000"];

#[derive(Debug, Clone, Copy, Default)]
pub struct PgDriver;

impl PgDriver {
    /// Like [`Driver::connect`] but returns the concrete session type.
    pub async fn connect_session(&self, cfg: &ResolvedConfig) -> Result<PgSession, DbError> {
        tokio::time::timeout(cfg.connect_timeout, connect_inner(cfg))
            .await
            .map_err(|_| DbError::Timeout {
                stage: ConnectStage::ConnectDb,
            })?
    }
}

#[async_trait]
impl Driver for PgDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        Ok(Box::new(self.connect_session(cfg).await?))
    }
}

/// Everything needed to open (and re-open) a physical connection.
#[derive(Clone)]
pub(crate) struct ConnectParams {
    pub(crate) config: tokio_postgres::Config,
    pub(crate) tls: tls::Tls,
    pub(crate) read_only: bool,
}

/// One physical connection: the client and the task driving its socket.
pub(crate) struct PgConn {
    pub(crate) client: Client,
    pub(crate) task: JoinHandle<()>,
}

async fn connect_inner(cfg: &ResolvedConfig) -> Result<PgSession, DbError> {
    let tls = tls::tls_setup(cfg.ssl.as_ref(), &cfg.tls_server_name)?;
    let mut config = tokio_postgres::Config::new();
    let app = if cfg.application_name.is_empty() {
        DEFAULT_APPLICATION_NAME
    } else {
        &cfg.application_name
    };
    config
        .host(&cfg.host)
        .port(cfg.port)
        .user(&cfg.username)
        .application_name(app)
        .connect_timeout(cfg.connect_timeout)
        .ssl_mode(tls.mode);
    if let Some(p) = &cfg.password {
        config.password(p.expose_secret());
    }
    if let Some(db) = &cfg.database {
        config.dbname(db);
    }
    let mut params = ConnectParams {
        config,
        tls,
        read_only: cfg.read_only,
    };
    let conn = match open(&params).await {
        Ok(conn) => conn,
        // libpq `sslmode=prefer`: a server that refuses SSL is handled by tokio-postgres
        // itself; a failed handshake is retried in plaintext. Never on other errors.
        Err(DbError::Tls { .. }) if cfg.ssl.as_ref().map(|s| s.mode) == Some(SslMode::Prefer) => {
            params.config.ssl_mode(PgSslMode::Disable);
            open(&params).await?
        }
        Err(e) => return Err(e),
    };
    let info = match detect(&conn.client).await {
        Ok(info) => info,
        Err(e) => {
            close_conn(conn).await;
            return Err(e);
        }
    };
    Ok(PgSession::new(conn, info, params))
}

/// Opens a connection and runs the startup statements (application name is in the config).
pub(crate) async fn open(params: &ConnectParams) -> Result<PgConn, DbError> {
    let (client, connection) = params
        .config
        .connect(params.tls.connector.clone())
        .await
        .map_err(|e| map_pg_error(e, ConnectStage::ConnectDb))?;
    let task = tokio::spawn(async move {
        // Errors surface on the client (`is_closed`, failing requests).
        let _ = connection.await;
    });
    let conn = PgConn { client, task };
    if params.read_only {
        if let Err(e) = conn.client.batch_execute(SET_READ_ONLY).await {
            close_conn(conn).await;
            return Err(map_pg_error(e, ConnectStage::ConnectDb));
        }
    }
    Ok(conn)
}

/// Closes a connection gracefully (Terminate message), bounded in time.
pub(crate) async fn close_conn(conn: PgConn) {
    let PgConn { client, mut task } = conn;
    drop(client);
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// `SHOW server_version_num` + `SELECT version()`; servers older than 12 are `Unsupported`.
async fn detect(client: &Client) -> Result<ServerInfo, DbError> {
    let msgs = client
        .simple_query("SHOW server_version_num; SELECT version()")
        .await
        .map_err(|e| map_pg_error(e, ConnectStage::DetectServer))?;
    let mut cells = msgs.iter().filter_map(|m| match m {
        SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap_or_default().to_string()),
        _ => None,
    });
    let num = cells.next().unwrap_or_default();
    let version_string = cells.next().unwrap_or_default();
    match num.trim().parse::<u32>() {
        Ok(n) if n >= MIN_SERVER_VERSION_NUM => Ok(detect_pg(n, &version_string)),
        _ => Err(DbError::Unsupported {
            feature: "server version".into(),
            version: if version_string.is_empty() {
                num
            } else {
                version_string
            },
        }),
    }
}

/// Maps a tokio-postgres error to `DbError`. Neither server messages nor tokio-postgres
/// errors contain the password.
pub(crate) fn map_pg_error(e: tokio_postgres::Error, stage: ConnectStage) -> DbError {
    if let Some(db) = e.as_db_error() {
        let state = db.code().code();
        if AUTH_STATES.contains(&state) {
            return DbError::Auth {
                message: format!("{} [{state}]", db.message()),
            };
        }
        return DbError::Server {
            code: None,
            sqlstate: Some(state.to_string()),
            message: db.message().to_string(),
            position: match db.position() {
                Some(ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            },
        };
    }
    if let Some(t) = tls::rustls_error(&e) {
        return DbError::Tls {
            message: t.to_string(),
        };
    }
    // tokio-postgres keeps its error kind private; its `Display` names the kind.
    let kind = e.to_string();
    let detail = e.source().map(|s| s.to_string());
    let with_detail = || match &detail {
        Some(d) => format!("{kind}: {d}"),
        None => kind.clone(),
    };
    match kind.as_str() {
        "error performing TLS handshake" => {
            return DbError::Tls {
                message: detail.unwrap_or(kind),
            }
        }
        "authentication error" => {
            return DbError::Auth {
                message: with_detail(),
            }
        }
        "invalid configuration" | "invalid connection string" => {
            return DbError::Config {
                field: None,
                message: with_detail(),
            }
        }
        "timeout waiting for server" => return DbError::Timeout { stage },
        _ => {}
    }
    if let Some(io) = io_error(&e) {
        return if io.kind() == std::io::ErrorKind::TimedOut {
            DbError::Timeout { stage }
        } else {
            DbError::Connect {
                message: io.to_string(),
            }
        };
    }
    if e.is_closed() {
        return DbError::Connect {
            message: "connection closed".into(),
        };
    }
    DbError::Internal {
        message: with_detail(),
    }
}

/// The `io::Error` behind a socket-level failure, if any.
pub(crate) fn io_error(e: &tokio_postgres::Error) -> Option<&std::io::Error> {
    e.source()?.downcast_ref::<std::io::Error>()
}
