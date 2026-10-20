//! Edit plans of the data view (spec §4.2b–d, §5.2): `data_preview` builds an immutable
//! plan and stores it; `data_apply` runs exactly that plan, once.
//!
//! # Preview
//!
//! The plan is built from the table's detail as the server has it now (not the tree's
//! cache), the result-set types of its columns (a `LIMIT 0` read) and the `Dialect` of a
//! data session, with the guard context of the connection as stored. It is stored with
//! what it was made for: the connection's runtime and SSH tunnel generation, the table,
//! the env tag and read-only flag. A plan lives [`crate::query_sessions::PLAN_TTL`]; a
//! newer preview of the same table on the same connection replaces it.
//!
//! # Apply
//!
//! The plan is taken out of the store first: it runs at most once, whatever happens.
//! Before anything is sent it is checked again, and a mismatch is
//! [`DbError::PlanStale`]: the same connection runtime (not closed or reopened) and SSH
//! tunnel, the guard verdict with the config as stored now (`Blocked`,
//! `ConfirmationRequired`), the env tag and read-only flag, the table's fingerprint, and
//! the `Dialect` of the data session that will run it.
//!
//! The session is taken from the data pool (never retried on another session: a write
//! is never sent twice) and given back only when the transaction ended cleanly.
//!
//! - `atomic == Yes`: `BEGIN`; the session checks (PostgreSQL
//!   `standard_conforming_strings` on, MySQL `character_set_client` utf8mb4, the dialect
//!   again); every statement, each expecting its row count (a mismatch rolls back and
//!   marks the statement a conflict); `COMMIT`. Any failure before `COMMIT` was sent
//!   leaves nothing applied ([`ApplyOutcome::RolledBack`]: rolled back, or the server
//!   rolls back when the connection is gone); a `COMMIT` without an answer is
//!   [`ApplyOutcome::Unknown`].
//! - otherwise the plan has one statement, run without a transaction. Right before it
//!   the session is pinged and checked again; a silent reconnect during the checks is
//!   `PlanStale` (nothing sent), one seen only after the write, a lost connection or
//!   timeout while it runs, or a row count that is neither 0 nor the one expected is
//!   `Unknown { at: 0 }`.
//!
//! A session goes back to the pool only after a commit or a conflict's clean rollback:
//! after a failed statement a cancel request may still be on its way to it.
//!
//! # Closing the connection mid-apply
//!
//! A running apply is registered with the connection's data sessions. Closing the
//! connection waits for it (at most `SessionManager::apply_close_wait`, by default
//! `SLOT_CLOSE_WAIT`), then cancels it: the statement in flight is abandoned and the
//! apply reports where it was (spec §4.2d) instead of an error. In a transaction before
//! `COMMIT` was sent: [`ApplyOutcome::RolledBack`] (the session is closed, so the server
//! rolls back); in `COMMIT`, or in a statement without a transaction:
//! [`ApplyOutcome::Unknown`]; during the checks of a plan without a transaction (nothing
//! sent): `PartiallyApplied { applied: 0 }`. The session is closed, never pooled.
//!
//! MySQL counts changed rows, not matched ones: an UPDATE that reports 0 rows is checked
//! with its `recheck` (a locking `SELECT … FOR UPDATE`, which sees the latest committed
//! row as the UPDATE did) before it is called a conflict. An INSERT's row is read back
//! (PostgreSQL `RETURNING`, MySQL its `read_back` SELECT) into [`ApplyReport::inserted`].
//!
//! # Complete cell values (spec §5.3)
//!
//! A page cuts long text and keeps a preview of binary data. `data_cell_fetch` reads
//! one cell whole by the row's key ([`dnt_query::edit::cell_select`], the table's key
//! with complete values) in [`CellMode::Full`], at most [`MAX_FETCH_BYTES`]; a larger
//! value is an error, never cut. It runs on a data session checked like an apply's
//! (PostgreSQL `standard_conforming_strings` on, MySQL `character_set_client` utf8mb4:
//! the key's literals are app-built), and a silent reconnect between the check and the
//! read discards the value. `data_cell_load_file` makes the same kind of value from a
//! file. Either stores the value under a token that lives [`LOADED_TTL`] after its last
//! successful use ([`LoadedStore`], at most [`MAX_LOADED_BYTES`] in all, the least
//! recently used dropped first), bound
//! to the connection, table and column it was read for: `data_preview` takes it as a
//! [`CellInput::Loaded`](dnt_query::edit::CellInput) value or, through
//! [`RowRef::loaded`](dnt_query::edit::RowRef), as the complete original of a cut cell
//! in the row guard's snapshot. A token used for another connection, table or column,
//! or after it expired, is a `config` error on that column.

use crate::commands::data::check_object;
use crate::commands::settings::saved_confirm_unbounded;
use crate::data_sessions::{BoundedPage, FirstCell, DATA_CANCEL_GRACE};
use crate::session_manager::{config_error, is_lost, lock, ConnectionRuntime, SessionManager};
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{CellMode, ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectRef, TableDetail};
use dnt_core::sql::ident::{qualified_name, quote_ident};
use dnt_core::sql::{Dialect, StatementKind, TxControl};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_query::edit::{
    base64_encode, build_edit_plan, cell_select, display_sql, edit_verdict, fingerprint, Atomicity,
    EditError, EditPlan, EditSet, PlannedEdit, Protection,
};
use dnt_query::{redact, GuardCtx, Verdict};
use futures_util::future::{select, Either};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::pin::pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Most edits one preview takes.
pub const MAX_EDITS: usize = 10_000;

/// What `data_preview` returns: the stored plan's id and the statements `data_apply`
/// will send.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
pub struct EditPreview {
    pub plan_id: Uuid,
    /// The statements, for display: exactly what is sent, except that string literals
    /// over 4 KiB are shortened to their start (`X'0a1b…'`, `'abc…'`) followed by an SQL
    /// comment with their size in bytes or chars; the stored plan keeps and runs them in
    /// full.
    pub statements: Vec<String>,
    pub verdict: Verdict,
    pub atomic: Atomicity,
    /// The conflict check of each statement, in order.
    pub protections: Vec<Protection>,
    /// Why the verdict is `block` (UI copy).
    pub reason: Option<String>,
}

/// What became of an applied plan (spec §4.2d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ApplyOutcome {
    /// Every statement was applied.
    Committed,
    /// Nothing was applied: the transaction was rolled back (or the server rolls it back
    /// because the connection is gone).
    RolledBack,
    /// No transaction: the first `applied` statements are applied, the next one was not.
    PartiallyApplied { applied: u32 },
    /// The statement at position `at` of the plan (`statements.len()`: the `COMMIT`) got
    /// no answer: it may or may not have been applied.
    Unknown { at: u32 },
}

