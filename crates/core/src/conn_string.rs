//! Connection strings pasted into the connection dialog.
//!
//! Accepted forms:
//! - URLs: `postgresql://`, `postgres://`, `mysql://`, `mariadb://` (an optional `jdbc:` prefix
//!   is ignored): `scheme://[user[:password]@][host][:port][/database][?key=value&…]`.
//! - libpq keyword/value strings: `host=db port=5432 user=app dbname=sales sslmode=require`.
//! - MongoDB: `mongodb://[user[:password]@]host[:port][,host[:port]…][/db][?options]` and
//!   `mongodb+srv://[user[:password]@]host[/db][?options]`.
//! - Redis: `redis://` / `rediss://` `[user][:password]@host[:port][/db]`.
//!
//! Nothing here is stored: the result only fills the dialog's fields. The password is handed
//! back to the UI (it was typed there) and never logged; `Debug` redacts it. [`to_uri`] goes the
//! other way, for display and copy, and never writes a password.

use crate::config::{ConnectionConfig, DriverOptions, HostPort, SslMode};
use crate::error::DbError;
use crate::server::DbKind;
use serde::Serialize;
use specta::Type;
use std::fmt;

/// The engine a connection string names (MariaDB is told apart from MySQL only by its scheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConnStringEngine {
    Mysql,
    Mariadb,
    Postgres,
    Mongo,
    MongoSrv,
    Redis,
    Rediss,
}

/// The values read from a connection string; a `None` field was not in the string.
#[derive(Clone, PartialEq, Serialize, Type)]
pub struct ParsedConnString {
    /// `None` for a libpq keyword/value string that names no engine (it is PostgreSQL's format,
    /// so the UI treats it as PostgreSQL).
    pub engine: Option<ConnStringEngine>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub ssl_mode: Option<SslMode>,
    pub application_name: Option<String>,
    pub connect_timeout_s: Option<u32>,
    /// Parameters that were not understood (`key=value`), so nothing is dropped silently.
    pub ignored: Vec<String>,
    /// Things worth telling the user (e.g. only the first of several hosts is used).
    pub warnings: Vec<String>,
    /// MongoDB / Redis options; `None` for the SQL engines.
    pub driver: Option<DriverOptions>,
}

/// The password is never printed.
impl fmt::Debug for ParsedConnString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedConnString")
            .field("engine", &self.engine)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("database", &self.database)
            .field("ssl_mode", &self.ssl_mode)
            .field("application_name", &self.application_name)
            .field("connect_timeout_s", &self.connect_timeout_s)
            .field("ignored", &self.ignored)
            .field("warnings", &self.warnings)
            .field("driver", &self.driver)
            .finish()
    }
}

const FIELD: &str = "conn_string";

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some(FIELD.into()),
        message: message.into(),
    }
}

/// A MongoDB / Redis URI problem (spec §6.3): a `config` error on the field `uri`.
fn invalid_uri(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some("uri".into()),
        message: message.into(),
    }
}

fn unsupported_option(name: &str) -> DbError {
    invalid_uri(format!("Unsupported URI option: {name}"))
}

const MONGO_PORT: u16 = 27017;
const REDIS_PORT: u16 = 6379;

impl ParsedConnString {
    fn empty(engine: Option<ConnStringEngine>) -> Self {
        ParsedConnString {
            engine,
            host: None,
            port: None,
            user: None,
            password: None,
            database: None,
            ssl_mode: None,
            application_name: None,
            connect_timeout_s: None,
            ignored: Vec::new(),
            warnings: Vec::new(),
            driver: None,
        }
    }

