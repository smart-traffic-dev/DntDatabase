//! The table designer's draft: one table as the user edits it (spec §4.2).
//!
//! Every element that exists on the server carries `original: Some(<server name>)`;
//! a new element has `None`. Renames are recognised through `original`, never guessed.
//! A `locked` element cannot be edited by the designer and never appears in a diff.

use dnt_core::meta::IdentityKind;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TableDef {
    /// `None` for a new table.
    pub original_name: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: Option<KeyDef>,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<FkDef>,
    pub checks: Vec<CheckDef>,
    pub options: TableOptionsDef,
    pub comment: Option<String>,
    /// Set when the whole table opens read-only (partitioned, system-versioned,
    /// a PostgreSQL partition or inheriting table).
    pub read_only_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ColumnDef {
    pub original: Option<String>,
    pub name: String,
    pub data_type: TypeSpec,
    pub nullable: bool,
    pub default: Option<DefaultSpec>,
    /// MySQL/MariaDB only.
    pub auto_increment: bool,
    /// PostgreSQL only.
    pub identity: Option<IdentityKind>,
    /// MySQL/MariaDB `ON UPDATE` expression (e.g. `CURRENT_TIMESTAMP(6)`).
    pub on_update: Option<String>,
    /// `None` when the column uses the table's character set.
    pub charset: Option<String>,
    /// `None` when the column uses the table's (or the type's default) collation.
    pub collation: Option<String>,
    pub comment: Option<String>,
    /// PostgreSQL `USING` expression for a type change, typed by the user.
    pub using: Option<String>,
    /// Generation expression of a generated column (generated columns are locked).
    pub generated: Option<String>,
    pub locked: bool,
}

/// A column type: a catalog type with its parameters, or text the designer does not
/// model (arrays, domains, user-defined types, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypeSpec {
    Known {
        base: String,
        length: Option<u32>,
        precision: Option<u32>,
        scale: Option<u32>,
        unsigned: bool,
        values: Vec<String>,
    },
    Custom {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DefaultSpec {
    /// A literal value, unquoted (rendered through the SQL literal builder).
    Value { text: String },
    /// A SQL expression, rendered verbatim after `check_expression`.
    Expression { text: String },
}

/// The primary key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct KeyDef {
    pub original: Option<String>,
    /// Constraint name (PostgreSQL); MySQL always names it `PRIMARY`.
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct IndexDef {
    pub original: Option<String>,
    pub name: String,
    pub unique: bool,
    pub parts: Vec<IndexPartDef>,
    pub method: Option<String>,
    /// PostgreSQL `INCLUDE (…)` columns.
    pub include: Vec<String>,
    /// MySQL 8 `INVISIBLE` is `false`.
    pub visible: bool,
    pub comment: Option<String>,
    /// PostgreSQL: the index backs a table constraint (`UNIQUE (…)` / `PRIMARY KEY (…)`)
    /// rather than being a plain `CREATE INDEX`.
    pub constraint: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct IndexPartDef {
    pub column: Option<String>,
    pub expression: Option<String>,
    /// MySQL prefix length.
    pub prefix: Option<u32>,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FkDef {
    pub original: Option<String>,
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
    /// PostgreSQL `MATCH FULL` / `MATCH PARTIAL`; `None` = the default (`SIMPLE`).
    pub match_type: Option<String>,
    pub deferrable: bool,
    pub initially_deferred: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct CheckDef {
    pub original: Option<String>,
    pub name: String,
    pub expression: String,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Type)]
pub struct TableOptionsDef {
    pub engine: Option<String>,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub row_format: Option<String>,
    pub locked: bool,
}

/// How a type uses a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum ParamUse {
    None,
    Optional,
    Required,
}

/// One entry of [`crate::type_catalog`]: which fields the designer shows for a type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TypeInfo {
    pub base: String,
    pub length: ParamUse,
    pub precision: ParamUse,
    pub scale: ParamUse,
    pub unsigned: bool,
    /// Takes a list of values (`enum` / `set`).
    pub values: bool,
    pub collation: bool,
}

/// A problem in the draft, shown at the element (and field) that causes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DraftError {
    pub element: ElementRef,
    pub field: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum ElementRef {
    Table,
    Column(String),
    PrimaryKey,
    Index(String),
    ForeignKey(String),
    Check(String),
    Options,
}
