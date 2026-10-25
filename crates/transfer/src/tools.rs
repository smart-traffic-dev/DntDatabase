//! N7 external tools, the pure half (spec §7.2, R25–R29): which programs may run, where
//! they are looked up, and the exact arguments, environment and secret files of each
//! invocation. `src-tauri/src/backup/ext_tools.rs` spawns them.
//!
//! Rules this module enforces:
//!
//! - Only the five whitelisted programs ([`Tool`]) are ever named; they are looked up as
//!   `<name>.exe` (Windows) in the configured folder first, then in each **absolute**
//!   `PATH` entry (a relative entry such as `.` would find a program planted in the
//!   current folder, so it is skipped) (R25).
//! - Arguments come only from typed options (R26). Object names starting with `-` or
//!   holding NUL/CR/LF are refused. MySQL tools get `--defaults-file=<secret file>` as
//!   the **first** argument (no global `my.ini` is read) and the database and tables after
//!   `--` (mysqldump stops reading options there; checked against mysqldump 8.4.3: a
//!   table named `-x` after `--` is a table name). PostgreSQL tools get one
//!   `--dbname=<conninfo>` without a password and `--no-password`.
//! - The password is never an argument or an environment value (R27): MySQL reads it
//!   from the option file ([`mysql_option_file`]), PostgreSQL from a pgpass file
//!   ([`pgpass_line`]) named by `PGPASSFILE` (a path). The pgpass line names the one
//!   host, port and user the tool was started for (never `*:*:*:*`), so a `\connect` to
//!   another server inside a restored file does not receive the password. [`ENV_REMOVE`]
//!   and every variable [`is_scrubbed_env`] matches are dropped from the child's
//!   environment. MySQL clients also read the login-path file (`.mylogin.cnf`) even with
//!   `--defaults-file`; `MYSQL_TEST_LOGIN_FILE` points it at a file that does not exist,
//!   so a saved login path cannot override the option file.
//! - Through an SSH tunnel `verify-full` / `VERIFY_IDENTITY` is refused (R28).
//! - On Windows ([`ARGV_ASCII_ONLY`]) non-ASCII names and paths are refused: the clients
//!   read their arguments and environment in the ANSI code page and would receive them
//!   mangled.
//!
//! Additions to spec §7.2 found against the real clients: mysqldump also gets
//! `--no-tablespaces` (MySQL 8.0.21+ needs the `PROCESS` privilege to dump tablespaces,
//! which ordinary users lack) and a MySQL 8 client gets `--get-server-public-key` when
//! the connection may be plaintext (TLS off or `prefer`): a `caching_sha2_password`
//! account fails without TLS otherwise. Trade-off: the client then takes the server's
//! RSA public key unverified over the plaintext connection, so an active attacker on the
//! path could substitute a key and learn the password; this is exactly what the app's
//! own driver does on such a connection (no new exposure), and TLS (`require` or a
//! verify mode) avoids it.

use dnt_core::config::{ResolvedConfig, SslConfig, SslMode};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// The only programs the app ever spawns (R25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    MysqlDump,
    MariadbDump,
    PgDump,
    PgRestore,
    Psql,
}

impl Tool {
    pub const ALL: [Tool; 5] = [
        Tool::MysqlDump,
        Tool::MariadbDump,
        Tool::PgDump,
        Tool::PgRestore,
        Tool::Psql,
    ];

    /// The program name (`mysqldump`, `mariadb-dump`, `pg_dump`, `pg_restore`, `psql`).
    pub fn name(self) -> &'static str {
        match self {
            Tool::MysqlDump => "mysqldump",
            Tool::MariadbDump => "mariadb-dump",
            Tool::PgDump => "pg_dump",
            Tool::PgRestore => "pg_restore",
            Tool::Psql => "psql",
        }
    }

    /// The tool of exactly this program name; nothing else is a tool.
    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The file looked up: `<name>.exe` on Windows, `<name>` elsewhere.
    pub fn file_name(self) -> String {
        if cfg!(windows) {
            format!("{}.exe", self.name())
        } else {
            self.name().to_string()
        }
    }

    /// The engine the tool works with.
    pub fn kind(self) -> DbKind {
        match self {
            Tool::MysqlDump | Tool::MariadbDump => DbKind::MySql,
            Tool::PgDump | Tool::PgRestore | Tool::Psql => DbKind::Postgres,
        }
    }
}