    /// `user[:password]` from before the `@`, percent-decoded.
    fn credentials(&mut self, creds: Option<&str>) -> Result<(), DbError> {
        if let Some(creds) = creds {
            let (user, password) = match creds.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (creds, None),
            };
            self.user = non_empty(decode(user, false)?);
            if let Some(p) = password {
                self.password = Some(decode(p, false)?);
            }
        }
        Ok(())
    }

    /// Applies one `key=value` option (URL query or libpq keyword); unknown keys are listed.
    fn option(&mut self, key: &str, value: String) -> Result<(), DbError> {
        let k = key.to_ascii_lowercase().replace('-', "_");
        match k.as_str() {
            "host" | "hostaddr" | "server" => self.host_list(&value)?,
            "port" => self.port = Some(parse_port(&value)?),
            "user" | "username" => self.user = non_empty(value),
            "password" => self.password = Some(value),
            "dbname" | "database" | "db" => self.database = non_empty(value),
            "sslmode" | "ssl_mode" => self.ssl_mode = Some(parse_ssl_mode(&value)?),
            "usessl" | "ssl" => match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => self.ssl_mode = Some(SslMode::Require),
                "false" | "0" | "no" | "off" => self.ssl_mode = Some(SslMode::Disable),
                _ => {
                    return Err(invalid(format!(
                        "{key} must be true or false, not \"{value}\""
                    )))
                }
            },
            "application_name" | "applicationname" => self.application_name = non_empty(value),
            "connect_timeout" => {
                let s = value.trim().parse::<u32>().map_err(|_| {
                    invalid(format!(
                        "connect_timeout must be whole seconds, not \"{value}\""
                    ))
                })?;
                // libpq: 0 means wait forever; the dialog's field has no such value.
                if s > 0 {
                    self.connect_timeout_s = Some(s);
                }
            }
            // MySQL / JDBC spellings of the password.
            "pwd" | "passwd" | "user_password" => self.password = Some(value),
            // Another secret (e.g. `sslpassword`): named, its value never shown.
            _ if is_secret_key(&k) => self.ignored.push(format!("{key}=•••")),
            _ => self.ignored.push(format!("{key}={value}")),
        }
        Ok(())
    }

    /// `host` may list several servers (`a:5432,b:5433`): the first one is used.
    fn host_list(&mut self, value: &str) -> Result<(), DbError> {
        let mut hosts = value.split(',').map(str::trim).filter(|h| !h.is_empty());
        let Some(first) = hosts.next() else {
            return Ok(());
        };
        if hosts.next().is_some() {
            self.warnings.push(format!(
                "Several hosts are listed; only the first ({first}) is used."
            ));
        }
        let (host, port) = split_host_port(first)?;
        self.host = Some(host);
        if port.is_some() {
            self.port = port;
        }
        Ok(())
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The error never repeats the value: in a mistyped URL (`user:secret` without its `@`) the
/// "port" is the password.
fn parse_port(s: &str) -> Result<u16, DbError> {
    match s.trim().parse::<u16>() {
        Ok(p) if p != 0 => Ok(p),
        _ => Err(invalid("the port must be a number between 1 and 65535")),
    }
}

/// Keys whose value is a secret: an unknown one is listed without its value.
fn is_secret_key(key: &str) -> bool {
    ["pass", "pwd", "secret", "token", "credential"]
        .iter()
        .any(|s| key.contains(s))
}

/// libpq `sslmode` values, plus MySQL's `ssl-mode` spellings.
fn parse_ssl_mode(s: &str) -> Result<SslMode, DbError> {
    let mode = match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "disable" | "disabled" => SslMode::Disable,
        // `allow` (try without TLS first) has no equivalent; `prefer` is the closest.
        "allow" | "prefer" | "preferred" => SslMode::Prefer,
        "require" | "required" => SslMode::Require,
        "verify_ca" => SslMode::VerifyCa,
        "verify_full" | "verify_identity" => SslMode::VerifyFull,
        _ => return Err(invalid(format!("unknown SSL mode \"{s}\""))),
    };
    Ok(mode)
}

/// `host`, `host:port`, `[v6]`, `[v6]:port`.
fn split_host_port(s: &str) -> Result<(String, Option<u16>), DbError> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| invalid("an IPv6 address must be closed with ]"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(parse_port(p)?),
            None if after.is_empty() => None,
            None => return Err(invalid("unexpected text after the IPv6 address")),
        };
        return Ok((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        // More than one colon without brackets: a bare IPv6 address.
        Some((h, _)) if h.contains(':') => Ok((s.to_string(), None)),
        Some((h, p)) => Ok((h.to_string(), Some(parse_port(p)?))),
        None => Ok((s.to_string(), None)),
    }
}

/// `%XX` escapes (and `+` as space in the query part, `plus_is_space`).
fn decode(s: &str, plus_is_space: bool) -> Result<String, DbError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| invalid("a % escape must be followed by two hex digits"))?;
                out.push(hex);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| invalid("the connection string is not valid UTF-8 after decoding"))
}

/// Where the query starts: the first `?` followed by `key=` (a password may hold a lone `?`).
fn query_start(rest: &str) -> usize {
    for (i, _) in rest.match_indices('?') {
        let tail = &rest[i + 1..];
        let key = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
            .unwrap_or(tail.len());
        if key > 0 && tail[key..].starts_with('=') {
            return i;
        }
    }
    rest.len()
}

/// Splits `[user[:password]@]rest` off the part after `scheme://`. The credentials end at the
/// last `@` before the query (a password may hold an unescaped `@`, `/`, `?`, `#` or `=`), as
/// long as the user name itself has no `/`, `?` or `#` (else that `@` is in the path).
fn split_credentials(rest: &str) -> (Option<&str>, &str) {
    if let Some(at) = rest[..query_start(rest)].rfind('@') {
        let creds = &rest[..at];
        let user = creds.split_once(':').map_or(creds, |(u, _)| u);
        if !user.contains(['/', '?', '#']) {
            return (Some(creds), &rest[at + 1..]);
        }
    }
    (None, rest)
}

/// The raw (still encoded) parts of what follows `scheme://`.
struct UrlParts<'a> {
    creds: Option<&'a str>,
    hosts: &'a str,
    path: Option<&'a str>,
    query: Option<&'a str>,
}

fn url_parts(rest: &str) -> UrlParts<'_> {
    let (creds, rest) = split_credentials(rest);
    // A `#fragment` means nothing to a server.
    let rest = rest.split_once('#').map_or(rest, |(r, _)| r);
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    let (hosts, path) = match main.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (main, None),
    };
    UrlParts {
        creds,
        hosts,
        path,
        query,
    }
}

