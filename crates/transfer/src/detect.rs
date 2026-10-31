//! Restore file detection and scan (spec §7.3, R30, R31, R33), the pure half of
//! `src-tauri/src/backup/restore.rs`.
//!
//! The path is chosen **by content, never by extension** (R30):
//!
//! 1. gzip (`1f 8b`) is decompressed on the fly; everything below looks at the
//!    decompressed bytes.
//! 2. `PGDMP` magic (pg_dump custom format) or a tar header whose first member is
//!    `toc.dat` (pg_dump tar format) → [`RestorePath::PgRestore`].
//! 3. Anything else is text, split by N6 ([`StreamSplitter`], client detection on): a
//!    `COPY … FROM stdin` data block or a `\` command line (PostgreSQL) →
//!    [`RestorePath::Psql`]; otherwise → [`RestorePath::AppRunner`]. A MySQL-dialect file
//!    with a `\` client command (a `\…` line, or a backslash outside quoted text such as
//!    `SELECT 1\G`) is refused. There is no fourth path.
//!
//! Refusals ([`Refusal`]) are found at preview so the typed confirmation covers what
//! runs (R31), and the restore job applies [`statement_refusal`] again to every statement
//! before it is sent: see [`statement_refusal`] (database statements, MySQL prepared /
//! stored code and `DELIMITER`-merged statements, backslash client commands, PostgreSQL
//! string-lexing settings), [`psql_command_refusal`] (psql lines), [`ModeTracker`]
//! (`sql_mode` changes it cannot follow), a file of the other engine (by its header, or
//! MySQL `/*!` comments on PostgreSQL), and a psql file not written by pg_dump.
//!
//! Classification uses `dnt_core::sql::classify` only (this crate does not depend on
//! `dnt_query`; the job judges every statement with the query guard).
//!
//! Known limits: statements naming another database by qualification (`DROP TABLE
//! other.t`) are not detected; a function or trigger the file creates can change settings
//! when it runs later.

use std::io::{self, BufReader, Chain, Cursor, Read};
use std::path::Path;

use dnt_core::server::SqlKind;
use dnt_core::sql::{
    classify, leading_blank_len, leading_keywords, readings, resplit_reading, session_effect,
    tokenize, Dialect, MySqlMode, SplitError, SplitEvent, SplitOptions, StatementKind,
    StreamSplitter, TokKind, Token,
};
use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;

use crate::dump::DUMP_MAGIC;
use crate::facts::FileFacts;

/// Bytes fed to the splitter at a time (spec §7.3).
pub const CHUNK: usize = 256 * 1024;
/// Refusals listed by name (the rest are counted).
pub const MAX_REFUSALS: usize = 20;
/// Text the header checks look at.
const HEAD_TEXT: usize = 4096;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const PGDMP_MAGIC: &[u8] = b"PGDMP";
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Which of the three restore paths a file takes (R30).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RestorePath {
    /// The app's own runner: N6 splitter + query guard, one statement at a time.
    AppRunner,
    /// `psql -f -` (COPY blocks, psql commands).
    Psql,
    /// `pg_restore` (pg_dump custom or tar archive).
    PgRestore,
}

/// What a text file holds (statement counts by kind).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScanStats {
    #[specta(type = f64)]
    pub statements: u64,
    #[specta(type = f64)]
    pub reads: u64,
    #[specta(type = f64)]
    pub writes: u64,
    #[specta(type = f64)]
    pub ddl: u64,
    #[specta(type = f64)]
    pub transaction: u64,
    #[specta(type = f64)]
    pub other: u64,
    /// `DROP …` statements.
    #[specta(type = f64)]
    pub drops: u64,
    /// `COPY … FROM stdin` data blocks.
    #[specta(type = f64)]
    pub copy_blocks: u64,
    /// `\…` client command lines.
    #[specta(type = f64)]
    pub client_commands: u64,
    /// The file starts with the app's dump header (`-- DntDatabase dump v1`).
    pub app_dump: bool,
}

/// What a file is wrapped in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// pg_dump custom format (`PGDMP`).
    PgCustom,
    /// pg_dump tar format (first member `toc.dat`).
    PgTar,
    /// Anything else: read as SQL text.
    Text,
}

/// Whether `head` (the raw file's first bytes) is gzip.
pub fn is_gzip(head: &[u8]) -> bool {
    head.starts_with(&GZIP_MAGIC)
}

/// The container of the (decompressed) content whose first bytes are `head` (at least
/// 512 bytes when the content is that long).
pub fn container(head: &[u8]) -> Container {
    if head.starts_with(PGDMP_MAGIC) {
        Container::PgCustom
    } else if is_pg_tar(head) {
        Container::PgTar
    } else {
        Container::Text
    }
}

/// A POSIX tar header (`ustar` at offset 257) whose member name is `toc.dat`.
fn is_pg_tar(head: &[u8]) -> bool {
    if head.len() < 512 || &head[257..262] != b"ustar" {
        return false;
    }
    let name = &head[..100];
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    &name[..end] == b"toc.dat"
}

