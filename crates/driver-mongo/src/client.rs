//! Connect (spec §5.1): `ClientOptions` from `ResolvedConfig`, TLS, SRV, the server check.
//!
//! No connection string with a password is ever built: options are set field by field, and
//! the SRV lookup parses `mongodb+srv://<host>/` before the credential is attached as a struct.

use dnt_core::config::{DriverOptions, ResolvedConfig, SslConfig};
use dnt_core::error::DbError;
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::tls::{command_tls, CommandTls};
use mongodb::bson::{doc, Document};
use mongodb::error::ErrorKind;
use mongodb::event::sdam::SdamEvent;
use mongodb::event::EventHandler;
use mongodb::options::{
    ClientOptions, ConnectionString, Credential, ServerAddress, Tls, TlsOptions,
};
use mongodb::Client;
use secrecy::ExposeSecret;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Used when the config carries no application name.
const DEFAULT_APP_NAME: &str = "DntDatabase";
/// The session's database when the config names none (mongosh's default too).
const DEFAULT_DB: &str = "test";
/// Connections a session's `Client` may open for commands.
const MAX_POOL_SIZE: u32 = 2;
/// Oldest supported server.
const MIN_VERSION: (u32, u32, u32) = (6, 0, 0);
/// Server error code of a rejected login.
const AUTHENTICATION_FAILED: i32 = 18;

const SRV_NEEDS_TLS: &str = "mongodb+srv connections use TLS; choose Require or Verify full";
const NO_VERIFY_CA: &str = "Verify CA is not available for MongoDB; use Verify full";
const NO_VERIFY_FULL_THROUGH_TUNNEL: &str =
    "Through an SSH tunnel MongoDB can only use Require; the host name cannot be verified";
const CERT_NOT_COMBINED: &str =
    "The client certificate must be one PEM file with the certificate and its private key";
const KEY_INSIDE_CERT: &str = "MongoDB takes the key inside the certificate file";
const BAD_SRV_HOST: &str = "An SRV host is a DNS name, without a port or credentials";
/// Unencrypted private key blocks (a key with a password is not supported).
const KEY_MARKERS: [&str; 3] = [
    "-----BEGIN PRIVATE KEY-----",
    "-----BEGIN RSA PRIVATE KEY-----",
    "-----BEGIN EC PRIVATE KEY-----",
];

/// A connected `Client` with what the server said about itself.
pub struct Opened {
    pub client: Client,
    pub info: ServerInfo,
    /// The session's first database: `cfg.database`, or `test`.
    pub db: String,
}

/// What [`open`] builds the `Client` from.
#[derive(Debug)]
pub struct ClientOptionsPlan {
    /// Every field spec §5.1 sets. With SRV `hosts` is empty: the lookup finds them, and
    /// the lookup's own `hosts`, replica set and auth source are kept.
    pub options: ClientOptions,
    /// SRV: the DNS name to look up.
    pub srv_host: Option<String>,
}

impl ClientOptionsPlan {
    /// The string the SRV lookup parses: no user, no password, no options.
    pub fn srv_uri(&self) -> Option<String> {
        self.srv_host
            .as_ref()
            .map(|host| format!("mongodb+srv://{host}/"))
    }

