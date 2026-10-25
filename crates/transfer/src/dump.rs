//! The pure parts of the integrated dump (spec §7.1, R22–R24): the header, the session
//! statements of R23, the table order by foreign key, the view order by textual
//! reference, the MySQL view rewrite, the PostgreSQL statement helpers, multi-row inserts
//! with dump literals and [`DumpWriter`].
//!
//! Every statement a dump holds ends with `;\n`, so `dnt_core::sql::split` with the
//! default dialect gives back exactly the statements written (comments written between
//! them lead the next statement's text).

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};

use dnt_core::server::DbKind;
use dnt_core::sql::build::{dump_row_tuple, BuildError, ColumnTarget};
use dnt_core::sql::ident::quote_ident;
use dnt_core::sql::{split, tokenize, Dialect, TokKind, Token};
use dnt_core::value::FullValue;
use flate2::write::GzEncoder;
use flate2::Compression;

/// The first line of every dump the app writes (restore detection reads it).
pub const DUMP_MAGIC: &str = "-- DntDatabase dump v1";
/// Rows per `INSERT` of a dump.
pub const DUMP_BATCH_ROWS: u32 = 1000;
/// Bytes of SQL per `INSERT` of a dump (1 MiB).
pub const DUMP_BATCH_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------------------
// Header and session statements
// ---------------------------------------------------------------------------------------

/// What the header says about the dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpHeader {
    /// `MySQL`, `MariaDB` or `PostgreSQL`.
    pub engine: String,
    pub version: String,
    pub database: String,
    /// PostgreSQL: the dumped schemas (empty on MySQL).
    pub schemas: Vec<String>,
    /// RFC 3339 UTC.
    pub created: String,
    pub tables: u32,
    pub tables_with_data: u32,
    pub views: u32,
    /// PostgreSQL only.
    pub sequences: Option<u32>,
    /// PostgreSQL only.
    pub types: Option<u32>,
}

/// The four header comment lines, each ending with `\n`.
pub fn header(h: &DumpHeader) -> String {
    let schemas = if h.schemas.is_empty() {
        String::new()
    } else {
        format!(", schemas {}", h.schemas.join(", "))
    };
    let mut objects = format!(
        "{} tables ({} with data), {} views",
        h.tables, h.tables_with_data, h.views
    );
    if let Some(s) = h.sequences {
        objects.push_str(&format!(", {s} sequences"));
    }
    if let Some(t) = h.types {
        objects.push_str(&format!(", {t} types"));
    }
    format!(
        "{DUMP_MAGIC}\n-- Source: {} {}, database {}{schemas}\n-- Created: {}\n-- Objects: {objects}\n",
        one_line(&h.engine),
        one_line(&h.version),
        one_line(&h.database),
        one_line(&h.created),
    )
}

/// The statements a restore runs first (R23).
pub fn session_prologue(kind: DbKind) -> Vec<&'static str> {
    match kind {
        DbKind::MySql => vec![
            "SET NAMES utf8mb4",
            "SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'",
            "SET time_zone = '+00:00'",
            "SET FOREIGN_KEY_CHECKS = 0",
        ],
        DbKind::Postgres => vec![
            "SET client_encoding = 'UTF8'",
            "SET standard_conforming_strings = on",
        ],
    }
}

/// The statements a restore runs last (R23).
pub fn session_epilogue(kind: DbKind) -> Vec<&'static str> {
    match kind {
        DbKind::MySql => vec!["SET FOREIGN_KEY_CHECKS = 1"],
        DbKind::Postgres => Vec::new(),
    }
}

/// The last line of a dump.
pub fn end_comment(statements: u64) -> String {
    format!("-- End of dump: {statements} statements\n")
}

/// `s` on one line (a comment must not run into the next line).
fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

// ---------------------------------------------------------------------------------------
// Orders
// ---------------------------------------------------------------------------------------

/// A table and the tables its foreign keys reference (`None` schema: the table's own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableNode {
    pub schema: Option<String>,
    pub name: String,
    pub references: Vec<(Option<String>, String)>,
}