/// The engine a dump's header names: the app's dump (`-- Source: MySQL|MariaDB|PostgreSQL`),
/// mysqldump / mariadb-dump (`-- MySQL dump`, `-- MariaDB dump`) and pg_dump
/// (`-- PostgreSQL database dump`). `None`: no known header.
pub fn header_engine(head: &str) -> Option<SqlKind> {
    for line in head.lines().take(40) {
        let l = line.trim();
        if let Some(src) = l.strip_prefix("-- Source: ") {
            if src.starts_with("PostgreSQL") {
                return Some(SqlKind::Postgres);
            }
            if src.starts_with("MySQL") || src.starts_with("MariaDB") {
                return Some(SqlKind::MySql);
            }
        }
        if l.starts_with("-- MySQL dump") || l.starts_with("-- MariaDB dump") {
            return Some(SqlKind::MySql);
        }
        if l.starts_with("-- PostgreSQL database dump") {
            return Some(SqlKind::Postgres);
        }
    }
    None
}

fn engine_name(kind: SqlKind) -> &'static str {
    match kind {
        SqlKind::MySql => "MySQL",
        SqlKind::Postgres => "PostgreSQL",
    }
}

// ---------------------------------------------------------------------------------------
// Statement checks (shared with the restore job)
// ---------------------------------------------------------------------------------------

/// Why a statement or client command cannot be restored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// The MySQL-family server a file is restored into: which executable comments it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlServer {
    pub version: (u32, u32, u32),
    pub mariadb: bool,
}

/// The identifier a token holds: backquotes / double quotes removed (doubled quotes
/// undone), words as they are.
fn ident_text(s: &str, t: &Token) -> String {
    let raw = &s[t.start..t.end];
    for q in ['`', '"', '\''] {
        if raw.len() >= 2 && raw.starts_with(q) && raw.ends_with(q) {
            let inner = &raw[1..raw.len() - 1];
            return inner.replace(&format!("{q}{q}"), &q.to_string());
        }
    }
    raw.to_string()
}

fn is_word(t: Option<&Token>, w: &str) -> bool {
    matches!(t.map(|t| t.kind), Some(TokKind::Word(x)) if x.eq_ignore_ascii_case(w))
}

/// The object name after the leading keywords `skip` (`IF [NOT] EXISTS` skipped).
/// PostgreSQL folds an unquoted name to lower case.
fn name_after(s: &str, dialect: Dialect, skip: usize) -> Option<String> {
    let toks = tokenize(s, dialect);
    let mut i = skip;
    if is_word(toks.get(i), "IF") {
        i += 1;
        if is_word(toks.get(i), "NOT") {
            i += 1;
        }
        if is_word(toks.get(i), "EXISTS") {
            i += 1;
        }
    }
    let t = toks.get(i)?;
    match t.kind {
        TokKind::Word(w) if dialect == Dialect::Postgres => Some(w.to_lowercase()),
        TokKind::Word(_) | TokKind::Quoted => Some(ident_text(s, t)),
        _ => None,
    }
}

/// `(MariaDB only, version)` of an executable comment token text: `/*!NNNNN` (5 digits
/// `Mmmrr`), `/*M!NNNNNN` (6 digits `MMmmrr`); no or other digits: `None` (it runs).
fn exec_gate(c: &str) -> (bool, Option<(u32, u32, u32)>) {
    let body = c.strip_prefix("/*").unwrap_or(c);
    let (maria, rest) = match body.strip_prefix("M!") {
        Some(r) => (true, r),
        None => (false, body.strip_prefix('!').unwrap_or(body)),
    };
    let digits: Vec<u32> = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .filter_map(|c| c.to_digit(10))
        .collect();
    let num = |d: &[u32]| d.iter().fold(0, |a, x| a * 10 + x);
    let version = match digits.len() {
        5 => Some((digits[0], num(&digits[1..3]), num(&digits[3..5]))),
        6 => Some((num(&digits[..2]), num(&digits[2..4]), num(&digits[4..6]))),
        _ => None,
    };
    (maria, version)
}

fn exec_body(c: &str) -> &str {
    let body = c.strip_prefix("/*").unwrap_or(c);
    let body = body.strip_prefix('M').unwrap_or(body);
    let body = body.strip_prefix('!').unwrap_or(body);
    let body = body.trim_start_matches(|ch: char| ch.is_ascii_digit());
    body.strip_suffix("*/").unwrap_or(body)
}

/// How `server` reads `text`: the bodies of the executable comments it runs spliced in,
/// the others dropped (`/*M!` only runs on MariaDB; a version above the server's never).
pub fn server_reading(text: &str, dialect: Dialect, server: MySqlServer) -> String {
    let toks = tokenize(text, dialect);
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for t in toks.iter().filter(|t| t.kind == TokKind::ExecComment) {
        let c = &text[t.start..t.end];
        let (maria, version) = exec_gate(c);
        let runs = (!maria || server.mariadb) && version.is_none_or(|v| v <= server.version);
        out.push_str(&text[from..t.start]);
        out.push(' ');
        if runs {
            out.push_str(exec_body(c));
            out.push(' ');
        }
        from = t.end;
    }
    out.push_str(&text[from..]);
    out
}

