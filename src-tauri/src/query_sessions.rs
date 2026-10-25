//! Query tabs (spec §4.7, §4.8): one query session per tab, separate from the metadata
//! sessions, and the run plans the UI confirms before a run.
//!
//! - A tab's session lives in the `ConnectionRuntime` of the connection the tab is on
//!   (sharing its tunnel) and opens lazily on the tab's first `query_plan`, which needs
//!   the session's `dialect()`. Moving the tab to another connection closes it first.
//! - Every tab has a [`TabSlot`]: an async mutex around its session. Whoever uses the
//!   session holds the slot for that long; a run (Task 15) holds it for the whole run
//!   with an owned guard. A second user of a held slot gets `Busy` at once
//!   (`try_lock`), so a tab's session is never used concurrently. Only closing waits
//!   for the slot (bounded by [`SLOT_CLOSE_WAIT`]).
//! - Plans live in the [`SessionManager`]'s [`PlanStore`] for [`PLAN_TTL`]. Every stored
//!   plan went through `dnt_query::guard`, with the connection's config as it is in the
//!   repository when the plan is made; `query_run` judges it again with the config as
//!   it is then.
//! - A run (`query_run`) is a task that holds the tab's slot until it ends and streams
//!   [`RunEvent`]s; `query_cancel`, closing the tab and closing the connection stop it
//!   (the run still ends with `done`).
//! - An export of all rows of a result (`query_export`) runs its statement again on the
//!   tab's session as a run of the tab (no events: its rows go to a file).
//! - A tab commits in [`TxMode::Auto`] or [`TxMode::Manual`] (`query_tx_mode`, spec
//!   §4.2e): in Manual, the run begins a transaction before every statement that needs
//!   one while none is open (`dnt_query::RunConfig::manual_tx`); the user ends it with
//!   `query_tx`. A Manual transaction lost with its connection blocks the tab's runs
//!   until `query_tx` acknowledges it.

use crate::commands::settings::saved_confirm_unbounded;
use crate::result_store::ResultStore;
use crate::run_events::{
    tick_rows, ChannelSink, EventSink, HistoryCtx, RunEvent, RunFlags, RunOptions, StoredRun,
};
use crate::session_manager::{
    close_quietly, closed_error, config_error, connect_db, lock, with_database, ConnState,
    ConnectionRuntime, SessionManager,
};
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::{AppError, DbError};
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, Dialect, StatementKind};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::{
    classic_table, confirm_names_match, estimate_sql, estimate_table, explain_sql, guard_with,
    mark_implicit_commits, parse_estimate, plan, run_with, DropTarget, ExplainFormat, GuardCtx,
    PlanTable, PlannedStmt, RowEstimate, RunCancel, RunConfig, RunReport, ScriptPlan,
    StmtKindLabel, StopReason, Target, UnboundedWrite, Verdict, WriteOp,
};
use dnt_store::repo::ConnectionRepo;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

/// How long a plan stays available to `query_run` after `query_plan` (and, once taken,
/// to [`SessionManager::plan_statement`] after the take).
pub const PLAN_TTL: Duration = Duration::from_secs(120);

/// Characters of a statement shown in the confirmation and block dialogs.
pub const PREVIEW_CHARS: usize = 120;

/// How long closing a tab (or its connection) waits for whoever holds the tab's session.
pub(crate) const SLOT_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Rows per `rows` event and per driver batch.
pub const RUN_BATCH_SIZE: usize = 500;
/// Rows decoded by a driver before they reach the disk/export backpressure boundary.
/// UI events are still coalesced to [`RUN_BATCH_SIZE`].
const STREAM_BATCH_SIZE: usize = 32;

/// Upper bound of `RunOptions::max_rows`.
pub const MAX_ROWS_LIMIT: u32 = 1_000_000;

/// How long the `EXPLAIN` of one row estimate may run (spec §4.3).
pub const ESTIMATE_TIMEOUT: Duration = Duration::from_secs(2);

/// Statements of one plan that get a row estimate; the others are `unavailable`.
pub const MAX_ESTIMATES: usize = 10;

/// The savepoint a PostgreSQL row estimate runs under inside an open transaction.
const ESTIMATE_SAVEPOINT: &str = "SAVEPOINT dnt_row_estimate";
const ESTIMATE_ROLLBACK: &str = "ROLLBACK TO SAVEPOINT dnt_row_estimate";
const ESTIMATE_RELEASE: &str = "RELEASE SAVEPOINT dnt_row_estimate";

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
pub struct PlanStmtView {
    pub index: u32,
    /// UTF-16 range in the editor text (end-exclusive).
    pub range: (u32, u32),
    pub kind: StmtKindLabel,
    pub verdict: Verdict,
    /// The first [`PREVIEW_CHARS`] characters, line breaks turned into spaces.
    pub preview: String,
    /// What of the statement changes every row of a table (`UPDATE`/`DELETE` without
    /// WHERE, `TRUNCATE`, …), whatever the verdict; empty for most statements. The
    /// guard makes such a statement Confirm (see `dnt_query::guard`). Always sent;
    /// optional in the bindings so older fixtures stay valid.
    #[serde(default)]
    pub unbounded: Vec<UnboundedWrite>,
    /// What a `DROP` of the statement removes (`DROP DATABASE` is confirmed on every
    /// connection, any other `DROP` on Staging and Production); empty for most
    /// statements. Always sent; optional in the bindings.
    #[serde(default)]
    pub drops: Vec<DropTarget>,
    /// On Production, the name (a table, object, database or schema, unquoted) the user
    /// must type to run this Confirm statement: pass it in `query_run`'s
    /// `confirm_names`, exactly (case-sensitive). `None` when no name is asked.
    #[serde(default)]
    pub confirm_name: Option<String>,
    /// On Staging and Production, for an `UPDATE` / `DELETE` that changes every row:
    /// the server's estimate of the rows it changes (`EXPLAIN`, at most
    /// [`ESTIMATE_TIMEOUT`]), or `unavailable`. `None`: not estimated.
    #[serde(default)]
    pub estimate: Option<RowEstimate>,
    /// MySQL: the statement commits the transaction open when it runs (DDL and the
    /// like, in a Manual tab or after the user's `BEGIN`): what ran before it can no
    /// longer be rolled back. Always sent; optional in the bindings.
    #[serde(default)]
    pub implicit_commit: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
pub struct PlanView {
    pub plan_id: Uuid,
    pub stmts: Vec<PlanStmtView>,
    pub dialect: Dialect,
    pub connection_name: String,
    pub env_tag: Option<EnvTag>,
    pub read_only: bool,
    /// The tab session's transaction state when the plan was made.
    pub tx_state: TxState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum TxAction {
    Commit,
    Rollback,
}

impl TxAction {
    fn sql(self) -> &'static str {
        match self {
            TxAction::Commit => "COMMIT",
            TxAction::Rollback => "ROLLBACK",
        }
    }
}

/// How a query tab commits (spec §4.2e). `Auto`: every statement commits on its own
/// unless the user opens a transaction. `Manual`: before every statement that needs a
/// transaction (a write; PostgreSQL DDL that can run in one) while none is open, the run
/// begins one (`BEGIN` / `START TRANSACTION`), which the user then commits or rolls back
/// (`query_tx`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum TxMode {
    #[default]
    Auto,
    Manual,
}

/// The transaction of a query tab, for the status bar and the idle-transaction warning.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, specta::Type)]
pub struct TabTxInfo {
    pub state: TxState,
    /// When the open transaction began (Unix epoch, milliseconds; as seen by the app:
    /// the start of the run or the `BEGIN` that opened it). `None` when no transaction
    /// is open, or its start is not known.
    pub since_ms: Option<f64>,
    /// Statements run in the open transaction (0 when none is open).
    pub statements: u32,
}

impl TabTxInfo {
    const IDLE: TabTxInfo = TabTxInfo {
        state: TxState::Idle,
        since_ms: None,
        statements: 0,
    };
}

/// Now, in Unix epoch milliseconds.
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

/// A `COMMIT` that was sent and then lost its connection or timed out may have
/// committed (spec §4.2d): its error says the outcome is unknown, whatever the driver
/// could tell. Other errors are kept.
fn commit_outcome_unknown(e: DbError) -> DbError {
    match e {
        DbError::AtStage {
            stage,
            timings,
            source,
        } => DbError::AtStage {
            stage,
            timings,
            source: Box::new(commit_outcome_unknown(*source)),
        },
        DbError::ConnectionLost { message, .. } => DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: true,
            message,
        },
        DbError::Timeout { stage, .. } => DbError::Timeout {
            stage,
            outcome_unknown: true,
            was_in_tx: true,
        },
        other => other,
    }
}

/// `text` without line breaks (`\r\n`, `\n`, `\r` each become one space), cut to
/// [`PREVIEW_CHARS`] characters.
pub fn preview(text: &str) -> String {
    text.replace("\r\n", " ")
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(PREVIEW_CHARS)
        .collect()
}

/// Tab ids are the UI's tab keys: non-empty, at most 64 characters.
fn validate_tab(tab: &str) -> Result<(), DbError> {
    if tab.is_empty() || tab.chars().count() > 64 {
        return Err(config_error(
            "tab_id",
            "a tab id must have 1 to 64 characters",
        ));
    }
    Ok(())
}

/// A tab's query session.
///
/// After a run (or `query_tx`) whose statement failed with `ConnectionLost` or
/// `Connect`, `session` is dropped, so the next plan opens a fresh session instead of
/// reusing a dead one. When every such error of the run is retryable (no transaction
/// was lost, the outcome is known), the tab stays bound to its database (`reopen`), so
/// its plans stay valid: Retry / Fetch all (`query_plan_statement`) and `query_run`
/// reopen the session. A `USE db` the user ran (MySQL,
/// `dnt_core::sql::session_effect`) updates `database` once it succeeded, so the tab's
/// next plan switches back to the database the UI shows.
#[derive(Default)]
pub struct TabSession {
    /// The database the session is on: the one the tab asked for, else the connect
    /// database. Meaningful only while `session` is set, or `reopen`.
    pub(crate) database: Option<String>,
    pub(crate) session: Option<Box<dyn Session>>,
    /// `session` was dropped after a retryable lost connection: the tab keeps its
    /// binding to `database` and the session is reopened on its next use.
    pub(crate) reopen: bool,
    /// `session` was dropped with a transaction open (the server rolled it back): the
    /// next `query_tx` reports `ConnectionLost` instead of `Idle`. Cleared once
    /// reported, or when a session opens again (except in a Manual tab, see
    /// `SessionManager::ensure_session`).
    pub(crate) lost_tx: bool,
    /// With `lost_tx`: the connection went while a statement that commits ran (the
    /// user's `COMMIT`, a MySQL implicit commit), so the transaction may have committed:
    /// it is reported as an unknown outcome, never as rolled back.
    pub(crate) lost_tx_unknown: bool,
    /// `ConnectionRuntime::tunnel_gen` when `session` was opened.
    pub(crate) tunnel_gen: u64,
    /// Set once the slot was closed (tab or connection closed): a caller that was
    /// waiting for it must get a fresh slot instead.
    pub(crate) closed: bool,
    /// Meaningful while the session's transaction is open: nothing has read or written
    /// in it since it began, so its first snapshot may not be taken yet (row estimates
    /// must not take it). Set by a run whose last transaction-control statement came
    /// after its last read / write / DDL; cleared by a read, write or DDL that succeeded
    /// (a run, an export). See [`tx_fresh_after`].
    pub(crate) tx_fresh: bool,
    /// Meaningful while the session's transaction is open: when it began (see
    /// [`TabTxInfo::since_ms`]) and how many statements ran in it.
    pub(crate) tx_since_ms: Option<f64>,
    pub(crate) tx_statements: u32,
}