/// One statement of an applied plan.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct StmtApply {
    /// The edit's index in the `EditSet`.
    pub index: u32,
    /// Rows the statement matched.
    pub affected: Option<f64>,
    pub error: Option<AppError>,
    /// It matched no row (or not the rows expected): the row changed or went away since
    /// it was loaded.
    pub conflict: bool,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ApplyReport {
    pub outcome: ApplyOutcome,
    /// The statements that were sent, in order (those after a failure were not).
    pub statements: Vec<StmtApply>,
    /// One row per applied INSERT, in order: every column of the table (in its column
    /// order) as the server stored it; empty when it could not be read back. Empty when
    /// nothing was applied.
    pub inserted: Vec<Vec<Value>>,
    /// The error that ended the apply outside a statement (the `COMMIT`).
    pub error: Option<AppError>,
}

/// A stored plan and what it was made for.
struct StoredEdit {
    conn: Uuid,
    /// The runtime the plan was made on (a reopened connection is another one).
    runtime: Weak<ConnectionRuntime>,
    tunnel_gen: u64,
    object: ObjectRef,
    env: Option<EnvTag>,
    read_only: bool,
    plan: EditPlan,
    at: Instant,
}

impl StoredEdit {
    fn same_table(&self, conn: Uuid, o: &ObjectRef) -> bool {
        let t = &self.object;
        self.conn == conn && (&t.database, &t.schema, &t.name) == (&o.database, &o.schema, &o.name)
    }
}

/// Edit plans by id (see the module docs).
#[derive(Default)]
pub(crate) struct EditPlanStore {
    plans: Mutex<HashMap<Uuid, StoredEdit>>,
}

impl EditPlanStore {
    fn prune(plans: &mut HashMap<Uuid, StoredEdit>, ttl: Duration) {
        plans.retain(|_, p| p.at.elapsed() < ttl);
    }

    /// Stores `e`, replacing the plan of the same table on the same connection.
    fn insert(&self, ttl: Duration, e: StoredEdit) -> Uuid {
        let id = Uuid::new_v4();
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        plans.retain(|_, p| !p.same_table(e.conn, &e.object));
        plans.insert(id, e);
        id
    }

    /// Takes plan `id` out: `PlanExpired` when it is unknown, replaced, taken or too old.
    fn take(&self, ttl: Duration, id: Uuid) -> Result<StoredEdit, DbError> {
        let mut plans = lock(&self.plans);
        let plan = plans.remove(&id);
        Self::prune(&mut plans, ttl);
        match plan {
            Some(p) if p.at.elapsed() < ttl => Ok(p),
            _ => Err(DbError::PlanExpired),
        }
    }

    /// Forgets the plans of connection `conn` (closed).
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.conn != conn);
    }
}

/// Largest complete value `data_cell_fetch` / `data_cell_load_file` take.
pub const MAX_FETCH_BYTES: usize = 64 * 1024 * 1024;
/// How long a loaded value lives after it was last used (sliding: every successful use
/// restarts it).
pub const LOADED_TTL: Duration = Duration::from_secs(30 * 60);
/// Loaded values kept at once, in bytes; the least recently used are dropped past it.
pub const MAX_LOADED_BYTES: usize = 256 * 1024 * 1024;

/// A complete cell value as the value editor gets it.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellPayload {
    Null,
    /// Text, JSON, numbers, dates…: the value's text.
    Text {
        text: String,
    },
    /// Binary data (standard base64, padded) of `len` bytes. Renamed explicitly, as
    /// `CellInput::Base64`.
    #[serde(rename = "base64")]
    Base64 {
        data: String,
        len: f64,
    },
}

/// A complete value and the token that stands for it in a later edit.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
pub struct LoadedCell {
    pub token: Uuid,
    pub payload: CellPayload,
}

/// A loaded value and the cell it was read for.
struct Loaded {
    conn: Uuid,
    database: String,
    schema: Option<String>,
    table: String,
    column: String,
    value: FullValue,
    at: Instant,
}

const LOADED_GONE: &str = "the loaded value is no longer available (it expired); load it again";

/// Complete cell values by token, least recently used first (see the module docs).
#[derive(Default)]
pub(crate) struct LoadedStore {
    values: Mutex<VecDeque<(Uuid, Loaded)>>,
}

impl LoadedStore {
    fn prune(values: &mut VecDeque<(Uuid, Loaded)>, ttl: Duration) {
        values.retain(|(_, v)| v.at.elapsed() < ttl);
    }

    /// Entry `i` was used now: its time is reset and it moves to the back (most
    /// recently used).
    fn touch(values: &mut VecDeque<(Uuid, Loaded)>, i: usize) -> Option<&Loaded> {
        let (token, mut v) = values.remove(i)?;
        v.at = Instant::now();
        values.push_back((token, v));
        values.back().map(|(_, v)| v)
    }

    /// Stores `v`; drops the least recently used values while the store holds more
    /// than [`MAX_LOADED_BYTES`] (never `v` itself).
    fn insert(&self, ttl: Duration, v: Loaded) -> Uuid {
        let token = Uuid::new_v4();
        let mut values = lock(&self.values);
        Self::prune(&mut values, ttl);
        values.push_back((token, v));
        let mut total: usize = values.iter().map(|(_, v)| v.value.byte_len()).sum();
        while total > MAX_LOADED_BYTES && values.len() > 1 {
            if let Some((_, old)) = values.pop_front() {
                total -= old.value.byte_len();
            }
        }
        token
    }

    /// The value of `token` for `column` of table `o` on connection `conn`, or why not
    /// (UI copy).
    fn lookup(
        &self,
        ttl: Duration,
        token: Uuid,
        conn: Uuid,
        o: &ObjectRef,
        column: &str,
    ) -> Result<FullValue, String> {
        let mut values = lock(&self.values);
        Self::prune(&mut values, ttl);
        let Some(i) = values.iter().position(|(t, _)| *t == token) else {
            return Err(LOADED_GONE.into());
        };
        let v = &values[i].1;
        if v.conn != conn || (&v.database, &v.schema, &v.table) != (&o.database, &o.schema, &o.name)
        {
            return Err("the loaded value belongs to another table or connection".into());
        }
        if v.column != column {
            return Err(format!(
                "the loaded value belongs to another column ({})",
                v.column
            ));
        }
        // A successful use keeps the value alive (sliding TTL, LRU order).
        Self::touch(&mut values, i)
            .map(|v| v.value.clone())
            .ok_or_else(|| LOADED_GONE.to_string())
    }

    /// The value of `token`, whatever it was read for (a read keeps it alive).
    fn value(&self, ttl: Duration, token: Uuid) -> Option<FullValue> {
        let mut values = lock(&self.values);
        Self::prune(&mut values, ttl);
        let i = values.iter().position(|(t, _)| *t == token)?;
        Self::touch(&mut values, i).map(|v| v.value.clone())
    }

