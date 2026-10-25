//! Data import (spec §6.5–§6.6): the target read (R13), the dry run (R18), single-use
//! import plans (R19–R21) and the import job (R15–R17).
//!
//! - `target`: the target's columns, keys and transaction policy, read on a dedicated
//!   session (`table_detail` + the driver's types of `SELECT <cols> FROM <t> WHERE 1 = 0`).
//! - `plan`: `import_prepare` (preview + plan) and `import_start` (re-checks, then a job).
//! - `run`: the executor shared by the dry run and the import job.
//!
//! Safety rules every path keeps:
//! - Values become SQL only through `dnt_core::sql::build` (the batch builder); identifiers
//!   only through `quote_ident` / `TableRef::relation`.
//! - Every write is judged by the query guard (`dnt_query::plan` + `guard_with`) right
//!   before it is sent; `Block` stops the job before sending. The plan's templates are
//!   judged at preview and again at start.
//! - Read-only connections: real imports and server-check dry runs are blocked here,
//!   file-only dry runs read only.
//! - The dry run never writes the target: its inserts go to a session temporary table
//!   created `LIKE` the target, rolled back and dropped at the end.
//! - No write is retried; an unanswered `COMMIT` or a lost in-flight batch outside a
//!   transaction is `outcome_unknown`, kept in the job and its History entry.
//!
//! Known limits (documented, R21): the file is hashed at start and again after the last
//! record was read (before `COMMIT`); a change in between that reverts before the end goes
//! unnoticed. Outside a transaction the batches written before a mismatch stay.
mod plan;
mod run;
mod target;
#[cfg(test)]
mod tests;

pub use plan::ImportPlanStore;

use crate::session_manager::config_error;
use dnt_core::config::EnvTag;
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::sql::build::{
    insert_head, insert_sql_with, insert_tail, BuildError, ColumnTarget, InsertOptions, OnConflict,
    TableRef,
};
use dnt_core::sql::Dialect;
use dnt_core::value::FullValue;
use dnt_query::Atomicity;
use dnt_transfer::batch::{BatchBuilder, DEFAULT_BATCH_BYTES, DEFAULT_BATCH_ROWS};
use dnt_transfer::convert::{convert, CellError, ConvertOptions, TargetColumn};
use dnt_transfer::mapping::{insert_options, ErrorPolicy, ImportMode, KeyChoice, Mapping};
use dnt_transfer::read::{Cell, Record, SourceOptions};
use dnt_transfer::FileFacts;
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

/// The savepoint taken before each batch in skip mode inside a transaction (R16).
pub(crate) const SAVEPOINT: &str = "dnt_import";
/// Prefix of the dry run's temporary table (session-private, R18).
pub(crate) const TEMP_PREFIX: &str = "dnt_import_check_";
/// Largest rows per batch a request may ask for.
pub(crate) const MAX_BATCH_ROWS: u32 = 100_000;
/// Smallest and largest statement size per batch a request may ask for.
pub(crate) const MIN_BATCH_BYTES: u32 = 1024;
pub(crate) const MAX_BATCH_BYTES: u32 = 64 * 1024 * 1024;
/// Rows the preview's sample statement holds at most.
pub(crate) const SAMPLE_ROWS: u32 = 10;

/// What the user asked to import (also the stored parameters of "Run again").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ImportRequest {
    pub connection_id: Uuid,
    pub table: ObjectRef,
    /// A path admitted by `import_pick_file`.
    pub path: String,
    pub source: SourceOptions,
    pub mapping: Vec<Mapping>,
    pub mode: ImportMode,
    pub errors: ErrorPolicy,
    pub convert: ConvertOptions,
    /// Rows per `INSERT` (0 = 1,000).
    pub batch_rows: u32,
    /// Bytes of SQL per `INSERT` (0 = 1 MiB).
    pub batch_bytes: u32,
    /// PostgreSQL `OVERRIDING SYSTEM VALUE` (write identity `ALWAYS` columns).
    pub override_identity: bool,
}

/// A dry run (R18): every record is read and converted; with `server_check` the rows are
/// also inserted into a temporary copy of the table (not on read-only connections).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DryRunRequest {
    pub request: ImportRequest,
    pub server_check: bool,
    /// The user confirmed the server check's statements where the guard asks (Production).
    #[serde(default)]
    pub confirmed: bool,
}

/// What runs before the inserts of a replace-all import (R17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct PreStep {
    pub sql: String,
    /// Sent and committed on its own before the inserts: it cannot be rolled back.
    pub irreversible: bool,
    pub warning: Option<String>,
}