/// [`TabSession::tx_fresh`] after a run of `plan` that sent `report.stmts`, from
/// `fresh` before it: transaction control (`BEGIN`, `COMMIT AND CHAIN`, …) starts a
/// fresh transaction, and so does the `BEGIN` a Manual run sends before a statement
/// (`StmtReport::began_tx`); a read / write / DDL that succeeded uses it, anything else
/// (`SET`, `SAVEPOINT`, a failure) keeps it as it was. Errs towards fresh: a fresh
/// transaction only costs a row estimate.
fn tx_fresh_after(mut fresh: bool, plan: &ScriptPlan, report: &RunReport) -> bool {
    for r in &report.stmts {
        let Some(s) = plan.stmts.iter().find(|s| s.index == r.index) else {
            continue;
        };
        if r.began_tx {
            fresh = true;
        }
        match s.kind {
            StatementKind::Transaction(_) => fresh = true,
            StatementKind::Read | StatementKind::Write | StatementKind::Ddl if r.result.is_ok() => {
                fresh = false
            }
            _ => {}
        }
    }
    fresh
}

impl TabSession {
    fn tx_open(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.tx_state() != TxState::Idle)
    }

    fn set_lost_tx(&mut self, lost: bool, unknown: bool) {
        self.lost_tx = lost;
        self.lost_tx_unknown = lost && unknown;
    }

    /// What the tab reports about its lost transaction ([`Self::lost_tx`]).
    fn lost_tx_error(&self) -> DbError {
        if self.lost_tx_unknown {
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: true,
                message: "the connection was lost while the tab's transaction was being \
                          committed; it may have committed"
                    .into(),
            }
        } else {
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message: "the tab's session was closed during the transaction; \
                          the server rolled it back"
                    .into(),
            }
        }
    }

    fn tx_info(&self) -> TabTxInfo {
        let state = self
            .session
            .as_deref()
            .map_or(TxState::Idle, |s| s.tx_state());
        if state == TxState::Idle {
            return TabTxInfo::IDLE;
        }
        TabTxInfo {
            state,
            since_ms: self.tx_since_ms,
            statements: self.tx_statements,
        }
    }

    /// Forgets the transaction's start and statement count once none is open.
    fn forget_tx_if_idle(&mut self) {
        if !self.tx_open() {
            self.tx_since_ms = None;
            self.tx_statements = 0;
        }
    }

    /// Updates the transaction's start and statement count after a run of `plan` that
    /// sent `report.stmts`, started at `started_ms` with a transaction open or not
    /// (`open_before`). A transaction-control statement of the run (`BEGIN`, `COMMIT
    /// AND CHAIN`, …) starts the count again after it; a Manual run's `BEGIN` before a
    /// statement (`began_tx`) starts it again with that statement.
    fn track_tx(
        &mut self,
        open_before: bool,
        started_ms: f64,
        plan: &ScriptPlan,
        report: &RunReport,
    ) {
        if !self.tx_open() {
            self.forget_tx_if_idle();
            return;
        }
        let mut restarted = false;
        let mut counted = 0u32;
        for r in &report.stmts {
            let Some(s) = plan.stmts.iter().find(|s| s.index == r.index) else {
                continue;
            };
            if r.began_tx {
                restarted = true;
                counted = 0;
            }
            if matches!(s.kind, StatementKind::Transaction(_)) {
                restarted = true;
                counted = 0;
            } else {
                counted = counted.saturating_add(1);
            }
        }
        if restarted || !open_before || self.tx_since_ms.is_none() {
            self.tx_since_ms = Some(started_ms);
            self.tx_statements = counted;
        } else {
            self.tx_statements = self.tx_statements.saturating_add(counted);
        }
    }

    async fn close(&mut self) {
        self.closed = true;
        if let Some(session) = self.session.take() {
            close_quietly(session).await;
        }
    }
}

/// One tab's session, held by one user at a time (see the module docs).
pub type TabSlot = Arc<tokio::sync::Mutex<TabSession>>;

#[derive(Default)]
struct Tabs {
    slots: HashMap<String, TabSlot>,
    /// The database of every tab that has a session: a copy of `TabSession::database`
    /// readable without the slot, so a stored plan can be checked against the tab's
    /// current database while a run holds the slot.
    bound: HashMap<String, Option<String>>,
}

/// The query tabs of one connection.
#[derive(Default)]
pub(crate) struct QuerySessions {
    tabs: Mutex<Tabs>,
}

impl QuerySessions {
    /// A DDL operation never settles or borrows a user's transaction.
    pub(crate) fn check_ddl_idle(&self) -> Result<(), DbError> {
        let slots: Vec<_> = lock(&self.tabs).slots.values().cloned().collect();
        for slot in slots {
            let ts = slot.try_lock().map_err(|_| DbError::Busy)?;
            if ts.lost_tx || ts.session.as_ref().is_some_and(|s| s.in_transaction()) {
                return Err(DbError::TransactionOpen);
            }
        }
        Ok(())
    }

    fn get(&self, tab: &str) -> Option<TabSlot> {
        lock(&self.tabs).slots.get(tab).cloned()
    }

    /// The slot of `tab`, created on first use. Refused once `closed` (the runtime's
    /// flag, which `shutdown` sets before it drains the slots) is set.
    fn get_or_create(&self, tab: &str, closed: &AtomicBool) -> Result<TabSlot, DbError> {
        let mut tabs = lock(&self.tabs);
        if closed.load(Ordering::SeqCst) {
            return Err(closed_error());
        }
        Ok(Arc::clone(tabs.slots.entry(tab.to_string()).or_default()))
    }

    /// Records the database of `ts` (or that it has no session) for `tab`. Call after
    /// every change of `ts.session` / `ts.database`, still holding the slot.
    pub(crate) fn sync_bound(&self, tab: &str, ts: &TabSession) {
        let mut tabs = lock(&self.tabs);
        if (ts.session.is_some() || ts.reopen) && !ts.closed {
            tabs.bound.insert(tab.to_string(), ts.database.clone());
        } else {
            tabs.bound.remove(tab);
        }
    }

    /// The database `tab`'s session is on; `None` when the tab has no session here.
    fn bound(&self, tab: &str) -> Option<Option<String>> {
        lock(&self.tabs).bound.get(tab).cloned()
    }

    /// Whether `slot` is still the slot of `tab` (not closed and replaced meanwhile).
    fn is_slot(&self, tab: &str, slot: &TabSlot) -> bool {
        lock(&self.tabs)
            .slots
            .get(tab)
            .is_some_and(|s| Arc::ptr_eq(s, slot))
    }

    /// Forgets `tab` if `slot` is still its slot.
    fn remove_if(&self, tab: &str, slot: &TabSlot) {
        let mut tabs = lock(&self.tabs);
        if tabs.slots.get(tab).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            tabs.slots.remove(tab);
            tabs.bound.remove(tab);
        }
    }

    pub(crate) fn drain(&self) -> Vec<(String, TabSlot)> {
        let mut tabs = lock(&self.tabs);
        tabs.bound.clear();
        tabs.slots.drain().collect()
    }
}

/// Closes a slot taken out of its map, waiting at most `wait` for its holder.
pub(crate) async fn close_slot(tab: String, slot: TabSlot, wait: Duration) {
    match tokio::time::timeout(wait, slot.lock_owned()).await {
        Ok(mut ts) => ts.close().await,
        // The holder (a run) still has the session; it is dropped with the slot.
        Err(_) => tracing::warn!(tab = %tab, "the tab's session is busy; closing without it"),
    }
}

/// A [`RowSink`] that drops everything (`USE`, `COMMIT`, `ROLLBACK`).
struct NullSink;

impl RowSink for NullSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Runs one statement on a tab session, its results dropped.
async fn execute_quietly(session: &dyn Session, sql: &str) -> Result<(), DbError> {
    let kind = classify(sql, session.dialect());
    session
        .execute_one(sql, kind, &ExecOptions::default(), &mut NullSink)
        .await
        .map(|_| ())
}

/// A [`RowSink`] that keeps the columns and rows of the first result (an `EXPLAIN`).
#[derive(Default)]
struct CollectSink {
    started: bool,
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl RowSink for CollectSink {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        if !self.started {
            self.columns = cols.iter().map(|c| c.name.clone()).collect();
        }
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if !self.started {
            self.rows.extend(batch);
        }
    }
    fn result_end(&mut self, _summary: &ResultSummary) {
        self.started = true;
    }
}

/// One row estimate on `session` (see `SessionManager::estimate_rows`): `Ok` with what
/// the explain said (`Unavailable` when it failed), `Err` when the session is no longer
/// usable: its connection was lost, or the explain did not end within
/// [`ESTIMATE_TIMEOUT`] plus `grace` plus one second (the driver should have stopped it
/// by then; its state is unknown).
async fn explain_estimate(
    session: &dyn Session,
    sql: &str,
    dialect: Dialect,
    table: Option<&str>,
    savepoint: bool,
    grace: Duration,
) -> Result<RowEstimate, EstimateError> {
    let bound = ESTIMATE_TIMEOUT + grace + Duration::from_secs(1);
    let attempt = explain_estimate_once(session, sql, dialect, table, savepoint, grace);
    match tokio::time::timeout(bound, attempt).await {
        Ok(r) => r,
        Err(_) => Err(EstimateError::Lost(DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message: "a row estimate did not stop; the session was dropped".into(),
        })),
    }
}

