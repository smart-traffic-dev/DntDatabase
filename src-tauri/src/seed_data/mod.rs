//! The data seeder (data seeder spec §5): fills one table with generated rows through a
//! background job.
//!
//! - `target`: the table, its columns and the generators inferred for them.
//! - `keys`: the keys of the parent tables of foreign keys (and a short-lived cache).
//! - `plan`: the preview, single-use plans and the start.
//! - `run`: the job.
//!
//! Safety rules (as the import, `import_data`): values become SQL only through
//! `dnt_core::sql::build`; identifiers only through `quote_ident` / `TableRef::relation`;
//! every write is judged by the query guard right before it is sent (`Writer`); read-only
//! connections are refused at prepare, at start and before every write; no write is retried.
//!
//! Statements that do NOT pass `plan` + guard: the target read (`table_detail`, `SELECT
//! <cols> FROM <t> WHERE 1 = 0`), the key reads `SELECT DISTINCT <ref cols> FROM <parent>
//! WHERE … IS NOT NULL ORDER BY … LIMIT 10000` (built from quoted identifiers only; reads
//! nothing but the parent's key columns) and the cleanup `ROLLBACK`.
pub(crate) mod keys;
mod plan;
mod run;
mod target;
#[cfg(test)]
mod tests;

pub use keys::SeedKeyCache;
pub use plan::SeedPlanStore;

use crate::import_data::{ConfirmLevel, PreStep};
use crate::session_manager::config_error;
use dnt_core::config::EnvTag;
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::value::Value;
use dnt_transfer::convert::TargetColumn;
use dnt_transfer::seed::engine::{FkKeys, SeedColumnSpec};
use dnt_transfer::seed::{ColumnIssue, ColumnPlan};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// Most rows one job generates.
pub(crate) const MAX_ROWS: u32 = 1_000_000;
/// Rows per `INSERT` by default and at most.
pub(crate) const DEFAULT_BATCH_ROWS: u32 = 1000;
pub(crate) const MAX_BATCH_ROWS: u32 = 100_000;
/// Rows the preview shows.
pub(crate) const SAMPLE_ROWS: u32 = 10;

/// What happens to the rows already in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExistingRows {
    Keep,
    /// `DELETE FROM t` first.
    Delete,
    /// `TRUNCATE t` first.
    Truncate,
}

/// What the user asked to generate (also the stored parameters of "Run again").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SeedRequest {
    pub connection_id: Uuid,
    pub table: ObjectRef,
    /// 1–1,000,000.
    pub rows: u32,
    /// Rows per `INSERT` (0 = 1,000; at most 100,000; 1 MiB of SQL at most).
    pub batch_rows: u32,
    pub existing: ExistingRows,
    pub seed: u32,
    /// Every column of the table, in table order.
    pub columns: Vec<ColumnPlan>,
}

/// A foreign key as the dialog shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SeedForeignKey {
    pub constraint: String,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub columns: Vec<String>,
    pub ref_columns: Vec<String>,
}

/// One row of the dialog's columns table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SeedColumn {
    pub column: TargetColumn,
    pub primary: bool,
    pub unique_index: bool,
    pub foreign_key: Option<SeedForeignKey>,
    /// Values of a CHECK `IN` list on this column.
    pub check_values: Vec<String>,
    /// A generated column: only `Skip`.
    pub locked: bool,
    /// The inferred plan.
    pub plan: ColumnPlan,
}

/// The table as the dialog opens it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SeedTarget {
    pub columns: Vec<SeedColumn>,
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub server: ServerInfo,
    /// The whole job is one transaction.
    pub transactional: bool,
    /// A random seed to start with.
    pub seed: u32,
}

/// Keys read for one foreign key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SeedKeyCount {
    pub constraint: String,
    pub count: u32,
}

/// The live preview: the first rows exactly as the job generates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SeedSample {
    /// The columns of the `INSERT`.
    pub columns: Vec<String>,
    /// At most 10 rows (display values).
    pub rows: Vec<Vec<Value>>,
    /// Those rows as one `INSERT`, exactly as the job builds its first batch (generated
    /// values only, so not redacted).
    pub sql: Option<String>,
    pub issues: Vec<ColumnIssue>,
    pub fk_keys: Vec<SeedKeyCount>,
}

/// The review step: what `seed_start` will run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SeedPreview {
    pub plan_id: Uuid,
    pub table: ObjectRef,
    pub rows: u32,
    /// `INSERT` statements the job sends (estimated by rows per batch).
    pub batches: u32,
    pub transactional: bool,
    pub pre_step: Option<PreStep>,
    pub insert_head: String,
    pub insert_tail: String,
    pub sample: Option<String>,
    pub warnings: Vec<String>,
    pub confirm: ConfirmLevel,
    /// Production with an irreversible pre-step: the name typed for it (the table's).
    pub irreversible_name: Option<String>,
    pub expires_in_seconds: u32,
}

/// The request's own checks (no server needed); the batch size is normalised.
pub(crate) fn normalise(mut req: SeedRequest, kind: DbKind) -> Result<SeedRequest, DbError> {
    if req.table.kind != ObjectKind::Table
        || req.table.database.is_empty()
        || req.table.name.is_empty()
    {
        return Err(config_error("table", "Choose a table to fill."));
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
    if req.rows == 0 || req.rows > MAX_ROWS {
        return Err(config_error("rows", "Choose between 1 and 1,000,000 rows."));
    }
    req.batch_rows = match req.batch_rows {
        0 => DEFAULT_BATCH_ROWS,
        n => n.min(MAX_BATCH_ROWS),
    };
    Ok(req)
}

/// The table changed since the dialog read it.
pub(crate) fn table_changed() -> DbError {
    DbError::PlanStale {
        reason: "The table changed; check the generators and preview again.".into(),
    }
}

/// The engine's columns: the request's plans over the table's columns (same names, same
/// order) with the keys read for their foreign keys.
pub(crate) fn specs(
    req: &SeedRequest,
    columns: &[TargetColumn],
    keys: &HashMap<String, Arc<FkKeys>>,
) -> Result<Vec<SeedColumnSpec>, DbError> {
    if req.columns.len() != columns.len()
        || req
            .columns
            .iter()
            .zip(columns)
            .any(|(p, c)| p.name != c.name)
    {
        return Err(table_changed());
    }
    Ok(req
        .columns
        .iter()
        .zip(columns)
        .map(|(plan, column)| SeedColumnSpec {
            fk: match &plan.generator {
                dnt_transfer::seed::Generator::ForeignKey { constraint } => {
                    keys.get(constraint).cloned()
                }
                _ => None,
            },
            column: column.clone(),
            plan: plan.clone(),
        })
        .collect())
}