/// A tool that was found, with what `<tool> --version` printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ToolInfo {
    pub tool: Tool,
    pub path: String,
    /// The first line `--version` printed (`mysqldump  Ver 8.4.3 for Win64 …`).
    pub version: String,
    /// A MariaDB client (`mariadb-dump`, or a `mysqldump` built by MariaDB).
    pub mariadb: bool,
}

/// The folders searched, in order: the configured one, then every absolute `PATH`
/// entry (duplicates once).
pub fn search_dirs(configured: Option<&Path>, path_env: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |d: PathBuf| {
        if d.is_absolute() && !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    if let Some(c) = configured {
        push(c.to_path_buf());
    }
    if let Some(p) = path_env {
        for d in std::env::split_paths(p) {
            push(d);
        }
    }
    dirs
}

/// The first `<tool>.exe` file in [`search_dirs`].
pub fn find_tool(
    tool: Tool,
    configured: Option<&Path>,
    path_env: Option<&OsStr>,
) -> Option<PathBuf> {
    let file = tool.file_name();
    search_dirs(configured, path_env)
        .into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// The [`ToolInfo`] of `tool` at `path` from its `--version` output.
pub fn tool_info(tool: Tool, path: &Path, version_output: &str) -> ToolInfo {
    let version = version_output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    let mariadb = tool == Tool::MariadbDump || version.to_ascii_lowercase().contains("mariadb");
    ToolInfo {
        tool,
        path: path.to_string_lossy().into_owned(),
        version,
        mariadb,
    }
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let mut parts = s[..end].split('.').filter(|p| !p.is_empty());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// The client's own version: `Distrib X.Y.Z` (old MySQL and MariaDB `mysqldump`),
/// `from X.Y.Z` (`mariadb-dump`), `Ver X.Y.Z` (MySQL 8), `(PostgreSQL) X.Y`.
pub fn client_version(info: &ToolInfo) -> Option<(u32, u32, u32)> {
    for marker in ["Distrib ", "from ", "Ver ", "(PostgreSQL) "] {
        if let Some(i) = info.version.find(marker) {
            if let Some(v) = parse_version(&info.version[i + marker.len()..]) {
                return Some(v);
            }
        }
    }
    None
}

/// An Oracle MySQL client of version 8 or later.
fn mysql8_client(info: &ToolInfo) -> bool {
    !info.mariadb && client_version(info).is_some_and(|v| v.0 >= 8)
}

/// Where and as whom a tool connects. Never holds the password.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolConn {
    pub kind: DbKind,
    /// The address to dial: the tunnel's local end through SSH.
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: Option<String>,
    pub ssl: Option<SslConfig>,
    /// Through an SSH tunnel (R28).
    pub tunnelled: bool,
}

impl ToolConn {
    /// From the runtime's resolved address (the tunnel's local port through SSH). The
    /// password stays in `resolved`.
    pub fn from_resolved(resolved: &ResolvedConfig, tunnelled: bool) -> ToolConn {
        ToolConn {
            kind: resolved.kind,
            host: resolved.host.clone(),
            port: resolved.port,
            user: resolved.username.clone(),
            database: resolved.database.clone(),
            ssl: resolved.ssl.clone(),
            tunnelled,
        }
    }
}

/// mysqldump / mariadb-dump options (spec §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MysqlDumpOptions {
    /// Empty: every table of the database.
    pub tables: Vec<String>,
    pub structure: bool,
    pub data: bool,
    pub routines: bool,
    pub events: bool,
    pub triggers: bool,
    pub single_transaction: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PgDumpFormat {
    Plain,
    Custom,
    Tar,
}

/// pg_dump options (spec §7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PgDumpOptions {
    /// Empty: every schema.
    pub schemas: Vec<String>,
    /// `(schema, table)`; empty: every table of the chosen schemas.
    pub tables: Vec<(String, String)>,
    pub structure: bool,
    pub data: bool,
    pub format: PgDumpFormat,
    pub no_owner: bool,
    pub no_privileges: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PsqlOptions {
    pub single_transaction: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PgRestoreOptions {
    pub single_transaction: bool,
    /// `--clean --if-exists`.
    pub clean: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
}

/// The secret file an invocation needs, written by the runner just before the spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretFileSpec {
    None,
    /// [`mysql_option_file`] contents.
    MysqlOptionFile(PathBuf),
    /// [`pgpass_line`] contents.
    PgPass(PathBuf),
}

/// What the runner does with the tool's stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutUse {
    /// Into the output file (`.dntpart`, gzip for `.sql.gz`).
    File,
    /// Captured (`pg_restore --list`, bounded).
    List,
    /// Read and dropped.
    Discard,
}

/// One run of a tool, fully decided before anything is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env_remove: Vec<&'static str>,
    /// Paths only, never a secret.
    pub env_set: Vec<(&'static str, OsString)>,
    pub secret: SecretFileSpec,
    /// The one server the pgpass line is valid for (PostgreSQL tools).
    pub pgpass_scope: Option<PgPassScope>,
    /// stdin is fed from the restore file.
    pub stdin_file: bool,
    pub stdout: StdoutUse,
}

/// Host, port and user a pgpass line matches: only the server the tool was started
/// for, so a `\connect` to another host in a restored file never gets the password.
/// Through an SSH tunnel the host is the tunnel's local end (`127.0.0.1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgPassScope {
    pub host: String,
    pub port: u16,
    pub user: String,
}