/// The time limit of the classic `EXPLAIN` an Explain runs first.
const CLASSIC_EXPLAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The classic tabular `EXPLAIN` of an Explain (MySQL 8 / MariaDB; it never executes
/// the statement), run on the tab's session before the plan. Best-effort: any failure
/// (logged at `debug`) leaves the plan without it; a lost connection is left for the
/// plan's own statement to report.
async fn classic_explain(
    session: &dyn Session,
    sql: &str,
    dialect: Dialect,
    opts: &ExecOptions,
) -> Option<PlanTable> {
    let mut sink = CollectSink::default();
    match session
        .execute_one(sql, classify(sql, dialect), opts, &mut sink)
        .await
    {
        Ok(_) => Some(classic_table(&sink.columns, &sink.rows)),
        Err(e) => {
            tracing::debug!(error = %e, "the classic explain failed");
            None
        }
    }
}

/// Why a row estimate left the session in a state the caller must act on.
enum EstimateError {
    /// The connection was lost (or the explain never stopped): drop the session.
    Lost(DbError),
    /// A savepoint statement failed (the connection is fine): the user's PostgreSQL
    /// transaction may now be aborted, so planning must fail with this error.
    Savepoint(DbError),
}

async fn explain_estimate_once(
    session: &dyn Session,
    sql: &str,
    dialect: Dialect,
    table: Option<&str>,
    savepoint: bool,
    grace: Duration,
) -> Result<RowEstimate, EstimateError> {
    // The savepoint statements must work: any failure leaves the transaction aborted.
    let guarded = |r: Result<(), DbError>| match r {
        Err(e) if is_lost(&e) => Err(EstimateError::Lost(e)),
        Err(e) => Err(EstimateError::Savepoint(e)),
        Ok(()) => Ok(()),
    };
    if savepoint {
        guarded(execute_quietly(session, ESTIMATE_SAVEPOINT).await)?;
    }
    let opts = ExecOptions {
        max_rows: 1_000,
        timeout: Some(ESTIMATE_TIMEOUT),
        cancel_grace: grace,
        ..ExecOptions::default()
    };
    let mut sink = CollectSink::default();
    let ran = session
        .execute_one(sql, classify(sql, dialect), &opts, &mut sink)
        .await
        .map(|_| ());
    // A failed explain is only `Unavailable`, unless the connection went with it.
    let ok = match ran {
        Err(e) if is_lost(&e) => return Err(EstimateError::Lost(e)),
        Err(e) => {
            tracing::debug!(error = %e, "a row estimate failed");
            false
        }
        Ok(()) => true,
    };
    if savepoint {
        if !ok {
            guarded(execute_quietly(session, ESTIMATE_ROLLBACK).await)?;
        }
        guarded(execute_quietly(session, ESTIMATE_RELEASE).await)?;
    }
    Ok(if ok {
        parse_estimate(dialect, &sink.columns, &sink.rows, table)
    } else {
        RowEstimate::Unavailable
    })
}

/// A MySQL identifier in backticks (embedded backticks doubled).
fn quote_mysql_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// What a stored plan was made for: it only runs while its tab is still there.
#[derive(Clone)]
pub(crate) struct PlanTarget {
    tab: String,
    conn: Uuid,
    /// The runtime the plan was made on (a reopened connection is another one).
    runtime: Weak<ConnectionRuntime>,
    /// The database the tab's session was on.
    database: Option<String>,
}

/// An Explain plan: its statement's text and kind are the explain statement's;
/// `original` is the plan of the explained statement, which is what the guard judges.
#[derive(Clone)]
pub(crate) struct ExplainPlan {
    format: ExplainFormat,
    original: ScriptPlan,
    /// The plain `EXPLAIN` run first for the classic table (`ExplainSql::classic`).
    classic: Option<String>,
}

struct StoredPlan {
    target: PlanTarget,
    plan: ScriptPlan,
    explain: Option<ExplainPlan>,
    /// When the plan was made.
    at: Instant,
    /// `query_run` took it: it cannot be run again, only single statements of it.
    taken: bool,
    /// A derived plan (`query_plan_statement`: Retry / Fetch all): the user plan it
    /// was cut from. Taking it never evicts that plan.
    source: Option<Uuid>,
}

/// A copy of a stored plan.
#[derive(Clone)]
pub(crate) struct PlanCopy {
    target: PlanTarget,
    plan: ScriptPlan,
    explain: Option<ExplainPlan>,
    /// See [`StoredPlan::source`]; for a statement copy, the plan it was cut from (its
    /// own source when that plan is itself derived).
    source: Option<Uuid>,
}

/// Statement `index` of `plan` as a one-statement plan: its text is the whole source
/// (`source_range` `(0, len)`, `base` its editor start); dialect, explain flag, guard
/// context, index and range are kept.
fn one_statement(plan: &ScriptPlan, index: u32) -> Option<ScriptPlan> {
    let mut one = plan.stmts.iter().find(|s| s.index == index)?.clone();
    one.source_range = (0, one.text.len());
    Some(ScriptPlan {
        dialect: plan.dialect,
        source: one.text.clone(),
        base: one.range.0,
        stmts: vec![one],
        explain: plan.explain,
        guard: plan.guard,
    })
}

/// Plans by id. A pending (never taken) plan lives for the plan TTL and only the newest
/// one of a tab is kept. Of the taken user plans (`query_plan`, `query_explain`) only
/// the latest of each tab is kept (its results are what the tab shows: Retry / Fetch
/// all use it), with no TTL: it goes with the next take of a user plan of the tab, or
/// when the tab or its connection closes. Taking a derived plan (Retry / Fetch all)
/// keeps the user plan and only replaces the tab's previous taken derived plan.
#[derive(Default)]
pub(crate) struct PlanStore {
    plans: Mutex<HashMap<Uuid, StoredPlan>>,
}

impl PlanStore {
    fn prune(plans: &mut HashMap<Uuid, StoredPlan>, ttl: Duration) {
        plans.retain(|_, p| p.taken || p.at.elapsed() < ttl);
    }

    /// Stores a guarded plan; drops the tab's pending plans (a dialog the user
    /// abandoned): only the newest pending plan of a tab can run.
    fn insert(
        &self,
        ttl: Duration,
        target: PlanTarget,
        plan: ScriptPlan,
        explain: Option<ExplainPlan>,
        source: Option<Uuid>,
    ) -> Uuid {
        debug_assert!(
            explain
                .as_ref()
                .map_or(&plan, |e| &e.original)
                .guard
                .is_some(),
            "stored plans must be guarded"
        );
        let id = Uuid::new_v4();
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        plans.retain(|_, p| p.taken || p.target.tab != target.tab);
        plans.insert(
            id,
            StoredPlan {
                target,
                plan,
                explain,
                at: Instant::now(),
                taken: false,
                source,
            },
        );
        id
    }

    /// The tab and connection of plan `id`, taken or not.
    fn locate(
        &self,
        ttl: Duration,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<(String, Uuid), DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        Ok((p.target.tab.clone(), p.target.conn))
    }

    /// A copy of the pending (not taken) plan `id`.
    fn pending(
        &self,
        ttl: Duration,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<PlanCopy, DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        if p.taken {
            return Err(DbError::PlanExpired);
        }
        Ok(PlanCopy {
            target: p.target.clone(),
            plan: p.plan.clone(),
            explain: p.explain.clone(),
            source: p.source,
        })
    }

    /// The live plan `id` when `current` still accepts its target; a plan whose target
    /// is gone is dropped.
    fn checked<'a>(
        plans: &'a mut HashMap<Uuid, StoredPlan>,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<&'a mut StoredPlan, DbError> {
        let ok = plans.get(&id).map(|p| current(&p.target));
        match ok {
            Some(true) => plans.get_mut(&id).ok_or(DbError::PlanExpired),
            Some(false) => {
                plans.remove(&id);
                Err(DbError::PlanExpired)
            }
            None => Err(DbError::PlanExpired),
        }
    }

    fn take(
        &self,
        ttl: Duration,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        if p.taken {
            return Err(DbError::PlanExpired);
        }
        p.taken = true;
        let derived = p.source.is_some();
        let taken = (p.target.tab.clone(), p.target.conn, p.plan.clone());
        // Of the tab's taken plans, a user plan replaces all others; a derived plan
        // replaces only the previous derived one (its source stays).
        plans.retain(|pid, p| {
            *pid == id || !p.taken || p.target.tab != taken.0 || (derived && p.source.is_none())
        });
        Ok(taken)
    }

    /// Statement `index` of plan `id` (taken or not) as a one-statement plan.
    fn statement(
        &self,
        ttl: Duration,
        id: Uuid,
        index: u32,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<PlanCopy, DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        let plan = one_statement(&p.plan, index).ok_or(DbError::PlanExpired)?;
        let explain = match &p.explain {
            Some(e) => Some(ExplainPlan {
                format: e.format,
                original: one_statement(&e.original, index).ok_or(DbError::PlanExpired)?,
                classic: e.classic.clone(),
            }),
            None => None,
        };
        Ok(PlanCopy {
            target: p.target.clone(),
            plan,
            explain,
            source: Some(p.source.unwrap_or(id)),
        })
    }

    pub(crate) fn drop_tab(&self, tab: &str) {
        lock(&self.plans).retain(|_, p| p.target.tab != tab);
    }

    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.target.conn != conn);
    }

    fn drop_tab_on(&self, tab: &str, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.target.tab != tab || p.target.conn != conn);
    }
}

impl ConnectionRuntime {
    /// The database a tab's session should be on: the requested one, else the connect
    /// database (none on MySQL without a default database).
    fn query_database(&self, database: Option<String>) -> Option<String> {
        database
            .filter(|d| !d.is_empty())
            .or_else(|| self.home_database())
    }

    /// Opens a query session on `database` through the connection's tunnel, reopening
    /// the tunnel first when it died (as metadata calls do). Returns the session and the
    /// tunnel generation it was opened through.
    pub(crate) async fn open_query_session(
        &self,
        database: Option<String>,
    ) -> Result<(Box<dyn Session>, u64), DbError> {
        let (resolved, generation) = {
            let mut link = self.link.lock().await;
            self.ensure_open(&link)?;
            if self.tunnel_dead() {
                self.set_state(ConnState::Reconnecting);
                let reopened = self.replace_dead_tunnel(&mut link).await;
                self.set_state(if reopened.is_ok() {
                    ConnState::Connected
                } else {
                    ConnState::Failed
                });
                reopened?;
            }
            (
                with_database(&link.resolved, database),
                self.tunnel_gen.load(Ordering::SeqCst),
            )
        };
        let session = connect_db(self.driver.as_ref(), &resolved).await?;
        Ok((session, generation))
    }

    /// A session opened through a tunnel that died or was replaced since cannot reach
    /// the server any more (its driver would reconnect to the old local port).
    fn is_stale(&self, ts: &TabSession) -> bool {
        ts.session.is_some()
            && (self.tunnel_dead() || ts.tunnel_gen != self.tunnel_gen.load(Ordering::SeqCst))
    }

    /// Puts `ts` on the database `database` resolves to: opens the session if there is
    /// none; otherwise switches (refused with an open transaction): MySQL runs `USE`,
    /// PostgreSQL opens a session on the new database and closes the old one.
    ///
    /// A stale session (see [`Self::is_stale`]) is closed first. Idle, it is simply
    /// replaced; with a transaction open, which the server has lost with the tunnel, the
    /// call fails once with `ConnectionLost { was_in_tx: true }` and the next one opens
    /// a fresh session.
    async fn ensure_tab_session(
        &self,
        ts: &mut TabSession,
        database: Option<String>,
    ) -> Result<(), DbError> {
        if self.is_stale(ts) {
            let in_tx = ts.tx_open();
            if let Some(old) = ts.session.take() {
                close_quietly(old).await;
            }
            if in_tx {
                // As after a run that lost its session in a transaction: a Commit now
                // must not look like it worked.
                ts.set_lost_tx(true, false);
                return Err(DbError::ConnectionLost {
                    was_in_tx: true,
                    outcome_unknown: false,
                    message: "the SSH tunnel was re-established; the open transaction was lost"
                        .into(),
                });
            }
        }
        let target = self.query_database(database);
        let Some(session) = ts.session.as_deref() else {
            let (session, generation) = self.open_query_session(target.clone()).await?;
            ts.session = Some(session);
            ts.tunnel_gen = generation;
            ts.database = target;
            ts.reopen = false;
            ts.set_lost_tx(false, false);
            return Ok(());
        };
        if ts.database == target {
            return Ok(());
        }
        if session.tx_state() != TxState::Idle {
            return Err(DbError::TransactionOpen);
        }
        match (self.cfg.kind, &target) {
            (DbKind::MySql, Some(db)) => {
                execute_quietly(session, &format!("USE {}", quote_mysql_ident(db))).await?;
            }
            // PostgreSQL, or MySQL back to "no database" (which `USE` cannot do).
            _ => {
                let (fresh, generation) = self.open_query_session(target.clone()).await?;
                if let Some(old) = ts.session.replace(fresh) {
                    close_quietly(old).await;
                }
                ts.tunnel_gen = generation;
            }
        }
        ts.database = target;
        Ok(())
    }
}

impl SessionManager {
    /// Overrides [`PLAN_TTL`] (tests).
    pub fn with_plan_ttl(mut self, ttl: Duration) -> Self {
        self.plan_ttl = ttl;
        self
    }

    /// Marks connection `conn`'s SSH tunnel as replaced, as a dead tunnel being reopened
    /// does (tests: the fake driver has no tunnel).
    #[doc(hidden)]
    pub fn simulate_tunnel_replaced(&self, conn: Uuid) {
        if let Some(rt) = self.runtime_opt(conn) {
            rt.tunnel_gen.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The connection's config as stored now (env tag and read-only may have been edited
    /// since it was opened); the one it was opened with when it cannot be read.
    fn current_config(&self, rt: &ConnectionRuntime) -> ConnectionConfig {
        match self.repo.get(rt.cfg.id) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(connection = %rt.cfg.id, error = %e, "could not re-read the connection");
                rt.cfg.clone()
            }
        }
    }

    /// What the guard judges a plan with on a connection whose config is `cfg` (as
    /// stored now): its env tag and read-only flag, and the app's "Confirm UPDATE/DELETE
    /// without WHERE" setting (Production and Staging confirm unbounded writes regardless).
    fn guard_ctx(&self, cfg: &ConnectionConfig, analyze: bool) -> GuardCtx {
        GuardCtx {
            env: cfg.env_tag,
            read_only: cfg.read_only,
            analyze,
            confirm_unbounded: self.confirm_unbounded(),
        }
    }

    /// The stored "Confirm UPDATE/DELETE without WHERE" setting (on by default).
    fn confirm_unbounded(&self) -> bool {
        saved_confirm_unbounded(&self.repo)
    }

    /// Whether a stored plan's tab is still where the plan was made: the same runtime of
    /// the connection (not closed, not reopened since) and a session on the same database.
    fn plan_target_current(&self, t: &PlanTarget) -> bool {
        self.runtime_opt(t.conn).is_some_and(|rt| {
            std::ptr::eq(Arc::as_ptr(&rt), t.runtime.as_ptr())
                && !rt.closed.load(Ordering::SeqCst)
                && rt.queries.bound(&t.tab).as_ref() == Some(&t.database)
        })
    }