/// The order to create and load `tables` in: a referenced table before the tables
/// referencing it; free tables by (schema, name). A cycle is broken at its member that
/// comes first by (schema, name). Self references and references to tables outside
/// `tables` are ignored.
pub fn table_order(tables: &[TableNode]) -> Vec<usize> {
    let key = |i: usize| (tables[i].schema.clone(), tables[i].name.clone());
    let index: HashMap<(Option<String>, String), usize> =
        (0..tables.len()).map(|i| (key(i), i)).collect();
    let deps: Vec<BTreeSet<usize>> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            t.references
                .iter()
                .filter_map(|(s, n)| {
                    let schema = s.clone().or_else(|| t.schema.clone());
                    index.get(&(schema, n.clone())).copied()
                })
                .filter(|&d| d != i)
                .collect()
        })
        .collect();
    let by_name = |a: &usize, b: &usize| key(*a).cmp(&key(*b));
    order_with(tables.len(), &deps, by_name)
}

/// A view and its DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewNode {
    pub name: String,
    pub ddl: String,
}

/// The order to create `views` in: a view whose DDL names another view (an identifier
/// token outside strings and comments) comes after it; free views by name.
pub fn view_order(views: &[ViewNode], d: Dialect) -> Vec<usize> {
    let deps: Vec<BTreeSet<usize>> = views
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let toks = tokenize(&v.ddl, d);
            (0..views.len())
                .filter(|&j| j != i && toks.iter().any(|t| names(&v.ddl, t, &views[j].name, d)))
                .collect()
        })
        .collect();
    order_with(views.len(), &deps, |a, b| {
        views[*a].name.cmp(&views[*b].name)
    })
}

/// Whether token `t` of `sql` is the identifier `name` (quoted, or bare: PostgreSQL folds
/// a bare name to lower case, MySQL compares it case-insensitively).
fn names(sql: &str, t: &Token<'_>, name: &str, d: Dialect) -> bool {
    match t.kind {
        TokKind::Word(w) => match d {
            Dialect::Postgres => w.to_lowercase() == name,
            Dialect::MySql(_) => w.eq_ignore_ascii_case(name),
        },
        TokKind::Quoted => {
            let text = &sql[t.start..t.end];
            let quoted = match d {
                Dialect::Postgres => quote_ident(DbKind::Postgres, name),
                Dialect::MySql(m) => {
                    if text.starts_with('"') && m.ansi_quotes {
                        format!("\"{}\"", name.replace('"', "\"\""))
                    } else {
                        quote_ident(DbKind::MySql, name)
                    }
                }
            };
            text == quoted
        }
        _ => false,
    }
}

/// Kahn's algorithm over `deps` (node → the nodes it needs), taking the first ready node
/// by `cmp`; when none is ready, the first node by `cmp` that lies on a cycle of the
/// nodes left is taken.
fn order_with(
    n: usize,
    deps: &[BTreeSet<usize>],
    cmp: impl Fn(&usize, &usize) -> std::cmp::Ordering,
) -> Vec<usize> {
    let mut done = vec![false; n];
    let mut out = Vec::with_capacity(n);
    let mut left: Vec<usize> = (0..n).collect();
    left.sort_by(&cmp);
    while !left.is_empty() {
        let ready = left
            .iter()
            .position(|&i| deps[i].iter().all(|&d| done[d]))
            .or_else(|| left.iter().position(|&i| on_cycle(i, deps, &done)))
            .unwrap_or(0);
        let i = left.remove(ready);
        done[i] = true;
        out.push(i);
    }
    out
}

/// Whether `start` can reach itself through the nodes not done yet.
fn on_cycle(start: usize, deps: &[BTreeSet<usize>], done: &[bool]) -> bool {
    let mut seen = vec![false; deps.len()];
    let mut stack: Vec<usize> = deps[start].iter().copied().filter(|&d| !done[d]).collect();
    while let Some(i) = stack.pop() {
        if i == start {
            return true;
        }
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        stack.extend(deps[i].iter().copied().filter(|&d| !done[d]));
    }
    false
}

// ---------------------------------------------------------------------------------------
// MySQL view rewrite
// ---------------------------------------------------------------------------------------