/// Removed from every tool's environment (R27).
pub const ENV_REMOVE: [&str; 4] = ["PGPASSWORD", "MYSQL_PWD", "PGSERVICE", "PGSERVICEFILE"];

/// A variable of the app's own environment a tool never inherits (R27): every `PG*`
/// (libpq reads `PGHOST`, `PGSSLMODE`, `PGPASSWORD`, `PGSERVICE`, …), `MYSQL_*`,
/// `LIBMYSQL_*` and `MARIADB_*`. What the invocation sets itself (`PGPASSFILE`,
/// `MYSQL_TEST_LOGIN_FILE`) is set after the scrub; `SystemRoot`, `PATH`, `TEMP` and the
/// rest are kept.
pub fn is_scrubbed_env(name: &OsStr) -> bool {
    let n = name.to_string_lossy().to_ascii_uppercase();
    n.starts_with("PG")
        || n.starts_with("MYSQL_")
        || n.starts_with("LIBMYSQL_")
        || n.starts_with("MARIADB_")
}

/// The login-path file MySQL clients are pointed at: next to the option file, never
/// created.
pub const NO_LOGIN_PATH_FILE: &str = "no-login-path.cnf";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error(
        "{what} {name:?} cannot be passed to an external tool: it is empty, starts with \"-\" \
         or holds a line break."
    )]
    UnsafeName { what: &'static str, name: String },
    #[error(
        "verify-full TLS cannot check the server name through an SSH tunnel with an external \
         tool. Use the built-in dump, or verify-ca."
    )]
    VerifyFullThroughTunnel,
    #[error("{tool} cannot be used for this connection.")]
    WrongTool { tool: &'static str },
    #[error("Choose the structure, the data or both.")]
    NothingToDump,
    #[error("Choose a database.")]
    NoDatabase,
    #[error(
        "The password holds a character the tool's password file cannot carry (NUL, or a line \
         break for PostgreSQL)."
    )]
    UnsafePassword,
    #[error(
        "External tools on Windows cannot receive non-ASCII names ({what} {name:?}); use the \
         built-in dump."
    )]
    NonAsciiName { what: &'static str, name: String },
    #[error(
        "External tools on Windows cannot receive non-ASCII paths ({what} {path:?}); use a \
         folder whose path is ASCII only."
    )]
    NonAsciiPath { what: &'static str, path: String },
}

/// On Windows the MySQL and PostgreSQL clients read their arguments (and environment) in
/// the ANSI code page, so a non-ASCII name or path arrives mangled (mysqldump 8.4:
/// `đơn_hàng` becomes `don_h?ng`, "Illegal mix of collations"). Such values are refused
/// up front instead.
pub const ARGV_ASCII_ONLY: bool = cfg!(windows);

fn has_control(s: &str) -> bool {
    s.contains(['\0', '\r', '\n'])
}