    /// Plans `target` of `text` on tab `tab`, bound to connection `conn` and `database`
    /// (`None`: the connect database), and judges every statement with the guard. The
    /// tab's session opens on first use; see the module docs for switching connection or
    /// database. Errors: `Config` (bad tab id), `NotOpen`, `Busy` (the tab is running),
    /// `TransactionOpen` (switching away from an open transaction), `ConnectionLost`
    /// (once, after the SSH tunnel was replaced under an open transaction), connect
    /// errors.
    pub async fn query_plan(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
        text: &str,
        target: Target,
    ) -> Result<PlanView, DbError> {
        let (rt, mut ts) = self.tab_session(tab, conn, database).await?;
        let (dialect, _, _) = session_facts(&ts)?;
        let plan_target = PlanTarget {
            tab: tab.to_string(),
            conn,
            runtime: Arc::downgrade(&rt),
            database: ts.database.clone(),
        };

        let cfg = self.current_config(&rt);
        let mut planned = plan(text, target, dialect);
        guard_with(&mut planned, self.guard_ctx(&cfg, false));
        if matches!(cfg.env_tag, Some(EnvTag::Staging | EnvTag::Production)) {
            self.estimate_rows(&rt, tab, &mut ts, &mut planned).await?;
        }
        // After the estimates (which never change it, short of a lost connection).
        let tx_state = ts
            .session
            .as_deref()
            .map_or(TxState::Idle, |s| s.tx_state());
        drop(ts);
        self.mark_implicit_commits(tab, &mut planned, tx_state);
        let copy = PlanCopy {
            target: plan_target,
            plan: planned,
            explain: None,
            source: None,
        };
        self.store_plan(&rt, &cfg, copy, dialect, tx_state)
    }

