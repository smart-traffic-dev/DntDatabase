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
use dnt_core::server::{DbKind, ServerInfo};
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

/// The statements that turn `old` (the stored table at `table`) into the draft that
/// [`crate::diff`] compared it with. `changes` is that diff; an empty diff renders no
/// statement. A [`Change::CreateTable`] diff goes through [`render_create`] instead.
///
/// **Precondition:** the draft passed [`crate::validate`]; expression fields (defaults,
/// checks, index expressions, custom types) are spliced verbatim.
pub fn render(
    table: &TableRef,
    old: &TableDef,
    changes: &[Change],
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<Rendered, RenderError> {
    match server.kind {
        DbKind::MySql => mysql::render(table, old, changes, server, dialect),
        DbKind::Postgres => pg::render(table, old, changes, server, dialect),
    }
}

/// The statements that create `def` as a new table in `table`'s database (and schema),
/// named `def.name`.
///
/// **Precondition:** `def` passed [`crate::validate`] (expression fields are spliced
/// verbatim).
pub fn render_create(
    table: &TableRef,
    def: &TableDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<Rendered, RenderError> {
    match server.kind {
        DbKind::MySql => mysql::render_create(table, def, server, dialect),
        DbKind::Postgres => pg::render_create(table, def, server, dialect),
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
    match server.kind {
        DbKind::MySql => mysql::column_sql(c, server, dialect),
        DbKind::Postgres => pg::column_sql(c, server, dialect),
    }
}

/// The primary key as it appears in `CREATE TABLE`. Precondition as [`column_sql`].
pub fn primary_key_sql(
    k: &KeyDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind {
        DbKind::MySql => mysql::primary_key_sql(k, server, dialect),
        DbKind::Postgres => pg::primary_key_sql(k, server, dialect),
    }
}

/// An index as it appears in `CREATE TABLE` (MySQL) or its own statement (PostgreSQL).
/// Precondition as [`column_sql`].
pub fn index_sql(
    i: &IndexDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind {
        DbKind::MySql => mysql::index_sql(i, server, dialect),
        DbKind::Postgres => pg::index_sql(i, server, dialect),
    }
}

/// A foreign key constraint as it appears in `CREATE TABLE`. Precondition as
/// [`column_sql`].
pub fn fk_sql(f: &FkDef, server: &ServerInfo, dialect: Dialect) -> Result<String, RenderError> {
    match server.kind {
        DbKind::MySql => mysql::fk_sql(f, server, dialect),
        DbKind::Postgres => pg::fk_sql(f, server, dialect),
    }
}

/// A check constraint as it appears in `CREATE TABLE`. Precondition as [`column_sql`].
pub fn check_sql(
    c: &CheckDef,
    server: &ServerInfo,
    dialect: Dialect,
) -> Result<String, RenderError> {
    match server.kind {
        DbKind::MySql => mysql::check_sql(c, server, dialect),
        DbKind::Postgres => pg::check_sql(c, server, dialect),
    }
}