    /// Forgets the values of connection `conn` (closed).
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.values).retain(|(_, v)| v.conn != conn);
    }
}

/// The payload of a complete value; arrays are refused (no single text form to edit).
fn payload(v: &FullValue, column: &str) -> Result<CellPayload, DbError> {
    if let Some(b) = v.as_bytes() {
        return Ok(CellPayload::Base64 {
            data: base64_encode(b),
            len: b.len() as f64,
        });
    }
    let text = match v.as_value() {
        None | Some(Value::Null) => return Ok(CellPayload::Null),
        Some(
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Json(s)
            | Value::Uuid(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::TimestampTz(s)
            | Value::Other { text: s, .. },
        ) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::UInt(u)) => u.to_string(),
        Some(Value::Float(f)) => f.to_string(),
        Some(Value::Array(_) | Value::Bytes { .. }) => {
            return Err(config_error(
                &format!("column:{column}"),
                "array values cannot be loaded in full",
            ))
        }
    };
    Ok(CellPayload::Text { text })
}

/// Collects the complete rows of one read ([`CellMode::Full`]).
#[derive(Default)]
struct FullRows {
    rows: Vec<Vec<FullValue>>,
    /// Display rows arrived: the driver ignored `CellMode::Full`.
    display: bool,
}

impl RowSink for FullRows {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {
        self.display = true;
    }
    fn full_rows(&mut self, batch: Vec<Vec<FullValue>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// The session must read app-built literals the way they were built (R9 / R12):
/// PostgreSQL `standard_conforming_strings` on, MySQL `character_set_client` utf8mb4.
/// Otherwise a `config` error (field `field`) saying `what` cannot be done safely.
async fn check_literals(
    s: &dyn Session,
    opts: &ExecOptions,
    field: &str,
    what: &str,
) -> Result<(), DbError> {
    let opts = ExecOptions {
        cells: CellMode::Preview,
        ..opts.clone()
    };
    let first = |sql: &'static str| {
        let opts = &opts;
        async move {
            let mut cell = FirstCell::default();
            s.execute_one(sql, StatementKind::Read, opts, &mut cell)
                .await
                .map(|_| cell.0)
        }
    };
    let (sql, want, setting) = match s.dialect() {
        Dialect::Postgres => (
            "SHOW standard_conforming_strings",
            "on",
            "the server has standard_conforming_strings off",
        ),
        Dialect::MySql(_) => (
            "SELECT @@character_set_client",
            "utf8mb4",
            "the session's client character set is not utf8mb4",
        ),
    };
    let v = first(sql).await?;
    if !matches!(&v, Some(Value::Text(s)) if s.eq_ignore_ascii_case(want)) {
        return Err(config_error(
            field,
            &format!("{setting}: {what} on this connection"),
        ));
    }
    Ok(())
}

impl SessionManager {
    /// Overrides [`MAX_FETCH_BYTES`] (tests).
    pub fn with_cell_fetch_limit(mut self, bytes: usize) -> Self {
        self.cell_fetch_cap = bytes;
        self
    }

    /// Overrides [`LOADED_TTL`] (tests).
    pub fn with_loaded_ttl(mut self, ttl: Duration) -> Self {
        self.loaded_ttl = ttl;
        self
    }

    /// Stores `value`, read for `column` of `o` on connection `id`; its token and payload.
    fn store_loaded(
        &self,
        id: Uuid,
        o: &ObjectRef,
        column: String,
        value: FullValue,
    ) -> Result<LoadedCell, DbError> {
        let payload = payload(&value, &column)?;
        let token = self.loaded.insert(
            self.loaded_ttl,
            Loaded {
                conn: id,
                database: o.database.clone(),
                schema: o.schema.clone(),
                table: o.name.clone(),
                column,
                value,
                at: Instant::now(),
            },
        );
        Ok(LoadedCell { token, payload })
    }

    /// The complete value of `column` in the row of table `obj` whose key is `key` (see
    /// the module docs). Errors: `config` (field `column:<name>` for a bad column or
    /// key, or a value over the limit; `row` when no row has the key; `edit` when the
    /// table has no key or the session cannot read app-built literals safely),
    /// `not_open`, driver errors.
    pub async fn cell_fetch(
        &self,
        id: Uuid,
        obj: ObjectRef,
        key: Vec<(String, Value)>,
        column: String,
    ) -> Result<LoadedCell, DbError> {
        let cfg = self.repo.get(id)?;
        check_object(cfg.kind, &obj)?;
        let rt = self.runtime(id)?;
        let detail = self.table_detail_fresh(id, &obj).await?;
        let (metas, dialect, _) = self.edit_metas(&rt, &detail).await?;
        // Refused here, before anything is read, when the column or key is wrong.
        cell_select(&detail, &metas, dialect, &key, &column).map_err(edit_error)?;
        let opts = ExecOptions {
            max_rows: 2,
            timeout: Some(self.edit_timeout(&rt)),
            cancel_grace: DATA_CANCEL_GRACE,
            cells: CellMode::Full {
                max_cell_bytes: self.cell_fetch_cap,
            },
            ..ExecOptions::default()
        };
        let mut retried = false;
        let value = loop {
            let taken = self.take_data_session(&rt, Some(&obj.database)).await?;
            let out = read_cell(
                taken.session.as_ref(),
                &opts,
                &detail,
                &metas,
                &key,
                &column,
            )
            .await;
            let pooled = taken.pooled;
            self.return_data_session(&rt, taken, out.is_ok());
            match out {
                Ok(v) => break v,
                Err(e) if pooled && !retried && is_lost(&e) => retried = true,
                Err(e) => return Err(e),
            }
        };
        self.store_loaded(id, &obj, column, value)
    }

    /// A value for `column` of table `obj` made from a file's `bytes` (the value
    /// editor's "Load from file"): the bytes of a binary column, else UTF-8 text (a
    /// leading BOM dropped). Errors: `config` (field `path`: too large or not UTF-8;
    /// `column:<name>`: no such column), `not_open`, driver errors.
    pub async fn cell_load(
        &self,
        id: Uuid,
        obj: ObjectRef,
        column: String,
        bytes: Vec<u8>,
    ) -> Result<LoadedCell, DbError> {
        if bytes.len() > self.cell_fetch_cap {
            return Err(config_error(
                "path",
                &format!(
                    "The file is larger than {}.",
                    size_text(self.cell_fetch_cap)
                ),
            ));
        }
        let cfg = self.repo.get(id)?;
        check_object(cfg.kind, &obj)?;
        let rt = self.runtime(id)?;
        let detail = self.table_detail_fresh(id, &obj).await?;
        if !detail.columns.iter().any(|c| c.name == column) {
            return Err(config_error(
                &format!("column:{column}"),
                "the table has no such column",
            ));
        }
        let (metas, _, _) = self.edit_metas(&rt, &detail).await?;
        let binary = metas
            .iter()
            .any(|m| m.name == column && m.logical == LogicalType::Binary);
        let value = if binary {
            FullValue::bytes(bytes)
        } else {
            let text = String::from_utf8(bytes)
                .map_err(|_| config_error("path", "The file is not UTF-8 text."))?;
            let text = match text.strip_prefix('\u{feff}') {
                Some(rest) => rest.to_string(),
                None => text,
            };
            FullValue::try_from_value(Value::Text(text)).map_err(|_| DbError::Internal {
                message: "text is always complete".into(),
            })?
        };
        self.store_loaded(id, &obj, column, value)
    }