fn check_ascii_name(what: &'static str, name: &str) -> Result<(), ToolError> {
    if ARGV_ASCII_ONLY && !name.is_ascii() {
        return Err(ToolError::NonAsciiName {
            what,
            name: name.to_string(),
        });
    }
    Ok(())
}

/// An object name passed as its own argument: not empty, not starting with `-`, no
/// NUL/CR/LF (R26); ASCII on Windows.
fn check_name(what: &'static str, name: &str) -> Result<(), ToolError> {
    if name.is_empty() || name.starts_with('-') || has_control(name) {
        return Err(ToolError::UnsafeName {
            what,
            name: name.to_string(),
        });
    }
    check_ascii_name(what, name)
}

/// A value inside an option (`--user=…`, conninfo): no NUL/CR/LF; ASCII on Windows.
fn check_text(what: &'static str, text: &str) -> Result<(), ToolError> {
    if has_control(text) {
        return Err(ToolError::UnsafeName {
            what,
            name: text.to_string(),
        });
    }
    check_ascii_name(what, text)
}

/// A path passed in an argument or the environment: UTF-8, no NUL/CR/LF; ASCII on
/// Windows.
fn path_text<'a>(what: &'static str, p: &'a Path) -> Result<&'a str, ToolError> {
    let s = p.to_str().ok_or_else(|| ToolError::UnsafeName {
        what,
        name: p.to_string_lossy().into_owned(),
    })?;
    if has_control(s) {
        return Err(ToolError::UnsafeName {
            what,
            name: s.to_string(),
        });
    }
    if ARGV_ASCII_ONLY && !s.is_ascii() {
        return Err(ToolError::NonAsciiPath {
            what,
            path: s.to_string(),
        });
    }
    Ok(s)
}

fn opt(name: &str, value: &str) -> OsString {
    OsString::from(format!("{name}={value}"))
}

fn database(conn: &ToolConn) -> Result<&str, ToolError> {
    conn.database
        .as_deref()
        .filter(|d| !d.is_empty())
        .ok_or(ToolError::NoDatabase)
}

fn expect_tool(info: &ToolInfo, conn: &ToolConn, allowed: &[Tool]) -> Result<(), ToolError> {
    if !allowed.contains(&info.tool) || conn.kind != info.tool.kind() {
        return Err(ToolError::WrongTool {
            tool: info.tool.name(),
        });
    }
    Ok(())
}

fn ssl_mode(conn: &ToolConn) -> Result<Option<SslMode>, ToolError> {
    let mode = conn.ssl.as_ref().map(|s| s.mode);
    if conn.tunnelled && mode == Some(SslMode::VerifyFull) {
        return Err(ToolError::VerifyFullThroughTunnel);
    }
    Ok(mode)
}

/// The TLS arguments of a MySQL-family client; `true` when the connection may be
/// plaintext.
fn mysql_tls(info: &ToolInfo, conn: &ToolConn) -> Result<(Vec<OsString>, bool), ToolError> {
    let mode = ssl_mode(conn)?;
    let ssl = conn.ssl.as_ref();
    let mut a: Vec<OsString> = Vec::new();
    let tls_on = !matches!(mode, None | Some(SslMode::Disable));
    let verify = matches!(mode, Some(SslMode::VerifyCa | SslMode::VerifyFull));
    let ca = |a: &mut Vec<OsString>| -> Result<(), ToolError> {
        if let Some(p) = ssl.and_then(|s| s.ca_path.as_deref()) {
            a.push(opt("--ssl-ca", path_text("CA certificate path", p)?));
        }
        Ok(())
    };
    if info.mariadb {
        match mode {
            None | Some(SslMode::Disable) => a.push("--skip-ssl".into()),
            // verify-ca: the chain is checked against `--ssl-ca`, the name is not.
            Some(SslMode::Prefer | SslMode::Require | SslMode::VerifyCa) => {
                a.push("--ssl".into());
                a.push("--skip-ssl-verify-server-cert".into());
            }
            Some(SslMode::VerifyFull) => {
                a.push("--ssl".into());
                a.push("--ssl-verify-server-cert".into());
            }
        }
    } else {
        let name = match mode {
            None | Some(SslMode::Disable) => "DISABLED",
            Some(SslMode::Prefer) => "PREFERRED",
            Some(SslMode::Require) => "REQUIRED",
            Some(SslMode::VerifyCa) => "VERIFY_CA",
            Some(SslMode::VerifyFull) => "VERIFY_IDENTITY",
        };
        a.push(opt("--ssl-mode", name));
    }
    if verify {
        ca(&mut a)?;
    }
    if tls_on {
        if let Some(p) = ssl.and_then(|s| s.cert_path.as_deref()) {
            a.push(opt("--ssl-cert", path_text("Client certificate path", p)?));
        }
        if let Some(p) = ssl.and_then(|s| s.key_path.as_deref()) {
            a.push(opt("--ssl-key", path_text("Client key path", p)?));
        }
    }
    let plaintext = matches!(mode, None | Some(SslMode::Disable | SslMode::Prefer));
    Ok((a, plaintext))
}