    /// Row estimates (spec §4.3), on the tab's own held session: for every statement of
    /// `planned` that is not blocked and holds an unbounded `UPDATE` / `DELETE`, runs
    /// `EXPLAIN` (never `ANALYZE`: nothing executes) with [`ESTIMATE_TIMEOUT`] and stores
    /// what it says in `estimate`. Best-effort: a statement that cannot be explained
    /// safely ([`estimate_sql`]), a failed or timed-out explain and every statement past
    /// [`MAX_ESTIMATES`] are `Unavailable`. Nothing is sent (`Unavailable`) when:
    ///
    /// - an earlier statement of the plan is not a plain read: the explain runs in the
    ///   session as it is BEFORE the script, so a `USE other_db`, `SET search_path`,
    ///   DDL, write or transaction control earlier in the script could make it estimate
    ///   another table (or see other rows), a confident wrong number;
    /// - the tab's transaction is aborted;
    /// - the tab's transaction is open but nothing has read or written in it yet
    ///   ([`TabSession::tx_fresh`]): the explain would take the transaction's first
    ///   snapshot, changing what a REPEATABLE READ / SERIALIZABLE transaction sees and
    ///   making a later `SET TRANSACTION` fail. (A `SET TRANSACTION` in the same plan is
    ///   covered by the first rule.)
    ///
    /// The tab's transaction state is kept: an explain does not change it on MySQL, and
    /// inside an open PostgreSQL transaction it runs under a savepoint, rolled back to
    /// when it fails (a failure would abort the transaction otherwise). Should a
    /// savepoint statement itself fail, the transaction may be aborted: planning fails
    /// with that server error.
    ///
    /// A lost connection (or an explain that does not stop after its timeout and
    /// grace) drops the session as after a run. Outside a transaction
    /// the tab keeps its binding and reopens on its next use, and planning goes on;
    /// inside one, `ConnectionLost { was_in_tx: true }` is returned (the server rolled
    /// the transaction back).
    async fn estimate_rows(
        &self,
        rt: &ConnectionRuntime,
        tab: &str,
        ts: &mut TabSession,
        planned: &mut ScriptPlan,
    ) -> Result<(), DbError> {
        let dialect = planned.dialect;
        let mut done = 0usize;
        // Whether every statement before this one is a plain read: only then is the
        // session, as it is now, the one the statement will run in.
        let mut reads_before = true;
        for s in &mut planned.stmts {
            let clean = reads_before;
            reads_before &= s.kind == StatementKind::Read;
            let flagged = s.verdict != Verdict::Block
                && s.unbounded
                    .iter()
                    .any(|w| matches!(w.op, WriteOp::Update | WriteOp::Delete));
            if !flagged {
                continue;
            }
            s.estimate = Some(RowEstimate::Unavailable);
            let Some(session) = ts.session.as_deref() else {
                continue;
            };
            let state = session.tx_state();
            // An earlier statement of the script (`USE`, `SET search_path`, DDL, a
            // write, transaction control) may change what this one touches; and an
            // EXPLAIN would take the first snapshot of a transaction nothing has read
            // in yet.
            let fresh_tx = state == TxState::Active && ts.tx_fresh;
            if !clean || fresh_tx || state == TxState::Aborted || done >= MAX_ESTIMATES {
                continue;
            }
            let Some(sql) = estimate_sql(&s.text, dialect) else {
                continue;
            };
            done += 1;
            let table = estimate_table(&s.unbounded);
            let savepoint = rt.cfg.kind == DbKind::Postgres && state == TxState::Active;
            match explain_estimate(
                session,
                &sql,
                dialect,
                table.as_deref(),
                savepoint,
                self.cancel_grace,
            )
            .await
            {
                Ok(estimate) => s.estimate = Some(estimate),
                Err(EstimateError::Savepoint(e)) => {
                    // The user's transaction may now be aborted: say so, never plan
                    // on as if nothing happened.
                    tracing::warn!(connection = %rt.cfg.id, error = %e, "a row estimate could not restore the transaction");
                    return Err(match e {
                        DbError::Server {
                            code,
                            sqlstate,
                            message,
                            ..
                        } => DbError::Server {
                            code,
                            sqlstate,
                            message: format!(
                                "Estimating rows could not restore the open transaction, \
                                 which may now be aborted: {message}"
                            ),
                            position: None,
                        },
                        other => other,
                    });
                }
                Err(EstimateError::Lost(e)) => {
                    tracing::warn!(connection = %rt.cfg.id, error = %e, "a row estimate lost the tab's session");
                    let in_tx = state != TxState::Idle;
                    if let Some(dead) = ts.session.take() {
                        close_quietly(dead).await;
                    }
                    ts.set_lost_tx(in_tx, false);
                    ts.reopen = !in_tx;
                    rt.queries.sync_bound(tab, ts);
                    if in_tx {
                        return Err(DbError::ConnectionLost {
                            was_in_tx: true,
                            outcome_unknown: false,
                            message: "the connection was lost while estimating rows; \
                                      the open transaction was rolled back"
                                .into(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Opens (or moves) the session of `tab` on `conn` / `database` and returns it held:
    /// see [`Self::query_plan`] for the rules and errors.
    async fn tab_session(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
    ) -> Result<(Arc<ConnectionRuntime>, OwnedMutexGuard<TabSession>), DbError> {
        validate_tab(tab)?;
        let rt = self.runtime(conn)?;
        self.release_tab_elsewhere(tab, conn).await?;
        let mut ts = loop {
            let slot = rt.queries.get_or_create(tab, &rt.closed)?;
            let ts = slot.try_lock_owned().map_err(|_| DbError::Busy)?;
            if !ts.closed {
                break ts;
            }
            // Closed while we got it (the tab was closed meanwhile): use a fresh slot.
        };
        let ensured = self.ensure_session(&rt, tab, &mut ts, database).await;
        rt.queries.sync_bound(tab, &ts);
        ensured?;
        Ok((rt, ts))
    }

    /// Stores a guarded plan (for an Explain, `copy.plan` is the explain statement and
    /// `copy.explain.original` the guarded plan of the explained one) and returns its
    /// view.
    fn store_plan(
        &self,
        rt: &ConnectionRuntime,
        cfg: &ConnectionConfig,
        copy: PlanCopy,
        dialect: Dialect,
        tx_state: TxState,
    ) -> Result<PlanView, DbError> {
        let PlanCopy {
            target,
            plan: planned,
            explain,
            source,
        } = copy;
        let judged = explain.as_ref().map_or(&planned, |e| &e.original);
        let stmts = judged
            .stmts
            .iter()
            .map(|s| PlanStmtView {
                index: s.index,
                range: s.range,
                kind: s.kind.into(),
                verdict: s.verdict,
                preview: preview(&s.text),
                unbounded: s.unbounded.clone(),
                drops: s.drops.clone(),
                confirm_name: s.confirm_name.clone(),
                estimate: s.estimate,
                implicit_commit: s.implicit_commit,
            })
            .collect();
        // The connection closed while we planned: `shutdown` closes the tab's session;
        // keep no plan for it.
        if rt.closed.load(Ordering::SeqCst) {
            return Err(closed_error());
        }
        let plan_id = self
            .plans
            .insert(self.plan_ttl, target, planned, explain, source);
        Ok(PlanView {
            plan_id,
            stmts,
            dialect,
            connection_name: cfg.name.clone(),
            env_tag: cfg.env_tag,
            read_only: cfg.read_only,
            tx_state,
        })
    }

    /// Closes the session of `tab` on every connection other than `conn`: a tab is on one
    /// connection at a time. Refused while that session runs (`Busy`) or has a
    /// transaction open (`TransactionOpen`).
    async fn release_tab_elsewhere(&self, tab: &str, conn: Uuid) -> Result<(), DbError> {
        for (id, rt) in self.runtimes_snapshot() {
            if id == conn {
                continue;
            }
            let Some(slot) = rt.queries.get(tab) else {
                continue;
            };
            let mut ts = Arc::clone(&slot)
                .try_lock_owned()
                .map_err(|_| DbError::Busy)?;
            if ts.tx_open() {
                return Err(DbError::TransactionOpen);
            }
            rt.queries.remove_if(tab, &slot);
            ts.close().await;
            self.plans.drop_tab_on(tab, id);
        }
        Ok(())
    }

    /// The runtime and slot of `tab`, on whichever connection it is.
    fn find_tab_runtime(&self, tab: &str) -> Option<(Arc<ConnectionRuntime>, TabSlot)> {
        self.runtimes_snapshot()
            .into_iter()
            .find_map(|(_, rt)| rt.queries.get(tab).map(|slot| (rt, slot)))
    }

    /// [`ConnectionRuntime::ensure_tab_session`] for tab `tab`. Opening a fresh session
    /// forgets a lost transaction ([`TabSession::lost_tx`]), except in a Manual tab: there
    /// it is kept until the user acknowledges it with `query_tx`, and runs are refused
    /// meanwhile (see [`Self::query_run_named`]).
    async fn ensure_session(
        &self,
        rt: &ConnectionRuntime,
        tab: &str,
        ts: &mut TabSession,
        database: Option<String>,
    ) -> Result<(), DbError> {
        let keep =
            (ts.lost_tx && self.tx_mode(tab) == TxMode::Manual).then_some(ts.lost_tx_unknown);
        let ensured = rt.ensure_tab_session(ts, database).await;
        if let Some(unknown) = keep {
            ts.set_lost_tx(true, unknown);
        }
        ensured
    }

    /// The commit mode of `tab` ([`TxMode::Auto`] until set).
    fn tx_mode(&self, tab: &str) -> TxMode {
        lock(&self.tx_modes).get(tab).copied().unwrap_or_default()
    }

    /// Flags the statements of `planned` that commit an open transaction
    /// ([`mark_implicit_commits`]), from the tab session's state `state` when the plan
    /// was made; in a Manual tab, as the run will begin a transaction before every
    /// statement that needs one (`dnt_query::RunConfig::manual_tx`).
    fn mark_implicit_commits(&self, tab: &str, planned: &mut ScriptPlan, state: TxState) {
        let manual = self.tx_mode(tab) == TxMode::Manual;
        mark_implicit_commits(planned, state, manual);
    }

    /// Sets the commit mode of `tab` (spec §4.2e) and returns the tab's transaction
    /// state. The mode belongs to the tab, whatever connection it is on, from now until
    /// the tab is closed (`query_close_tab`); a tab without a session takes it too.
    /// Errors: `config` (bad tab id), `busy` while the tab runs, `transaction_open` when
    /// a Manual tab with a transaction open is switched to Auto (commit or roll back
    /// first), `connection_lost` (`was_in_tx`) when its transaction was lost with the
    /// connection and `query_tx` has not acknowledged it yet.
    pub async fn query_tx_mode(&self, tab: &str, mode: TxMode) -> Result<TxState, DbError> {
        validate_tab(tab)?;
        // Held while the mode changes, so a run of the tab cannot start meanwhile.
        let held = match self.find_tab_runtime(tab) {
            Some((_, slot)) => Some(slot.try_lock_owned().map_err(|_| DbError::Busy)?),
            None => None,
        };
        let state = held
            .as_ref()
            .and_then(|ts| ts.session.as_deref())
            .map_or(TxState::Idle, |s| s.tx_state());
        let mut modes = lock(&self.tx_modes);
        let current = modes.get(tab).copied().unwrap_or_default();
        if current == TxMode::Manual && mode == TxMode::Auto {
            if state != TxState::Idle {
                return Err(DbError::TransactionOpen);
            }
            // A lost transaction not acknowledged yet (`query_tx`) keeps the tab Manual.
            if let Some(ts) = held.as_ref().filter(|ts| ts.lost_tx) {
                return Err(ts.lost_tx_error());
            }
        }
        match mode {
            TxMode::Auto => modes.remove(tab),
            TxMode::Manual => modes.insert(tab.to_string(), mode),
        };
        Ok(state)
    }

    /// The tab's transaction: its state, when it began and how many statements ran in
    /// it (for the idle-transaction warning, setting `idle_tx_warn_minutes`). A tab
    /// without a session is idle. `busy` while the tab runs.
    pub async fn query_tx_info(&self, tab: &str) -> Result<TabTxInfo, DbError> {
        validate_tab(tab)?;
        let Some((_, slot)) = self.find_tab_runtime(tab) else {
            return Ok(TabTxInfo::IDLE);
        };
        let ts = slot.try_lock_owned().map_err(|_| DbError::Busy)?;
        Ok(ts.tx_info())
    }

    /// Commits or rolls back the tab's transaction (`execute_one("COMMIT"/"ROLLBACK")`)
    /// and returns the session's new state. A tab without a session is `Idle`, except
    /// once after its session was dropped with a transaction open (a failed run, see
    /// [`TabSession::lost_tx`]): that is `ConnectionLost { was_in_tx }`.
    pub async fn query_tx(&self, tab: &str, action: TxAction) -> Result<TxState, DbError> {
        self.query_tx_inner(tab, action).await.map_err(|(e, _)| e)
    }

    /// [`Self::query_tx`] for the UI: a failed `COMMIT` / `ROLLBACK` reports the
    /// session's transaction state after the failure in `AppError::tx_state` (`idle`
    /// after a lost connection: the tab has no transaction any more). A `COMMIT` that
    /// lost its connection or timed out after it was sent has `outcome_unknown` set: it
    /// may have committed (spec §4.2d), so it is never reported as rolled back.
    // `AppError` is the IPC payload the command returns as is (see `commands`).
    #[allow(clippy::result_large_err)]
    pub async fn query_tx_reported(
        &self,
        tab: &str,
        action: TxAction,
    ) -> Result<TxState, AppError> {
        self.query_tx_inner(tab, action)
            .await
            .map_err(|(e, state)| {
                let mut app = AppError::from(e);
                app.tx_state = state.map(|s| s.as_str().to_string());
                app
            })
    }

    /// The error comes with the session's state after a failed statement.
    async fn query_tx_inner(
        &self,
        tab: &str,
        action: TxAction,
    ) -> Result<TxState, (DbError, Option<TxState>)> {
        validate_tab(tab).map_err(|e| (e, None))?;
        let Some((rt, slot)) = self.find_tab_runtime(tab) else {
            return Ok(TxState::Idle);
        };
        let mut ts = Arc::clone(&slot)
            .try_lock_owned()
            .map_err(|_| (DbError::Busy, None))?;
        // The session died with a transaction open (rolled back, or maybe committed:
        // `lost_tx_unknown`), so a Commit must not look like it worked. Nothing is
        // sent: this call is the acknowledgement (a Manual tab's runs are refused until
        // it, see `query_run_named`), also when the tab's session was reopened since —
        // unless a transaction is open on it now: that one is new, and its Commit /
        // Rollback is sent (the stale loss is forgotten).
        if ts.lost_tx {
            if !ts.tx_open() {
                let lost = ts.lost_tx_error();
                ts.set_lost_tx(false, false);
                return Err((lost, Some(TxState::Idle)));
            }
            ts.set_lost_tx(false, false);
        }
        let Some(session) = ts.session.as_deref() else {
            return Ok(TxState::Idle);
        };
        let sent = execute_quietly(session, action.sql()).await;
        let state = session.tx_state();
        match sent {
            Ok(()) => {
                ts.forget_tx_if_idle();
                Ok(state)
            }
            Err(e) => {
                // A COMMIT that may have gone through is never reported as rolled back.
                let (e, unknown) = match action {
                    TxAction::Commit => {
                        let e = commit_outcome_unknown(e);
                        let unknown = AppError::from(e.clone()).outcome_unknown;
                        (e, unknown)
                    }
                    TxAction::Rollback => (e, false),
                };
                if is_lost(&e) || unknown {
                    // Dropped: after a lost connection the session is dead; after a
                    // COMMIT whose outcome is unknown, its transaction state cannot be
                    // trusted (a Rollback would "succeed" whether or not it committed).
                    // The tab keeps its database and reopens on its next use.
                    if let Some(dead) = ts.session.take() {
                        close_quietly(dead).await;
                    }
                    ts.reopen = unknown;
                    if rt.queries.is_slot(tab, &slot) {
                        rt.queries.sync_bound(tab, &ts);
                    }
                    ts.forget_tx_if_idle();
                    Err((e, Some(TxState::Idle)))
                } else {
                    ts.forget_tx_if_idle();
                    Err((e, Some(state)))
                }
            }
        }
    }

    /// Closes the tab's session and forgets its plans and commit mode. Without `force`,
    /// a session with a transaction open is kept (with the mode) and `TransactionOpen`
    /// returned, so the UI can ask
    /// Commit / Rollback / Cancel (spec §7.2); with `force` the session is closed anyway
    /// (the server rolls the transaction back). Waits up to [`SLOT_CLOSE_WAIT`] for a
    /// holder of the session: without `force` that ends in `Busy`. Closing an unknown
    /// tab is a no-op.
    ///
    /// A run of the tab is stopped FIRST, before the open-transaction check: a call that
    /// then returns `TransactionOpen` (or `Busy`, when the stopped run does not release
    /// the session within [`SLOT_CLOSE_WAIT`]) has still stopped the run. The UI must
    /// therefore ask the user before closing a tab whose run is active.
    pub async fn query_close_tab(&self, tab: &str, force: bool) -> Result<(), DbError> {
        validate_tab(tab)?;
        // A run of the tab is stopped first (it still ends with `done`), so closing does
        // not wait for it to finish on its own.
        self.stop_runs(|r| r.tab == tab).await;
        for (_, rt) in self.runtimes_snapshot() {
            let Some(slot) = rt.queries.get(tab) else {
                continue;
            };
            match tokio::time::timeout(SLOT_CLOSE_WAIT, Arc::clone(&slot).lock_owned()).await {
                Ok(mut ts) => {
                    if !force && ts.tx_open() {
                        return Err(DbError::TransactionOpen);
                    }
                    rt.queries.remove_if(tab, &slot);
                    ts.close().await;
                }
                Err(_) if !force => return Err(DbError::Busy),
                Err(_) => {
                    rt.queries.remove_if(tab, &slot);
                    tracing::warn!(tab = %tab, "the tab's session is busy; closing without it");
                }
            }
        }
        self.plans.drop_tab(tab);
        lock(&self.tx_modes).remove(tab);
        self.results.remove_tab(tab);
        Ok(())
    }

    /// Takes plan `plan_id` for `query_run`: `(tab, connection, plan)`. A plan runs once;
    /// unknown, expired, replaced or already taken plans are `PlanExpired`, and so is a
    /// plan whose tab has since moved to another connection or database, or whose
    /// connection was closed or reopened. Taking it drops the tab's previous taken plan.
    ///
    /// The plan was guarded with the config as it was when it was made (`plan.guard` is
    /// set); `query_run` judges it again with the current config before it runs. Not
    /// public: nothing outside may take a plan around `query_run`'s re-judgement (tests
    /// use [`SessionManager::test_take_plan`]).
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn take_plan(&self, plan_id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plans
            .take(self.plan_ttl, plan_id, &|t| self.plan_target_current(t))
    }

    /// Statement `index` of plan `plan_id` (taken or not) as a one-statement plan, for
    /// Retry and Fetch all: the stored statement text runs verbatim, never re-planned
    /// (so its `DELIMITER` context is kept). Keeps the plan's dialect, guard context and
    /// the statement's index and range; the plan's `source` is just the statement (its
    /// `source_range` is `(0, len)`, `base` its editor start). `PlanExpired` when the
    /// plan or index is unknown or the plan's target changed (see [`Self::take_plan`]).
    ///
    /// To run it, [`Self::query_plan_statement`] stores it as a new plan (tests use
    /// [`SessionManager::test_plan_statement`]).
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let copy = self.plans.statement(self.plan_ttl, plan_id, index, &|t| {
            self.plan_target_current(t)
        })?;
        Ok((copy.target.tab, copy.target.conn, copy.plan))
    }

    /// [`Self::take_plan`] for integration tests (feature `test-helpers`).
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn test_take_plan(&self, plan_id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.take_plan(plan_id)
    }

    /// [`Self::plan_statement`] for integration tests (feature `test-helpers`).
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn test_plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plan_statement(plan_id, index)
    }

    /// Retry / Fetch all: stores statement `index` of plan `plan_id` (taken or not) as a
    /// new pending plan of the same tab, judged by the guard with the connection's
    /// current config, and returns its view (confirm it like any plan, then
    /// `query_run`). The stored statement text runs verbatim, never re-planned, so its
    /// `DELIMITER` context is kept. `PlanExpired` when the plan or index is unknown or
    /// its target changed; `Busy` while the tab runs.
    pub async fn query_plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<PlanView, DbError> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let (tab, conn) = self.plans.locate(self.plan_ttl, plan_id, &current)?;
        let rt = self.runtime(conn)?;
        let slot = rt.queries.get(&tab).ok_or(DbError::PlanExpired)?;
        let mut ts = Arc::clone(&slot)
            .try_lock_owned()
            .map_err(|_| DbError::Busy)?;
        let mut copy = self
            .plans
            .statement(self.plan_ttl, plan_id, index, &current)?;
        if ts.closed || ts.database != copy.target.database {
            return Err(DbError::PlanExpired);
        }
        if ts.session.is_none() && ts.reopen {
            // Dropped after a lost connection: reopen on the tab's database.
            let ensured = self
                .ensure_session(&rt, &tab, &mut ts, copy.target.database.clone())
                .await;
            if rt.queries.is_slot(&tab, &slot) {
                rt.queries.sync_bound(&tab, &ts);
            }
            ensured?;
        }
        let (dialect, tx_state, _) = session_facts(&ts)?;
        drop(ts);
        let cfg = self.current_config(&rt);
        let judged = match &mut copy.explain {
            Some(e) => &mut e.original,
            None => &mut copy.plan,
        };
        let analyze = judged.guard.is_some_and(|g| g.analyze);
        guard_with(judged, self.guard_ctx(&cfg, analyze));
        if copy.explain.is_none() {
            self.mark_implicit_commits(&tab, &mut copy.plan, tx_state);
        }
        self.store_plan(&rt, &cfg, copy, dialect, tx_state)
    }

    /// The engine of plan `plan_id`'s connection (an export's SQL format needs it before
    /// anything runs). `PlanExpired` as for [`Self::query_plan_statement`].
    pub fn plan_kind(&self, plan_id: Uuid) -> Result<DbKind, DbError> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let (_, conn) = self.plans.locate(self.plan_ttl, plan_id, &current)?;
        Ok(self.runtime(conn)?.cfg.kind)
    }

    /// Export of all rows: runs statement `index` of plan `plan_id` (taken or not) again
    /// ON THE TAB'S OWN SESSION, as Fetch all does, streaming every row (binary values
    /// whole, no row limit, no statement timeout) into `sink` instead of the grid. The
    /// session keeps what the user's result was read under: `sql_mode` (so the dialect
    /// the plan was split with is the server's), user variables, `search_path`, role,
    /// temporary tables and the open transaction.
    ///
    /// - Only while the tab is idle: a tab that is running (or planning) is refused at
    ///   once with a `config` error (field `tab`), never queued.
    /// - Only a statement the planner classified as a read (one statement for the
    ///   server, not an Explain) that the guard allows with the connection's current
    ///   config without confirmation; anything else is a `config` error (field `scope`):
    ///   export the loaded rows instead.
    /// - Right before it is sent, the statement is split, classified and judged again
    ///   with the session's CURRENT dialect (a `SET sql_mode` since the plan): if it no
    ///   longer is one allowed read, it is refused (field `scope`).
    /// - While it runs it is a run of the tab under `run_id` (a fresh id from the UI):
    ///   `query_cancel(run_id)`, closing the tab and closing the connection stop it,
    ///   with a run's cancel grace. A write error of the sink is reported through
    ///   `failed` (`Notify::notify_one`), which cancels the statement too.
    ///
    /// A failure comes with the session's transaction state after it (`None` when the
    /// statement never ran), as `query_tx_reported` reports it.
    ///
    /// [`Self::query_export_begin`] then [`ExportRun::execute`]; the export command
    /// opens its file between the two, once the tab is held.
    pub async fn query_export(
        &self,
        plan_id: Uuid,
        index: u32,
        run_id: Uuid,
        sink: &mut dyn RowSink,
        failed: &tokio::sync::Notify,
    ) -> Result<(), (DbError, Option<TxState>)> {
        self.query_export_begin(plan_id, index, run_id)
            .await?
            .execute(sink, failed)
            .await
    }

    /// The checks of [`Self::query_export`], up to the statement it will send: holds the
    /// tab's session (refused while the tab runs), judges the statement, reopens the
    /// session when needed and registers the run `run_id`. Nothing is sent and nothing
    /// is written yet; dropping the returned run releases the tab and the run id.
    pub async fn query_export_begin(
        &self,
        plan_id: Uuid,
        index: u32,
        run_id: Uuid,
    ) -> Result<ExportRun, (DbError, Option<TxState>)> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let early = |e: DbError| (e, None);
        let (tab, conn) = self
            .plans
            .locate(self.plan_ttl, plan_id, &current)
            .map_err(early)?;
        let rt = self.runtime(conn).map_err(early)?;
        let slot = rt.queries.get(&tab).ok_or((DbError::PlanExpired, None))?;
        let mut ts = Arc::clone(&slot).try_lock_owned().map_err(|_| {
            early(config_error(
                "tab",
                "The tab is running a statement. Wait for it to finish (or stop it), \
                 then export all rows.",
            ))
        })?;
        let copy = self
            .plans
            .statement(self.plan_ttl, plan_id, index, &current)
            .map_err(early)?;
        if ts.closed || (ts.session.is_none() && !ts.reopen) || ts.database != copy.target.database
        {
            return Err(early(DbError::PlanExpired));
        }
        // Judged with the same guard context as a run (config and the unbounded-write
        // setting as they are now).
        let ctx = self.guard_ctx(&self.current_config(&rt), false);
        let stmt = export_read(&copy, ctx).map_err(early)?;
        // A stale session is replaced; one dropped after a lost connection reopens.
        if rt.is_stale(&ts) || ts.session.is_none() {
            let ensured = self
                .ensure_session(&rt, &tab, &mut ts, copy.target.database.clone())
                .await;
            rt.queries.sync_bound(&tab, &ts);
            ensured.map_err(|e| {
                let state = tx_state_after(&e);
                (e, state)
            })?;
        }
        let session = ts.session.as_deref().ok_or((DbError::PlanExpired, None))?;
        still_one_read(&stmt, session.dialect(), ctx).map_err(early)?;

        let cancel = RunCancel::new(session.cancel_handle());
        let finished = Arc::new(AtomicBool::new(false));
        // An export has no event stream: its rows go to the file.
        let emit: EventSink = Arc::new(|_| true);
        self.runs
            .try_insert(
                run_id,
                RunEntry {
                    tab: tab.clone(),
                    conn,
                    cancel: cancel.clone(),
                    flags: Arc::new(RunFlags::default()),
                    emit,
                    finished: Arc::clone(&finished),
                },
            )
            .map_err(early)?;
        let registration = Registration {
            runs: self.runs.clone(),
            run_id,
        };
        Ok(ExportRun {
            rt,
            tab,
            slot,
            ts,
            stmt,
            cancel,
            finished,
            cancel_grace: self.cancel_grace,
            registration,
        })
    }
}

/// Removes a registered run when dropped: on every path out of an export, including a
/// panic or the export's future being dropped.
struct Registration {
    runs: Runs,
    run_id: Uuid,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.runs.remove(self.run_id);
    }
}

/// An export holding its tab's session, its statement judged and its run registered
/// ([`SessionManager::query_export_begin`]); [`Self::execute`] sends it.
pub struct ExportRun {
    rt: Arc<ConnectionRuntime>,
    tab: String,
    slot: TabSlot,
    ts: OwnedMutexGuard<TabSession>,
    stmt: PlannedStmt,
    cancel: RunCancel,
    finished: Arc<AtomicBool>,
    cancel_grace: Duration,
    registration: Registration,
}

impl ExportRun {
    /// The connection the export runs on (its tracked job's connection).
    pub fn connection_id(&self) -> Uuid {
        self.rt.cfg.id
    }

    /// Sends the statement, streaming its rows into `sink` (see
    /// [`SessionManager::query_export`]). As a run's task does, a panic while it runs
    /// (driver or sink) is caught: the session, in an unknown state, is dropped and an
    /// `Internal` error returned with `tx_state` `Idle`.
    pub async fn execute(
        self,
        sink: &mut dyn RowSink,
        failed: &tokio::sync::Notify,
    ) -> Result<(), (DbError, Option<TxState>)> {
        let ExportRun {
            rt,
            tab,
            slot,
            mut ts,
            stmt,
            cancel,
            finished,
            cancel_grace,
            registration,
        } = self;
        let exec = ExecOptions {
            max_rows: usize::MAX,
            batch_size: STREAM_BATCH_SIZE,
            timeout: None,
            cancel_grace,
            stop: cancel.signal(),
            full_bytes: true,
            ..ExecOptions::default()
        };
        tracing::info!(connection = %rt.cfg.id, "export reads a query result again on its tab");
        let ran = AssertUnwindSafe(async {
            let session = ts.session.as_deref()?;
            let exec = std::pin::pin!(session.execute_one(&stmt.text, stmt.kind, &exec, sink));
            let watch = std::pin::pin!(async {
                failed.notified().await;
                if let Err(e) = cancel.cancel().await {
                    tracing::warn!(error = %e, "could not cancel an export");
                }
                std::future::pending::<()>().await
            });
            match futures_util::future::select(exec, watch).await {
                futures_util::future::Either::Left((outcome, _)) => {
                    Some((outcome, session.tx_state()))
                }
                futures_util::future::Either::Right(((), _)) => {
                    unreachable!("the watcher never completes")
                }
            }
        })
        .catch_unwind()
        .await;
        // No statement runs any more: from here on nothing may cancel the session.
        finished.store(true, Ordering::SeqCst);
        drop(registration);
        let (outcome, state) = match ran {
            Ok(Some((outcome, state))) => {
                if outcome.is_ok() {
                    // The export read in the tab's transaction.
                    ts.tx_fresh = false;
                }
                let lost = outcome.as_ref().err().filter(|e| is_lost(e)).cloned();
                if let Some(e) = lost {
                    // As after a run: the next use opens a fresh session.
                    let in_tx = ts.tx_open();
                    if let Some(dead) = ts.session.take() {
                        close_quietly(dead).await;
                        ts.set_lost_tx(in_tx, false);
                        ts.reopen = !in_tx && AppError::from(e).retryable;
                    }
                }
                (outcome.map(|_| ()), state)
            }
            Ok(None) | Err(_) => {
                tracing::error!(connection = %rt.cfg.id, "an export failed unexpectedly");
                // As after a run that panicked: the session's state is unknown, drop it.
                let in_tx = ts.tx_open();
                if let Some(dead) = ts.session.take() {
                    close_quietly(dead).await;
                    ts.set_lost_tx(in_tx, false);
                    ts.reopen = false;
                }
                let error = DbError::Internal {
                    message: "the export failed unexpectedly; see the log for details".into(),
                };
                (Err(error), TxState::Idle)
            }
        };
        if rt.closed.load(Ordering::SeqCst) || !rt.queries.is_slot(&tab, &slot) || ts.closed {
            // The tab or connection was closed while the export held the session.
            ts.close().await;
        } else {
            rt.queries.sync_bound(&tab, &ts);
        }
        let state = if ts.session.is_some() {
            state
        } else {
            TxState::Idle
        };
        outcome.map_err(|e| (e, Some(state)))
    }
}

fn not_read() -> DbError {
    config_error(
        "scope",
        "Only the result of a read statement can be read again for export; \
         export the loaded rows instead.",
    )
}

/// The statement of a one-statement plan copy an export may run again: a read (one
/// statement for the server, not an Explain) the guard allows under `ctx` (the guard
/// context a run is judged with, `SessionManager::guard_ctx`) without confirmation.
fn export_read(copy: &PlanCopy, ctx: GuardCtx) -> Result<PlannedStmt, DbError> {
    if copy.explain.is_some() || copy.plan.explain {
        return Err(not_read());
    }
    let mut judged = copy.plan.clone();
    guard_with(&mut judged, ctx);
    let dialect = judged.dialect;
    let stmt = judged.stmts.first().ok_or(DbError::PlanExpired)?;
    if stmt.kind != StatementKind::Read
        || stmt.verdict != Verdict::Allow
        || dnt_core::sql::resplits(&stmt.text, dialect)
    {
        return Err(not_read());
    }
    Ok(stmt.clone())
}

/// Defense in depth before an export sends `stmt`: split, classified and judged with
/// the session's dialect `now` (its `sql_mode` may have changed since the plan), it
/// must still be exactly one read the guard allows under `ctx`.
fn still_one_read(stmt: &PlannedStmt, now: Dialect, ctx: GuardCtx) -> Result<(), DbError> {
    let mut again = plan(&stmt.text, Target::All, now);
    guard_with(&mut again, ctx);
    let one_read = match again.stmts.as_slice() {
        [only] => {
            only.kind == StatementKind::Read
                && only.verdict == Verdict::Allow
                && !dnt_core::sql::resplits(&stmt.text, now)
        }
        _ => false,
    };
    if one_read {
        Ok(())
    } else {
        Err(config_error(
            "scope",
            "The session's SQL mode changed since this result was read, and the statement \
             now reads differently. Run it again, then export.",
        ))
    }
}

/// The tab's transaction state to report with `e`, an error of a call that opens or
/// moves the tab's session (`query_plan`, `query_explain`, `query_plan_statement`,
/// `query_run`, `query_export`) before anything runs: a stale session dropped with its
/// transaction open (`ConnectionLost { was_in_tx: true }`, see
/// `ensure_tab_session`) leaves the tab without a session, so `Idle` (the server
/// rolled the transaction back). `None` otherwise: the state did not change.
pub fn tx_state_after(e: &DbError) -> Option<TxState> {
    matches!(
        e.unstaged(),
        DbError::ConnectionLost {
            was_in_tx: true,
            ..
        }
    )
    .then_some(TxState::Idle)
}

/// `e` for the UI, with [`tx_state_after`] in `AppError::tx_state`.
pub fn with_tx_state(e: DbError) -> AppError {
    let state = tx_state_after(&e);
    let mut app = AppError::from(e);
    app.tx_state = state.map(|s| s.as_str().to_string());
    app
}

fn is_lost(e: &DbError) -> bool {
    matches!(
        e.unstaged(),
        DbError::ConnectionLost { .. } | DbError::Connect { .. }
    )
}

/// The dialect, transaction state and server of a held tab session.
fn session_facts(
    ts: &TabSession,
) -> Result<(Dialect, TxState, dnt_core::server::ServerInfo), DbError> {
    let session = ts.session.as_deref().ok_or_else(|| DbError::Internal {
        message: "the tab's session vanished".into(),
    })?;
    Ok((
        session.dialect(),
        session.tx_state(),
        session.server_info().clone(),
    ))
}

/// A run in progress, for `query_cancel` and for closing its tab or connection.
#[derive(Clone)]
pub(crate) struct RunEntry {
    pub(crate) tab: String,
    pub(crate) conn: Uuid,
    cancel: RunCancel,
    flags: Arc<RunFlags>,
    emit: EventSink,
    /// See [`RunTask::finished`].
    finished: Arc<AtomicBool>,
}

impl RunEntry {
    /// Sends `stopping{user}` (once per run), then stops the run: no further statement
    /// is sent and the running one is cancelled. A run whose statements are over is
    /// left alone (a cancel then could hit the tab's next statement).
    async fn stop(&self) -> Result<(), DbError> {
        if self.finished.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.flags.user_stop(&self.emit);
        self.cancel.cancel().await
    }
}

/// The runs in progress, by run id.
#[derive(Default, Clone)]
pub(crate) struct Runs(Arc<Mutex<HashMap<Uuid, RunEntry>>>);

impl Runs {
    fn get(&self, id: Uuid) -> Option<RunEntry> {
        lock(&self.0).get(&id).cloned()
    }

    /// Registers run `id`: the check and the insert happen under one lock, so two calls
    /// with the same id cannot both register it. An id in use is refused (`config`,
    /// field `run_id`) and the registered run is left alone.
    fn try_insert(&self, id: Uuid, entry: RunEntry) -> Result<(), DbError> {
        match lock(&self.0).entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => {
                Err(config_error("run_id", "this run id is in use"))
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(entry);
                Ok(())
            }
        }
    }

    fn remove(&self, id: Uuid) {
        lock(&self.0).remove(&id);
    }

    fn matching(&self, pred: impl Fn(&RunEntry) -> bool) -> Vec<RunEntry> {
        lock(&self.0)
            .values()
            .filter(|r| pred(r))
            .cloned()
            .collect()
    }
}

/// How many statements of `plan` have the verdict `v`.
fn count_verdict(plan: &ScriptPlan, v: Verdict) -> u32 {
    let n = plan.stmts.iter().filter(|s| s.verdict == v).count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Judges a stored plan again with the connection's current config and returns the plan
/// to run. `Blocked` when any statement is Block; `ConfirmationRequired` when any is
/// Confirm and the user did not confirm. An Explain plan is judged on the explained
/// statement; the explain statement it runs carries those verdicts. `now`: the guard
/// context as it is now (`SessionManager::guard_ctx`: current config and "Confirm
/// UPDATE/DELETE without WHERE" setting); the plan keeps its own `analyze`.
///
/// On Production a confirmation also needs the typed names (spec §4.3): `confirm_names`
/// must be exactly the `confirm_name`s the guard gives the Confirm statements now
/// ([`confirm_names_match`], case-sensitive), else `ConfirmationRequired`.
fn rejudge(
    copy: &PlanCopy,
    now: GuardCtx,
    confirmed: bool,
    confirm_names: &[String],
) -> Result<ScriptPlan, DbError> {
    let mut judged = copy
        .explain
        .as_ref()
        .map_or(&copy.plan, |e| &e.original)
        .clone();
    let Some(ctx) = judged.guard else {
        return Err(DbError::Internal {
            message: "the run plan was never guarded".into(),
        });
    };
    guard_with(
        &mut judged,
        GuardCtx {
            analyze: ctx.analyze,
            ..now
        },
    );
    let blocked = count_verdict(&judged, Verdict::Block);
    if blocked > 0 {
        return Err(DbError::Blocked { count: blocked });
    }
    let confirm = count_verdict(&judged, Verdict::Confirm);
    let required: Vec<String> = judged
        .stmts
        .iter()
        .filter(|s| s.verdict == Verdict::Confirm)
        .filter_map(|s| s.confirm_name.clone())
        .collect();
    if (confirm > 0 && !confirmed) || !confirm_names_match(&required, confirm_names) {
        return Err(DbError::ConfirmationRequired {
            count: confirm.max(1),
        });
    }
    if copy.explain.is_none() {
        return Ok(judged);
    }
    let mut run_plan = copy.plan.clone();
    for (s, j) in run_plan.stmts.iter_mut().zip(&judged.stmts) {
        s.verdict = j.verdict;
    }
    // Intentional: an Explain plan skips `run`'s re-guard after a `sql_mode` change.
    // The explain statement's own text is not what the guard judged (`query_run` just
    // judged the explained statement with the current config), so `run` only re-checks
    // its boundaries.
    run_plan.guard = None;
    Ok(run_plan)
}

/// Wraps a run's `emit` so that the first event it cannot deliver (the window whose
/// channel listened is gone) stops the run: the stop signal is raised at once (no
/// further statement is sent) and the running statement is cancelled in the
/// background, so the run ends and frees the tab's session. Nothing is sent after that.
///
/// Once `finished` is set (the run's statements are over, before the slot is released
/// and `done` is sent) a failed send never cancels: the drivers' cancel targets the
/// `execute_one` running when it is invoked (generation-scoped), so a late cancel
/// could otherwise hit the next run or `query_tx` on the tab's session.
fn stop_when_undelivered(
    emit: EventSink,
    cancel: RunCancel,
    finished: Arc<AtomicBool>,
) -> EventSink {
    let dead = Arc::new(AtomicBool::new(false));
    Arc::new(move |event| {
        if dead.load(Ordering::SeqCst) {
            return false;
        }
        if emit(event) {
            return true;
        }
        if !dead.swap(true, Ordering::SeqCst) && !finished.load(Ordering::SeqCst) {
            tracing::warn!("a query run lost its listener; stopping it");
            cancel.signal().stop();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let cancel = cancel.clone();
                let finished = Arc::clone(&finished);
                handle.spawn(async move {
                    // The run may have ended meanwhile.
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Err(e) = cancel.cancel().await {
                        tracing::warn!(error = %e, "could not cancel a running query");
                    }
                });
            }
        }
        false
    })
}

/// Everything a spawned run owns.
struct RunTask {
    run_id: Uuid,
    tab: String,
    rt: Arc<ConnectionRuntime>,
    slot: TabSlot,
    ts: OwnedMutexGuard<TabSession>,
    plan: ScriptPlan,
    explain: Option<ExplainFormat>,
    /// Explain mode: the plain `EXPLAIN` run before the plan for the classic table.
    classic: Option<String>,
    exec: ExecOptions,
    continue_on_error: bool,
    cancel: RunCancel,
    flags: Arc<RunFlags>,
    emit: EventSink,
    repo: Arc<ConnectionRepo>,
    runs: Runs,
    results: ResultStore,
    /// Set when the run's statements are over: no cancel may be sent any more.
    finished: Arc<AtomicBool>,
    /// A transaction was open when the run started.
    tx_open_before: bool,
    /// The tab is in Manual commit mode (`RunConfig::manual_tx`).
    manual_tx: bool,
}

impl RunTask {
    async fn execute(mut self) {
        let started = Instant::now();
        let started_ms = now_ms();
        let mut sink = ChannelSink::new(
            Arc::clone(&self.emit),
            Arc::clone(&self.flags),
            self.exec.batch_size,
            self.plan.dialect,
            HistoryCtx {
                repo: Arc::clone(&self.repo),
                connection_id: self.rt.cfg.id,
                database: self.ts.database.clone(),
            },
            self.explain,
        );
        let store_error = Arc::new(Mutex::new(None::<DbError>));
        if self.explain.is_none() {
            let error_slot = Arc::clone(&store_error);
            let cancel = self.cancel.clone();
            sink.store_results(StoredRun {
                store: self.results.clone(),
                connection: self.rt.cfg.id,
                tab: self.tab.clone(),
                run: self.run_id,
                fail: Arc::new(move |error| {
                    let mut slot = lock(&error_slot);
                    if slot.is_some() {
                        return;
                    }
                    *slot = Some(error);
                    drop(slot);
                    cancel.signal().stop();
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        let cancel = cancel.clone();
                        handle.spawn(async move {
                            let _ = cancel.cancel().await;
                        });
                    }
                }),
            });
        }
        sink.tx_now = self
            .ts
            .session
            .as_deref()
            .map_or(TxState::Idle, |s| s.tx_state());
        let rows = sink.rows();
        let index = sink.index();
        let ticker = tokio::spawn(tick_rows(Arc::clone(&rows)));
        let ran = AssertUnwindSafe(async {
            let session = self.ts.session.as_deref()?;
            if let Some(sql) = &self.classic {
                let opts = ExecOptions {
                    max_rows: 1_000,
                    timeout: Some(CLASSIC_EXPLAIN_TIMEOUT),
                    cancel_grace: self.exec.cancel_grace,
                    stop: self.cancel.signal(),
                    ..ExecOptions::default()
                };
                sink.set_classic(classic_explain(session, sql, self.plan.dialect, &opts).await);
            }
            Some(
                run_with(
                    session,
                    &self.plan,
                    &self.exec,
                    RunConfig {
                        continue_on_error: self.continue_on_error,
                        manual_tx: self.manual_tx,
                    },
                    &self.cancel,
                    &mut sink,
                )
                .await,
            )
        })
        .catch_unwind()
        .await;
        // No statement runs any more: from here on nothing may cancel the session (a
        // late cancel would hit the tab's next statement).
        self.finished.store(true, Ordering::SeqCst);
        ticker.abort();
        rows.flush();
        if let Some(writer) = sink.take_writer() {
            writer.finish().await;
        }
        let mut stop = match ran {
            Ok(Some(report)) => {
                self.after_run(Some(&report), &sink, started_ms).await;
                report.stop
            }
            Ok(None) | Err(_) => {
                tracing::error!(connection = %self.rt.cfg.id, "a query run failed unexpectedly");
                // The session's state is unknown: drop it (or close it, with its tab).
                self.after_run(None, &sink, started_ms).await;
                let at = index.load(Ordering::SeqCst);
                let mut error = AppError::from(DbError::Internal {
                    message: "the run failed unexpectedly; see the log for details".into(),
                });
                // A statement that was sent and never ended may have been applied.
                error.outcome_unknown = sink.in_statement();
                (self.emit)(RunEvent::StatementError { index: at, error });
                Some(StopReason::Error { index: at })
            }
        };
        if let Some(error) = lock(&store_error).take() {
            let at = index.load(Ordering::SeqCst);
            (self.emit)(RunEvent::StatementError {
                index: at,
                error: AppError::from(error),
            });
            stop = Some(StopReason::Error { index: at });
        }
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            connection = %self.rt.cfg.id,
            statements = self.plan.stmts.len(),
            elapsed_ms,
            stopped = stop.is_some(),
            "query run finished"
        );
        // The slot is free (and the tab's plans usable) before the UI hears `done`.
        let RunTask {
            run_id,
            runs,
            emit,
            ts,
            ..
        } = self;
        drop(ts);
        runs.remove(run_id);
        emit(RunEvent::Done { stop, elapsed_ms });
    }

    /// Applies what the run did to the tab's session: a lost connection drops it, a
    /// MySQL `USE` moves its database, a closed tab or connection closes it; a
    /// successful `Ddl` reloads the database's SchemaCache in the background.
    ///
    /// `report: None` (the run task panicked, or the session vanished) is handled like
    /// a lost connection: the session, in an unknown state, is dropped (and the tab's
    /// binding with it). After a lost connection whose errors are all retryable the
    /// tab keeps its binding ([`TabSession::reopen`]); a session dropped with a
    /// transaction open sets [`TabSession::lost_tx`].
    ///
    /// The SchemaCache refresh covers only the session's current database (or the
    /// connect database): DDL on another database (`CREATE TABLE other.t`) or another
    /// PostgreSQL schema's database is not picked up until that cache is refreshed.
    async fn after_run(&mut self, report: Option<&RunReport>, sink: &ChannelSink, started_ms: f64) {
        let lost_errors: Vec<&AppError> = report.map_or_else(Vec::new, |report| {
            report
                .stmts
                .iter()
                .filter_map(|s| s.result.as_ref().err())
                .filter(|e| e.kind == "connection_lost" || e.kind == "connect")
                .collect()
        });
        let lost = report.is_none() || !lost_errors.is_empty();
        let ts = &mut *self.ts;
        if lost {
            // The driver resets its state on a lost connection: the sink saw whether a
            // transaction was open when it happened.
            let in_tx = ts.tx_open() || sink.lost_in_tx || sink.lost_committing;
            if let Some(dead) = ts.session.take() {
                close_quietly(dead).await;
                ts.set_lost_tx(in_tx, sink.lost_committing);
                // Retry / Fetch all may run the tab's statements again on a fresh
                // session, unless a transaction was lost or an outcome is unknown.
                ts.reopen = report.is_some() && !in_tx && lost_errors.iter().all(|e| e.retryable);
            }
        } else if let Some(db) = &sink.use_db {
            ts.database = Some(db.clone());
        }
        match report {
            Some(report) => {
                ts.tx_fresh = tx_fresh_after(ts.tx_fresh, &self.plan, report)
                    // Begun, then stopped before its statement: nothing used it.
                    || report.began_unused;
                ts.track_tx(self.tx_open_before, started_ms, &self.plan, report);
            }
            None => ts.forget_tx_if_idle(),
        }
        let current = self.rt.queries.is_slot(&self.tab, &self.slot);
        if self.rt.closed.load(Ordering::SeqCst) || !current || ts.closed {
            // The tab or connection was closed while the run held the session.
            ts.close().await;
        } else {
            self.rt.queries.sync_bound(&self.tab, ts);
        }
        if sink.ddl_ok {
            let database = ts.database.clone().or_else(|| self.rt.home_database());
            if let Some(db) = database {
                self.rt.refresh_schema_cache(&db);
            }
        }
    }
}