/// The readings checked for refusals: every way a server may read the text, plus the
/// target server's own reading.
fn check_readings(text: &str, dialect: Dialect, target: &ScanTarget) -> Vec<String> {
    let mut rs = readings(text, dialect);
    if let (Dialect::MySql(_), Some(s)) = (dialect, target.server) {
        let own = server_reading(text, dialect, s);
        if !rs.contains(&own) {
            rs.push(own);
        }
    }
    rs
}

/// The readings that decide what the session does: the target server's own when it is
/// known, else every reading.
fn own_readings(text: &str, dialect: Dialect, server: Option<MySqlServer>) -> Vec<String> {
    match (dialect, server) {
        (Dialect::MySql(_), Some(s)) => vec![server_reading(text, dialect, s)],
        _ => readings(text, dialect),
    }
}

/// The refusals of one statement (or one piece of it) by its keywords.
fn keyword_refusal(r: &str, dialect: Dialect, target: &ScanTarget) -> Option<Refusal> {
    let mysql = match target.kind {
        SqlKind::MySql => true,
        SqlKind::Postgres => false,
    };
    let target_db = target.database.as_str();
    let words = leading_keywords(r, dialect, 4);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let db_word = |x: &str| x == "DATABASE" || (mysql && x == "SCHEMA");
    match w.as_slice() {
        ["CREATE", x, ..] if db_word(x) => {
            return Some(Refusal(format!(
                "The file creates a database (\"{}\"); restore runs into the chosen database \
                 only.",
                name_after(r, dialect, 2).unwrap_or_default()
            )))
        }
        ["CREATE", "OR", "REPLACE", x, ..] if db_word(x) => {
            return Some(Refusal(
                "The file creates a database; restore runs into the chosen database only.".into(),
            ))
        }
        ["DROP", x, ..] if db_word(x) => {
            return Some(Refusal(format!(
                "The file drops a database (\"{}\").",
                name_after(r, dialect, 2).unwrap_or_default()
            )))
        }
        ["ALTER", x, ..] if db_word(x) => {
            let name = name_after(r, dialect, 2);
            // MySQL `ALTER DATABASE CHARACTER SET …` (no name) is the current one.
            let current = mysql
                && name.as_deref().is_some_and(|n| {
                    ["CHARACTER", "CHARSET", "COLLATE", "DEFAULT", "READ"]
                        .iter()
                        .any(|k| n.eq_ignore_ascii_case(k))
                });
            if !current && name.as_deref() != Some(target_db) {
                return Some(Refusal(format!(
                    "The file changes another database (\"{}\").",
                    name.unwrap_or_default()
                )));
            }
        }
        ["USE", ..] if mysql => {
            let name = name_after(r, dialect, 1).unwrap_or_default();
            if name != target_db {
                return Some(Refusal(format!(
                    "The file switches to another database (USE \"{name}\"); restore runs into \
                     \"{target_db}\" only."
                )));
            }
        }
        ["PREPARE" | "EXECUTE" | "CALL", ..] | ["DEALLOCATE" | "DROP", "PREPARE", ..] if mysql => {
            return Some(Refusal(format!(
                "The file runs {} (prepared or stored code), whose statements the app cannot \
                 check before they run.",
                w.iter().take(2).copied().collect::<Vec<_>>().join(" ")
            )))
        }
        _ => {}
    }
    match target.kind {
        SqlKind::MySql => None,
        SqlKind::Postgres => pg_setting_refusal(r),
    }
}

