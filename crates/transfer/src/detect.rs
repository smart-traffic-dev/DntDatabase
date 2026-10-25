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
//! Refusals ([`Refusal`]), found at preview so the typed confirmation covers what runs
//! (R31): `CREATE` / `DROP` / `ALTER DATABASE` (MySQL also `SCHEMA`) naming another
//! database, `USE <db>` naming a database other than the target, a file of the other
//! engine (by its header, or MySQL `/*!` comments on PostgreSQL), and (psql path) client
//! commands other than `\connect` / `\c` to the target and pg_dump's `\restrict` /
//! `\unrestrict` (a `\!` would run a shell command; `\i`, `\o`, `\copy`, `\set` …
//! would read or write other files or change psql's error handling). `CREATE DATABASE`
//! and `DROP DATABASE` are always refused, whatever they name. The restore job applies
//! [`statement_refusal`] again to every statement before it is sent.
//!
//! Classification uses `dnt_core::sql::classify` only (this crate does not depend on
//! `dnt_query`; the job judges every statement with the query guard).
//!
//! Known limit: the scan cannot run `SET sql_mode`, so it follows a literal
//! `SET [SESSION] sql_mode = '…'` lexically ([`sql_mode_after`]) and falls back to the
//! session's mode for anything else (`SET sql_mode = @OLD_SQL_MODE`); the job re-reads
//! the real session mode after every statement (R33) and checks every statement again.

use std::io::{self, BufReader, Chain, Cursor, Read};
use std::path::Path;

use dnt_core::server::DbKind;
use dnt_core::sql::{
    classify, leading_blank_len, leading_keywords, readings, session_effect, tokenize, Dialect,
    MySqlMode, SplitError, SplitEvent, SplitOptions, StatementKind, StreamSplitter, TokKind, Token,
};
use flate2::read::GzDecoder;
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
pub fn header_engine(head: &str) -> Option<DbKind> {
    for line in head.lines().take(40) {
        let l = line.trim();
        if let Some(src) = l.strip_prefix("-- Source: ") {
            if src.starts_with("PostgreSQL") {
                return Some(DbKind::Postgres);
            }
            if src.starts_with("MySQL") || src.starts_with("MariaDB") {
                return Some(DbKind::MySql);
            }
        }
        if l.starts_with("-- MySQL dump") || l.starts_with("-- MariaDB dump") {
            return Some(DbKind::MySql);
        }
        if l.starts_with("-- PostgreSQL database dump") {
            return Some(DbKind::Postgres);
        }
    }
    None
}

fn engine_name(kind: DbKind) -> &'static str {
    match kind {
        DbKind::MySql => "MySQL",
        DbKind::Postgres => "PostgreSQL",
    }
}

// ---------------------------------------------------------------------------------------
// Statement checks (shared with the restore job)
// ---------------------------------------------------------------------------------------

/// Why a statement or client command cannot be restored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

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

/// The object name after the leading keywords `skip` (`IF [NOT] EXISTS` skipped).
fn name_after(s: &str, dialect: Dialect, skip: usize) -> Option<String> {
    let toks = tokenize(s, dialect);
    let mut i = skip;
    let word = |t: Option<&Token>, w: &str| matches!(t.map(|t| t.kind), Some(TokKind::Word(x)) if x.eq_ignore_ascii_case(w));
    if word(toks.get(i), "IF") {
        i += 1;
        if word(toks.get(i), "NOT") {
            i += 1;
        }
        if word(toks.get(i), "EXISTS") {
            i += 1;
        }
    }
    let t = toks.get(i)?;
    match t.kind {
        TokKind::Word(_) | TokKind::Quoted => Some(ident_text(s, t)),
        _ => None,
    }
}

/// The app runner's refusal of one statement (R31): `CREATE` / `DROP DATABASE` (MySQL
/// also `SCHEMA`, MariaDB `CREATE OR REPLACE`), `ALTER DATABASE` naming another database,
/// `USE` naming another database; on MySQL a backslash outside quoted text (a mysql
/// client command such as `\G`). Every reading of a MySQL executable comment is checked.
pub fn statement_refusal(
    text: &str,
    dialect: Dialect,
    kind: DbKind,
    target_db: &str,
) -> Option<Refusal> {
    let mysql = kind == DbKind::MySql;
    for r in readings(text, dialect) {
        let words = leading_keywords(&r, dialect, 4);
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        let db_word = |x: &str| x == "DATABASE" || (mysql && x == "SCHEMA");
        match w.as_slice() {
            ["CREATE", x, ..] if db_word(x) => {
                return Some(Refusal(format!(
                    "The file creates a database (\"{}\"); restore runs into the chosen \
                     database only.",
                    name_after(&r, dialect, 2).unwrap_or_default()
                )))
            }
            ["CREATE", "OR", "REPLACE", x, ..] if db_word(x) => {
                return Some(Refusal(
                    "The file creates a database; restore runs into the chosen database only."
                        .into(),
                ))
            }
            ["DROP", x, ..] if db_word(x) => {
                return Some(Refusal(format!(
                    "The file drops a database (\"{}\").",
                    name_after(&r, dialect, 2).unwrap_or_default()
                )))
            }
            ["ALTER", x, ..] if db_word(x) => {
                let name = name_after(&r, dialect, 2);
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
                let name = name_after(&r, dialect, 1).unwrap_or_default();
                if name != target_db {
                    return Some(Refusal(format!(
                        "The file switches to another database (USE \"{name}\"); restore runs \
                         into \"{target_db}\" only."
                    )));
                }
            }
            _ => {}
        }
        if mysql {
            if let Some(t) = tokenize(&r, dialect)
                .iter()
                .find(|t| t.kind == TokKind::Punct(b'\\'))
            {
                let cmd: String = r[t.start..].chars().take(2).collect();
                return Some(Refusal(format!(
                    "The file uses the mysql client command {cmd}, which the app does not run."
                )));
            }
        }
    }
    None
}

