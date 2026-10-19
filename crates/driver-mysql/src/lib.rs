//! MySQL / MariaDB driver (MySQL 5.7, MySQL 8.x, MariaDB 10.3+) built on `mysql_async` + rustls.

mod cancel;
mod introspect;
mod queries;
mod session;
mod tls;
mod values;

pub use session::MySqlSession;

use async_trait::async_trait;
use dnt_core::config::{ResolvedConfig, SslMode};
use dnt_core::driver::{Driver, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::{detect_mysql, DbKind, MySqlCapabilities};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, Error as MyError, IoError, Opts, OptsBuilder, SslOpts};
use secrecy::ExposeSecret;

/// Statements run on every new physical connection (also on reconnect, since they live in `Opts`).
const SET_NAMES: &str = "SET NAMES utf8mb4";
const SET_READ_ONLY: &str = "SET SESSION TRANSACTION READ ONLY";

/// Server error codes that mean the credentials (or the user's host/db grant) were rejected.
const AUTH_CODES: [u16; 3] = [1044, 1045, 1698];

#[derive(Debug, Clone, Copy, Default)]
pub struct MySqlDriver;

impl MySqlDriver {
    /// Like [`Driver::connect`] but returns the concrete session type.
    pub async fn connect_session(&self, cfg: &ResolvedConfig) -> Result<MySqlSession, DbError> {
        tokio::time::timeout(cfg.connect_timeout, connect_inner(cfg))
            .await
            .map_err(|_| DbError::timeout(ConnectStage::ConnectDb))?
    }
}

#[async_trait]
impl Driver for MySqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }

    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        Ok(Box::new(self.connect_session(cfg).await?))
    }
}

async fn connect_inner(cfg: &ResolvedConfig) -> Result<MySqlSession, DbError> {
    let (mut conn, opts) = open(cfg).await?;
    let version = match conn.query_first::<String, _>("SELECT VERSION()").await {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            let _ = conn.disconnect().await;
            return Err(map_mysql_error(e, ConnectStage::DetectServer));
        }
    };
    let info = match detect_mysql(&version) {
        Ok(info) => info,
        Err(e) => {
            let _ = conn.disconnect().await;
            return Err(e);
        }
    };
    let caps = MySqlCapabilities::for_server(&info);
    Ok(MySqlSession::new(conn, info, caps, opts))
}

/// Opens the physical connection, returning the options that worked (for reconnects).
async fn open(cfg: &ResolvedConfig) -> Result<(Conn, Opts), DbError> {
    let mode = cfg.ssl.as_ref().map(|s| s.mode);
    let tls = match &cfg.ssl {
        Some(s) => tls::ssl_opts(s, &cfg.tls_server_name)?,
        None => None,
    };
    let opts = build_opts(cfg, tls.clone());
    let err = match Conn::new(opts.clone()).await {
        Ok(conn) => return Ok((conn, opts)),
        Err(e) => e,
    };
    match mode {
        // Chain verified but the name did not match: retry against a name the
        // certificate is valid for (see `tls::verify_ca_retry`).
        Some(SslMode::VerifyCa) => match tls::verify_ca_retry(&err) {
            Some(Ok(name)) => {
                let tls = tls.map(|t| t.with_danger_tls_hostname_override(Some(name)));
                connect_with(build_opts(cfg, tls)).await
            }
            Some(Err(no_san)) => Err(no_san),
            None => Err(map_mysql_error(err, ConnectStage::ConnectDb)),
        },
        // The server has no (usable) TLS: fall back to plaintext.
        Some(SslMode::Prefer) => match map_mysql_error(err, ConnectStage::ConnectDb) {
            DbError::Tls { .. } => connect_with(build_opts(cfg, None)).await,
            other => Err(other),
        },
        _ => Err(map_mysql_error(err, ConnectStage::ConnectDb)),
    }
}

async fn connect_with(opts: Opts) -> Result<(Conn, Opts), DbError> {
    Conn::new(opts.clone())
        .await
        .map(|conn| (conn, opts))
        .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))
}

fn build_opts(cfg: &ResolvedConfig, tls: Option<SslOpts>) -> Opts {
    let mut init = vec![SET_NAMES.to_string()];
    if cfg.read_only {
        init.push(SET_READ_ONLY.to_string());
    }
    OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .prefer_socket(false)
        .user(Some(cfg.username.clone()))
        .pass(cfg.password.as_ref().map(|p| p.expose_secret().to_string()))
        .db_name(cfg.database.clone())
        .init(init)
        .ssl_opts(tls)
        .into()
}

/// Maps a `mysql_async` error to `DbError`. Server messages never contain the password.
pub(crate) fn map_mysql_error(e: MyError, stage: ConnectStage) -> DbError {
    if let Some(t) = tls::rustls_error(&e) {
        return DbError::Tls {
            message: tls::handshake_message(t),
        };
    }
    match e {
        MyError::Server(se) if AUTH_CODES.contains(&se.code) => DbError::Auth {
            message: format!("{} [{}]", se.message, se.code),
        },
        MyError::Server(se) => DbError::Server {
            code: Some(se.code.to_string()),
            sqlstate: Some(se.state),
            message: se.message,
            position: None,
        },
        MyError::Io(IoError::Tls(t)) => DbError::Tls {
            message: t.to_string(),
        },
        MyError::Io(IoError::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => {
            DbError::timeout(stage)
        }
        MyError::Io(IoError::Io(io)) => DbError::Connect {
            message: io.to_string(),
        },
        MyError::Driver(DriverError::NoClientSslFlagFromServer) => DbError::Tls {
            message: "the server does not support TLS".into(),
        },
        MyError::Driver(d) => DbError::Connect {
            message: d.to_string(),
        },
        MyError::Url(u) => DbError::Config {
            field: None,
            message: u.to_string(),
        },
        MyError::Other(o) => DbError::Internal {
            message: o.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::error::hint;
    use mysql_async::ServerError;

    fn server(code: u16, state: &str, message: &str) -> MyError {
        MyError::Server(ServerError {
            code,
            state: state.into(),
            message: message.into(),
        })
    }

    #[test]
    fn auth_codes_become_auth_with_hint() {
        for code in AUTH_CODES {
            let e = map_mysql_error(
                server(code, "28000", "Access denied for user 'u'@'h'"),
                ConnectStage::ConnectDb,
            );
            assert!(matches!(e, DbError::Auth { .. }), "{code}: {e:?}");
            assert!(hint(&e).is_some(), "{code}");
        }
    }

    #[test]
    fn server_error_keeps_code_and_sqlstate() {
        match map_mysql_error(server(1792, "25006", "read only"), ConnectStage::Execute) {
            DbError::Server { code, sqlstate, .. } => {
                assert_eq!(code.as_deref(), Some("1792"));
                assert_eq!(sqlstate.as_deref(), Some("25006"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn io_and_tls_errors() {
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(
            map_mysql_error(refused.into(), ConnectStage::ConnectDb),
            DbError::Connect { .. }
        ));
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(matches!(
            map_mysql_error(timed_out.into(), ConnectStage::ConnectDb),
            DbError::Timeout {
                stage: ConnectStage::ConnectDb,
                outcome_unknown: false
            }
        ));
        let handshake = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        assert!(matches!(
            map_mysql_error(handshake.into(), ConnectStage::ConnectDb),
            DbError::Tls { .. }
        ));
        assert!(matches!(
            map_mysql_error(
                DriverError::NoClientSslFlagFromServer.into(),
                ConnectStage::ConnectDb
            ),
            DbError::Tls { .. }
        ));
    }
}