    /// The hosts named in errors.
    fn seeds(&self) -> String {
        match &self.srv_host {
            Some(host) => host.clone(),
            None => self
                .options
                .hosts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// Builds the `Client` and checks the server (`hello`, `buildInfo`), all within
/// `cfg.connect_timeout`.
pub async fn open(cfg: &ResolvedConfig) -> Result<Opened, DbError> {
    let plan = client_options(cfg, cfg.via_tunnel)?;
    let seeds = plan.seeds();
    let last_failure = LastFailure::default();
    let connecting = connect(plan, &seeds, Arc::clone(&last_failure));
    let (client, info) = match tokio::time::timeout(cfg.connect_timeout, connecting).await {
        Ok(result) => result?,
        Err(_) => return Err(timed_out(cfg.connect_timeout, &seeds, &last_failure)),
    };
    Ok(Opened {
        client,
        info,
        db: non_empty(cfg.database.as_ref()).unwrap_or_else(|| DEFAULT_DB.into()),
    })
}

/// The options of spec §5.1. Pure apart from reading the client certificate file.
pub fn client_options(
    cfg: &ResolvedConfig,
    through_tunnel: bool,
) -> Result<ClientOptionsPlan, DbError> {
    let (srv, auth_source, replica_set, direct_connection, extra_hosts) = match &cfg.driver {
        DriverOptions::Mongo {
            srv,
            auth_source,
            replica_set,
            direct_connection,
            extra_hosts,
        } => (
            *srv,
            auth_source,
            replica_set,
            *direct_connection,
            extra_hosts,
        ),
        DriverOptions::None | DriverOptions::Redis { .. } => {
            return Err(config(
                "options.driver",
                "Driver options do not match the engine",
            ))
        }
    };
    let tls = tls(cfg.ssl.as_ref(), srv, through_tunnel)?;
    let srv_host = if srv {
        Some(srv_host(&cfg.host)?)
    } else {
        None
    };

    let mut options = ClientOptions::default();
    options.hosts = if srv {
        Vec::new()
    } else {
        std::iter::once((cfg.host.trim(), cfg.port))
            .chain(extra_hosts.iter().map(|h| (h.host.trim(), h.port)))
            .map(|(host, port)| ServerAddress::Tcp {
                host: host.to_string(),
                port: Some(port),
            })
            .collect()
    };
    options.credential = credential(cfg, auth_source.as_ref(), srv);
    options.repl_set_name = non_empty(replica_set.as_ref());
    options.direct_connection = Some(through_tunnel || direct_connection);
    options.app_name = Some(if cfg.application_name.trim().is_empty() {
        DEFAULT_APP_NAME.into()
    } else {
        cfg.application_name.clone()
    });
    options.connect_timeout = Some(cfg.connect_timeout);
    options.server_selection_timeout = Some(cfg.connect_timeout);
    options.max_pool_size = Some(MAX_POOL_SIZE);
    options.retry_reads = Some(false);
    options.retry_writes = Some(false);
    options.tls = tls;
    // The auth-source fallback of an SRV lookup (after the TXT record's `authSource`).
    options.default_database = non_empty(cfg.database.as_ref());
    Ok(ClientOptionsPlan { options, srv_host })
}

/// SCRAM (SHA-256 or SHA-1, as the server offers) only when there is a user name. The
/// source is `auth_source`, else the database, else `admin`; with SRV an empty
/// `auth_source` is left to the lookup (TXT `authSource`, then the database, then `admin`).
fn credential(cfg: &ResolvedConfig, auth_source: Option<&String>, srv: bool) -> Option<Credential> {
    if cfg.username.trim().is_empty() {
        return None;
    }
    let source = non_empty(auth_source).or_else(|| {
        (!srv).then(|| non_empty(cfg.database.as_ref()).unwrap_or_else(|| "admin".into()))
    });
    let mut c = Credential::default();
    c.username = Some(cfg.username.clone());
    c.password = cfg.password.as_ref().map(|p| p.expose_secret().to_string());
    c.source = source;
    Some(c)
}

/// The TLS table of spec §5.1, plus the CA and client-certificate rules.
fn tls(ssl: Option<&SslConfig>, srv: bool, through_tunnel: bool) -> Result<Option<Tls>, DbError> {
    let allow_invalid_certificates = match (command_tls(ssl), through_tunnel) {
        (CommandTls::Off, _) if srv => return Err(config("ssl.mode", SRV_NEEDS_TLS)),
        (CommandTls::Off, _) => return Ok(None),
        (CommandTls::Unverified, _) => true,
        (CommandTls::VerifyCa, _) => return Err(config("ssl.mode", NO_VERIFY_CA)),
        (CommandTls::VerifyFull, true) => {
            return Err(config("ssl.mode", NO_VERIFY_FULL_THROUGH_TUNNEL))
        }
        (CommandTls::VerifyFull, false) => false,
    };
    // `command_tls(None)` is `Off`, handled above.
    let Some(ssl) = ssl else { return Ok(None) };
    if non_empty_path(ssl.key_path.as_ref()).is_some() {
        return Err(config("ssl.key_path", KEY_INSIDE_CERT));
    }
    let cert = non_empty_path(ssl.cert_path.as_ref());
    if let Some(path) = &cert {
        check_combined_pem(path)?;
    }
    let mut options = TlsOptions::default();
    options.allow_invalid_certificates = allow_invalid_certificates.then_some(true);
    // Empty: the crate trusts the bundled Mozilla roots (webpki-roots).
    options.ca_file_path = non_empty_path(ssl.ca_path.as_ref());
    options.cert_key_file_path = cert;
    Ok(Some(Tls::Enabled(options)))
}

/// The client certificate file must hold the certificate and an unencrypted key.
fn check_combined_pem(path: &Path) -> Result<(), DbError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        config(
            "ssl.cert_path",
            &format!(
                "cannot read the client certificate file {}: {e}",
                path.display()
            ),
        )
    })?;
    let has_cert = text.contains("-----BEGIN CERTIFICATE-----");
    let has_key = KEY_MARKERS.iter().any(|m| text.contains(m));
    if has_cert && has_key {
        Ok(())
    } else {
        Err(config("ssl.cert_path", CERT_NOT_COMBINED))
    }
}