/// The decoded `key=value` pairs of a query, in order.
fn query_pairs(query: Option<&str>) -> Result<Vec<(String, String)>, DbError> {
    let mut pairs = Vec::new();
    for pair in query.into_iter().flat_map(|q| q.split('&')) {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((decode(k, true)?, decode(v, true)?));
    }
    Ok(pairs)
}

fn parse_url(engine: ConnStringEngine, rest: &str) -> Result<ParsedConnString, DbError> {
    let mut parsed = ParsedConnString::empty(Some(engine));
    let parts = url_parts(rest);
    parsed.credentials(parts.creds)?;
    parsed.host_list(&decode(parts.hosts, false)?)?;
    if let Some(path) = parts.path {
        parsed.database = non_empty(decode(path.trim_end_matches('/'), false)?);
    }
    for (k, v) in query_pairs(parts.query)? {
        parsed.option(&k, v)?;
    }
    Ok(parsed)
}

/// `true` / `false` (any case) for a MongoDB boolean option; the value is not repeated.
fn uri_bool(key: &str, value: &str) -> Result<bool, DbError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(invalid_uri(format!("{key} must be true or false"))),
    }
}

/// `mongodb://` (`srv = false`) and `mongodb+srv://`.
fn parse_mongo(srv: bool, rest: &str) -> Result<ParsedConnString, DbError> {
    let engine = if srv {
        ConnStringEngine::MongoSrv
    } else {
        ConnStringEngine::Mongo
    };
    let mut parsed = ParsedConnString::empty(Some(engine));
    let parts = url_parts(rest);
    parsed.credentials(parts.creds)?;

    let host_list = decode(parts.hosts, false)?;
    let hosts: Vec<&str> = host_list
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    let mut extra_hosts = Vec::new();
    if srv {
        if hosts.len() > 1 {
            return Err(invalid_uri("mongodb+srv:// takes a single host name"));
        }
        if let Some(h) = hosts.first() {
            let (host, port) = split_host_port(h)?;
            if port.is_some() {
                return Err(invalid_uri("mongodb+srv:// does not allow a port"));
            }
            parsed.host = Some(host);
        }
    } else {
        parsed.port = Some(MONGO_PORT);
        for (i, h) in hosts.iter().enumerate() {
            let (host, port) = split_host_port(h)?;
            let port = port.unwrap_or(MONGO_PORT);
            if i == 0 {
                parsed.host = Some(host);
                parsed.port = Some(port);
            } else {
                extra_hosts.push(HostPort { host, port });
            }
        }
    }
    if let Some(path) = parts.path {
        parsed.database = non_empty(decode(path.trim_end_matches('/'), false)?);
    }

    let mut auth_source = None;
    let mut replica_set = None;
    let mut direct_connection = false;
    // MongoDB option names are case-insensitive; errors use the name as written.
    for (key, value) in query_pairs(parts.query)? {
        match key.to_ascii_lowercase().as_str() {
            "authsource" => auth_source = non_empty(value),
            "replicaset" => replica_set = non_empty(value),
            "directconnection" => direct_connection = uri_bool(&key, &value)?,
            "tls" | "ssl" => {
                parsed.ssl_mode = Some(if uri_bool(&key, &value)? {
                    SslMode::Require
                } else {
                    SslMode::Disable
                })
            }
            "tlscafile" => {
                return Err(invalid_uri(
                    "Unsupported URI option: tlsCAFile (pick the CA file in the dialog)",
                ))
            }
            "appname" => parsed.application_name = non_empty(value),
            "connecttimeoutms" => {
                let ms = value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| invalid_uri(format!("{key} must be whole milliseconds")))?;
                // 0 means no limit; the dialog's field has no such value.
                if ms > 0 {
                    let s = u32::try_from(ms.div_ceil(1000))
                        .map_err(|_| invalid_uri(format!("{key} is too large")))?;
                    parsed.connect_timeout_s = Some(s);
                }
            }
            _ => return Err(unsupported_option(&key)),
        }
    }
    parsed.driver = Some(DriverOptions::Mongo {
        srv,
        auth_source,
        replica_set,
        direct_connection,
        extra_hosts,
    });
    Ok(parsed)
}

/// `redis://` and `rediss://` (`tls = true`).
fn parse_redis(tls: bool, rest: &str) -> Result<ParsedConnString, DbError> {
    let engine = if tls {
        ConnStringEngine::Rediss
    } else {
        ConnStringEngine::Redis
    };
    let mut parsed = ParsedConnString::empty(Some(engine));
    if tls {
        parsed.ssl_mode = Some(SslMode::Require);
    }
    let parts = url_parts(rest);
    parsed.credentials(parts.creds)?;
    let host = decode(parts.hosts, false)?;
    let (host, port) = match host.trim() {
        "" => (None, None),
        h => {
            let (h, p) = split_host_port(h)?;
            (Some(h), p)
        }
    };
    parsed.host = host;
    parsed.port = Some(port.unwrap_or(REDIS_PORT));
    let db = match parts.path.map(|p| p.trim_end_matches('/')) {
        None | Some("") => 0,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|_| p.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| invalid_uri("Redis database must be a number"))?,
    };
    for (key, value) in query_pairs(parts.query)? {
        if !key.eq_ignore_ascii_case("timeout") {
            return Err(unsupported_option(&key));
        }
        let s = value
            .trim()
            .parse::<u32>()
            .map_err(|_| invalid_uri(format!("{key} must be whole seconds")))?;
        if s > 0 {
            parsed.connect_timeout_s = Some(s);
        }
    }
    parsed.driver = Some(DriverOptions::Redis { db });
    Ok(parsed)
}