impl SessionManager {
    /// Overrides the cancel grace of query runs (tests).
    pub fn with_cancel_grace(mut self, grace: Duration) -> Self {
        self.cancel_grace = grace;
        self
    }

    /// Runs plan `plan_id` on its tab and returns the run's id at once; everything the
    /// run produces goes to `emit` (see [`RunEvent`]), ending with `done`.
    ///
    /// The plan is judged again with the connection's current config: any Block →
    /// `Blocked`, any Confirm without `confirmed` → `ConfirmationRequired` (the plan
    /// stays available, so the UI can ask and run it again with `confirmed`). A tab
    /// that is already running (or planning) → `Busy`. A plan already run, expired,
    /// replaced, or whose tab moved → `PlanExpired`. The run holds the tab's session
    /// until it ends.
    ///
    /// No name is typed: a plan whose Production confirmation asks for one is refused
    /// (`ConfirmationRequired`); see [`Self::query_run_named`].
    pub async fn query_run(
        &self,
        plan_id: Uuid,
        confirmed: bool,
        opts: RunOptions,
        emit: EventSink,
    ) -> Result<Uuid, DbError> {
        self.query_run_named(plan_id, confirmed, Vec::new(), opts, emit)
            .await
    }

    /// [`Self::query_run`] with the names the user typed to confirm it on Production
    /// (`PlanStmtView::confirm_name`): they must be exactly the names the plan asks for
    /// when judged again now (case-sensitive; order and repeats do not matter), else
    /// `ConfirmationRequired` and the plan stays available.
    pub async fn query_run_named(
        &self,
        plan_id: Uuid,
        confirmed: bool,
        confirm_names: Vec<String>,
        opts: RunOptions,
        emit: EventSink,
    ) -> Result<Uuid, DbError> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let (tab, conn) = self.plans.locate(self.plan_ttl, plan_id, &current)?;
        let rt = self.runtime(conn)?;
        let slot = rt.queries.get(&tab).ok_or(DbError::PlanExpired)?;
        let mut ts = Arc::clone(&slot)
            .try_lock_owned()
            .map_err(|_| DbError::Busy)?;
        let copy = self.plans.pending(self.plan_ttl, plan_id, &current)?;
        if ts.closed || (ts.session.is_none() && !ts.reopen) || ts.database != copy.target.database
        {
            return Err(DbError::PlanExpired);
        }
        let manual = self.tx_mode(&tab) == TxMode::Manual;
        if manual && ts.lost_tx {
            // The Manual transaction was lost with the session: nothing runs (and no
            // new transaction begins) until the user acknowledges it with `query_tx`.
            return Err(ts.lost_tx_error());
        }
        let cfg = self.current_config(&rt);
        let run_plan = rejudge(
            &copy,
            self.guard_ctx(&cfg, false),
            confirmed,
            &confirm_names,
        )?;
        // A stale session is replaced; one dropped after a lost connection reopens.
        if rt.is_stale(&ts) || ts.session.is_none() {
            let ensured = self
                .ensure_session(&rt, &tab, &mut ts, copy.target.database.clone())
                .await;
            rt.queries.sync_bound(&tab, &ts);
            ensured?;
        }
        self.plans.take(self.plan_ttl, plan_id, &current)?;
        let tx_open_before = ts.tx_open();
        let session = ts.session.as_deref().ok_or(DbError::PlanExpired)?;