/// A DNS name only, so nothing in it can read as a port, credentials or options.
fn srv_host(host: &str) -> Result<String, DbError> {
    let host = host.trim();
    let valid = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if valid {
        Ok(host.to_string())
    } else {
        Err(config("host", BAD_SRV_HOST))
    }
}

/// The SRV lookup's input: the parsed `mongodb+srv://<host>/` with the credential, the
/// user's replica set and the database attached as fields.
fn srv_connection_string(plan: &ClientOptionsPlan) -> Result<ConnectionString, DbError> {
    let uri = plan.srv_uri().ok_or_else(|| DbError::Internal {
        message: "not an SRV connection".into(),
    })?;
    let mut cs = ConnectionString::parse(&uri).map_err(|_| config("host", BAD_SRV_HOST))?;
    cs.credential = plan.options.credential.clone();
    cs.replica_set = plan.options.repl_set_name.clone();
    cs.default_database = plan.options.default_database.clone();
    Ok(cs)
}

/// Puts the plan's settings on the options an SRV lookup produced, keeping what the lookup
/// found: hosts, replica set (TXT, unless the user gave one) and the credential's source.
fn apply_srv_overrides(found: &mut ClientOptions, plan: ClientOptionsPlan) {
    let o = plan.options;
    found.app_name = o.app_name;
    found.connect_timeout = o.connect_timeout;
    found.server_selection_timeout = o.server_selection_timeout;
    found.max_pool_size = o.max_pool_size;
    found.retry_reads = o.retry_reads;
    found.retry_writes = o.retry_writes;
    found.direct_connection = o.direct_connection;
    found.tls = o.tls;
}

/// The last heartbeat failure the client's monitors saw, named when the time runs out.
type LastFailure = Arc<Mutex<Option<String>>>;

async fn connect(
    plan: ClientOptionsPlan,
    seeds: &str,
    last_failure: LastFailure,
) -> Result<(Client, ServerInfo), DbError> {
    let mut options = match &plan.srv_host {
        None => plan.options,
        Some(host) => {
            let failed = DbError::Connect {
                message: format!("SRV lookup failed for {host}"),
            };
            let cs = srv_connection_string(&plan)?;
            let mut found = ClientOptions::parse(cs).await.map_err(|_| failed)?;
            apply_srv_overrides(&mut found, plan);
            found
        }
    };
    options.sdam_event_handler = Some(EventHandler::callback(move |event: SdamEvent| {
        if let SdamEvent::ServerHeartbeatFailed(failed) = event {
            *last_failure.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(failed.failure.kind.to_string());
        }
    }));
    let client = Client::with_options(options).map_err(|e| connect_error(&e, seeds))?;
    match check_server(&client, seeds).await {
        Ok(info) => Ok((client, info)),
        Err(e) => {
            client.shutdown().immediate(true).await;
            Err(e)
        }
    }
}