/// How the user confirms the start (R20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfirmLevel {
    /// The review step's Start button.
    None,
    /// Staging: "I understand".
    Click,
    /// Production: type the table name.
    TypeName { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ImportConfirm {
    pub confirmed: bool,
    /// The typed table name (Production).
    pub name: Option<String>,
    /// The irreversible pre-step was acknowledged.
    pub irreversible_ack: bool,
}

/// The target as the wizard shows it (R13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ImportTarget {
    pub columns: Vec<TargetColumn>,
    pub keys: Vec<KeyChoice>,
    pub atomicity: Atomicity,
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub server: ServerInfo,
}

/// The review step: what `import_start` will run (R19). Exported to TypeScript as
/// `DataImportPreview` (the connections import already has an `ImportPreview`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename = "DataImportPreview")]
pub struct ImportPreview {
    pub plan_id: Uuid,
    pub file: FileFacts,
    pub table: ObjectRef,
    /// The whole import is one transaction (R15).
    pub transactional: bool,
    pub pre_step: Option<PreStep>,
    pub insert_head: String,
    pub insert_tail: String,
    /// The first rows of the file as one `INSERT` (at most 10 rows).
    pub sample: Option<String>,
    pub warnings: Vec<String>,
    pub confirm: ConfirmLevel,
    /// Production with an irreversible pre-step: the name typed for it (the table's).
    pub irreversible_name: Option<String>,
    pub expires_in_seconds: u32,
}

/// The relation `obj` names on `kind`.
pub(crate) fn table_ref(kind: DbKind, obj: &ObjectRef) -> TableRef {
    TableRef {
        kind,
        database: obj.database.clone(),
        schema: obj.schema.clone(),
        name: obj.name.clone(),
    }
}

/// The session database of an import: PostgreSQL the table's database, MySQL none (names
/// are qualified by the database).
pub(crate) fn session_db(kind: DbKind, obj: &ObjectRef) -> Option<String> {
    (kind == DbKind::Postgres).then(|| obj.database.clone())
}

/// The request's own checks (no server needed); batch sizes are normalised.
pub(crate) fn normalise(mut req: ImportRequest, kind: DbKind) -> Result<ImportRequest, DbError> {
    if req.table.kind != ObjectKind::Table {
        return Err(config_error("table", "Choose a table to import into."));
    }
    if req.table.database.is_empty() || req.table.name.is_empty() {
        return Err(config_error("table", "Choose a table to import into."));
    }
    match (kind, &req.table.schema) {
        (DbKind::MySql, Some(_)) => {
            return Err(config_error(
                "table",
                "MySQL has no schemas inside a database.",
            ))
        }
        (DbKind::Postgres, None) => {
            return Err(config_error("table", "A schema is required on PostgreSQL."))
        }
        _ => {}
    }
    req.batch_rows = match req.batch_rows {
        0 => DEFAULT_BATCH_ROWS,
        n => n.min(MAX_BATCH_ROWS),
    };
    req.batch_bytes = match req.batch_bytes {
        0 => DEFAULT_BATCH_BYTES as u32,
        n => n.clamp(MIN_BATCH_BYTES, MAX_BATCH_BYTES),
    };
    if kind == DbKind::MySql {
        // `OVERRIDING SYSTEM VALUE` is PostgreSQL only.
        req.override_identity = false;
    }
    Ok(req)
}

/// The most rows an error policy (or a dry run) tolerates.
pub(crate) fn max_errors(p: &ErrorPolicy) -> u32 {
    match p {
        ErrorPolicy::Stop => ErrorPolicy::DEFAULT_MAX_ERRORS,
        ErrorPolicy::Skip { max_errors } => *max_errors,
    }
}

/// The mode as History and the job log name it.
pub(crate) fn mode_label(mode: &ImportMode) -> &'static str {
    match mode {
        ImportMode::Append => "append",
        ImportMode::Replace { truncate: false } => "replace all (delete)",
        ImportMode::Replace { truncate: true } => "replace all (truncate)",
        ImportMode::Upsert { .. } => "upsert",
    }
}

/// How records become rows of one `INSERT` shape: the mapped target columns (in table
/// order), the source column each reads, the builder options and the relation.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub(crate) table: TableRef,
    pub(crate) cols: Vec<TargetColumn>,
    targets: Vec<ColumnTarget>,
    sources: Vec<u32>,
    pub(crate) opts: InsertOptions,
    pub(crate) dialect: Dialect,
    pub(crate) kind: DbKind,
    convert: ConvertOptions,
}