/// PostgreSQL settings that change how strings are lexed, and `set_config` other than
/// `search_path`: the file would be split and judged in a way the server does not read
/// it (R30).
fn pg_setting_refusal(text: &str) -> Option<Refusal> {
    let toks = tokenize(text, Dialect::Postgres);
    let lexing = |t: &Token| {
        let name = match t.kind {
            TokKind::Word(w) => w.to_ascii_lowercase(),
            TokKind::Quoted if text[t.start..].starts_with('"') => {
                ident_text(text, t).to_ascii_lowercase()
            }
            _ => return false,
        };
        name == "standard_conforming_strings" || name == "backslash_quote"
    };
    if toks.iter().any(lexing) {
        let mut i = 1;
        if is_word(toks.get(1), "SESSION") || is_word(toks.get(1), "LOCAL") {
            i = 2;
        }
        let on = toks.get(i + 2).is_some_and(|v| match v.kind {
            TokKind::Word(w) => w.eq_ignore_ascii_case("on") || w.eq_ignore_ascii_case("true"),
            TokKind::Quoted => ident_text(text, v).eq_ignore_ascii_case("on"),
            _ => false,
        });
        let allowed = is_word(toks.first(), "SET")
            && is_word(toks.get(i), "standard_conforming_strings")
            && (toks.get(i + 1).map(|t| t.kind) == Some(TokKind::Punct(b'='))
                || is_word(toks.get(i + 1), "TO"))
            && on
            && toks.len() == i + 3;
        if !allowed {
            return Some(Refusal(
                "The file changes standard_conforming_strings or backslash_quote; only `SET \
                 standard_conforming_strings = on` is restored."
                    .into(),
            ));
        }
    }
    for (i, t) in toks.iter().enumerate() {
        if !is_word(Some(t), "set_config") || toks.get(i + 1).map(|t| t.kind) != Some(TokKind::Open)
        {
            continue;
        }
        let ok = toks.get(i + 2).is_some_and(|a| {
            a.kind == TokKind::Quoted
                && text[a.start..].starts_with('\'')
                && ident_text(text, a) == "search_path"
        }) && toks.get(i + 3).map(|t| t.kind) == Some(TokKind::Punct(b','));
        if !ok {
            return Some(Refusal(
                "The file calls set_config for a setting other than search_path.".into(),
            ));
        }
    }
    let w = leading_keywords(text, Dialect::Postgres, 2);
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    if matches!(w.as_slice(), ["RESET", "ALL"] | ["DISCARD", "ALL"]) {
        return Some(Refusal(format!(
            "The file runs {}, which may turn standard_conforming_strings off.",
            w.join(" ")
        )));
    }
    None
}

/// A backslash outside quoted text and comments: a client command (`\G`, `\!`, `\gexec`,
/// `\set` …) that the server never sees as SQL and a client would run.
fn backslash_refusal(r: &str, dialect: Dialect) -> Option<Refusal> {
    let t = tokenize(r, dialect)
        .into_iter()
        .find(|t| t.kind == TokKind::Punct(b'\\'))?;
    let cmd: String = r[t.start..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .take(12)
        .collect();
    Some(Refusal(format!(
        "The file uses the client command {cmd}, which the app does not run."
    )))
}

/// The refusal of one statement (R31, R30), checked at preview and again by the job
/// before it is sent:
///
/// - `CREATE` / `DROP DATABASE` (MySQL also `SCHEMA`, MariaDB `CREATE OR REPLACE`),
///   `ALTER DATABASE` / `USE` naming another database;
/// - MySQL: `PREPARE`, `EXECUTE`, `DEALLOCATE` / `DROP PREPARE`, `CALL`; a statement that
///   holds several statements for the server (a `DELIMITER` left on) unless it is a
///   compound-body definition, every piece checked too;
/// - a backslash outside quoted text and comments (a client command), on MySQL in the
///   target server's own reading (an executable comment gated above its version, such
///   as mariadb-dump's `/*M!999999\- enable the sandbox mode */`, is not run);
/// - PostgreSQL: [`pg_setting_refusal`].
///
/// Every reading of a MySQL executable comment is checked.
pub fn statement_refusal(text: &str, dialect: Dialect, target: &ScanTarget) -> Option<Refusal> {
    for r in check_readings(text, dialect, target) {
        if let Some(x) = keyword_refusal(&r, dialect, target) {
            return Some(x);
        }
        match target.kind {
            SqlKind::MySql => {
                let toks = tokenize(&r, dialect);
                if let Some(pieces) = resplit_reading(&r, &toks, dialect) {
                    for p in &pieces {
                        if let Some(x) = keyword_refusal(p, dialect, target) {
                            return Some(x);
                        }
                    }
                    return Some(Refusal(format!(
                        "The file holds {} statements in one (a DELIMITER left on); the server \
                         would run them without the app checking each.",
                        pieces.len()
                    )));
                }
            }
            SqlKind::Postgres => {}
        }
    }
    let own_server = match target.kind {
        SqlKind::MySql => target.server,
        SqlKind::Postgres => None,
    };
    for r in own_readings(text, dialect, own_server) {
        if let Some(x) = backslash_refusal(&r, dialect) {
            return Some(x);
        }
    }
    None
}

/// The psql path's check of one `\` command line: `\connect` / `\c` to the target (or
/// `-`, or no database); pg_dump's `\restrict` / `\unrestrict` with exactly one
/// alphanumeric key; anything else is refused.
pub fn psql_command_refusal(line: &str, target_db: &str) -> Option<Refusal> {
    let body = line.trim().trim_start_matches('\\');
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();
    match cmd {
        "restrict" | "unrestrict" => match args.as_slice() {
            [key] if key.chars().all(|c| c.is_ascii_alphanumeric()) => None,
            _ => Some(Refusal(format!(
                "The file runs the psql command \\{cmd} with unexpected arguments ({}).",
                line.trim()
            ))),
        },
        "connect" | "c" => {
            let unquote = |a: &str| -> String {
                for q in ['"', '\''] {
                    if a.len() >= 2 && a.starts_with(q) && a.ends_with(q) {
                        return a[1..a.len() - 1].replace(&format!("{q}{q}"), &q.to_string());
                    }
                }
                a.to_string()
            };
            match args.as_slice() {
                [] | ["-"] => None,
                [db] if unquote(db) == target_db => None,
                _ => Some(Refusal(format!(
                    "The file connects to another database or server ({}); restore runs into \
                     \"{target_db}\" only.",
                    line.trim()
                ))),
            }
        }
        _ => Some(Refusal(format!(
            "The file runs the psql command \\{cmd}, which the app does not run."
        ))),
    }
}

/// PostgreSQL statements that cannot run inside a transaction block (as
/// `dnt_query::run::manual_needs_tx` lists them).
pub fn pg_outside_tx_block(text: &str) -> bool {
    let words = leading_keywords(text, Dialect::Postgres, 5);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let concurrently = w.contains(&"CONCURRENTLY");
    match w.as_slice() {
        ["CREATE" | "DROP", "DATABASE" | "TABLESPACE", ..] => true,
        ["CREATE" | "ALTER" | "DROP", "SUBSCRIPTION", ..] => true,
        ["ALTER", "SYSTEM", ..] | ["VACUUM", ..] => true,
        ["REINDEX", ..] => concurrently,
        ["CREATE" | "DROP", rest @ ..] => concurrently && rest.contains(&"INDEX"),
        _ => false,
    }
}

/// The lexing flags of a `sql_mode` value: `ANSI_QUOTES`, or a combination mode that
/// includes it (`ANSI`, `ORACLE`, `MSSQL`, `POSTGRESQL`, `DB2`, `MAXDB`), sets
/// `ansi_quotes`; `NO_BACKSLASH_ESCAPES` sets `no_backslash_escapes`.
pub fn parse_sql_mode(s: &str) -> MySqlMode {
    let mut mode = MySqlMode::default();
    for flag in s.split(',').map(str::trim) {
        let f = flag.to_ascii_uppercase();
        match f.as_str() {
            "ANSI_QUOTES" | "ANSI" | "ORACLE" | "MSSQL" | "POSTGRESQL" | "DB2" | "MAXDB" => {
                mode.ansi_quotes = true
            }
            "NO_BACKSLASH_ESCAPES" => mode.no_backslash_escapes = true,
            _ => {}
        }
    }
    mode
}

/// Follows the session `sql_mode` through a file without running it (R33), so the scan
/// splits the file as the job will, and the job can stop when the session disagrees.
///
/// Understood: every assignment of a `SET` (left to right, the last wins), session
/// `sql_mode` set to a string literal or to a user variable saved earlier from
/// `@@sql_mode` (`SET @OLD_SQL_MODE=@@SQL_MODE` … `SET SQL_MODE=@OLD_SQL_MODE`, as
/// mysqldump writes it); `GLOBAL` / `PERSIST` assignments leave the session alone.
/// Anything else that changes the session `sql_mode` (`DEFAULT`, an expression, readings
/// that disagree) is uncertain: a [`Refusal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeTracker {
    current: Dialect,
    saved: Vec<(String, MySqlMode)>,
    server: Option<MySqlServer>,
}