/// libpq keyword/value: `key = value` pairs separated by spaces; a value may be single-quoted,
/// with `\'` and `\\` escapes inside.
fn parse_keywords(s: &str) -> Result<ParsedConnString, DbError> {
    let mut parsed = ParsedConnString::empty(None);
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next() != Some('=') {
            // The "key" is not repeated: it may be the rest of an unquoted password with a space.
            return Err(invalid(
                "expected key=value pairs; quote a value that has spaces ('…')",
            ));
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\\') => match chars.next() {
                        Some(c) => value.push(c),
                        None => {
                            return Err(invalid(format!("unterminated quoted value for \"{key}\"")))
                        }
                    },
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => {
                        return Err(invalid(format!("unterminated quoted value for \"{key}\"")))
                    }
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                value.push(c);
            }
        }
        parsed.option(&key, value)?;
    }
    Ok(parsed)
}

/// Parses a pasted connection string. The error is a `config` error on the field `conn_string`
/// whose message says what is wrong (it never repeats the string: it may hold a password).
pub fn parse_connection_string(input: &str) -> Result<ParsedConnString, DbError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(invalid("paste a connection string"));
    }
    let s = s.strip_prefix("jdbc:").unwrap_or(s);
    if let Some((scheme, rest)) = s.split_once("://") {
        let engine = match scheme.to_ascii_lowercase().as_str() {
            "postgresql" | "postgres" | "pgsql" => ConnStringEngine::Postgres,
            "mysql" | "mysqlx" => ConnStringEngine::Mysql,
            "mariadb" => ConnStringEngine::Mariadb,
            "mongodb" => return parse_mongo(false, rest),
            "mongodb+srv" => return parse_mongo(true, rest),
            "redis" => return parse_redis(false, rest),
            "rediss" => return parse_redis(true, rest),
            other => {
                return Err(invalid(format!(
                    "unsupported scheme \"{other}://\"; use postgresql://, mysql://, mariadb://, \
                     mongodb://, mongodb+srv://, redis:// or rediss://"
                )))
            }
        };
        return parse_url(engine, rest);
    }
    if s.contains('=') {
        return parse_keywords(s);
    }
    Err(invalid(
        "not a connection string; expected postgresql://, mysql://, mariadb:// or host=… port=…",
    ))
}

/// Percent-encodes everything but the URI's unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A host for a URI: an IPv6 address goes in brackets, anything else is encoded.
fn encode_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        encode(host)
    }
}

/// libpq `sslmode` spelling.
fn pg_ssl_mode(mode: SslMode) -> &'static str {
    match mode {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
        SslMode::VerifyCa => "verify-ca",
        SslMode::VerifyFull => "verify-full",
    }
}

/// MySQL `ssl-mode` spelling.
fn mysql_ssl_mode(mode: SslMode) -> &'static str {
    match mode {
        SslMode::Disable => "DISABLED",
        SslMode::Prefer => "PREFERRED",
        SslMode::Require => "REQUIRED",
        SslMode::VerifyCa => "VERIFY_CA",
        SslMode::VerifyFull => "VERIFY_IDENTITY",
    }
}

fn driver_mismatch() -> DbError {
    DbError::Config {
        field: Some("options.driver".into()),
        message: "Driver options do not match the engine".into(),
    }
}

