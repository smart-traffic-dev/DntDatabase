use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
    Trigger,
    Event,
    Sequence,
    Type,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct ObjectRef {
    pub database: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
}

// Introspection model. Drivers must fill every `Vec` in a stable order
// (by ordinal, otherwise by name) so snapshots do not flap.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DatabaseInfo {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SchemaInfo {
    pub name: String,
}

/// Where objects are listed: a database, plus a schema on PostgreSQL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Scope {
    pub database: String,
    pub schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ObjectSummary {
    pub name: String,
    pub kind: ObjectKind,
    pub comment: Option<String>,
    /// Exported as a JS number: estimates stay far below 2^53.
    #[specta(type = Option<f64>)]
    pub row_estimate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ColumnDetail {
    pub name: String,
    pub ordinal: u32,
    pub db_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub auto_increment: bool,
    pub identity: Option<String>,
    pub generated: Option<String>,
    pub invisible: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct IndexDetail {
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    pub columns: Vec<String>,
    pub expression: Option<String>,
    pub method: Option<String>,
    /// Either a full `CREATE INDEX …` statement or, for an index backing a table
    /// constraint, the constraint fragment (e.g. `PRIMARY KEY (id)`, `UNIQUE (email)`).
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ForeignKeyDetail {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: String,
    pub on_delete: String,
    /// Verbatim server constraint definition when the backend provides one
    /// (e.g. `FOREIGN KEY (a) REFERENCES t(id) DEFERRABLE INITIALLY DEFERRED`).
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct CheckDetail {
    pub name: String,
    pub expression: String,
    /// Verbatim server constraint definition when the backend provides one
    /// (e.g. `CHECK ((total >= 0)) NOT VALID`).
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TriggerDetail {
    pub name: String,
    pub timing: String,
    pub event: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TableOptions {
    pub engine: Option<String>,
    pub collation: Option<String>,
    pub tablespace: Option<String>,
    pub partitioned_by: Option<String>,
    pub system_versioned: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TableDetail {
    pub object: ObjectRef,
    pub columns: Vec<ColumnDetail>,
    pub primary_key: Option<IndexDetail>,
    pub indexes: Vec<IndexDetail>,
    pub foreign_keys: Vec<ForeignKeyDetail>,
    pub checks: Vec<CheckDetail>,
    pub triggers: Vec<TriggerDetail>,
    pub options: TableOptions,
    /// Exported as JS numbers: estimates and sizes stay far below 2^53.
    #[specta(type = Option<f64>)]
    pub row_estimate: Option<u64>,
    #[specta(type = Option<f64>)]
    pub size_bytes: Option<u64>,
    pub comment: Option<String>,
}