/// mysqldump / mariadb-dump (spec §7.2, R26): `--defaults-file=<secret>` first, the
/// connection, TLS, the dump switches, then `--`, the database and the tables.
pub fn mysql_dump_args(
    info: &ToolInfo,
    conn: &ToolConn,
    server: &ServerInfo,
    o: &MysqlDumpOptions,
    secret: &Path,
) -> Result<Invocation, ToolError> {
    expect_tool(info, conn, &[Tool::MysqlDump, Tool::MariadbDump])?;
    if !o.structure && !o.data {
        return Err(ToolError::NothingToDump);
    }
    let db = database(conn)?;
    check_name("Database", db)?;
    for t in &o.tables {
        check_name("Table", t)?;
    }
    check_text("Host", &conn.host)?;
    check_text("User", &conn.user)?;
    let secret_text = path_text("Temporary folder", secret)?;
    let mut a = vec![
        opt("--defaults-file", secret_text),
        "--protocol=TCP".into(),
        opt("--host", &conn.host),
        opt("--port", &conn.port.to_string()),
        opt("--user", &conn.user),
    ];
    let (tls, plaintext) = mysql_tls(info, conn)?;
    a.extend(tls);
    let mysql8 = mysql8_client(info);
    if mysql8 && plaintext {
        a.push("--get-server-public-key".into());
    }
    for s in [
        "--default-character-set=utf8mb4",
        "--hex-blob",
        "--no-tablespaces",
    ] {
        a.push(s.into());
    }
    if o.single_transaction {
        a.push("--single-transaction".into());
    }
    if o.routines {
        a.push("--routines".into());
    }
    if o.events {
        a.push("--events".into());
    }
    a.push(
        if o.triggers {
            "--triggers"
        } else {
            "--skip-triggers"
        }
        .into(),
    );
    if o.structure && !o.data {
        a.push("--no-data".into());
    } else if o.data && !o.structure {
        a.push("--no-create-info".into());
    }
    // A MySQL 8 client asks for column statistics, which MariaDB and MySQL 5.7 lack.
    if mysql8
        && matches!(
            server.variant,
            ServerVariant::MariaDb | ServerVariant::MySql57
        )
    {
        a.push("--column-statistics=0".into());
    }
    if !info.mariadb {
        a.push("--set-gtid-purged=OFF".into());
    }
    a.push("--".into());
    a.push(db.into());
    a.extend(o.tables.iter().map(OsString::from));
    Ok(Invocation {
        program: PathBuf::from(&info.path),
        args: a,
        env_remove: ENV_REMOVE.to_vec(),
        env_set: vec![(
            "MYSQL_TEST_LOGIN_FILE",
            secret.with_file_name(NO_LOGIN_PATH_FILE).into_os_string(),
        )],
        secret: SecretFileSpec::MysqlOptionFile(secret.to_path_buf()),
        pgpass_scope: None,
        stdin_file: false,
        stdout: StdoutUse::File,
    })
}

/// A libpq conninfo value: single-quoted, `\` and `'` backslash-escaped.
fn ci(key: &str, value: &str) -> String {
    format!(
        "{key}='{}'",
        value.replace('\\', "\\\\").replace('\'', "\\'")
    )
}