/// `SHOW CREATE VIEW` text made portable (R23): the `DEFINER=<user>` clause before `VIEW`
/// is dropped, and so is every `` `<source_db>`. `` qualifier that starts a dotted name.
/// Strings and comments are never touched (lexed with the default MySQL dialect).
pub fn mysql_view_ddl(ddl: &str, source_db: &str) -> String {
    let toks = tokenize(ddl, Dialect::MYSQL);
    let text = |t: &Token<'_>| &ddl[t.start..t.end];
    let adjacent = |a: &Token<'_>, b: &Token<'_>| a.end == b.start;
    let mut cut: Vec<(usize, usize)> = Vec::new();

    // DEFINER = <user>: only in the clauses before the first `VIEW` keyword.
    let view_at = toks
        .iter()
        .position(|t| matches!(t.kind, TokKind::Word(w) if w.eq_ignore_ascii_case("VIEW")))
        .unwrap_or(toks.len());
    if let Some(at) = toks[..view_at]
        .iter()
        .position(|t| matches!(t.kind, TokKind::Word(w) if w.eq_ignore_ascii_case("DEFINER")))
    {
        if matches!(toks.get(at + 1).map(|t| t.kind), Some(TokKind::Punct(b'=')))
            && at + 2 < view_at
        {
            let mut last = at + 2;
            while last + 1 < view_at && adjacent(&toks[last], &toks[last + 1]) {
                last += 1;
            }
            let mut end = toks[last].end;
            end += ddl[end..].len() - ddl[end..].trim_start().len();
            cut.push((toks[at].start, end));
        }
    }

    // `<source_db>`. starting a dotted name.
    let qualifier = quote_ident(DbKind::MySql, source_db);
    for i in 0..toks.len() {
        let t = &toks[i];
        if t.kind != TokKind::Quoted || text(t) != qualifier {
            continue;
        }
        let after_dot =
            i > 0 && toks[i - 1].kind == TokKind::Punct(b'.') && adjacent(&toks[i - 1], t);
        let (Some(dot), Some(next)) = (toks.get(i + 1), toks.get(i + 2)) else {
            continue;
        };
        if !after_dot
            && dot.kind == TokKind::Punct(b'.')
            && adjacent(t, dot)
            && adjacent(dot, next)
            && matches!(next.kind, TokKind::Quoted | TokKind::Word(_))
        {
            cut.push((t.start, dot.end));
        }
    }

    cut.sort_unstable();
    let mut out = String::with_capacity(ddl.len());
    let mut at = 0;
    for (from, to) in cut {
        if from >= at {
            out.push_str(&ddl[at..from]);
            at = to;
        }
    }
    out.push_str(&ddl[at..]);
    out
}

// ---------------------------------------------------------------------------------------
// PostgreSQL statements
// ---------------------------------------------------------------------------------------

/// A PostgreSQL string literal (standard-conforming: only `'` doubled) of an identifier
/// text the dump names (never a cell value).
fn pg_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `CREATE SCHEMA IF NOT EXISTS "<schema>"`.
pub fn create_schema_sql(schema: &str) -> String {
    format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(DbKind::Postgres, schema)
    )
}

/// The `setval` of the sequence behind an identity / serial column: `relation` is the
/// table's quoted qualified name, `last_value` / `is_called` literals read in the snapshot.
pub fn setval_column_sql(
    relation: &str,
    column: &str,
    last_value: &str,
    is_called: &str,
) -> String {
    format!(
        "SELECT pg_catalog.setval(pg_catalog.pg_get_serial_sequence({}, {}), {last_value}, {is_called})",
        pg_text(relation),
        pg_text(column)
    )
}

/// The `setval` of a sequence by its quoted qualified name.
pub fn setval_sql(sequence: &str, last_value: &str, is_called: &str) -> String {
    format!(
        "SELECT pg_catalog.setval({}, {last_value}, {is_called})",
        pg_text(sequence)
    )
}

/// `ALTER TABLE <relation> ADD CONSTRAINT "<name>" <definition>` (the server's
/// `pg_get_constraintdef` text).
pub fn add_fk_sql(relation: &str, name: &str, definition: &str) -> String {
    format!(
        "ALTER TABLE {relation} ADD CONSTRAINT {} {}",
        quote_ident(DbKind::Postgres, name),
        definition.trim().trim_end_matches(';')
    )
}

// ---------------------------------------------------------------------------------------
// Inserts
// ---------------------------------------------------------------------------------------

/// `INSERT INTO <relation> (<cols>)[ OVERRIDING SYSTEM VALUE] VALUES ` (with the trailing
/// space). `relation` is already quoted (MySQL dumps name tables unqualified, R23).
pub fn insert_head(
    kind: DbKind,
    relation: &str,
    cols: &[ColumnTarget],
    overriding: bool,
) -> String {
    let names: Vec<String> = cols.iter().map(|c| quote_ident(kind, &c.name)).collect();
    let overriding = if overriding && kind == DbKind::Postgres {
        " OVERRIDING SYSTEM VALUE"
    } else {
        ""
    };
    format!(
        "INSERT INTO {relation} ({}){overriding} VALUES ",
        names.join(", ")
    )
}

