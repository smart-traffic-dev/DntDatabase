//! Connection strings pasted into the connection dialog.
//!
//! Accepted forms:
//! - URLs: `postgresql://`, `postgres://`, `mysql://`, `mariadb://` (an optional `jdbc:` prefix
//!   is ignored): `scheme://[user[:password]@][host][:port][/database][?key=value&…]`.
//! - libpq keyword/value strings: `host=db port=5432 user=app dbname=sales sslmode=require`.
//!
//! Nothing here is stored: the result only fills the dialog's fields. The password is handed
//! back to the UI (it was typed there) and never logged; the type has no `Debug`.

use crate::config::SslMode;
use crate::error::DbError;
use serde::Serialize;
use specta::Type;

/// The engine a connection string names (MariaDB is told apart from MySQL only by its scheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum ConnStringEngine {
    Mysql,
    Mariadb,
    Postgres,
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
}

const FIELD: &str = "conn_string";

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some(FIELD.into()),
        message: message.into(),
    }
}

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
        }
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

fn parse_url(engine: ConnStringEngine, rest: &str) -> Result<ParsedConnString, DbError> {
    let mut parsed = ParsedConnString::empty(Some(engine));
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
    if let Some(creds) = creds {
        let (user, password) = match creds.split_once(':') {
            Some((u, p)) => (u, Some(p)),
            None => (creds, None),
        };
        parsed.user = non_empty(decode(user, false)?);
        if let Some(p) = password {
            parsed.password = Some(decode(p, false)?);
        }
    }
    parsed.host_list(&decode(hosts, false)?)?;
    if let Some(path) = path {
        parsed.database = non_empty(decode(path.trim_end_matches('/'), false)?);
    }
    if let Some(query) = query {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            parsed.option(&decode(k, true)?, decode(v, true)?)?;
        }
    }
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
            other => {
                return Err(invalid(format!(
                    "unsupported scheme \"{other}://\"; use postgresql://, mysql:// or mariadb://"
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
        assert!(err("redis://h").contains("unsupported scheme"));
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
}