        let exec = ExecOptions {
            max_rows: opts.max_rows.clamp(1, MAX_ROWS_LIMIT) as usize,
            batch_size: STREAM_BATCH_SIZE,
            timeout: (cfg.options.query_timeout_s > 0)
                .then(|| Duration::from_secs(cfg.options.query_timeout_s.into())),
            cancel_grace: self.cancel_grace,
            full_bytes: copy.explain.is_none(),
            ..ExecOptions::default()
        };
        let cancel = RunCancel::new(session.cancel_handle());
        let finished = Arc::new(AtomicBool::new(false));
        let emit = stop_when_undelivered(emit, cancel.clone(), Arc::clone(&finished));
        let flags = Arc::new(RunFlags::default());
        let run_id = Uuid::new_v4();
        if copy.explain.is_none() {
            self.results.begin_run(&tab);
        }
        self.runs.try_insert(
            run_id,
            RunEntry {
                tab: tab.clone(),
                conn,
                cancel: cancel.clone(),
                flags: Arc::clone(&flags),
                emit: Arc::clone(&emit),
                finished: Arc::clone(&finished),
            },
        )?;
        let task = RunTask {
            tx_open_before,
            // An Explain runs no write: it never begins a transaction.
            manual_tx: manual && copy.explain.is_none(),
            run_id,
            tab,
            rt,
            slot,
            ts,
            plan: run_plan,
            classic: copy.explain.as_ref().and_then(|e| e.classic.clone()),
            explain: copy.explain.map(|e| e.format),
            exec,
            continue_on_error: opts.continue_on_error,
            cancel,
            flags,
            emit,
            repo: Arc::clone(&self.repo),
            runs: self.runs.clone(),
            results: self.results.clone(),
            finished,
        };
        tokio::spawn(task.execute());
        Ok(run_id)
    }

    /// Stops run `run_id`: emits `stopping{user}` (once), then cancels it. The run ends
    /// with `done` as usual. An unknown or finished run is a no-op.
    pub async fn query_cancel(&self, run_id: Uuid) -> Result<(), DbError> {
        match self.runs.get(run_id) {
            Some(entry) => entry.stop().await,
            None => Ok(()),
        }
    }

    /// Stops every run `pred` selects (closing a tab or connection), each bounded by
    /// [`SLOT_CLOSE_WAIT`].
    pub(crate) async fn stop_runs(&self, pred: impl Fn(&RunEntry) -> bool) {
        let runs = self.runs.matching(pred);
        futures_util::future::join_all(runs.iter().map(|r| async move {
            match tokio::time::timeout(SLOT_CLOSE_WAIT, r.stop()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "could not cancel a running query"),
                Err(_) => tracing::warn!("cancelling a running query timed out"),
            }
        }))
        .await;
    }

    /// Plans an Explain of the statement at `cursor` (UTF-16) on tab `tab` (session rules
    /// as for [`Self::query_plan`]) and returns its view; run it with `query_run`, which
    /// streams one `explain` event. `Ddl` (or no statement) → `Config` "Explain is not
    /// available for this statement"; `analyze` on a statement that is not `Read` →
    /// `Config` "Explain Analyze is only available for read statements" (it would
    /// execute the statement); `Unsupported` where the server has no such explain.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_explain(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
        text: &str,
        cursor: u32,
        analyze: bool,
    ) -> Result<PlanView, DbError> {
        let (rt, ts) = self.tab_session(tab, conn, database).await?;
        let (dialect, tx_state, info) = session_facts(&ts)?;
        let target = PlanTarget {
            tab: tab.to_string(),
            conn,
            runtime: Arc::downgrade(&rt),
            database: ts.database.clone(),
        };
        drop(ts);

        let mut original = plan(text, Target::Cursor(cursor), dialect);
        let Some(stmt) = original.stmts.first() else {
            return Err(config_error(
                "explain",
                "Explain is not available for this statement",
            ));
        };
        if stmt.kind == StatementKind::Ddl {
            return Err(config_error(
                "explain",
                "Explain is not available for this statement",
            ));
        }
        if analyze && stmt.kind != StatementKind::Read {
            return Err(config_error(
                "explain",
                "Explain Analyze is only available for read statements",
            ));
        }
        let sql = explain_sql(&stmt.text, &info, dialect, analyze)?;
        let cfg = self.current_config(&rt);
        original.explain = true;
        guard_with(&mut original, self.guard_ctx(&cfg, analyze));

        // The explain statement replaces the statement (one, the Cursor's): it is the
        // whole source of the plan that runs.
        let mut planned = original.clone();
        for s in &mut planned.stmts {
            s.kind = classify(&sql.sql, dialect);
            s.text = sql.sql.clone();
            s.source_range = (0, s.text.len());
        }
        planned.source = sql.sql;
        planned.base = planned.stmts.first().map_or(0, |s| s.range.0);
        let explain = ExplainPlan {
            format: sql.format,
            original,
            classic: sql.classic,
        };
        let copy = PlanCopy {
            target,
            plan: planned,
            explain: Some(explain),
            source: None,
        };
        self.store_plan(&rt, &cfg, copy, dialect, tx_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_flattens_line_breaks_and_cuts() {
        assert_eq!(preview("a\r\nb\nc\rd"), "a b c d");
        let long = "é".repeat(200);
        assert_eq!(preview(&long).chars().count(), PREVIEW_CHARS);
    }

    #[test]
    fn mysql_ident_quoting() {
        assert_eq!(quote_mysql_ident("other db"), "`other db`");
        assert_eq!(quote_mysql_ident("a`b"), "`a``b`");
    }

    #[test]
    fn tx_action_wire_names() {
        let a: TxAction = serde_json::from_str("\"Rollback\"").unwrap();
        assert_eq!(a, TxAction::Rollback);
        assert_eq!(TxAction::Commit.sql(), "COMMIT");
    }

    struct NoCancel;

    #[async_trait::async_trait]
    impl dnt_core::driver::Canceller for NoCancel {
        async fn cancel(&self) -> Result<(), DbError> {
            Ok(())
        }
    }

    fn entry(tab: &str) -> RunEntry {
        RunEntry {
            tab: tab.into(),
            conn: Uuid::nil(),
            cancel: RunCancel::new(Arc::new(NoCancel)),
            flags: Arc::new(RunFlags::default()),
            emit: Arc::new(|_| true),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn a_run_id_registers_once() {
        let runs = Runs::default();
        let id = Uuid::new_v4();
        runs.try_insert(id, entry("first")).unwrap();
        let err = runs.try_insert(id, entry("second")).unwrap_err();
        assert!(
            matches!(&err, DbError::Config { field: Some(f), .. } if f == "run_id"),
            "{err:?}"
        );
        // The registered run is untouched.
        assert_eq!(runs.get(id).unwrap().tab, "first");

        // Racing registrations of one id: exactly one wins.
        let id = Uuid::new_v4();
        let wins: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let runs = runs.clone();
                    s.spawn(move || runs.try_insert(id, entry(&format!("t{i}"))).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum()
        });
        assert_eq!(wins, 1);
    }
}