/// Collects rows into multi-row `INSERT`s of at most `max_rows` rows and `max_bytes` bytes;
/// values through `build::dump_row_tuple` only. A row that alone exceeds `max_bytes` is
/// its own statement.
#[derive(Debug)]
pub struct DumpInserts {
    head: String,
    cols: Vec<ColumnTarget>,
    dialect: Dialect,
    max_rows: u32,
    max_bytes: usize,
    sql: String,
    rows: u32,
}

impl DumpInserts {
    pub fn new(
        head: String,
        cols: Vec<ColumnTarget>,
        dialect: Dialect,
        max_rows: u32,
        max_bytes: usize,
    ) -> DumpInserts {
        DumpInserts {
            sql: String::new(),
            head,
            cols,
            dialect,
            max_rows: max_rows.max(1),
            max_bytes,
            rows: 0,
        }
    }

    /// Adds a row; returns the previous statement when the row does not fit in it.
    pub fn push(&mut self, row: &[FullValue]) -> Result<Option<String>, BuildError> {
        let tuple = dump_row_tuple(&self.cols, row, self.dialect)?;
        let out = if self.rows > 0
            && (self.rows >= self.max_rows || self.sql.len() + 2 + tuple.len() > self.max_bytes)
        {
            self.flush()
        } else {
            None
        };
        if self.rows == 0 {
            self.sql.push_str(&self.head);
        } else {
            self.sql.push_str(", ");
        }
        self.sql.push_str(&tuple);
        self.rows += 1;
        Ok(out)
    }

    /// The pending statement, if any row waits.
    pub fn flush(&mut self) -> Option<String> {
        if self.rows == 0 {
            return None;
        }
        self.rows = 0;
        Some(std::mem::take(&mut self.sql))
    }
}

// ---------------------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------------------

enum Out {
    Plain(Box<dyn Write + Send>),
    Gzip(GzEncoder<Box<dyn Write + Send>>),
}

/// Writes a dump: raw text (the header), comments and statements, each statement ending
/// with `;\n`; counts statements and (uncompressed) bytes. `compress`: gzip (`.sql.gz`).
pub struct DumpWriter {
    out: Out,
    statements: u64,
    bytes: u64,
}

impl DumpWriter {
    pub fn new(inner: Box<dyn Write + Send>, compress: bool) -> DumpWriter {
        DumpWriter {
            out: if compress {
                Out::Gzip(GzEncoder::new(inner, Compression::default()))
            } else {
                Out::Plain(inner)
            },
            statements: 0,
            bytes: 0,
        }
    }

    fn put(&mut self, s: &str) -> io::Result<()> {
        match &mut self.out {
            Out::Plain(w) => w.write_all(s.as_bytes())?,
            Out::Gzip(w) => w.write_all(s.as_bytes())?,
        }
        self.bytes += s.len() as u64;
        Ok(())
    }

    /// Text written as is (the header, the end comment).
    pub fn raw(&mut self, text: &str) -> io::Result<()> {
        self.put(text)
    }

    /// `-- <text>` on its own line.
    pub fn comment(&mut self, text: &str) -> io::Result<()> {
        self.put(&format!("-- {}\n", one_line(text)))
    }

    /// One statement (surrounding whitespace and trailing `;` removed) and `;\n`. A
    /// statement that is blank afterwards is not written.
    pub fn statement(&mut self, sql: &str) -> io::Result<()> {
        let mut s = sql.trim();
        while let Some(rest) = s.strip_suffix(';') {
            s = rest.trim_end();
        }
        if s.is_empty() {
            return Ok(());
        }
        self.put(s)?;
        self.put(";\n")?;
        self.statements += 1;
        Ok(())
    }

    /// Every statement of `script` (split by `d`), one by one; returns how many.
    pub fn script(&mut self, script: &str, d: Dialect) -> io::Result<u64> {
        let before = self.statements;
        for s in split(script, d) {
            self.statement(&s.text)?;
        }
        Ok(self.statements - before)
    }

    /// Statements written so far.
    pub fn statements(&self) -> u64 {
        self.statements
    }

    /// Uncompressed bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Completes the output (gzip trailer, flush); returns the uncompressed bytes.
    pub fn finish(self) -> io::Result<u64> {
        match self.out {
            Out::Plain(mut w) => w.flush()?,
            Out::Gzip(w) => w.finish()?.flush()?,
        }
        Ok(self.bytes)
    }
}