/// `--dbname=<conninfo>`: host, port, user, database and TLS; never a password.
fn conninfo(conn: &ToolConn) -> Result<OsString, ToolError> {
    let db = database(conn)?;
    check_text("Database", db)?;
    check_text("Host", &conn.host)?;
    check_text("User", &conn.user)?;
    let mode = ssl_mode(conn)?;
    let ssl = conn.ssl.as_ref();
    let mut parts = vec![
        ci("host", &conn.host),
        ci("port", &conn.port.to_string()),
        ci("user", &conn.user),
        ci("dbname", db),
    ];
    let name = match mode {
        None | Some(SslMode::Disable) => "disable",
        Some(SslMode::Prefer) => "prefer",
        Some(SslMode::Require) => "require",
        Some(SslMode::VerifyCa) => "verify-ca",
        Some(SslMode::VerifyFull) => "verify-full",
    };
    parts.push(ci("sslmode", name));
    if matches!(mode, Some(SslMode::VerifyCa | SslMode::VerifyFull)) {
        if let Some(p) = ssl.and_then(|s| s.ca_path.as_deref()) {
            parts.push(ci("sslrootcert", path_text("CA certificate path", p)?));
        }
    }
    if !matches!(mode, None | Some(SslMode::Disable)) {
        if let Some(p) = ssl.and_then(|s| s.cert_path.as_deref()) {
            parts.push(ci("sslcert", path_text("Client certificate path", p)?));
        }
        if let Some(p) = ssl.and_then(|s| s.key_path.as_deref()) {
            parts.push(ci("sslkey", path_text("Client key path", p)?));
        }
    }
    Ok(OsString::from(format!("--dbname={}", parts.join(" "))))
}