fn is_mode_read(toks: &[Token], r: &str) -> bool {
    let text: String = toks.iter().map(|t| &r[t.start..t.end]).collect();
    matches!(
        text.to_ascii_lowercase().as_str(),
        "@@sql_mode" | "@@session.sql_mode" | "@@local.sql_mode"
    )
}

impl ModeTracker {
    pub fn new(dialect: Dialect, server: Option<MySqlServer>) -> ModeTracker {
        ModeTracker {
            current: dialect,
            saved: Vec::new(),
            server,
        }
    }

    /// The dialect the session is in now.
    pub fn current(&self) -> Dialect {
        self.current
    }

    fn mode(&self) -> MySqlMode {
        match self.current {
            Dialect::MySql(m) => m,
            Dialect::Postgres => MySqlMode::default(),
        }
    }

    /// The session after `text` ran (it succeeded).
    pub fn after(&mut self, text: &str) -> Result<(), Refusal> {
        let Dialect::MySql(_) = self.current else {
            return Ok(());
        };
        let uncertain = || {
            let head: String = text.chars().take(80).collect();
            Refusal(format!(
                "The file changes sql_mode in a way the analysis cannot follow ({head}); the \
                 rest of the file could be split differently from what was analyzed."
            ))
        };
        let mut outcomes: Vec<(ModeTracker, bool)> = Vec::new();
        for r in own_readings(text, self.current, self.server) {
            let mut t = self.clone();
            let found = t.apply(&r).map_err(|()| uncertain())?;
            outcomes.push((t, found));
        }
        if outcomes.windows(2).any(|w| w[0].0 != w[1].0) {
            return Err(uncertain());
        }
        let Some((next, found)) = outcomes.pop() else {
            return Ok(());
        };
        // Safety net: the session effect says sql_mode changed but no assignment was read.
        if !found && session_effect(text, self.current).is_some_and(|e| e.sql_mode) {
            return Err(uncertain());
        }
        *self = next;
        Ok(())
    }