    /// The bytes of loaded value `token` ("Save to file"): binary data as is, anything
    /// else as UTF-8 text. `config` (field `token`) when it expired or is NULL.
    pub fn loaded_bytes(&self, token: Uuid) -> Result<Vec<u8>, DbError> {
        let v = self
            .loaded
            .value(self.loaded_ttl, token)
            .ok_or_else(|| config_error("token", LOADED_GONE))?;
        if let Some(b) = v.as_bytes() {
            return Ok(b.to_vec());
        }
        match payload(&v, "")? {
            CellPayload::Text { text } => Ok(text.into_bytes()),
            _ => Err(config_error(
                "token",
                "the value is NULL: there is nothing to save",
            )),
        }
    }
}

/// `n` bytes as UI copy (`64 MB`, or bytes below 1 MB).
pub(crate) fn size_text(n: usize) -> String {
    const MB: usize = 1024 * 1024;
    if n >= MB && n.is_multiple_of(MB) {
        format!("{} MB", n / MB)
    } else {
        format!("{n} bytes")
    }
}

/// One cell read by key on session `s` (see the module docs).
async fn read_cell(
    s: &dyn Session,
    opts: &ExecOptions,
    detail: &TableDetail,
    metas: &[ColumnMeta],
    key: &[(String, Value)],
    column: &str,
) -> Result<FullValue, DbError> {
    let reconnects = s.reconnects();
    check_literals(s, opts, "edit", "the value cannot be read safely").await?;
    let dialect = s.dialect();
    let sql = cell_select(detail, metas, dialect, key, column).map_err(edit_error)?;
    tracing::debug!(
        target: "dnt::sql",
        sql = %redact(&sql, dialect).text,
        "reading a complete cell value"
    );
    let mut sink = FullRows::default();
    let read = s
        .execute_one(&sql, StatementKind::Read, opts, &mut sink)
        .await;
    if let (Err(DbError::Unsupported { .. }), CellMode::Full { max_cell_bytes }) =
        (&read, opts.cells)
    {
        return Err(config_error(
            &format!("column:{column}"),
            &format!(
                "the value is larger than {}: it cannot be loaded",
                size_text(max_cell_bytes)
            ),
        ));
    }
    read?;
    if s.reconnects() != reconnects || s.dialect() != dialect {
        return Err(DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message: "the connection was replaced while the value was read; load it again".into(),
        });
    }
    if sink.display {
        return Err(DbError::Internal {
            message: "the driver returned display values for a complete read".into(),
        });
    }
    let mut rows = sink.rows.into_iter();
    match (rows.next(), rows.next()) {
        (Some(row), None) => row.into_iter().next().ok_or_else(|| DbError::Internal {
            message: "the read returned no column".into(),
        }),
        (None, _) => Err(config_error(
            "row",
            "the row is no longer there; reload the data",
        )),
        (Some(_), Some(_)) => Err(config_error(
            "row",
            "the key matches more than one row; reload the data",
        )),
    }
}

fn stale(reason: &str) -> DbError {
    DbError::PlanStale {
        reason: reason.into(),
    }
}

/// An [`EditError`] as a `config` error: field `one_at_a_time` (apply one edit at a
/// time), `column:<name>` (that column's value), else `edit`.
pub fn edit_error(e: EditError) -> DbError {
    let (field, message) = match &e {
        EditError::OneAtATime => ("one_at_a_time".to_string(), e.to_string()),
        EditError::KeyNotComplete { column } => (
            format!("column:{column}"),
            "the key value may be cut; reload the row before editing it".to_string(),
        ),
        EditError::Invalid { column, reason } if !column.is_empty() => {
            (format!("column:{column}"), reason.clone())
        }
        _ => ("edit".to_string(), e.to_string()),
    };
    DbError::Config {
        field: Some(field),
        message,
    }
}