/// The connection as a URI, for display and copy. It never holds the password (nor any
/// other secret); every component is percent-encoded.
pub fn to_uri(cfg: &ConnectionConfig) -> Result<String, DbError> {
    let ssl_mode = cfg.ssl.as_ref().map(|s| s.mode);
    let tls = matches!(
        ssl_mode,
        Some(SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull)
    );
    let database = cfg
        .default_db
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(encode);
    let mut query: Vec<(&str, String)> = Vec::new();
    let (scheme, hosts, path) = match cfg.kind {
        DbKind::Postgres => {
            if let Some(mode) = ssl_mode {
                query.push(("sslmode", pg_ssl_mode(mode).into()));
            }
            if let Some(app) = &cfg.options.application_name {
                query.push(("application_name", encode(app)));
            }
            let hosts = format!("{}:{}", encode_host(&cfg.host), cfg.port);
            ("postgresql", hosts, database)
        }
        DbKind::MySql => {
            if let Some(mode) = ssl_mode {
                query.push(("ssl-mode", mysql_ssl_mode(mode).into()));
            }
            let hosts = format!("{}:{}", encode_host(&cfg.host), cfg.port);
            ("mysql", hosts, database)
        }
        DbKind::Mongo => {
            let DriverOptions::Mongo {
                srv,
                auth_source,
                replica_set,
                direct_connection,
                extra_hosts,
            } = &cfg.options.driver
            else {
                return Err(driver_mismatch());
            };
            if let Some(v) = auth_source {
                query.push(("authSource", encode(v)));
            }
            if let Some(v) = replica_set {
                query.push(("replicaSet", encode(v)));
            }
            if *direct_connection {
                query.push(("directConnection", "true".into()));
            }
            if tls {
                query.push(("tls", "true".into()));
            }
            if let Some(v) = &cfg.options.application_name {
                query.push(("appName", encode(v)));
            }
            let (scheme, hosts) = if *srv {
                ("mongodb+srv", encode_host(&cfg.host))
            } else {
                let all = std::iter::once((cfg.host.as_str(), cfg.port))
                    .chain(extra_hosts.iter().map(|h| (h.host.as_str(), h.port)));
                let hosts: Vec<String> = all
                    .map(|(h, p)| format!("{}:{p}", encode_host(h)))
                    .collect();
                ("mongodb", hosts.join(","))
            };
            // Options need the `/` even without a database.
            let path = match (database, query.is_empty()) {
                (Some(d), _) => Some(d),
                (None, false) => Some(String::new()),
                (None, true) => None,
            };
            (scheme, hosts, path)
        }
        DbKind::Redis => {
            let DriverOptions::Redis { db } = &cfg.options.driver else {
                return Err(driver_mismatch());
            };
            let scheme = if tls { "rediss" } else { "redis" };
            let hosts = format!("{}:{}", encode_host(&cfg.host), cfg.port);
            (scheme, hosts, (*db != 0).then(|| db.to_string()))
        }
    };
    let mut uri = format!("{scheme}://");
    if !cfg.username.is_empty() {
        uri.push_str(&encode(&cfg.username));
        uri.push('@');
    }
    uri.push_str(&hosts);
    if let Some(path) = path {
        uri.push('/');
        uri.push_str(&path);
    }
    for (i, (k, v)) in query.iter().enumerate() {
        uri.push(if i == 0 { '?' } else { '&' });
        uri.push_str(k);
        uri.push('=');
        uri.push_str(v);
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> ParsedConnString {
        match parse_connection_string(s) {
            Ok(p) => p,
            Err(e) => panic!("{s}: {e}"),
        }
    }

    fn err(s: &str) -> String {
        match parse_connection_string(s) {
            Err(DbError::Config { field, message }) => {
                assert_eq!(field.as_deref(), Some(FIELD));
                message
            }
            Err(e) => panic!("{s}: unexpected error {e}"),
            Ok(_) => panic!("{s}: parsed"),
        }
    }

    #[test]
    fn full_postgres_url() {
        let p = ok("postgresql://app_user:s3cr%40t@db.internal:6432/sales?sslmode=require&application_name=dnt&connect_timeout=5");
        assert_eq!(p.engine, Some(ConnStringEngine::Postgres));
        assert_eq!(p.host.as_deref(), Some("db.internal"));
        assert_eq!(p.port, Some(6432));
        assert_eq!(p.user.as_deref(), Some("app_user"));
        assert_eq!(p.password.as_deref(), Some("s3cr@t"));
        assert_eq!(p.database.as_deref(), Some("sales"));
        assert_eq!(p.ssl_mode, Some(SslMode::Require));
        assert_eq!(p.application_name.as_deref(), Some("dnt"));
        assert_eq!(p.connect_timeout_s, Some(5));
        assert!(p.ignored.is_empty() && p.warnings.is_empty());
    }

    #[test]
    fn schemes_pick_the_engine() {
        assert_eq!(ok("postgres://h").engine, Some(ConnStringEngine::Postgres));
        assert_eq!(ok("MySQL://h").engine, Some(ConnStringEngine::Mysql));
        assert_eq!(ok("mariadb://h").engine, Some(ConnStringEngine::Mariadb));
        assert_eq!(ok("jdbc:mysql://h:3307/db").port, Some(3307));
        assert!(err("sqlserver://h").contains("unsupported scheme"));
    }

    #[test]
    fn missing_parts_stay_none() {
        let p = ok("mysql://");
        assert!(p.host.is_none() && p.port.is_none() && p.user.is_none() && p.database.is_none());
        let p = ok("mysql://root@localhost");
        assert_eq!(p.user.as_deref(), Some("root"));
        assert!(p.password.is_none());
        assert_eq!(p.host.as_deref(), Some("localhost"));
        // An empty password is a password (it was written).
        assert_eq!(ok("mysql://root:@h").password.as_deref(), Some(""));
    }

    #[test]
    fn ipv6_and_unescaped_at_in_password() {
        let p = ok("postgresql://u:p@ss@[::1]:5433/db");
        assert_eq!(p.password.as_deref(), Some("p@ss"));
        assert_eq!(p.host.as_deref(), Some("::1"));
        assert_eq!(p.port, Some(5433));
        assert!(err("postgresql://[::1/db").contains("IPv6"));
    }

    #[test]
    fn several_hosts_use_the_first_with_a_warning() {
        let p = ok("postgresql://a:5432,b:5433/db");
        assert_eq!(p.host.as_deref(), Some("a"));
        assert_eq!(p.port, Some(5432));
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn mysql_ssl_spellings() {
        assert_eq!(
            ok("mysql://h?ssl-mode=VERIFY_IDENTITY").ssl_mode,
            Some(SslMode::VerifyFull)
        );
        assert_eq!(
            ok("mysql://h?sslMode=REQUIRED").ssl_mode,
            Some(SslMode::Require)
        );
        assert_eq!(
            ok("mysql://h?useSSL=false").ssl_mode,
            Some(SslMode::Disable)
        );
        assert!(err("mysql://h?sslmode=maybe").contains("unknown SSL mode"));
    }

    #[test]
    fn unknown_parameters_are_listed() {
        let p = ok("mysql://h/db?charset=utf8mb4&serverTimezone=UTC");
        assert_eq!(p.ignored, vec!["charset=utf8mb4", "serverTimezone=UTC"]);
    }

    #[test]
    fn bad_port_and_escape() {
        assert!(err("mysql://h:0").contains("port"));
        assert!(err("mysql://h:99999").contains("port"));
        assert!(err("mysql://u:%zz@h").contains("hex"));
    }

    #[test]
    fn libpq_keywords() {
        let p = ok("host=db.internal port=6432 user=app password='it\\'s secret' dbname = sales sslmode=verify-full");
        assert_eq!(p.engine, None);
        assert_eq!(p.host.as_deref(), Some("db.internal"));
        assert_eq!(p.port, Some(6432));
        assert_eq!(p.password.as_deref(), Some("it's secret"));
        assert_eq!(p.database.as_deref(), Some("sales"));
        assert_eq!(p.ssl_mode, Some(SslMode::VerifyFull));
        assert!(err("host=db port").contains("expected key=value"));
        // An unquoted password with a space: the second word is not echoed as a "key".
        assert!(!err("host=db password=hun ter2").contains("ter2"));
        assert!(err("password='open").contains("unterminated"));
    }

    #[test]
    fn unescaped_url_characters_in_the_password() {
        for (s, pw) in [
            ("postgresql://u:hun/ter2@h:5433/db", "hun/ter2"),
            ("postgresql://u:hun?ter2@h:5433/db", "hun?ter2"),
            ("postgresql://u:hun#ter2@h:5433/db", "hun#ter2"),
            ("postgresql://u:a=b@h:5433/db?sslmode=require", "a=b"),
        ] {
            let p = ok(s);
            assert_eq!(p.password.as_deref(), Some(pw), "{s}");
            assert_eq!(p.host.as_deref(), Some("h"), "{s}");
            assert_eq!(p.port, Some(5433), "{s}");
            assert_eq!(p.database.as_deref(), Some("db"), "{s}");
        }
        // An `@` in the path or the query is not a credential separator.
        let p = ok("mysql://h/db?user=a@b");
        assert_eq!(p.host.as_deref(), Some("h"));
        assert_eq!(p.user.as_deref(), Some("a@b"));
        assert!(p.password.is_none());
        assert_eq!(ok("mysql://h/db#frag").database.as_deref(), Some("db"));
    }

    #[test]
    fn secret_parameters_never_show_their_value() {
        assert_eq!(
            ok("mysql://h?pwd=hunter2").password.as_deref(),
            Some("hunter2")
        );
        assert_eq!(
            ok("host=h passwd=hunter2").password.as_deref(),
            Some("hunter2")
        );
        let p = ok("postgresql://h?sslpassword=hunter2&apiToken=hunter2");
        assert!(p.password.is_none());
        assert_eq!(p.ignored, vec!["sslpassword=•••", "apiToken=•••"]);
    }

    #[test]
    fn errors_never_repeat_the_password() {
        for s in [
            "postgresql://u:hunter2@h:bad",
            "host=h password=hunter2 port=x",
            // No `@`: the password reads as the port.
            "postgresql://u:hunter2",
            "postgresql://u:hunter2/db",
            "postgresql://[::1]hunter2",
        ] {
            assert!(!err(s).contains("hunter2"), "{s}");
        }
        assert!(err("   ").contains("paste"));
        assert!(err("just text").contains("not a connection string"));
    }

    // ---- MongoDB / Redis (R3) ----

    use crate::config::{ConnectionConfig, DriverOptions, HostPort, SslConfig};
    use crate::server::DbKind;

    /// A `config` error on the field `uri`; returns its message.
    fn uri_err(s: &str) -> String {
        match parse_connection_string(s) {
            Err(DbError::Config { field, message }) => {
                assert_eq!(field.as_deref(), Some("uri"), "{s}: {message}");
                message
            }
            Err(e) => panic!("{s}: unexpected error {e}"),
            Ok(_) => panic!("{s}: parsed"),
        }
    }

    /// The config the dialog would build from a parsed string.
    fn config_from(p: &ParsedConnString) -> ConnectionConfig {
        let mut c = crate::test_util::sample_config();
        c.kind = match p.engine {
            Some(ConnStringEngine::Mongo | ConnStringEngine::MongoSrv) => DbKind::Mongo,
            Some(ConnStringEngine::Redis | ConnStringEngine::Rediss) => DbKind::Redis,
            Some(ConnStringEngine::Postgres) | None => DbKind::Postgres,
            Some(ConnStringEngine::Mysql | ConnStringEngine::Mariadb) => DbKind::MySql,
        };
        c.host = p.host.clone().unwrap_or_default();
        // The dialog's engine default for a port the string did not name (none under SRV).
        c.port = p.port.unwrap_or(match c.kind {
            DbKind::Postgres => 5432,
            DbKind::MySql => 3306,
            DbKind::Mongo | DbKind::Redis => 0,
        });
        c.username = p.user.clone().unwrap_or_default();
        c.default_db = p.database.clone();
        c.ssl = p.ssl_mode.map(|mode| SslConfig {
            version: 1,
            mode,
            ca_path: None,
            cert_path: None,
            key_path: None,
        });
        c.options.application_name = p.application_name.clone();
        c.options.driver = p.driver.clone().unwrap_or_default();
        c
    }

    fn mongo_driver(p: &ParsedConnString) -> (bool, Option<&str>, Option<&str>, bool, &[HostPort]) {
        match &p.driver {
            Some(DriverOptions::Mongo {
                srv,
                auth_source,
                replica_set,
                direct_connection,
                extra_hosts,
            }) => (
                *srv,
                auth_source.as_deref(),
                replica_set.as_deref(),
                *direct_connection,
                extra_hosts,
            ),
            other => panic!("expected Mongo driver options, got {other:?}"),
        }
    }

    fn redis_db(p: &ParsedConnString) -> u16 {
        match &p.driver {
            Some(DriverOptions::Redis { db }) => *db,
            other => panic!("expected Redis driver options, got {other:?}"),
        }
    }

    #[test]
    fn mongo_password_is_extracted_and_never_printed() {
        let p = ok("mongodb://app:p%40ss%2Fw@h1:27018,h2/sales?authSource=admin&replicaSet=rs0");
        assert_eq!(p.engine, Some(ConnStringEngine::Mongo));
        assert_eq!(p.password.as_deref(), Some("p@ss/w"));
        assert_eq!(p.user.as_deref(), Some("app"));
        assert_eq!(p.host.as_deref(), Some("h1"));
        assert_eq!(p.port, Some(27018));
        assert_eq!(p.database.as_deref(), Some("sales"));
        let (srv, auth_source, replica_set, direct, extra) = mongo_driver(&p);
        assert!(!srv && !direct);
        assert_eq!(auth_source, Some("admin"));
        assert_eq!(replica_set, Some("rs0"));
        assert_eq!(
            extra,
            [HostPort {
                host: "h2".into(),
                port: 27017
            }]
        );
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("p@ss/w") && !dbg.contains("p%40ss"), "{dbg}");
        assert!(dbg.contains("***"), "{dbg}");
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "mongodb://app@h1:27018,h2:27017/sales?authSource=admin&replicaSet=rs0"
        );
    }

    #[test]
    fn srv_rejects_port_and_host_list() {
        assert_eq!(
            uri_err("mongodb+srv://u:pw@cluster0.x.mongodb.net:27017/db"),
            "mongodb+srv:// does not allow a port"
        );
        assert_eq!(
            uri_err("mongodb+srv://a.example.com,b.example.com/db"),
            "mongodb+srv:// takes a single host name"
        );
    }

    #[test]
    fn srv_sets_the_flag_and_no_port() {
        let p = ok("mongodb+srv://u@cluster0.x.mongodb.net/db");
        assert_eq!(p.engine, Some(ConnStringEngine::MongoSrv));
        assert_eq!(p.host.as_deref(), Some("cluster0.x.mongodb.net"));
        assert_eq!(p.port, None);
        assert_eq!(p.database.as_deref(), Some("db"));
        let (srv, _, _, _, extra) = mongo_driver(&p);
        assert!(srv && extra.is_empty());
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "mongodb+srv://u@cluster0.x.mongodb.net/db"
        );
    }

    #[test]
    fn mongo_options_map_to_fields() {
        let p = ok(
            "mongodb://h?directConnection=true&tls=true&appName=dnt%20app&connectTimeoutMS=1500",
        );
        assert_eq!(p.port, Some(27017));
        assert!(mongo_driver(&p).3);
        assert_eq!(p.ssl_mode, Some(SslMode::Require));
        assert_eq!(p.application_name.as_deref(), Some("dnt app"));
        assert_eq!(p.connect_timeout_s, Some(2), "rounded up");
        assert_eq!(ok("mongodb://h?ssl=false").ssl_mode, Some(SslMode::Disable));
        assert!(!mongo_driver(&ok("mongodb://h?directConnection=false")).3);
        assert!(p.ignored.is_empty() && p.warnings.is_empty());
        // Option order in `to_uri`: authSource, replicaSet, directConnection, tls, appName.
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "mongodb://h:27017/?directConnection=true&tls=true&appName=dnt%20app"
        );
        let p = ok("mongodb://[::1]:27019/?appName=a&tls=true&directConnection=true&replicaSet=r&authSource=s");
        assert_eq!(p.host.as_deref(), Some("::1"));
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "mongodb://[::1]:27019/?authSource=s&replicaSet=r&directConnection=true&tls=true&appName=a"
        );
        // No options and no database: no trailing `/`.
        assert_eq!(
            to_uri(&config_from(&ok("mongodb://h"))).unwrap(),
            "mongodb://h:27017"
        );
    }

    #[test]
    fn mongo_unknown_option_is_an_error() {
        assert_eq!(
            uri_err("mongodb://h/db?w=majority"),
            "Unsupported URI option: w"
        );
        assert_eq!(
            uri_err("mongodb://h/db?tlsCAFile=%2Fetc%2Fca.pem"),
            "Unsupported URI option: tlsCAFile (pick the CA file in the dialog)"
        );
        assert!(uri_err("mongodb://h?tls=maybe").contains("true or false"));
    }

    #[test]
    fn rediss_requires_tls_and_reads_db() {
        let p = ok("rediss://default:pw@cache:6380/2");
        assert_eq!(p.engine, Some(ConnStringEngine::Rediss));
        assert_eq!(p.ssl_mode, Some(SslMode::Require));
        assert_eq!(p.host.as_deref(), Some("cache"));
        assert_eq!(p.port, Some(6380));
        assert_eq!(redis_db(&p), 2);
        assert_eq!(p.user.as_deref(), Some("default"));
        assert_eq!(p.password.as_deref(), Some("pw"));
        assert!(!format!("{p:?}").contains("\"pw\""));
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "rediss://default@cache:6380/2"
        );
    }

    #[test]
    fn redis_defaults() {
        let p = ok("redis://cache");
        assert_eq!(p.engine, Some(ConnStringEngine::Redis));
        assert_eq!(p.host.as_deref(), Some("cache"));
        assert_eq!(p.port, Some(6379));
        assert_eq!(redis_db(&p), 0);
        assert_eq!(p.ssl_mode, None);
        // db 0 is not written.
        assert_eq!(to_uri(&config_from(&p)).unwrap(), "redis://cache:6379");
        assert_eq!(ok("redis://cache/").port, Some(6379));
        assert_eq!(ok("redis://cache?timeout=7").connect_timeout_s, Some(7));
        let p = ok("redis://:pw@cache");
        assert_eq!((p.user, p.password.as_deref()), (None, Some("pw")));
    }

    #[test]
    fn redis_bad_db() {
        for s in ["redis://cache/x", "redis://cache/65536", "redis://cache/-1"] {
            assert_eq!(uri_err(s), "Redis database must be a number", "{s}");
        }
        assert_eq!(
            uri_err("redis://cache?protocol=resp3"),
            "Unsupported URI option: protocol"
        );
    }

    #[test]
    fn to_uri_never_contains_the_password() {
        for s in [
            "postgresql://u:hunter2@h:5432/db?sslmode=verify-full&application_name=x",
            "mysql://u:hunter2@h:3306/db?sslMode=REQUIRED",
            "mariadb://u:hunter2@h/db",
            "mongodb://u:hunter2@h1,h2:27018/db?authSource=admin",
            "mongodb+srv://u:hunter2@c.example.com/db",
            "redis://u:hunter2@h:6379/3",
            "rediss://u:hunter2@h",
        ] {
            let p = ok(s);
            assert_eq!(p.password.as_deref(), Some("hunter2"), "{s}");
            assert!(!format!("{p:?}").contains("hunter2"), "{s}");
            let uri = to_uri(&config_from(&p)).unwrap();
            assert!(!uri.contains("hunter2"), "{s} -> {uri}");
            // The URI reads back to the same fields, without a password.
            let back = ok(&uri);
            assert!(back.password.is_none(), "{uri}");
            assert_eq!(config_from(&back), config_from(&p), "{uri}");
        }
    }

    #[test]
    fn to_uri_sql_shape() {
        let p =
            ok("postgresql://app@db.internal:6432/sales?sslmode=verify-ca&application_name=dnt");
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "postgresql://app@db.internal:6432/sales?sslmode=verify-ca&application_name=dnt"
        );
        let p = ok("mysql://root@h/db?ssl-mode=VERIFY_IDENTITY");
        assert_eq!(
            to_uri(&config_from(&p)).unwrap(),
            "mysql://root@h:3306/db?ssl-mode=VERIFY_IDENTITY"
        );
        // Every component is percent-encoded.
        let mut c = crate::test_util::sample_config();
        c.kind = DbKind::Postgres;
        c.host = "h".into();
        c.port = 5432;
        c.username = "a b@c".into();
        c.default_db = Some("x/y".into());
        c.ssl = None;
        c.options.application_name = None;
        assert_eq!(to_uri(&c).unwrap(), "postgresql://a%20b%40c@h:5432/x%2Fy");
    }

    #[test]
    fn to_uri_refuses_mismatched_driver_options() {
        let mut c = crate::test_util::sample_config();
        c.kind = DbKind::Redis;
        c.options.driver = DriverOptions::None;
        assert!(matches!(
            to_uri(&c),
            Err(DbError::Config { field: Some(f), .. }) if f == "options.driver"
        ));
    }

    #[test]
    fn sql_parsing_has_no_driver_options() {
        assert_eq!(ok("postgresql://h/db").driver, None);
        assert_eq!(ok("mysql://h").driver, None);
        assert_eq!(ok("host=h").driver, None);
        assert!(err("sqlserver://h").contains("mongodb+srv://"));
    }
}
