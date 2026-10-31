//! The DDL that applies a diff (or creates a new table), with what it does to the data
//! (spec §4.2). Engine-neutral types and the dispatch by [`ServerInfo::kind`]; the
//! dialects live in `mysql` and `pg`.
//!
//! Every function takes the session [`Dialect`] right after the server (ruling T7-1):
//! MySQL string literals depend on its `NO_BACKSLASH_ESCAPES` flag.

mod mysql;
mod pg;

use crate::diff::Change;
use crate::model::*;
use dnt_core::server::{ServerInfo, SqlKind};
use dnt_core::sql::build::TableRef;
use dnt_core::sql::Dialect;
use serde::{Deserialize, Serialize};
use specta::Type;

/// The statements to send, in order, and their impacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Rendered {
    pub statements: Vec<String>,
    pub impacts: Vec<Impact>,
}

/// Something a change does beyond the schema: values it may lose, a table rebuild,
/// work that cannot be rolled back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Impact {
    pub kind: ImpactKind,
    pub element: ElementRef,
    pub message: String,
    /// A read-only `SELECT COUNT(*)` of the affected rows, run before applying.
    pub count_sql: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ImpactKind {
    DataLoss,
    Rebuild,
    NotTransactional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderError {
    /// The server version cannot do this.
    Unsupported {
        feature: String,
        min_version: String,
    },
    /// The draft or the changes cannot be rendered as given.
    Invalid { message: String },
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Unsupported {
                feature,
                min_version,
            } => write!(f, "{feature} needs {min_version}."),
            RenderError::Invalid { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for RenderError {}

fn invalid(message: impl Into<String>) -> RenderError {
    RenderError::Invalid {
        message: message.into(),
    }
}

fn unsupported(feature: &str, min_version: &str) -> RenderError {
    RenderError::Unsupported {
        feature: feature.into(),
        min_version: min_version.into(),
    }
}

/// A name spliced unquoted (engine, character set, collation, row format, index
/// method): letters, digits and `_` only.
fn word<'a>(what: &str, v: &'a str) -> Result<&'a str, RenderError> {
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(v)
    } else {
        Err(invalid(format!("Invalid {what} \"{v}\".")))
    }
}

/// A referential action in its canonical upper-case spelling.
fn fk_action(action: &str) -> Result<&'static str, RenderError> {
    const FK_ACTIONS: &[&str] = &[
        "RESTRICT",
        "CASCADE",
        "SET NULL",
        "NO ACTION",
        "SET DEFAULT",
    ];
    FK_ACTIONS
        .iter()
        .find(|a| a.eq_ignore_ascii_case(action.trim()))
        .copied()
        .ok_or_else(|| invalid(format!("Unknown referential action \"{action}\".")))
}

/// `[-+]digits[.digits][e[-+]digits]`.
fn is_number(s: &str) -> bool {
    fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    digits(int)
        && frac.is_none_or(digits)
        && exp.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

/// Impacts by kind: data loss first, then rebuilds (one per element), then the rest.
#[derive(Default)]
struct Impacts {
    data_loss: Vec<Impact>,
    rebuild: Vec<Impact>,
    not_transactional: Vec<Impact>,
}

impl Impacts {
    fn data_loss(&mut self, column: &str, message: String, count_sql: Option<String>) {
        self.data_loss.push(Impact {
            kind: ImpactKind::DataLoss,
            element: ElementRef::Column(column.into()),
            message,
            count_sql,
        });
    }

    fn rebuild(&mut self, column: &str, message: String) {
        let element = ElementRef::Column(column.into());
        if !self.rebuild.iter().any(|i| i.element == element) {
            self.rebuild.push(Impact {
                kind: ImpactKind::Rebuild,
                element,
                message,
                count_sql: None,
            });
        }
    }

    fn finish(self) -> Vec<Impact> {
        let mut out = self.data_loss;
        out.extend(self.rebuild);
        out.extend(self.not_transactional);
        out
    }
}

/// The statements that turn `old` (the stored table at `table`) into the draft that
/// [`crate::diff`] compared it with. `changes` is that diff; an empty diff renders no
/// statement. A [`Change::CreateTable`] diff goes through [`render_create`] instead.
///
/// **Precondition:** the draft passed [`crate::validate`]; expression fields (defaults,
/// checks, index expressions, custom types, PostgreSQL `ColumnDef.using`) are spliced
/// verbatim. PostgreSQL output (here and in [`render_create`] and the `*_sql`
/// fragments) uses standard `'…'` literals and needs `standard_conforming_strings = on`,
/// which the backend sets with `SET LOCAL` inside the apply transaction (ruling T8-1).
pub fn render(
    table: &TableRef,
    old: &TableDef,
    changes: &[Change],
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<Rendered, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::render(table, old, changes, server, dialect),
        SqlKind::Postgres => pg::render(table, old, changes, server, dialect),
    }
}

/// The statements that create `def` as a new table in `table`'s database (and schema),
/// named `def.name`.
///
/// **Precondition:** `def` passed [`crate::validate`] (expression fields are spliced
/// verbatim); on PostgreSQL `standard_conforming_strings = on` (see [`render`]).
pub fn render_create(
    table: &TableRef,
    def: &TableDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<Rendered, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::render_create(table, def, server, dialect),
        SqlKind::Postgres => pg::render_create(table, def, server, dialect),
    }
}

/// A column's definition as it appears in `CREATE TABLE`, name first.
///
/// **Precondition** (all `*_sql` fragments): the element passed [`crate::validate`] or
/// is a server-derived locked element; expression fields are spliced verbatim, so the
/// fragment of an unchecked element must never be sent.
pub fn column_sql(
    c: &ColumnDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::column_sql(c, server, dialect),
        SqlKind::Postgres => pg::column_sql(c, server, dialect),
    }
}

/// The primary key as it appears in `CREATE TABLE`. Precondition as [`column_sql`].
pub fn primary_key_sql(
    k: &KeyDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::primary_key_sql(k, server, dialect),
        SqlKind::Postgres => pg::primary_key_sql(k, server, dialect),
    }
}

/// An index as it appears in `CREATE TABLE` (MySQL). PostgreSQL: a constraint index
/// (`IndexDef::constraint`) as it appears in `CREATE TABLE`
/// (`CONSTRAINT n UNIQUE (…) [INCLUDE (…)]`); a plain index as the tail of its
/// `pg_get_indexdef` after `ON <table> ` (`USING method (…) [INCLUDE (…)]`), since the
/// fragment does not know the table. Precondition as [`column_sql`].
pub fn index_sql(
    i: &IndexDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::index_sql(i, server, dialect),
        SqlKind::Postgres => pg::index_sql(i, server, dialect),
    }
}

/// A foreign key constraint as it appears in `CREATE TABLE`. Precondition as
/// [`column_sql`].
pub fn fk_sql(f: &FkDef, server: &ServerInfo, dialect: Dialect) -> Result<String, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::fk_sql(f, server, dialect),
        SqlKind::Postgres => pg::fk_sql(f, server, dialect),
    }
}

/// A check constraint as it appears in `CREATE TABLE`. Precondition as [`column_sql`].
pub fn check_sql(
    c: &CheckDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind.as_sql() {
        SqlKind::MySql => mysql::check_sql(c, server, dialect),
        SqlKind::Postgres => pg::check_sql(c, server, dialect),
    }
}