    /// Applies one reading; `Ok(true)` when it assigned the session `sql_mode`.
    fn apply(&mut self, r: &str) -> Result<bool, ()> {
        let toks = tokenize(r, self.current);
        if !is_word(toks.first(), "SET") || is_word(toks.get(1), "STATEMENT") {
            return Ok(false);
        }
        let mut found = false;
        let mut depth = 0i32;
        let mut parts: Vec<&[Token]> = Vec::new();
        let mut from = 1;
        for (i, t) in toks.iter().enumerate().skip(1) {
            match t.kind {
                TokKind::Open => depth += 1,
                TokKind::Close => depth -= 1,
                TokKind::Punct(b',') if depth == 0 => {
                    parts.push(&toks[from..i]);
                    from = i + 1;
                }
                _ => {}
            }
        }
        parts.push(&toks[from..]);
        for part in parts {
            let mut part = part;
            let mut global = false;
            if let Some(TokKind::Word(w)) = part.first().map(|t| t.kind) {
                let w = w.to_ascii_uppercase();
                if matches!(
                    w.as_str(),
                    "SESSION" | "LOCAL" | "GLOBAL" | "PERSIST" | "PERSIST_ONLY"
                ) {
                    global = !matches!(w.as_str(), "SESSION" | "LOCAL");
                    part = &part[1..];
                }
            }
            let Some(eq) = part.iter().position(|t| t.kind == TokKind::Punct(b'=')) else {
                continue;
            };
            let mut target_toks = &part[..eq];
            if target_toks.last().map(|t| t.kind) == Some(TokKind::Punct(b':')) {
                target_toks = &target_toks[..target_toks.len() - 1];
            }
            let target: String = target_toks
                .iter()
                .map(|t| &r[t.start..t.end])
                .collect::<String>()
                .to_ascii_lowercase();
            let value = &part[eq + 1..];
            let mode_target = target == "sql_mode"
                || target.strip_prefix("@@").is_some_and(|v| {
                    v == "sql_mode"
                        || v.strip_suffix(".sql_mode").is_some_and(|s| {
                            matches!(
                                s,
                                "session" | "local" | "global" | "persist" | "persist_only"
                            )
                        })
                });
            if mode_target {
                let global =
                    global || target.starts_with("@@global.") || target.starts_with("@@persist");
                if global {
                    continue;
                }
                found = true;
                let mode = match value {
                    [v] if v.kind == TokKind::Quoted
                        && (r[v.start..].starts_with('\'')
                            || (r[v.start..].starts_with('"') && !self.mode().ansi_quotes)) =>
                    {
                        parse_sql_mode(&ident_text(r, v))
                    }
                    [v] if matches!(v.kind, TokKind::Word(w) if w.starts_with('@') && !w.starts_with("@@")) =>
                    {
                        let name = r[v.start..v.end].to_ascii_lowercase();
                        match self.saved.iter().find(|(n, _)| *n == name) {
                            Some((_, m)) => *m,
                            None => return Err(()),
                        }
                    }
                    v if is_mode_read(v, r) => self.mode(),
                    _ => return Err(()),
                };
                self.current = Dialect::MySql(mode);
            } else if target.starts_with('@') && !target.starts_with("@@") {
                self.saved.retain(|(n, _)| *n != target);
                if is_mode_read(value, r) {
                    let m = self.mode();
                    self.saved.push((target, m));
                }
            }
        }
        Ok(found)
    }
}

/// The text a statement is sent as: `split`'s text without its leading comments.
pub fn sent_text(text: &str, dialect: Dialect) -> &str {
    &text[leading_blank_len(text, dialect)..]
}

// ---------------------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------------------

/// Where a text file is restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTarget {
    pub kind: SqlKind,
    /// The target database.
    pub database: String,
    /// The target session's dialect (its `sql_mode` on MySQL).
    pub dialect: Dialect,
    /// MySQL family: the server (which executable comments it runs).
    pub server: Option<MySqlServer>,
}

/// What [`scan`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub stats: ScanStats,
    /// `AppRunner` or `Psql`.
    pub path: RestorePath,
    /// The first [`MAX_REFUSALS`] refusals.
    pub refusals: Vec<String>,
    /// All refusals.
    pub refused: u64,
    /// PostgreSQL: statements that cannot run inside a transaction block.
    pub outside_tx: u64,
    /// `\connect` / `\c` lines (psql's `--single-transaction` covers the first connection
    /// only).
    pub connects: u64,
    /// The engine the header names.
    pub engine: Option<SqlKind>,
    /// The header says pg_dump wrote the file (`-- PostgreSQL database dump`).
    pub pg_dump: bool,
}