async fn check_server(client: &Client, seeds: &str) -> Result<ServerInfo, DbError> {
    let admin = client.database("admin");
    let hello = admin
        .run_command(doc! { "hello": 1 })
        .await
        .map_err(|e| connect_error(&e, seeds))?;
    let build_info = admin
        .run_command(doc! { "buildInfo": 1 })
        .await
        .map_err(|e| connect_error(&e, seeds))?;
    server_info(&hello, &build_info)
}

/// Version from `buildInfo`; transactions on a replica set member or mongos. A sharded
/// cluster is allowed, though untested.
fn server_info(hello: &Document, build_info: &Document) -> Result<ServerInfo, DbError> {
    let version_string = build_info
        .get_str("version")
        .unwrap_or_default()
        .to_string();
    let version = match parse_version(&version_string) {
        Some(v) if v >= MIN_VERSION => v,
        _ => {
            return Err(DbError::Unsupported {
                feature: "MongoDB before 6.0".into(),
                version: version_string,
            })
        }
    };
    let replica_set = hello.get_str("setName").is_ok();
    let mongos = hello.get_str("msg").ok() == Some("isdbgrid");
    let mut info = ServerInfo::new(DbKind::Mongo, ServerVariant::Mongo, version, version_string);
    info.capabilities.transactions = replica_set || mongos;
    Ok(info)
}

/// Leading `major.minor[.patch]`, ignoring a suffix such as `-rc0`.
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

/// Errors of the connect step. Driver messages never contain the password.
fn connect_error(e: &mongodb::error::Error, seeds: &str) -> DbError {
    match e.kind.as_ref() {
        ErrorKind::Authentication { message, .. } => DbError::Auth {
            message: message.clone(),
        },
        ErrorKind::Command(c) if c.code == AUTHENTICATION_FAILED => DbError::Auth {
            message: c.message.clone(),
        },
        ErrorKind::Command(c) => DbError::Server {
            code: Some(if c.code_name.is_empty() {
                c.code.to_string()
            } else {
                c.code_name.clone()
            }),
            sqlstate: None,
            message: c.message.clone(),
            position: None,
        },
        ErrorKind::ServerSelection { message, .. } => DbError::Connect {
            message: format!("No MongoDB server was available at {seeds}: {message}"),
        },
        ErrorKind::DnsResolve { message, .. } => DbError::Connect {
            message: message.clone(),
        },
        ErrorKind::Io(io) => DbError::Connect {
            message: io.to_string(),
        },
        ErrorKind::InvalidTlsConfig { message, .. } => DbError::Tls {
            message: message.clone(),
        },
        ErrorKind::InvalidArgument { message, .. } => DbError::Config {
            field: None,
            message: message.clone(),
        },
        _ => DbError::Internal {
            message: e.kind.to_string(),
        },
    }
}

/// `connect_timeout` ran out before a server answered: the hosts, and why the last attempt
/// to reach one failed (when a monitor saw it).
fn timed_out(limit: Duration, seeds: &str, last_failure: &LastFailure) -> DbError {
    let mut message = format!("No MongoDB server answered within {limit:?} at {seeds}");
    if let Some(failure) = last_failure
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
    {
        message.push_str(": ");
        message.push_str(failure);
    }
    DbError::Connect { message }
}

fn config(field: &str, message: &str) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: message.into(),
    }
}