impl Layout {
    /// The layout of `req` over `targets` (already checked by `check_mapping`).
    pub(crate) fn new(
        req: &ImportRequest,
        targets: &[TargetColumn],
        kind: DbKind,
        dialect: Dialect,
    ) -> Result<Layout, DbError> {
        let mut cols = Vec::new();
        let mut sources = Vec::new();
        for t in targets {
            if let Some(src) = req
                .mapping
                .iter()
                .find(|m| m.target == t.name)
                .and_then(|m| m.source)
            {
                cols.push(t.clone());
                sources.push(src);
            }
        }
        if cols.is_empty() {
            return Err(config_error("mapping", "Map at least one column."));
        }
        let opts = insert_options(&req.mode, &cols, req.override_identity);
        let layout = Layout {
            table: table_ref(kind, &req.table),
            targets: cols.iter().map(TargetColumn::column_target).collect(),
            cols,
            sources,
            opts,
            dialect,
            kind,
            convert: req.convert.clone(),
        };
        // The options must build (e.g. an upsert key that is inserted).
        layout.head().map_err(build_error)?;
        layout.tail().map_err(build_error)?;
        Ok(layout)
    }

    /// The same rows into another relation with plain append semantics (the dry run's
    /// temporary table, R18).
    pub(crate) fn appending_to(&self, table: TableRef) -> Layout {
        Layout {
            table,
            opts: InsertOptions {
                overriding_system_value: self.opts.overriding_system_value,
                on_conflict: OnConflict::Error,
            },
            ..self.clone()
        }
    }

    pub(crate) fn head(&self) -> Result<String, BuildError> {
        insert_head(&self.table, &self.targets, &self.opts, self.dialect)
    }

    pub(crate) fn tail(&self) -> Result<String, BuildError> {
        insert_tail(&self.targets, &self.opts, self.dialect)
    }

    /// `head + (NULL, …) + tail`: the statement shape the guard judges for the template.
    pub(crate) fn template_statement(&self) -> Result<String, BuildError> {
        let nulls = vec!["NULL"; self.targets.len()].join(", ");
        Ok(format!("{}({nulls}){}", self.head()?, self.tail()?))
    }

    pub(crate) fn builder(&self, max_rows: u32, max_bytes: u32) -> Result<BatchBuilder, DbError> {
        BatchBuilder::new(
            self.table.clone(),
            self.targets.clone(),
            self.opts.clone(),
            self.dialect,
            max_rows,
            max_bytes as usize,
        )
        .map_err(build_error)
    }

    /// One row on its own (row isolation, R16).
    pub(crate) fn single(&self, row: &[FullValue]) -> Result<String, BuildError> {
        insert_sql_with(
            &self.table,
            &self.targets,
            &[row.to_vec()],
            &self.opts,
            self.dialect,
        )
    }

    /// The record's values for the mapped columns. A source column missing from the
    /// record reads as NULL.
    pub(crate) fn convert(&self, record: &Record) -> Result<Vec<FullValue>, CellError> {
        self.cols
            .iter()
            .zip(&self.sources)
            .map(|(col, src)| {
                let cell = record.cells.get(*src as usize).unwrap_or(&Cell::Null);
                convert(cell, col, &self.convert, self.kind)
            })
            .collect()
    }
}

/// A builder failure that is the plan's (not a row's) fault.
pub(crate) fn build_error(e: BuildError) -> DbError {
    config_error("mapping", &format!("The import cannot be built: {e}."))
}

/// The verdict of one statement under `ctx` (`Block` when it is not exactly one
/// statement for the server).
pub(crate) fn verdict(sql: &str, dialect: Dialect, ctx: dnt_query::GuardCtx) -> dnt_query::Verdict {
    let mut judged = dnt_query::plan(sql, dnt_query::Target::All, dialect);
    dnt_query::guard_with(&mut judged, ctx);
    if judged.stmts.len() != 1 {
        return dnt_query::Verdict::Block;
    }
    judged.stmts[0].verdict
}

/// The guard inputs of a connection (unbounded writes always confirmed: the job
/// confirmation covers them).
pub(crate) fn guard_ctx(env: Option<EnvTag>, read_only: bool) -> dnt_query::GuardCtx {
    dnt_query::GuardCtx {
        env,
        read_only,
        analyze: false,
        confirm_unbounded: true,
    }
}

/// Judges every statement: any `Block` → `Blocked`; any `Confirm` without `confirmed` →
/// `ConfirmationRequired`.
pub(crate) fn judge_all(
    statements: &[String],
    dialect: Dialect,
    ctx: dnt_query::GuardCtx,
    confirmed: bool,
) -> Result<(), DbError> {
    let mut blocked = 0u32;
    let mut confirm = 0u32;
    for s in statements {
        match verdict(s, dialect, ctx) {
            dnt_query::Verdict::Block => blocked += 1,
            dnt_query::Verdict::Confirm => confirm += 1,
            dnt_query::Verdict::Allow => {}
        }
    }
    if blocked > 0 {
        Err(DbError::Blocked { count: blocked })
    } else if confirm > 0 && !confirmed {
        Err(DbError::ConfirmationRequired { count: confirm })
    } else {
        Ok(())
    }
}