impl ScanResult {
    /// PostgreSQL: the whole file can run in one transaction (no transaction control of
    /// its own, nothing that refuses a transaction block, R31).
    pub fn transaction_ok(&self) -> bool {
        self.stats.transaction == 0 && self.outside_tx == 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("Could not read the file: {0}")]
    Io(#[from] io::Error),
    #[error("The file is not a SQL script: {0}")]
    Split(#[from] SplitError),
}

struct Scan<'a> {
    target: &'a ScanTarget,
    res: ScanResult,
    mysql_comments: bool,
}

impl Scan<'_> {
    fn refuse(&mut self, r: Refusal) {
        self.res.refused += 1;
        if self.res.refusals.len() < MAX_REFUSALS && !self.res.refusals.contains(&r.0) {
            self.res.refusals.push(r.0);
        }
    }

    fn statement(&mut self, text: &str, dialect: Dialect) {
        let s = &mut self.res.stats;
        s.statements += 1;
        match classify(text, dialect) {
            StatementKind::Read => s.reads += 1,
            StatementKind::Write => s.writes += 1,
            StatementKind::Ddl => s.ddl += 1,
            StatementKind::Transaction(_) => s.transaction += 1,
            StatementKind::Other => s.other += 1,
        }
        if leading_keywords(text, dialect, 1)
            .first()
            .map(String::as_str)
            == Some("DROP")
        {
            s.drops += 1;
        }
        let outside_tx = match self.target.kind {
            SqlKind::MySql => false,
            SqlKind::Postgres => pg_outside_tx_block(text),
        };
        if outside_tx {
            self.res.outside_tx += 1;
        }
        if let Some(r) = statement_refusal(text, dialect, self.target) {
            self.refuse(r);
        }
    }

    fn client_command(&mut self, line: &str) {
        self.res.stats.client_commands += 1;
        let cmd = line.split_whitespace().next().unwrap_or("");
        if cmd == "\\c" || cmd == "\\connect" {
            self.res.connects += 1;
        }
        let r = match self.target.kind {
            SqlKind::MySql => Some(Refusal(format!(
                "The file uses the client command {}, which the app does not run.",
                line.trim().chars().take(2).collect::<String>()
            ))),
            SqlKind::Postgres => psql_command_refusal(line, &self.target.database),
        };
        if let Some(r) = r {
            self.refuse(r);
        }
    }
}

/// Scans a SQL text (already decompressed): statement counts, the path (app runner or
/// psql) and the refusals. A UTF-8 BOM at the start is skipped. The session `sql_mode`
/// is followed with [`ModeTracker`] (as the job follows it).
pub fn scan(mut reader: impl Read, target: &ScanTarget) -> Result<ScanResult, ScanError> {
    let mut splitter = StreamSplitter::new(
        target.dialect,
        SplitOptions {
            detect_client: true,
            ..SplitOptions::default()
        },
    );
    let mut mode = ModeTracker::new(target.dialect, target.server);
    let mut st = Scan {
        target,
        mysql_comments: false,
        res: ScanResult {
            stats: ScanStats::default(),
            path: RestorePath::AppRunner,
            refusals: Vec::new(),
            refused: 0,
            outside_tx: 0,
            connects: 0,
            engine: None,
            pg_dump: false,
        },
    };
    let mut buf = vec![0u8; CHUNK];
    let mut head: Vec<u8> = Vec::new();
    let mut first = true;
    let mut ended = false;
    loop {
        match splitter.next_event()? {
            Some(SplitEvent::Statement(s)) => {
                let d = mode.current();
                let text = sent_text(&s.text, d);
                if text.is_empty() {
                    continue;
                }
                st.statement(text, d);
                if let Err(r) = mode.after(text) {
                    st.refuse(r);
                }
                if mode.current() != splitter.dialect() {
                    splitter.set_dialect(mode.current());
                }
            }
            Some(SplitEvent::CopyData { .. }) => st.res.stats.copy_blocks += 1,
            Some(SplitEvent::ClientCommand(c)) => st.client_command(&c.text),
            None if splitter.is_done() => break,
            None if ended => splitter.finish(),
            None => {
                let n = read_full(&mut reader, &mut buf)?;
                if n == 0 {
                    ended = true;
                    splitter.finish();
                    continue;
                }
                let mut chunk = &buf[..n];
                if first {
                    first = false;
                    if chunk.starts_with(BOM) {
                        chunk = &chunk[BOM.len()..];
                    }
                }
                if head.len() < HEAD_TEXT {
                    let take = (HEAD_TEXT - head.len()).min(chunk.len());
                    head.extend_from_slice(&chunk[..take]);
                }
                splitter.push(chunk);
            }
        }
    }
    let head = String::from_utf8_lossy(&head);
    st.res.stats.app_dump = head.starts_with(&format!("{DUMP_MAGIC}\n"))
        || head.starts_with(&format!("{DUMP_MAGIC}\r\n"));
    st.res.engine = header_engine(&head);
    st.res.pg_dump = head
        .lines()
        .any(|l| l.trim().starts_with("-- PostgreSQL database dump"));
    // PostgreSQL lexes MySQL's `/*! … */` as comments and would skip them silently.
    st.mysql_comments |= head.lines().any(|l| l.trim_start().starts_with("/*!"));
    if st.mysql_comments && st.res.engine.is_none() {
        st.res.engine = Some(SqlKind::MySql);
    }
    if let Some(e) = st.res.engine.filter(|e| *e != target.kind) {
        st.refuse(Refusal(format!(
            "This is a {} dump; it cannot be restored into {}.",
            engine_name(e),
            engine_name(target.kind)
        )));
    }
    let needs_psql = match target.kind {
        SqlKind::MySql => false,
        SqlKind::Postgres => st.res.stats.copy_blocks > 0 || st.res.stats.client_commands > 0,
    };
    if needs_psql {
        st.res.path = RestorePath::Psql;
        if !st.res.pg_dump {
            st.refuse(Refusal(
                "This file needs psql but was not written by pg_dump (no \"-- PostgreSQL \
                 database dump\" header); the app runs psql for pg_dump files only."
                    .into(),
            ));
        }
    }
    Ok(st.res)
}