/// The psql path's check of one `\` command line: `\connect` / `\c` to the target (or
/// `-`, or no database), pg_dump's `\restrict` / `\unrestrict`; anything else is refused.
pub fn psql_command_refusal(line: &str, target_db: &str) -> Option<Refusal> {
    let body = line.trim().trim_start_matches('\\');
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();
    match cmd {
        "restrict" | "unrestrict" => None,
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

/// The dialect after `text` ran, read lexically: a session `SET sql_mode = '<literal>'`
/// (also inside `/*! */`) gives its flags; another session `sql_mode` assignment gives
/// `fallback` (the mode the session started with); anything else `None` (unchanged).
pub fn sql_mode_after(text: &str, dialect: Dialect, fallback: Dialect) -> Option<Dialect> {
    let Dialect::MySql(_) = dialect else {
        return None;
    };
    if !session_effect(text, dialect).is_some_and(|e| e.sql_mode) {
        return None;
    }
    for r in readings(text, dialect) {
        let toks = tokenize(&r, dialect);
        for (i, t) in toks.iter().enumerate() {
            let TokKind::Word(w) = t.kind else { continue };
            let w = w.to_ascii_lowercase();
            if !matches!(
                w.as_str(),
                "sql_mode" | "@@sql_mode" | "@@session.sql_mode" | "@@local.sql_mode"
            ) {
                continue;
            }
            if toks.get(i + 1).map(|t| t.kind) != Some(TokKind::Punct(b'=')) {
                continue;
            }
            if let Some(v) = toks.get(i + 2).filter(|v| v.kind == TokKind::Quoted) {
                let lit = ident_text(&r, v);
                return Some(Dialect::MySql(parse_sql_mode(&lit)));
            }
        }
    }
    Some(fallback)
}

/// The lexing flags of a `sql_mode` value (as the MySQL driver reads them).
pub fn parse_sql_mode(s: &str) -> MySqlMode {
    let mut mode = MySqlMode::default();
    for flag in s.split(',').map(str::trim) {
        if flag.eq_ignore_ascii_case("ANSI_QUOTES") || flag.eq_ignore_ascii_case("ANSI") {
            mode.ansi_quotes = true;
        } else if flag.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES") {
            mode.no_backslash_escapes = true;
        }
    }
    mode
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
    pub kind: DbKind,
    /// The target database.
    pub database: String,
    /// The target session's dialect (its `sql_mode` on MySQL).
    pub dialect: Dialect,
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
    /// The engine the header names.
    pub engine: Option<DbKind>,
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
        let text = sent_text(text, dialect);
        if text.is_empty() {
            return;
        }
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
        let pg = self.target.kind == DbKind::Postgres;
        if pg && pg_outside_tx_block(text) {
            self.res.outside_tx += 1;
        }
        if let Some(r) = statement_refusal(text, dialect, self.target.kind, &self.target.database) {
            self.refuse(r);
        }
    }

    fn client_command(&mut self, line: &str) {
        self.res.stats.client_commands += 1;
        let r = match self.target.kind {
            DbKind::MySql => Some(Refusal(format!(
                "The file uses the client command {}, which the app does not run.",
                line.trim().chars().take(2).collect::<String>()
            ))),
            DbKind::Postgres => psql_command_refusal(line, &self.target.database),
        };
        if let Some(r) = r {
            self.refuse(r);
        }
    }
}

/// Scans a SQL text (already decompressed): statement counts, the path (app runner or
/// psql) and the refusals. A UTF-8 BOM at the start is skipped.
pub fn scan(mut reader: impl Read, target: &ScanTarget) -> Result<ScanResult, ScanError> {
    let mut splitter = StreamSplitter::new(
        target.dialect,
        SplitOptions {
            detect_client: true,
            ..SplitOptions::default()
        },
    );
    let mut st = Scan {
        target,
        mysql_comments: false,
        res: ScanResult {
            stats: ScanStats::default(),
            path: RestorePath::AppRunner,
            refusals: Vec::new(),
            refused: 0,
            outside_tx: 0,
            engine: None,
        },
    };
    let mut buf = vec![0u8; CHUNK];
    let mut head: Vec<u8> = Vec::new();
    let mut first = true;
    let mut ended = false;
    loop {
        match splitter.next_event()? {
            Some(SplitEvent::Statement(s)) => {
                let d = splitter.dialect();
                st.statement(&s.text, d);
                if let Some(next) = sql_mode_after(sent_text(&s.text, d), d, target.dialect) {
                    splitter.set_dialect(next);
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
    // PostgreSQL lexes MySQL's `/*! … */` as comments and would skip them silently.
    st.mysql_comments |= head.lines().any(|l| l.trim_start().starts_with("/*!"));
    if st.mysql_comments && st.res.engine.is_none() {
        st.res.engine = Some(DbKind::MySql);
    }
    if let Some(e) = st.res.engine.filter(|e| *e != target.kind) {
        st.refuse(Refusal(format!(
            "This is a {} dump; it cannot be restored into {}.",
            engine_name(e),
            engine_name(target.kind)
        )));
    }
    if target.kind == DbKind::Postgres
        && (st.res.stats.copy_blocks > 0 || st.res.stats.client_commands > 0)
    {
        st.res.path = RestorePath::Psql;
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
            Box::new(GzDecoder::new(prefix.chain(src)))
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