/// A pg_dump pattern matching exactly `name`: double-quoted, `"` doubled.
fn pg_pattern(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `PGPASSFILE=<pgpass>` (a path, checked like an argument: libpq reads the environment
/// in the ANSI code page too).
fn pg_env(pgpass: &Path) -> Result<Vec<(&'static str, OsString)>, ToolError> {
    let p = path_text("Temporary folder", pgpass)?;
    Ok(vec![("PGPASSFILE", OsString::from(p))])
}

fn pg_scope(conn: &ToolConn) -> Option<PgPassScope> {
    Some(PgPassScope {
        host: conn.host.clone(),
        port: conn.port,
        user: conn.user.clone(),
    })
}

/// pg_dump (spec §7.2): output on stdout in every format (R29).
pub fn pg_dump_args(
    info: &ToolInfo,
    conn: &ToolConn,
    o: &PgDumpOptions,
    pgpass: &Path,
) -> Result<Invocation, ToolError> {
    expect_tool(info, conn, &[Tool::PgDump])?;
    if !o.structure && !o.data {
        return Err(ToolError::NothingToDump);
    }
    for s in &o.schemas {
        check_name("Schema", s)?;
    }
    for (s, t) in &o.tables {
        check_name("Schema", s)?;
        check_name("Table", t)?;
    }
    let dbname = conninfo(conn)?;
    let format = match o.format {
        PgDumpFormat::Plain => "p",
        PgDumpFormat::Custom => "c",
        PgDumpFormat::Tar => "t",
    };
    let mut a: Vec<OsString> = vec![
        "--no-password".into(),
        opt("--format", format),
        "--encoding=UTF8".into(),
    ];
    if o.structure && !o.data {
        a.push("--schema-only".into());
    } else if o.data && !o.structure {
        a.push("--data-only".into());
    }
    if o.no_owner {
        a.push("--no-owner".into());
    }
    if o.no_privileges {
        a.push("--no-privileges".into());
    }
    for s in &o.schemas {
        a.push(opt("--schema", &pg_pattern(s)));
    }
    for (s, t) in &o.tables {
        a.push(opt(
            "--table",
            &format!("{}.{}", pg_pattern(s), pg_pattern(t)),
        ));
    }
    a.push(dbname);
    Ok(Invocation {
        program: PathBuf::from(&info.path),
        args: a,
        env_remove: ENV_REMOVE.to_vec(),
        env_set: pg_env(pgpass)?,
        secret: SecretFileSpec::PgPass(pgpass.to_path_buf()),
        pgpass_scope: pg_scope(conn),
        stdin_file: false,
        stdout: StdoutUse::File,
    })
}

/// psql reading the restore file on stdin (spec §7.2, R26, R34).
pub fn psql_args(
    info: &ToolInfo,
    conn: &ToolConn,
    o: &PsqlOptions,
    pgpass: &Path,
) -> Result<Invocation, ToolError> {
    expect_tool(info, conn, &[Tool::Psql])?;
    let dbname = conninfo(conn)?;
    let mut a: Vec<OsString> = vec![
        "-X".into(),
        "--no-password".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
    ];
    if o.single_transaction {
        a.push("--single-transaction".into());
    }
    a.push(dbname);
    a.push("-f".into());
    a.push("-".into());
    Ok(Invocation {
        program: PathBuf::from(&info.path),
        args: a,
        env_remove: ENV_REMOVE.to_vec(),
        env_set: pg_env(pgpass)?,
        secret: SecretFileSpec::PgPass(pgpass.to_path_buf()),
        pgpass_scope: pg_scope(conn),
        stdin_file: true,
        stdout: StdoutUse::Discard,
    })
}

fn file_arg(file: &Path) -> Result<OsString, ToolError> {
    let s = path_text("File", file)?;
    check_name("File", s)?;
    Ok(OsString::from(s))
}

/// pg_restore of `file` (spec §7.2).
pub fn pg_restore_args(
    info: &ToolInfo,
    conn: &ToolConn,
    o: &PgRestoreOptions,
    pgpass: &Path,
    file: &Path,
) -> Result<Invocation, ToolError> {
    expect_tool(info, conn, &[Tool::PgRestore])?;
    let file = file_arg(file)?;
    let dbname = conninfo(conn)?;
    let mut a: Vec<OsString> = vec!["--no-password".into(), "--exit-on-error".into()];
    if o.single_transaction {
        a.push("--single-transaction".into());
    }
    if o.clean {
        a.push("--clean".into());
        a.push("--if-exists".into());
    }
    if o.no_owner {
        a.push("--no-owner".into());
    }
    if o.no_privileges {
        a.push("--no-privileges".into());
    }
    a.push(dbname);
    a.push(file);
    Ok(Invocation {
        program: PathBuf::from(&info.path),
        args: a,
        env_remove: ENV_REMOVE.to_vec(),
        env_set: pg_env(pgpass)?,
        secret: SecretFileSpec::PgPass(pgpass.to_path_buf()),
        pgpass_scope: pg_scope(conn),
        stdin_file: false,
        stdout: StdoutUse::Discard,
    })
}

/// `pg_restore --list <file>` for the restore preview: no connection, no secret.
pub fn pg_restore_list_args(info: &ToolInfo, file: &Path) -> Result<Invocation, ToolError> {
    if info.tool != Tool::PgRestore {
        return Err(ToolError::WrongTool {
            tool: info.tool.name(),
        });
    }
    Ok(Invocation {
        program: PathBuf::from(&info.path),
        args: vec!["--list".into(), file_arg(file)?],
        env_remove: ENV_REMOVE.to_vec(),
        env_set: Vec::new(),
        secret: SecretFileSpec::None,
        pgpass_scope: None,
        stdin_file: false,
        stdout: StdoutUse::List,
    })
}

/// The MySQL option file: `[client]` and the double-quoted password with `\`, `"`, tab,
/// CR and LF escaped (MySQL and MariaDB read these escapes; `#` and `'` inside the quotes
/// are literal). NUL cannot be carried.
pub fn mysql_option_file(password: &str) -> Result<String, ToolError> {
    if password.contains('\0') {
        return Err(ToolError::UnsafePassword);
    }
    let mut esc = String::with_capacity(password.len() + 2);
    for c in password.chars() {
        match c {
            '\\' => esc.push_str("\\\\"),
            '"' => esc.push_str("\\\""),
            '\t' => esc.push_str("\\t"),
            '\n' => esc.push_str("\\n"),
            '\r' => esc.push_str("\\r"),
            c => esc.push(c),
        }
    }
    Ok(format!("[client]\npassword=\"{esc}\"\n"))
}

fn pgpass_field(s: &str) -> String {
    s.replace('\\', "\\\\").replace(':', "\\:")
}

/// The pgpass line `host:port:*:user:password` valid for exactly the scope's server
/// and user (any database), every field with `\` and `:` escaped (R27). NUL, CR and LF
/// cannot be carried.
pub fn pgpass_line(scope: &PgPassScope, password: &str) -> Result<String, ToolError> {
    if has_control(password) {
        return Err(ToolError::UnsafePassword);
    }
    for (what, s) in [("Host", &scope.host), ("User", &scope.user)] {
        if has_control(s) {
            return Err(ToolError::UnsafeName {
                what,
                name: s.clone(),
            });
        }
    }
    Ok(format!(
        "{}:{}:*:{}:{}\n",
        pgpass_field(&scope.host),
        scope.port,
        pgpass_field(&scope.user),
        pgpass_field(password)
    ))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '"') {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('"', "\\\""))
}

/// The command line the preview and the job log show: the environment the app sets
/// (paths), the program and the arguments. Holds no secret: the invocation has none.
pub fn shown_command(inv: &Invocation) -> String {
    let mut parts: Vec<String> = inv
        .env_set
        .iter()
        .map(|(k, v)| format!("{k}={}", shell_quote(&v.to_string_lossy())))
        .collect();
    parts.push(shell_quote(&inv.program.to_string_lossy()));
    parts.extend(inv.args.iter().map(|a| shell_quote(&a.to_string_lossy())));
    parts.join(" ")
}

/// `line` with every occurrence of `password` replaced by `******`.
pub fn mask(line: &str, password: &str) -> String {
    if password.is_empty() {
        return line.to_string();
    }
    line.replace(password, "******")
}

enum Out {
    Plain(Box<dyn Write + Send>),
    Gzip(GzEncoder<Box<dyn Write + Send>>),
}

/// The output file of a tool dump: the tool's stdout as is, or gzip (`.sql.gz`, R29).
pub struct ToolOutput {
    out: Out,
    bytes: u64,
}

impl ToolOutput {
    pub fn new(inner: Box<dyn Write + Send>, compress: bool) -> ToolOutput {
        ToolOutput {
            out: if compress {
                Out::Gzip(GzEncoder::new(inner, Compression::default()))
            } else {
                Out::Plain(inner)
            },
            bytes: 0,
        }
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        match &mut self.out {
            Out::Plain(w) => w.write_all(data)?,
            Out::Gzip(w) => w.write_all(data)?,
        }
        self.bytes += data.len() as u64;
        Ok(())
    }

    /// Completes the file (gzip trailer, flush); returns the bytes the tool wrote.
    pub fn finish(self) -> io::Result<u64> {
        match self.out {
            Out::Plain(mut w) => w.flush()?,
            Out::Gzip(w) => w.finish()?.flush()?,
        }
        Ok(self.bytes)
    }
}

/// The restore file as fed to a tool's stdin: decompressed when gzip (R34).
pub fn open_input(path: &Path, gzip: bool) -> io::Result<Box<dyn Read + Send>> {
    let file = BufReader::with_capacity(256 * 1024, std::fs::File::open(path)?);
    Ok(if gzip {
        Box::new(MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    })
}

/// How a restore through a tool ended (framework §7.6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    /// Exit code 0.
    Done,
    /// Non-zero exit with `--single-transaction` / `ON_ERROR_STOP`: rolled back.
    FailedRolledBack,
    /// Non-zero otherwise, killed or lost: the target may be partially restored
    /// (`outcome_unknown`).
    Partial,
}

/// Only psql's exit code 3 (an SQL error stopped the script, `ON_ERROR_STOP`) under
/// `--single-transaction` is known to have rolled back. psql 1 / 2 (fatal, connection
/// lost), any pg_restore failure (`--exit-on-error` exits 1 for SQL and other errors
/// alike), a kill or a missing exit code may have left part of the restore.
pub fn restore_outcome(
    tool: Tool,
    code: Option<i32>,
    killed: bool,
    transactional: bool,
) -> ToolOutcome {
    match (code, killed) {
        (Some(0), false) => ToolOutcome::Done,
        (Some(3), false) if transactional && tool == Tool::Psql => ToolOutcome::FailedRolledBack,
        _ => ToolOutcome::Partial,
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