/// Reads until `buf` is full or the input ends.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------

/// A reader that hashes (SHA-256) and counts the raw bytes it passes on.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        HashingReader {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    /// Raw bytes read so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Reads the rest of the input (so the hash covers the whole file); `(sha256, size)`.
    pub fn finish(mut self) -> io::Result<(String, u64)> {
        let mut buf = vec![0u8; 64 * 1024];
        while self.read(&mut buf)? > 0 {}
        Ok((hex::encode(self.hasher.finalize()), self.bytes))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

/// The content of a restore file: decompressed when gzip, the first bytes peeked for
/// [`container`], the raw bytes hashed underneath.
pub struct Content<R: Read> {
    pub gzip: bool,
    pub container: Container,
    reader: Chain<Cursor<Vec<u8>>, Box<dyn Read + Send>>,
    hashed: std::sync::Arc<std::sync::Mutex<Option<HashingReader<R>>>>,
}

/// Shares the hashing reader between the (decompressing) content reader and
/// [`Content::finish`].
struct Shared<R>(std::sync::Arc<std::sync::Mutex<Option<HashingReader<R>>>>);

impl<R: Read> Read for Shared<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            Some(r) => r.read(buf),
            None => Ok(0),
        }
    }
}

impl<R: Read + Send + 'static> Content<R> {
    /// Opens the content of `raw`.
    pub fn open(raw: R) -> io::Result<Content<R>> {
        let hashed = std::sync::Arc::new(std::sync::Mutex::new(Some(HashingReader::new(raw))));
        let mut src = Shared(std::sync::Arc::clone(&hashed));
        let mut magic = [0u8; 2];
        let n = read_full(&mut src, &mut magic)?;
        let gzip = is_gzip(&magic[..n]);
        let prefix = Cursor::new(magic[..n].to_vec());
        let mut inner: Box<dyn Read + Send> = if gzip {
            Box::new(MultiGzDecoder::new(prefix.chain(src)))
        } else {
            Box::new(prefix.chain(src))
        };
        let mut head = vec![0u8; 512];
        let n = read_full(&mut inner, &mut head)?;
        head.truncate(n);
        let container = container(&head);
        Ok(Content {
            gzip,
            container,
            reader: Cursor::new(head).chain(inner),
            hashed,
        })
    }

    /// Raw bytes read so far.
    pub fn raw_bytes(&self) -> u64 {
        self.hashed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map_or(0, HashingReader::bytes)
    }

    /// Reads the rest of the raw file; `(sha256, size)` of every raw byte.
    pub fn finish(self) -> io::Result<(String, u64)> {
        drop(self.reader);
        let r = self
            .hashed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| io::Error::other("the file was already finished"))?;
        r.finish()
    }
}

impl<R: Read> Read for Content<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

/// What the preview learns from a file in one pass (R30, R21): its facts (SHA-256 of the
/// raw bytes), gzip, the container, and for text the scan.
#[derive(Debug)]
pub struct Analysis {
    pub file: FileFacts,
    pub gzip: bool,
    pub container: Container,
    /// Text files only.
    pub scan: Option<Result<ScanResult, String>>,
}

/// Detects, scans (text) and hashes `path` in one pass.
pub fn analyze_file(path: &Path, target: &ScanTarget) -> io::Result<Analysis> {
    let file = std::fs::File::open(path)?;
    let mut content = Content::open(BufReader::with_capacity(CHUNK, file))?;
    let scan = match content.container {
        Container::Text => Some(scan(&mut content, target).map_err(|e| e.to_string())),
        _ => None,
    };
    let (gzip, container) = (content.gzip, content.container);
    let (sha256, size) = content.finish()?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Analysis {
        file: FileFacts { name, size, sha256 },
        gzip,
        container,
        scan,
    })
}

#[cfg(test)]
#[path = "detect_tests.rs"]
mod tests;