/// The trimmed value, or `None` when it is blank.
fn non_empty(s: Option<&String>) -> Option<String> {
    s.map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn non_empty_path(p: Option<&PathBuf>) -> Option<PathBuf> {
    p.filter(|p| !p.as_os_str().is_empty()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::config::{DriverOptions, HostPort, SslConfig, SslMode};
    use dnt_core::server::{DbKind, ServerVariant};
    use mongodb::bson::doc;
    use mongodb::options::{HostInfo, ServerAddress, Tls};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const PASSWORD: &str = "s3cret-p@ss";

    fn mongo(srv: bool) -> DriverOptions {
        DriverOptions::Mongo {
            srv,
            auth_source: None,
            replica_set: None,
            direct_connection: false,
            extra_hosts: vec![],
        }
    }

    fn cfg() -> ResolvedConfig {
        let mut c = dnt_core::test_util::sample_resolved(Some(PASSWORD));
        c.kind = DbKind::Mongo;
        c.host = "db1.example.com".into();
        c.port = 27017;
        c.tls_server_name = c.host.clone();
        c.username = "app".into();
        c.database = Some("shop".into());
        c.driver = mongo(false);
        c
    }

    fn srv_cfg() -> ResolvedConfig {
        let mut c = cfg();
        c.host = "cluster0.example.net".into();
        c.port = 0;
        c.driver = mongo(true);
        c.ssl = Some(ssl(SslMode::Require));
        c
    }

    fn ssl(mode: SslMode) -> SslConfig {
        SslConfig {
            version: 1,
            mode,
            ca_path: None,
            cert_path: None,
            key_path: None,
        }
    }

    fn set_driver(
        c: &mut ResolvedConfig,
        f: impl FnOnce(&mut Option<String>, &mut Option<String>, &mut bool, &mut Vec<HostPort>),
    ) {
        match &mut c.driver {
            DriverOptions::Mongo {
                auth_source,
                replica_set,
                direct_connection,
                extra_hosts,
                ..
            } => f(auth_source, replica_set, direct_connection, extra_hosts),
            DriverOptions::None | DriverOptions::Redis { .. } => unreachable!(),
        }
    }

    /// Pins the crate's redacting `Debug` of `Credential`: an upgrade that starts printing
    /// the password fails here.
    fn assert_no_password(what: &str, value: &impl std::fmt::Debug) {
        let text = format!("{value:?}");
        assert!(
            !text.contains(PASSWORD),
            "{what} Debug shows the password: {text}"
        );
    }

    fn plan(c: &ResolvedConfig) -> ClientOptionsPlan {
        let p = client_options(c, false).unwrap_or_else(|e| panic!("{e}"));
        assert_no_password("ClientOptionsPlan", &p);
        p
    }

    fn config_error(r: Result<ClientOptionsPlan, DbError>) -> (String, String) {
        match r {
            Err(DbError::Config {
                field: Some(field),
                message,
            }) => (field, message),
            Err(e) => panic!("expected a Config error with a field, got {e:?}"),
            Ok(p) => panic!("expected a Config error, got {p:?}"),
        }
    }

    fn err(field: &str, message: &str) -> (String, String) {
        (field.into(), message.into())
    }

    fn tcp(host: &str, port: u16) -> ServerAddress {
        ServerAddress::Tcp {
            host: host.into(),
            port: Some(port),
        }
    }

    fn tls_options(p: &ClientOptionsPlan) -> mongodb::options::TlsOptions {
        match &p.options.tls {
            Some(Tls::Enabled(o)) => o.clone(),
            other => panic!("expected TLS, got {other:?}"),
        }
    }

    const SRV_TLS: &str = "mongodb+srv connections use TLS; choose Require or Verify full";
    const VERIFY_CA: &str = "Verify CA is not available for MongoDB; use Verify full";
    const TUNNEL_FULL: &str =
        "Through an SSH tunnel MongoDB can only use Require; the host name cannot be verified";
    const COMBINED: &str =
        "The client certificate must be one PEM file with the certificate and its private key";
    const KEY_PATH: &str = "MongoDB takes the key inside the certificate file";

    #[test]
    fn options_plain() {
        let mut c = cfg();
        set_driver(&mut c, |auth, rs, _, extra| {
            *auth = Some("admin".into());
            *rs = Some("rs0".into());
            extra.push(HostPort {
                host: "db2.example.com".into(),
                port: 27018,
            });
        });
        let p = plan(&c);
        assert_eq!(p.srv_host, None);
        assert_eq!(p.srv_uri(), None);
        let o = &p.options;
        assert_eq!(
            o.hosts,
            [tcp("db1.example.com", 27017), tcp("db2.example.com", 27018)]
        );
        let cred = o.credential.as_ref().expect("credential");
        assert_eq!(cred.username.as_deref(), Some("app"));
        assert_eq!(cred.password.as_deref(), Some(PASSWORD));
        assert_eq!(cred.source.as_deref(), Some("admin"));
        assert_eq!(cred.mechanism, None, "SCRAM-SHA-256/SHA-1 negotiated");
        assert_eq!(o.repl_set_name.as_deref(), Some("rs0"));
        assert_eq!(o.direct_connection, Some(false));
        assert_eq!(o.tls, None, "no ssl config: no TLS");
        assert_eq!(o.connect_timeout, Some(Duration::from_secs(10)));
        assert_eq!(o.server_selection_timeout, Some(Duration::from_secs(10)));
        assert_eq!(o.default_database.as_deref(), Some("shop"));

        // Disable is no TLS too; the direct_connection option is passed on.
        c.ssl = Some(ssl(SslMode::Disable));
        set_driver(&mut c, |_, _, direct, _| *direct = true);
        let o = plan(&c).options;
        assert_eq!((o.tls, o.direct_connection), (None, Some(true)));

        // Through a tunnel the client always talks to that one server.
        let mut c = cfg();
        c.host = "127.0.0.1".into();
        c.port = 50123;
        let o = client_options(&c, true).unwrap().options;
        assert_eq!(o.direct_connection, Some(true));
        assert_eq!(o.hosts, [tcp("127.0.0.1", 50123)]);
    }

    #[test]
    fn options_srv_has_no_password_in_uri() {
        let mut c = srv_cfg();
        set_driver(&mut c, |_, rs, _, _| *rs = Some("atlas-rs".into()));
        let p = plan(&c);
        assert_eq!(p.srv_host.as_deref(), Some("cluster0.example.net"));
        let uri = p.srv_uri().unwrap();
        assert_eq!(uri, "mongodb+srv://cluster0.example.net/");
        assert!(!uri.contains(PASSWORD) && !uri.contains("app") && !uri.contains('@'));
        assert!(p.options.hosts.is_empty(), "the SRV lookup finds the hosts");
        assert_eq!(p.options.direct_connection, Some(false));
        assert!(tls_options(&p).allow_invalid_certificates == Some(true));

        // The lookup gets the credential as a struct, never in the string.
        assert_no_password("ClientOptionsPlan", &p);
        assert_no_password("ClientOptions", &p.options);
        let cs = srv_connection_string(&p).unwrap();
        assert_no_password("ConnectionString", &cs);
        assert_eq!(
            cs.host_info,
            HostInfo::DnsRecord("cluster0.example.net".into())
        );
        let cred = cs.credential.as_ref().unwrap();
        assert_eq!(
            (cred.username.as_deref(), cred.password.as_deref()),
            (Some("app"), Some(PASSWORD))
        );
        assert_eq!(cred.source, None, "left to TXT, then the database");
        assert_eq!(cs.default_database.as_deref(), Some("shop"));
        assert_eq!(cs.replica_set.as_deref(), Some("atlas-rs"));

        // Nothing in the host can turn into credentials, a port or options.
        for host in [
            "evil:pw@cluster0.example.net",
            "cluster0.example.net:27017",
            "a/b",
            "a?tls=false",
            " ",
        ] {
            let mut c = srv_cfg();
            c.host = host.into();
            assert_eq!(
                config_error(client_options(&c, false)),
                err(
                    "host",
                    "An SRV host is a DNS name, without a port or credentials"
                ),
                "{host:?}"
            );
        }
    }

    #[test]
    fn srv_lookup_keeps_found_hosts() {
        let p = plan(&srv_cfg());
        let mut found = ClientOptions::default();
        found.hosts = vec![tcp("shard-00-00.example.net", 27017)];
        found.repl_set_name = Some("from-txt".into());
        found.max_pool_size = Some(100);
        found.retry_writes = Some(true);
        found.credential = p.options.credential.clone();
        let want_tls = p.options.tls.clone();
        apply_srv_overrides(&mut found, p);
        assert_no_password("ClientOptions after the SRV lookup", &found);
        assert_eq!(
            found.credential.as_ref().unwrap().password.as_deref(),
            Some(PASSWORD)
        );
        assert_eq!(found.hosts, [tcp("shard-00-00.example.net", 27017)]);
        assert_eq!(found.repl_set_name.as_deref(), Some("from-txt"));
        assert_eq!(found.max_pool_size, Some(2));
        assert_eq!(
            (found.retry_reads, found.retry_writes),
            (Some(false), Some(false))
        );
        assert_eq!(found.app_name.as_deref(), Some("DntDatabase"));
        assert_eq!(found.tls, want_tls);
        assert_eq!(
            found.server_selection_timeout,
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn srv_without_tls_is_config_error() {
        for ssl_cfg in [None, Some(ssl(SslMode::Disable))] {
            let mut c = srv_cfg();
            c.ssl = ssl_cfg;
            assert_eq!(
                config_error(client_options(&c, false)),
                err("ssl.mode", SRV_TLS)
            );
        }
        let mut c = srv_cfg();
        c.ssl = Some(ssl(SslMode::Prefer));
        assert!(
            client_options(&c, false).is_ok(),
            "Prefer is TLS for Command engines"
        );
    }

    #[test]
    fn verify_ca_refused() {
        for tunnel in [false, true] {
            let mut c = cfg();
            c.ssl = Some(SslConfig {
                ca_path: Some(PathBuf::from("ca.pem")),
                ..ssl(SslMode::VerifyCa)
            });
            assert_eq!(
                config_error(client_options(&c, tunnel)),
                err("ssl.mode", VERIFY_CA)
            );
        }
    }

    #[test]
    fn verify_full_through_tunnel_refused() {
        let mut c = cfg();
        c.ssl = Some(SslConfig {
            ca_path: Some(PathBuf::from("ca.pem")),
            ..ssl(SslMode::VerifyFull)
        });
        assert_eq!(
            config_error(client_options(&c, true)),
            err("ssl.mode", TUNNEL_FULL)
        );

        // Without a tunnel: full verification against the CA file.
        let t = tls_options(&client_options(&c, false).unwrap());
        assert_eq!(t.allow_invalid_certificates, None);
        assert_eq!(t.ca_file_path, Some(PathBuf::from("ca.pem")));
        assert_eq!(t.cert_key_file_path, None);
        // No CA file: the bundled Mozilla roots.
        c.ssl = Some(ssl(SslMode::VerifyFull));
        assert_eq!(tls_options(&plan(&c)).ca_file_path, None);

        // Require (Unverified) is the same with or without a tunnel.
        for mode in [SslMode::Prefer, SslMode::Require] {
            c.ssl = Some(ssl(mode));
            for tunnel in [false, true] {
                let t = tls_options(&client_options(&c, tunnel).unwrap());
                assert_eq!(t.allow_invalid_certificates, Some(true), "{mode:?}");
            }
        }
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGH\n-----END PRIVATE KEY-----\n";
    const EC_KEY: &str = "-----BEGIN EC PRIVATE KEY-----\nMHc\n-----END EC PRIVATE KEY-----\n";
    const ENCRYPTED_KEY: &str =
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIF\n-----END ENCRYPTED PRIVATE KEY-----\n";

    #[test]
    fn cert_must_be_combined_pem() {
        let dir = tempfile::tempdir().unwrap();
        let with_cert = |path: PathBuf| {
            let mut c = cfg();
            c.ssl = Some(SslConfig {
                cert_path: Some(path),
                ..ssl(SslMode::Require)
            });
            client_options(&c, false)
        };
        for (name, text) in [
            ("cert-only.pem", CERT.to_string()),
            ("key-only.pem", KEY.to_string()),
            ("encrypted.pem", format!("{CERT}{ENCRYPTED_KEY}")),
        ] {
            let path = write(dir.path(), name, &text);
            assert_eq!(
                config_error(with_cert(path)),
                err("ssl.cert_path", COMBINED),
                "{name}"
            );
        }
        let (field, message) = config_error(with_cert(dir.path().join("missing.pem")));
        assert_eq!(field, "ssl.cert_path");
        assert!(message.contains("missing.pem"), "{message}");

        for (name, text) in [
            ("combined.pem", format!("{CERT}{KEY}")),
            ("key-first.pem", format!("{EC_KEY}{CERT}")),
        ] {
            let path = write(dir.path(), name, &text);
            let t = tls_options(&with_cert(path.clone()).unwrap());
            assert_eq!(t.cert_key_file_path, Some(path), "{name}");
        }
    }

    #[test]
    fn key_path_refused() {
        let mut c = cfg();
        c.ssl = Some(SslConfig {
            cert_path: Some(PathBuf::from("client.pem")),
            key_path: Some(PathBuf::from("client-key.pem")),
            ..ssl(SslMode::Require)
        });
        assert_eq!(
            config_error(client_options(&c, false)),
            err("ssl.key_path", KEY_PATH)
        );
        // An empty path is no path.
        c.ssl = Some(SslConfig {
            key_path: Some(PathBuf::new()),
            ..ssl(SslMode::Require)
        });
        assert!(client_options(&c, false).is_ok());
    }

    #[test]
    fn credential_only_with_username() {
        for user in ["", "  "] {
            let mut c = cfg();
            c.username = user.into();
            assert!(plan(&c).options.credential.is_none(), "{user:?}");
        }
        let mut c = cfg();
        c.password = None;
        let cred = plan(&c).options.credential.unwrap();
        assert_eq!(
            (cred.username.as_deref(), cred.password),
            (Some("app"), None)
        );
    }

    #[test]
    fn auth_source_fallbacks() {
        let source = |auth: Option<&str>, db: Option<&str>| {
            let mut c = cfg();
            c.database = db.map(str::to_string);
            set_driver(&mut c, |a, _, _, _| *a = auth.map(str::to_string));
            plan(&c).options.credential.unwrap().source.unwrap()
        };
        assert_eq!(source(Some("authdb"), Some("shop")), "authdb");
        assert_eq!(source(None, Some("shop")), "shop");
        assert_eq!(source(Some(""), Some("shop")), "shop");
        assert_eq!(source(None, None), "admin");
        assert_eq!(source(Some(" "), Some("")), "admin");

        // SRV: the user's value wins; otherwise the lookup decides (TXT, database, admin).
        let mut c = srv_cfg();
        set_driver(&mut c, |a, _, _, _| *a = Some("authdb".into()));
        let p = plan(&c);
        assert_eq!(
            p.options.credential.unwrap().source.as_deref(),
            Some("authdb")
        );
        let p = plan(&srv_cfg());
        assert_eq!(p.options.credential.unwrap().source, None);
    }

    #[test]
    fn retries_off() {
        let o = plan(&cfg()).options;
        assert_eq!(o.retry_reads, Some(false));
        assert_eq!(o.retry_writes, Some(false));
        assert_eq!(o.max_pool_size, Some(2));
    }

    #[test]
    fn app_name_from_config() {
        let mut c = cfg();
        c.application_name = "Reports".into();
        assert_eq!(plan(&c).options.app_name.as_deref(), Some("Reports"));
        c.application_name = String::new();
        assert_eq!(plan(&c).options.app_name.as_deref(), Some("DntDatabase"));
    }

    #[test]
    fn server_checks() {
        let build = |v: &str| doc! { "version": v, "ok": 1 };
        let standalone = doc! { "isWritablePrimary": true, "ok": 1 };
        let info = server_info(&standalone, &build("6.0.28")).unwrap();
        assert_eq!(
            (info.kind, info.variant, info.version),
            (DbKind::Mongo, ServerVariant::Mongo, (6, 0, 28))
        );
        assert_eq!(info.version_string, "6.0.28");
        assert!(!info.capabilities.transactions);
        assert!(info.capabilities.data_edit);

        let rs = doc! { "isWritablePrimary": true, "setName": "rs0", "ok": 1 };
        assert!(
            server_info(&rs, &build("8.0.4"))
                .unwrap()
                .capabilities
                .transactions
        );
        let mongos = doc! { "isWritablePrimary": true, "msg": "isdbgrid", "ok": 1 };
        let info = server_info(&mongos, &build("7.0.1-rc0")).unwrap();
        assert!(info.capabilities.transactions);
        assert_eq!(info.version, (7, 0, 1));

        for old in ["5.0.30", "4.4.29", "garbage", ""] {
            match server_info(&standalone, &build(old)) {
                Err(DbError::Unsupported { feature, version }) => {
                    assert_eq!(
                        (feature.as_str(), version.as_str()),
                        ("MongoDB before 6.0", old)
                    );
                }
                other => panic!("{old:?}: expected Unsupported, got {other:?}"),
            }
        }
    }
}