/// Whether a failed statement may have been applied.
fn outcome_unknown(e: &DbError) -> bool {
    match e.unstaged() {
        DbError::Timeout {
            outcome_unknown, ..
        }
        | DbError::ConnectionLost {
            outcome_unknown, ..
        }
        | DbError::Cancelled {
            outcome_unknown, ..
        } => *outcome_unknown,
        _ => false,
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The guard context of a connection whose config is `cfg` (as stored now).
fn guard_ctx(sm: &SessionManager, cfg: &ConnectionConfig) -> GuardCtx {
    GuardCtx {
        env: cfg.env_tag,
        read_only: cfg.read_only,
        analyze: false,
        confirm_unbounded: saved_confirm_unbounded(&sm.repo),
    }
}

impl SessionManager {
    /// The statement timeout of an edit: the connection's query timeout, else the data
    /// view's.
    fn edit_timeout(&self, rt: &ConnectionRuntime) -> Duration {
        match rt.cfg.options.query_timeout_s {
            0 => self.data_timeout,
            s => Duration::from_secs(s.into()),
        }
    }

    /// The result-set types of every column of `detail` (`SELECT <cols> … LIMIT 0`), the
    /// dialect of the data session that read them and its tunnel generation. A pooled
    /// session that went stale is replaced once (a read).
    async fn edit_metas(
        &self,
        rt: &ConnectionRuntime,
        detail: &TableDetail,
    ) -> Result<(Vec<ColumnMeta>, Dialect, u64), DbError> {
        let kind = rt.cfg.kind;
        let o = &detail.object;
        let cols: Vec<String> = detail
            .columns
            .iter()
            .map(|c| quote_ident(kind, &c.name))
            .collect();
        let sql = format!(
            "SELECT {} FROM {} LIMIT 0",
            cols.join(", "),
            qualified_name(kind, &o.database, o.schema.as_deref(), &o.name)
        );
        let opts = ExecOptions {
            max_rows: 1,
            timeout: Some(self.edit_timeout(rt)),
            cancel_grace: DATA_CANCEL_GRACE,
            ..ExecOptions::default()
        };
        let mut retried = false;
        loop {
            let taken = self.take_data_session(rt, Some(&o.database)).await?;
            let mut page = BoundedPage::new(self.data_cell_cap, self.data_page_cap);
            let out = taken
                .session
                .execute_one(&sql, StatementKind::Read, &opts, &mut page)
                .await;
            let (dialect, gen, pooled) = (taken.session.dialect(), taken.tunnel_gen, taken.pooled);
            self.return_data_session(rt, taken, out.is_ok());
            match out {
                Ok(_) => return Ok((page.columns, dialect, gen)),
                Err(e) if pooled && !retried && is_lost(&e) => retried = true,
                Err(e) => return Err(e),
            }
        }
    }

    /// Builds and stores the plan of `set` on connection `id` (see the module docs).
    /// Errors: `config` (field `edit`, `one_at_a_time` or `column:<name>`) for a set that
    /// cannot be planned, `not_open`, driver errors of the reads.
    pub async fn edit_preview(&self, id: Uuid, set: EditSet) -> Result<EditPreview, DbError> {
        if set.edits.len() > MAX_EDITS {
            return Err(config_error(
                "edit",
                &format!("apply at most {MAX_EDITS} changes at once"),
            ));
        }
        let cfg = self.repo.get(id)?;
        check_object(cfg.kind, &set.object)?;
        let rt = self.runtime(id)?;
        let detail = self.table_detail_fresh(id, &set.object).await?;
        let (metas, dialect, tunnel_gen) = self.edit_metas(&rt, &detail).await?;
        let lookup = |token: Uuid, column: &str| {
            self.loaded
                .lookup(self.loaded_ttl, token, id, &set.object, column)
        };
        let plan = build_edit_plan(
            &set,
            &detail,
            &metas,
            dialect,
            guard_ctx(self, &cfg),
            &lookup,
        )
        .map_err(edit_error)?;
        let mut preview = EditPreview {
            plan_id: Uuid::nil(),
            // Long literals shortened for display; the stored plan runs the full SQL.
            statements: plan
                .stmts
                .iter()
                .map(|s| display_sql(&s.sql, plan.dialect))
                .collect(),
            verdict: plan.verdict,
            atomic: plan.atomic,
            protections: plan.stmts.iter().map(|s| s.protection.clone()).collect(),
            reason: plan.reason.clone(),
        };
        preview.plan_id = self.edit_plans.insert(
            self.plan_ttl,
            StoredEdit {
                conn: id,
                runtime: Arc::downgrade(&rt),
                tunnel_gen,
                object: set.object,
                env: cfg.env_tag,
                read_only: cfg.read_only,
                plan,
                at: Instant::now(),
            },
        );
        Ok(preview)
    }

    /// Runs the stored plan `plan_id` once (see the module docs). Errors before anything
    /// was applied: `plan_expired`, `plan_stale`, `blocked`, `confirmation_required`,
    /// `config` (the session cannot run app-built literals safely), driver errors of the
    /// checks; once statements were sent, the outcome is in the report.
    pub async fn edit_apply(&self, plan_id: Uuid, confirmed: bool) -> Result<ApplyReport, DbError> {
        let stored = self.edit_plans.take(self.plan_ttl, plan_id)?;
        let rt = match self.runtime_opt(stored.conn) {
            Some(rt)
                if std::ptr::eq(Arc::as_ptr(&rt), stored.runtime.as_ptr())
                    && !rt.closed.load(Ordering::SeqCst) =>
            {
                rt
            }
            _ => return Err(stale("the connection was closed or reopened")),
        };
        if rt.tunnel_dead() || rt.tunnel_gen.load(Ordering::SeqCst) != stored.tunnel_gen {
            return Err(stale("the connection's SSH tunnel was replaced"));
        }
        let plan = &stored.plan;
        let cfg = self.repo.get(stored.conn)?;
        let n = count(plan.stmts.len());
        match edit_verdict(
            &plan.stmts,
            plan.atomic,
            plan.dialect,
            guard_ctx(self, &cfg),
        ) {
            (Verdict::Block, Some(reason)) => return Err(config_error("edit", &reason)),
            (Verdict::Block, None) => return Err(DbError::Blocked { count: n }),
            (Verdict::Confirm, _) if !confirmed => {
                return Err(DbError::ConfirmationRequired { count: n })
            }
            _ => {}
        }
        if cfg.env_tag != stored.env || cfg.read_only != stored.read_only {
            return Err(stale(
                "the connection's environment or read-only setting changed",
            ));
        }
        let detail = self.table_detail_fresh(stored.conn, &stored.object).await?;
        if fingerprint(&detail) != plan.fingerprint {
            return Err(stale("the table's structure changed"));
        }
        let taken = self
            .take_data_session(&rt, Some(&stored.object.database))
            .await?;
        // The tunnel may have been replaced while the table was read (R24): the session
        // taken now must come through the tunnel the plan was made for.
        if taken.tunnel_gen != stored.tunnel_gen {
            self.return_data_session(&rt, taken, true);
            return Err(stale("the connection's SSH tunnel was replaced"));
        }
        if taken.session.dialect() != plan.dialect {
            self.return_data_session(&rt, taken, true);
            return Err(stale("the session's SQL mode changed"));
        }
        // Registered before anything is sent, so closing the connection finds it.
        let stop = StopSignal::new();
        let Some(running) = rt
            .data
            .register_apply(&rt.closed, taken.session.as_ref(), &stop)
        else {
            self.return_data_session(&rt, taken, false);
            return Err(stale("the connection was closed or reopened"));
        };
        let apply = Apply {
            s: taken.session.as_ref(),
            plan,
            conn: stored.conn,
            opts: ExecOptions {
                max_rows: 2,
                timeout: Some(self.edit_timeout(&rt)),
                cancel_grace: DATA_CANCEL_GRACE,
                stop,
                ..ExecOptions::default()
            },
            cell_cap: self.data_cell_cap,
            page_cap: self.data_page_cap,
            progress: Mutex::default(),
        };
        let run = async {
            if plan.atomic == Atomicity::Yes {
                apply.atomic().await
            } else {
                apply.single().await
            }
        };
        let abort = running.abort.notified();
        let (result, clean) = match select(pin!(abort), pin!(run)).await {
            // Closing gave up waiting: the statement in flight is abandoned with `run`.
            Either::Left(((), _run)) => (Ok(apply.closed_report()), false),
            Either::Right((out, _)) => out,
        };
        if let Ok(r) = &result {
            tracing::info!(connection = %stored.conn, outcome = ?r.outcome, "edit plan applied");
        }
        // The session is closed (or pooled) before closing is told the apply ended.
        self.return_data_session(&rt, taken, clean);
        drop(running);
        result
    }
}

/// What one statement of a plan did.
struct Done {
    /// Rows it matched (after a recheck).
    affected: Option<u64>,
    /// An INSERT's row as read back.
    inserted: Option<Vec<Value>>,
}

/// Where a run of a plan is: what closing the connection mid-apply reports.
#[derive(Debug, Clone, Copy, Default)]
enum Stage {
    /// `BEGIN` and the session checks: nothing of the plan was sent.
    #[default]
    Checks,
    /// Statement `i` of the plan (with its recheck / read-back).
    Statement(usize),
    /// Rolling back after a conflict or a failed statement.
    Rollback,
    /// `COMMIT` is sent (or about to be).
    Commit,
}

/// What a run did so far.
#[derive(Default)]
struct Progress {
    stage: Stage,
    statements: Vec<StmtApply>,
    inserted: Vec<Vec<Value>>,
}

/// One run of a plan on its session.
struct Apply<'a> {
    s: &'a dyn Session,
    plan: &'a EditPlan,
    conn: Uuid,
    opts: ExecOptions,
    cell_cap: usize,
    page_cap: usize,
    /// Kept outside the run's future, so it is still there when the run is abandoned.
    progress: Mutex<Progress>,
}

/// The error of what closing the connection cut off.
fn closed_mid_apply(was_in_tx: bool, outcome_unknown: bool) -> DbError {
    DbError::ConnectionLost {
        was_in_tx,
        outcome_unknown,
        message: "the connection was closed while the changes were being applied".into(),
    }
}

impl Apply<'_> {
    fn stage(&self, stage: Stage) {
        lock(&self.progress).stage = stage;
    }

    /// Records a statement that ended, with an INSERT's row when it counts.
    fn record(&self, stmt: StmtApply, inserted: Option<Vec<Value>>) {
        let mut p = lock(&self.progress);
        p.statements.push(stmt);
        p.inserted.extend(inserted);
    }

    /// The report of the statements recorded so far; inserted rows only when something
    /// may have been applied.
    fn report(&self, outcome: ApplyOutcome, error: Option<DbError>) -> ApplyReport {
        let mut p = lock(&self.progress);
        let statements = std::mem::take(&mut p.statements);
        let inserted = match outcome {
            ApplyOutcome::Committed | ApplyOutcome::Unknown { .. } => {
                std::mem::take(&mut p.inserted)
            }
            _ => Vec::new(),
        };
        ApplyReport {
            outcome,
            statements,
            inserted,
            error: error.map(AppError::from),
        }
    }

    /// The report of a run abandoned because the connection is closing (see the module
    /// docs).
    fn closed_report(&self) -> ApplyReport {
        let stage = lock(&self.progress).stage;
        let atomic = self.plan.atomic == Atomicity::Yes;
        match stage {
            Stage::Checks if atomic => self.report(
                ApplyOutcome::RolledBack,
                Some(closed_mid_apply(true, false)),
            ),
            Stage::Checks => self.report(
                ApplyOutcome::PartiallyApplied { applied: 0 },
                Some(closed_mid_apply(false, false)),
            ),
            Stage::Statement(i) => {
                let (outcome, error) = if atomic {
                    (ApplyOutcome::RolledBack, closed_mid_apply(true, false))
                } else {
                    (
                        ApplyOutcome::Unknown { at: count(i) },
                        closed_mid_apply(false, true),
                    )
                };
                if let Some(st) = self.plan.stmts.get(i) {
                    self.record(
                        StmtApply {
                            index: st.index,
                            affected: None,
                            error: Some(AppError::from(error)),
                            conflict: false,
                        },
                        None,
                    );
                }
                self.report(outcome, None)
            }
            Stage::Rollback => self.report(
                ApplyOutcome::RolledBack,
                Some(closed_mid_apply(true, false)),
            ),
            Stage::Commit => self.report(
                ApplyOutcome::Unknown {
                    at: count(self.plan.stmts.len()),
                },
                Some(closed_mid_apply(true, true)),
            ),
        }
    }

    /// Runs `sql` (one statement of kind `kind`): the rows it affected (as the driver
    /// reports them) and its first result's rows.
    async fn exec(
        &self,
        sql: &str,
        kind: StatementKind,
    ) -> Result<(Option<u64>, Vec<Vec<Value>>), DbError> {
        tracing::debug!(
            target: "dnt::sql",
            connection = %self.conn,
            sql = %redact(sql, self.plan.dialect).text,
            "applying an edit plan"
        );
        let mut page = BoundedPage::new(self.cell_cap, self.page_cap);
        let out = self.s.execute_one(sql, kind, &self.opts, &mut page).await?;
        Ok((out.results.first().and_then(|r| r.affected), page.rows))
    }

    async fn tx(&self, sql: &str, c: TxControl) -> Result<(), DbError> {
        let mut cell = FirstCell::default();
        self.s
            .execute_one(sql, StatementKind::Transaction(c), &self.opts, &mut cell)
            .await
            .map(|_| ())
    }

    /// Ends the transaction; whether the session is clean afterwards.
    async fn rollback(&self) -> bool {
        self.tx("ROLLBACK", TxControl::Rollback).await.is_ok() && !self.s.in_transaction()
    }

    /// The session must read app-built literals the way they were built (R9 / R12) and
    /// still have the plan's dialect.
    async fn check_session(&self) -> Result<(), DbError> {
        check_literals(
            self.s,
            &self.opts,
            "edit",
            "changes cannot be written safely",
        )
        .await?;
        if self.s.dialect() != self.plan.dialect {
            return Err(stale("the session's SQL mode changed"));
        }
        Ok(())
    }

    /// Runs one planned statement: its matched rows (an UPDATE reporting 0 on MySQL is
    /// rechecked) and an INSERT's row. A failed read-back fails the statement when
    /// `strict` (inside a transaction, which is then rolled back); otherwise its row is
    /// left empty.
    async fn statement(&self, st: &PlannedEdit, strict: bool) -> Result<Done, DbError> {
        let (mut affected, rows) = self.exec(&st.sql, StatementKind::Write).await?;
        // PostgreSQL `INSERT … RETURNING` reports rows, not an affected count: every
        // row it wrote is returned (a single-row insert is never cut by `max_rows`).
        if affected.is_none() && st.returning && self.plan.dialect == Dialect::Postgres {
            affected = Some(rows.len() as u64);
        }
        if affected == Some(0) {
            if let Some(recheck) = &st.recheck {
                let (_, found) = self.exec(recheck, StatementKind::Read).await?;
                affected = Some(found.len() as u64);
            }
        }
        if !st.returning || affected != Some(st.expect_affected) {
            return Ok(Done {
                affected,
                inserted: None,
            });
        }
        let row = match (&st.read_back, self.plan.dialect) {
            (_, Dialect::Postgres) => Ok(rows),
            (Some(sql), _) => self.exec(sql, StatementKind::Read).await.map(|(_, r)| r),
            (None, _) => Ok(Vec::new()),
        };
        let row = match row {
            Ok(rows) => rows.into_iter().next().unwrap_or_default(),
            Err(e) if strict => return Err(e),
            Err(e) => {
                tracing::warn!(connection = %self.conn, error = %e, "could not read the inserted row back");
                Vec::new()
            }
        };
        Ok(Done {
            affected,
            inserted: Some(row),
        })
    }

    /// `atomic == Yes`: one transaction. Returns the report (or an error when nothing of
    /// the plan was sent) and whether the session is clean.
    async fn atomic(&self) -> (Result<ApplyReport, DbError>, bool) {
        let begin = match self.plan.dialect {
            Dialect::Postgres => "BEGIN",
            Dialect::MySql(_) => "START TRANSACTION",
        };
        if let Err(e) = self.tx(begin, TxControl::Begin).await {
            return (Err(e), false);
        }
        // Inside the transaction: the driver never silently replaces the connection now.
        // After any failure the session is closed, never pooled: a timeout or cancel
        // sent a KILL QUERY / cancel request that could still land on the next request.
        if let Err(e) = self.check_session().await {
            if !is_lost(&e) {
                self.rollback().await;
            }
            return (Err(e), false);
        }
        for (i, st) in self.plan.stmts.iter().enumerate() {
            self.stage(Stage::Statement(i));
            match self.statement(st, true).await {
                Ok(done) => {
                    let conflict = done.affected != Some(st.expect_affected);
                    let stmt = StmtApply {
                        index: st.index,
                        affected: done.affected.map(|n| n as f64),
                        error: None,
                        conflict,
                    };
                    if conflict {
                        self.record(stmt, None);
                        self.stage(Stage::Rollback);
                        let clean = self.rollback().await;
                        return (Ok(self.report(ApplyOutcome::RolledBack, None)), clean);
                    }
                    self.record(stmt, done.inserted);
                }
                Err(e) => {
                    // Not committed: rolled back here, or by the server once the
                    // connection is gone. Either way the session is closed (see above).
                    let lost = is_lost(&e);
                    self.record(
                        StmtApply {
                            index: st.index,
                            affected: None,
                            error: Some(AppError::from(e)),
                            conflict: false,
                        },
                        None,
                    );
                    self.stage(Stage::Rollback);
                    if !lost {
                        self.rollback().await;
                    }
                    return (Ok(self.report(ApplyOutcome::RolledBack, None)), false);
                }
            }
        }
        self.stage(Stage::Commit);
        match self.tx("COMMIT", TxControl::Commit).await {
            Ok(()) => (
                Ok(self.report(ApplyOutcome::Committed, None)),
                !self.s.in_transaction(),
            ),
            // A PostgreSQL COMMIT the server refused (a deferred constraint) rolled back.
            Err(e @ DbError::Server { .. }) if self.plan.dialect == Dialect::Postgres => {
                (Ok(self.report(ApplyOutcome::RolledBack, Some(e))), false)
            }
            Err(e) => {
                let at = count(self.plan.stmts.len());
                (
                    Ok(self.report(ApplyOutcome::Unknown { at }, Some(e))),
                    false,
                )
            }
        }
    }

    /// `atomic != Yes`: the plan's one statement, without a transaction.
    async fn single(&self) -> (Result<ApplyReport, DbError>, bool) {
        let [st] = self.plan.stmts.as_slice() else {
            return (
                Err(DbError::Internal {
                    message: "a plan without a transaction has one statement".into(),
                }),
                true,
            );
        };
        // No transaction, so the driver may replace a dead connection silently before a
        // statement (R22). Right before it: ping, then check the session again, and note
        // its reconnect count. A reconnect during the checks stops the plan (nothing
        // sent); one seen only after the write means it ran on a connection the checks
        // did not cover.
        if let Err(e) = self.s.ping().await {
            return (Err(e), false);
        }
        let reconnects = self.s.reconnects();
        if let Err(e) = self.check_session().await {
            return (Err(e), false);
        }
        if self.s.reconnects() != reconnects {
            return (
                Err(stale("the connection was replaced while it was checked")),
                false,
            );
        }
        self.stage(Stage::Statement(0));
        match self.statement(st, false).await {
            Ok(done) => {
                let replaced = self.s.reconnects() != reconnects;
                let (outcome, conflict) = match done.affected {
                    _ if replaced => (ApplyOutcome::Unknown { at: 0 }, false),
                    Some(n) if n == st.expect_affected => (ApplyOutcome::Committed, false),
                    Some(0) => (ApplyOutcome::PartiallyApplied { applied: 0 }, true),
                    // No count, or another one: whether it applied cannot be told.
                    _ => (ApplyOutcome::Unknown { at: 0 }, false),
                };
                let clean = !matches!(outcome, ApplyOutcome::Unknown { .. });
                let stmt = StmtApply {
                    index: st.index,
                    affected: done.affected.map(|n| n as f64),
                    error: None,
                    conflict,
                };
                let inserted = match outcome {
                    ApplyOutcome::Committed => done.inserted,
                    _ => None,
                };
                self.record(stmt, inserted);
                (Ok(self.report(outcome, None)), clean)
            }
            Err(e) => {
                let outcome = if outcome_unknown(&e) {
                    ApplyOutcome::Unknown { at: 0 }
                } else {
                    ApplyOutcome::PartiallyApplied { applied: 0 }
                };
                self.record(
                    StmtApply {
                        index: st.index,
                        affected: None,
                        error: Some(AppError::from(e)),
                        conflict: false,
                    },
                    None,
                );
                (Ok(self.report(outcome, None)), false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_errors_become_config_errors_with_a_field() {
        let field = |e: EditError| match edit_error(e) {
            DbError::Config { field, message } => (field.unwrap(), message),
            other => panic!("{other:?}"),
        };
        assert_eq!(field(EditError::OneAtATime).0, "one_at_a_time");
        assert_eq!(
            field(EditError::Invalid {
                column: "qty".into(),
                reason: "the column cannot be NULL".into()
            }),
            ("column:qty".into(), "the column cannot be NULL".into())
        );
        assert_eq!(
            field(EditError::KeyNotComplete {
                column: "id".into()
            })
            .0,
            "column:id"
        );
        assert_eq!(
            field(EditError::Invalid {
                column: String::new(),
                reason: "there are no changes to apply".into()
            }),
            ("edit".into(), "there are no changes to apply".into())
        );
        assert_eq!(
            field(EditError::NotEditable(
                dnt_query::edit::NotEditable::ReadOnlyConnection
            ))
            .0,
            "edit"
        );
    }

    #[test]
    fn outcome_wire_format() {
        let j = |o: ApplyOutcome| serde_json::to_value(o).unwrap();
        assert_eq!(
            j(ApplyOutcome::Committed),
            serde_json::json!({"outcome": "committed"})
        );
        assert_eq!(
            j(ApplyOutcome::RolledBack),
            serde_json::json!({"outcome": "rolled_back"})
        );
        assert_eq!(
            j(ApplyOutcome::PartiallyApplied { applied: 2 }),
            serde_json::json!({"outcome": "partially_applied", "applied": 2})
        );
        assert_eq!(
            j(ApplyOutcome::Unknown { at: 3 }),
            serde_json::json!({"outcome": "unknown", "at": 3})
        );
    }

    #[test]
    fn loaded_values_are_bound_to_their_cell_and_bounded() {
        let store = LoadedStore::default();
        let ttl = Duration::from_secs(60);
        let conn = Uuid::new_v4();
        let o = |name: &str| ObjectRef {
            database: "shop".into(),
            schema: None,
            name: name.into(),
            kind: dnt_core::meta::ObjectKind::Table,
        };
        let loaded = |conn: Uuid, table: &str, column: &str, value: FullValue| Loaded {
            conn,
            database: "shop".into(),
            schema: None,
            table: table.into(),
            column: column.into(),
            value,
            at: Instant::now(),
        };
        let v = FullValue::bytes(vec![1u8, 2, 3]);
        let t = store.insert(ttl, loaded(conn, "items", "data", v.clone()));
        assert_eq!(
            store.lookup(ttl, t, conn, &o("items"), "data"),
            Ok(v.clone())
        );
        let refused = |r: Result<FullValue, String>| r.unwrap_err();
        assert!(refused(store.lookup(ttl, t, conn, &o("items"), "note")).contains("another column"));
        assert!(refused(store.lookup(ttl, t, conn, &o("other"), "data")).contains("another table"));
        assert!(
            refused(store.lookup(ttl, t, Uuid::new_v4(), &o("items"), "data"))
                .contains("another table or connection")
        );
        assert!(
            refused(store.lookup(ttl, Uuid::new_v4(), conn, &o("items"), "data"))
                .contains("load it again")
        );
        // Expired.
        assert!(
            refused(store.lookup(Duration::ZERO, t, conn, &o("items"), "data")).contains("expired")
        );
        assert_eq!(store.value(ttl, t), None);

        // The oldest values go once the store holds too much; the newest always stays.
        let half = FullValue::bytes(vec![0u8; MAX_LOADED_BYTES / 2 + 1]);
        let a = store.insert(ttl, loaded(conn, "items", "data", half.clone()));
        let b = store.insert(ttl, loaded(conn, "items", "data", half.clone()));
        assert!(store.value(ttl, a).is_none());
        assert!(store.value(ttl, b).is_some());
        let big = FullValue::bytes(vec![0u8; MAX_LOADED_BYTES + 1]);
        let c = store.insert(ttl, loaded(conn, "items", "data", big));
        assert!(store.value(ttl, b).is_none());
        assert!(store.value(ttl, c).is_some());
        store.drop_conn(conn);
        assert!(store.value(ttl, c).is_none());
    }

    #[test]
    fn loaded_values_live_while_used_and_go_least_recently_used_first() {
        let store = LoadedStore::default();
        let conn = Uuid::new_v4();
        let items = ObjectRef {
            database: "shop".into(),
            schema: None,
            name: "items".into(),
            kind: dnt_core::meta::ObjectKind::Table,
        };
        let loaded = |value: FullValue| Loaded {
            conn,
            database: "shop".into(),
            schema: None,
            table: "items".into(),
            column: "data".into(),
            value,
            at: Instant::now(),
        };
        // Sliding: each read restarts the TTL, so the value outlives its first deadline.
        let ttl = Duration::from_millis(300);
        let t = store.insert(ttl, loaded(FullValue::bytes(vec![1u8])));
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(150));
            assert!(store.lookup(ttl, t, conn, &items, "data").is_ok());
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            store.value(ttl, t).is_some(),
            "450 ms after insert, read 150 ms ago"
        );
        std::thread::sleep(Duration::from_millis(400));
        assert!(store.value(ttl, t).is_none());

        // LRU: `a` was read after `b` was stored, so `b` goes first.
        let ttl = Duration::from_secs(60);
        let half = FullValue::bytes(vec![0u8; MAX_LOADED_BYTES / 2 - 1]);
        let a = store.insert(ttl, loaded(half.clone()));
        let b = store.insert(ttl, loaded(half.clone()));
        assert!(store.value(ttl, a).is_some());
        let c = store.insert(ttl, loaded(FullValue::bytes(vec![0u8; 3])));
        assert!(store.value(ttl, b).is_none(), "least recently used dropped");
        assert!(store.value(ttl, a).is_some());
        assert!(store.value(ttl, c).is_some());
    }

    #[test]
    fn payloads_and_sizes() {
        let p = |v: Value| payload(&FullValue::try_from_value(v).unwrap(), "c");
        assert_eq!(p(Value::Null).unwrap(), CellPayload::Null);
        assert_eq!(
            p(Value::Int(-4)).unwrap(),
            CellPayload::Text { text: "-4".into() }
        );
        assert_eq!(
            p(Value::Json("{}".into())).unwrap(),
            CellPayload::Text { text: "{}".into() }
        );
        assert!(p(Value::Array(vec![])).is_err());
        assert_eq!(
            payload(&FullValue::bytes(b"Man".to_vec()), "c").unwrap(),
            CellPayload::Base64 {
                data: "TWFu".into(),
                len: 3.0
            }
        );
        assert_eq!(
            serde_json::to_value(CellPayload::Base64 {
                data: "TQ==".into(),
                len: 1.0
            })
            .unwrap(),
            serde_json::json!({"kind": "base64", "data": "TQ==", "len": 1.0})
        );
        assert_eq!(size_text(MAX_FETCH_BYTES), "64 MB");
        assert_eq!(size_text(1000), "1000 bytes");
    }

    #[test]
    fn store_keeps_one_plan_per_table_and_takes_once() {
        let store = EditPlanStore::default();
        let ttl = Duration::from_secs(60);
        let conn = Uuid::new_v4();
        let obj = |name: &str| ObjectRef {
            database: "shop".into(),
            schema: None,
            name: name.into(),
            kind: dnt_core::meta::ObjectKind::Table,
        };
        let stored = |o: ObjectRef| StoredEdit {
            conn,
            runtime: Weak::new(),
            tunnel_gen: 0,
            object: o,
            env: None,
            read_only: false,
            plan: EditPlan {
                stmts: vec![],
                atomic: Atomicity::Yes,
                verdict: Verdict::Allow,
                reason: None,
                fingerprint: 0,
                dialect: Dialect::MYSQL,
            },
            at: Instant::now(),
        };
        let a = store.insert(ttl, stored(obj("a")));
        let b = store.insert(ttl, stored(obj("b")));
        let a2 = store.insert(ttl, stored(obj("a")));
        assert!(matches!(store.take(ttl, a), Err(DbError::PlanExpired)));
        assert!(store.take(ttl, a2).is_ok());
        assert!(matches!(store.take(ttl, a2), Err(DbError::PlanExpired)));
        store.drop_conn(conn);
        assert!(matches!(store.take(ttl, b), Err(DbError::PlanExpired)));
    }
}
