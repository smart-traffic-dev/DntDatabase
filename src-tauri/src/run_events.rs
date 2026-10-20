//! What a run streams to the UI (spec §6.1): [`RunEvent`]s through a Tauri `Channel`,
//! emitted by [`ChannelSink`], the [`RunSink`] of `query_run`. The sink also records
//! the query history (spec §8.1) and logs every statement (redacted) at `debug`.
//!
//! Event order of one statement: `statement_start`, then per result `result_start`,
//! `rows`… and `result_end` (in Explain mode no result events: one `explain` instead),
//! then `statement_error` when it failed, then its `notice`s. `tx_state` comes whenever
//! the session's transaction state changes, `stopping` / `abandoning` while a statement
//! is being stopped. `done` is always the last event of a run, also when the run was
//! cancelled, its connection or tab closed, or the run task failed.

use crate::session_manager::lock;
use dnt_core::driver::{ExecProgress, ResultSummary, RowSink, StmtOutcome};
use dnt_core::error::AppError;
use dnt_core::sql::{session_effect, Dialect, StatementKind};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::{
    parse_plan, redact, ExplainFormat, PlanTree, PlannedStmt, RunSink, StmtKindLabel, StopReason,
};
use dnt_store::history::{HistoryStatus, NewHistoryEntry};
use dnt_store::repo::ConnectionRepo;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Partial `rows` batches of a result are merged and sent at most once per interval.
pub const ROWS_EVENT_INTERVAL: Duration = Duration::from_millis(50);

/// How often held partial batches are checked against [`ROWS_EVENT_INTERVAL`].
const ROWS_TICK: Duration = Duration::from_millis(10);

/// Why a run is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum StopCause {
    /// The user pressed Stop (or closed the tab / connection).
    User,
    /// The statement's time limit expired.
    Timeout,
}

/// One event of a run, in the order described in the module docs.
#[derive(Debug, Clone, Serialize, specta::Type)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RunEvent {
    /// Statement `index` of the plan is sent; `range` is its UTF-16 range in the editor.
    StatementStart { index: u32, range: (u32, u32) },
    /// A result set of statement `index` begins. `result_id` counts up from 0 per run.
    ResultStart {
        result_id: u32,
        index: u32,
        columns: Vec<ColumnMeta>,
    },
    /// At most 500 (`batch_size`) rows of result `result_id`, in order.
    Rows {
        result_id: u32,
        rows: Vec<Vec<Value>>,
    },
    ResultEnd {
        result_id: u32,
        #[specta(type = Option<f64>)]
        affected: Option<u64>,
        #[specta(type = f64)]
        row_count: u64,
        truncated: bool,
        #[specta(type = f64)]
        elapsed_ms: u64,
    },
    /// Statement `index` failed. `error.position` is a UTF-16 offset into the editor
    /// text, `error.tx_state` the session's state after the failure, and
    /// `error.outcome_unknown` set when the statement may have been applied (show it as
    /// such, never as "not applied").
    StatementError { index: u32, error: AppError },
    /// A Messages line about statement `index` (a silent reconnect, a rolled-back
    /// transaction…).
    Notice { index: u32, message: String },
    /// Explain mode: the normalized plan of statement `index`.
    Explain { index: u32, plan: PlanTree },
    /// Stopping the running statement was requested. Sent once per stop: a Stop
    /// pressed while a timeout is already stopping the statement sends nothing more
    /// (the user's stop is still recorded, and ends the run), and a second Stop of the
    /// same run sends nothing.
    Stopping { reason: StopCause },
    /// The statement did not stop within the cancel grace; its connection is being
    /// replaced.
    Abandoning,
    /// The session's transaction state changed.
    TxState { state: TxState },
    /// The run ended. `stop: null` means every planned statement ran. A Stop that lands
    /// while the LAST statement runs, when that statement still completes, also ends
    /// with `stop: null` (after a `stopping` event): everything ran.
    Done {
        stop: Option<StopReason>,
        #[specta(type = f64)]
        elapsed_ms: u64,
    },
}

/// Where a run's events go (the Tauri channel in the app, a collector in tests).
/// Returns whether the event was delivered: once it returns `false` (the window that
/// listened is gone) the run is stopped and nothing more is sent.
pub type EventSink = Arc<dyn Fn(RunEvent) -> bool + Send + Sync>;

/// Per-run options from the toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RunOptions {
    /// Row limit per result; clamped to 1..=1 000 000.
    pub max_rows: u32,
    pub continue_on_error: bool,
}

/// Stop state shared by a run's sink and `query_cancel`.
#[derive(Default)]
pub(crate) struct RunFlags {
    /// The user stopped the run (recorded even when no `stopping{user}` was sent).
    pub(crate) user: AtomicBool,
    /// A `stopping` event was sent for the current statement.
    pub(crate) stopping: AtomicBool,
}

impl RunFlags {
    /// Records the user's stop; sends `stopping{user}` unless a `stopping` was already
    /// sent (by a timeout, or an earlier Stop).
    pub(crate) fn user_stop(&self, emit: &EventSink) {
        if !self.user.swap(true, Ordering::SeqCst) && !self.stopping.swap(true, Ordering::SeqCst) {
            emit(RunEvent::Stopping {
                reason: StopCause::User,
            });
        }
    }
}

#[derive(Default)]
struct RowBuffer {
    /// The open result.
    result_id: Option<u32>,
    rows: Vec<Vec<Value>>,
    /// When the oldest held row arrived.
    since: Option<Instant>,
}

/// The rows of the open result, held until a full batch, [`ROWS_EVENT_INTERVAL`] or the
/// end of the result. Shared with a ticker that flushes held rows on time. Events are
/// emitted under the buffer's lock, so the ticker and the sink never reorder them.
pub(crate) struct RowStream {
    emit: EventSink,
    batch_size: usize,
    buf: Mutex<RowBuffer>,
}

impl RowStream {
    fn new(emit: EventSink, batch_size: usize) -> Self {
        RowStream {
            emit,
            batch_size: batch_size.max(1),
            buf: Mutex::new(RowBuffer::default()),
        }
    }

    fn flush_locked(&self, b: &mut RowBuffer) {
        b.since = None;
        if b.rows.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut b.rows);
        if let Some(result_id) = b.result_id {
            (self.emit)(RunEvent::Rows { result_id, rows });
        }
    }

    fn start(&self, result_id: u32, event: RunEvent) {
        let mut b = lock(&self.buf);
        self.flush_locked(&mut b);
        b.result_id = Some(result_id);
        (self.emit)(event);
    }

    fn push(&self, mut batch: Vec<Vec<Value>>) {
        let mut b = lock(&self.buf);
        let Some(result_id) = b.result_id else {
            return;
        };
        if batch.is_empty() {
            return;
        }
        if b.rows.is_empty() {
            b.since = Some(Instant::now());
        }
        b.rows.append(&mut batch);
        while b.rows.len() >= self.batch_size {
            let rest = b.rows.split_off(self.batch_size);
            let full = std::mem::replace(&mut b.rows, rest);
            (self.emit)(RunEvent::Rows {
                result_id,
                rows: full,
            });
            b.since = (!b.rows.is_empty()).then(Instant::now);
        }
        if b.since.is_some_and(|t| t.elapsed() >= ROWS_EVENT_INTERVAL) {
            self.flush_locked(&mut b);
        }
    }

    fn end(&self, event: RunEvent) {
        let mut b = lock(&self.buf);
        self.flush_locked(&mut b);
        b.result_id = None;
        (self.emit)(event);
    }

    /// Sends whatever is held.
    pub(crate) fn flush(&self) {
        let mut b = lock(&self.buf);
        self.flush_locked(&mut b);
    }

    fn flush_due(&self) {
        let mut b = lock(&self.buf);
        if b.since.is_some_and(|t| t.elapsed() >= ROWS_EVENT_INTERVAL) {
            self.flush_locked(&mut b);
        }
    }
}

/// Flushes held rows on time until aborted (spawned next to the run).
pub(crate) async fn tick_rows(rows: Arc<RowStream>) {
    loop {
        tokio::time::sleep(ROWS_TICK).await;
        rows.flush_due();
    }
}

/// Explain mode: the explain statement's output, collected instead of streamed.
struct ExplainBuf {
    format: ExplainFormat,
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

/// The statement being run.
struct Current {
    sql_text: String,
    redacted: bool,
    started_at: String,
    t0: Instant,
}

/// Where the history goes.
pub(crate) struct HistoryCtx {
    pub(crate) repo: Arc<ConnectionRepo>,
    pub(crate) connection_id: Uuid,
    pub(crate) database: Option<String>,
}

/// The [`RunSink`] of `query_run`: turns the run into [`RunEvent`]s, records one history
/// row per statement passed to `execute_one`, and logs each statement (redacted) at
/// `debug` under `dnt::sql`.
pub(crate) struct ChannelSink {
    emit: EventSink,
    rows: Arc<RowStream>,
    flags: Arc<RunFlags>,
    dialect: Dialect,
    history: HistoryCtx,
    /// The run's history writer, started with the first statement.
    writer: Option<HistoryWriter>,
    next_result_id: u32,
    index: Arc<AtomicU32>,
    current: Option<Current>,
    explain: Option<ExplainBuf>,
    /// A `Ddl` statement succeeded.
    pub(crate) ddl_ok: bool,
    /// The database a successful MySQL `USE` moved the session to (the last one).
    pub(crate) use_db: Option<String>,
    /// The session's transaction state as the run last reported it (set it to the
    /// state at the start of the run: `run` reports changes only).
    pub(crate) tx_now: TxState,
    /// A statement lost the connection while a transaction was open (the drivers
    /// reset their state to `Idle` on a lost connection, so the state after the run
    /// cannot tell).
    pub(crate) lost_in_tx: bool,
}

/// `%Y-%m-%dT%H:%M:%S%.3fZ` of now, UTC (the history's fixed format).
pub(crate) fn utc_millis_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

fn kind_label(kind: StatementKind) -> &'static str {
    match StmtKindLabel::from(kind) {
        StmtKindLabel::Read => "read",
        StmtKindLabel::Write => "write",
        StmtKindLabel::Ddl => "ddl",
        StmtKindLabel::Transaction => "transaction",
        StmtKindLabel::Other => "other",
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The `explain` event of statement `index` from its collected output; an output the
/// parser does not know becomes a `statement_error` (`internal`) whose message says
/// what the user can do.
fn explain_event(index: u32, buf: &ExplainBuf) -> RunEvent {
    match parse_plan(buf.format, &buf.columns, &buf.rows) {
        Ok(plan) => RunEvent::Explain { index, plan },
        Err(e) => {
            let mut error = AppError::from(e);
            error.message = match buf.format {
                // MySQL 8.3+ with `explain_json_format_version = 2` writes another shape.
                ExplainFormat::MySqlJson => "The server returned an EXPLAIN format \
                     DntDatabase cannot read. If explain_json_format_version is 2, run \
                     SET explain_json_format_version = 1 and try again."
                    .into(),
                _ => "The server returned an EXPLAIN format DntDatabase cannot read.".into(),
            };
            RunEvent::StatementError { index, error }
        }
    }
}

impl ChannelSink {
    pub(crate) fn new(
        emit: EventSink,
        flags: Arc<RunFlags>,
        batch_size: usize,
        dialect: Dialect,
        history: HistoryCtx,
        explain: Option<ExplainFormat>,
    ) -> Self {
        ChannelSink {
            rows: Arc::new(RowStream::new(Arc::clone(&emit), batch_size)),
            emit,
            flags,
            dialect,
            history,
            writer: None,
            next_result_id: 0,
            index: Arc::new(AtomicU32::new(0)),
            current: None,
            explain: explain.map(|format| ExplainBuf {
                format,
                columns: Vec::new(),
                rows: Vec::new(),
            }),
            ddl_ok: false,
            use_db: None,
            tx_now: TxState::Idle,
            lost_in_tx: false,
        }
    }

    pub(crate) fn rows(&self) -> Arc<RowStream> {
        Arc::clone(&self.rows)
    }

    /// The index of the statement started last.
    pub(crate) fn index(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.index)
    }

    /// Whether a statement was started and has not ended (it was sent, so it may
    /// have been applied).
    pub(crate) fn in_statement(&self) -> bool {
        self.current.is_some()
    }

    /// The history writer, if anything was recorded (finish it before `done`).
    pub(crate) fn take_writer(&mut self) -> Option<HistoryWriter> {
        self.writer.take()
    }

    fn record(&mut self, stmt: &PlannedStmt, result: &Result<StmtOutcome, AppError>) {
        let Some(cur) = self.current.take() else {
            return;
        };
        let (status, row_count, affected, error_kind, error_code) = match result {
            Ok(outcome) => {
                let rows = (!outcome.results.is_empty())
                    .then(|| outcome.results.iter().map(|r| r.row_count).sum());
                let affected = outcome
                    .results
                    .iter()
                    .filter_map(|r| r.affected)
                    .reduce(u64::saturating_add);
                (HistoryStatus::Ok, rows, affected, None, None)
            }
            // A `cancelled` row does not mean "not applied": the cancel may have landed
            // after the statement took effect (`outcome_unknown`, which the history does
            // not store). The UI must not render `cancelled` history rows as not applied.
            Err(e) => (
                if e.kind == "cancelled" {
                    HistoryStatus::Cancelled
                } else {
                    HistoryStatus::Error
                },
                None,
                None,
                Some(e.kind.clone()),
                e.server_code.clone().or_else(|| e.sqlstate.clone()),
            ),
        };
        let entry = NewHistoryEntry {
            connection_id: self.history.connection_id,
            database: self.history.database.clone(),
            sql_text: cur.sql_text,
            redacted: cur.redacted,
            kind: kind_label(stmt.kind).into(),
            started_at: cur.started_at,
            elapsed_ms: Some(millis(cur.t0.elapsed())),
            status,
            row_count,
            affected,
            error_kind,
            error_code,
        };
        let writer = self.writer.get_or_insert_with(|| {
            HistoryWriter::start(Arc::clone(&self.history.repo), self.history.connection_id)
        });
        writer.send(entry);
    }
}

/// Rows the history writer inserts in one transaction at most.
const HISTORY_BATCH: usize = 500;

/// The run's one history writer: a blocking task fed through a channel, so the rows are
/// inserted in statement order and a long script costs one blocking task, not one per
/// statement. Whatever is queued (up to [`HISTORY_BATCH`] rows) is written in one
/// transaction. A failed insert logs `warn` and never fails the run.
pub(crate) struct HistoryWriter {
    tx: std::sync::mpsc::Sender<NewHistoryEntry>,
    task: tokio::task::JoinHandle<()>,
}

impl HistoryWriter {
    pub(crate) fn start(repo: Arc<ConnectionRepo>, connection: Uuid) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<NewHistoryEntry>();
        let task = tokio::task::spawn_blocking(move || {
            // Waits for the next row, then takes whatever else is already queued.
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                while batch.len() < HISTORY_BATCH {
                    match rx.try_recv() {
                        Ok(entry) => batch.push(entry),
                        Err(_) => break,
                    }
                }
                if let Err(e) = repo.history_insert_batch(&batch) {
                    tracing::warn!(
                        connection = %connection,
                        error = %e,
                        rows = batch.len(),
                        "could not record query history"
                    );
                }
            }
        });
        HistoryWriter { tx, task }
    }

    pub(crate) fn send(&self, entry: NewHistoryEntry) {
        // The writer only stops once every sender is gone.
        let _ = self.tx.send(entry);
    }

    /// Waits until every row sent is written.
    pub(crate) async fn finish(self) {
        let HistoryWriter { tx, task } = self;
        drop(tx);
        if let Err(e) = task.await {
            tracing::warn!(error = %e, "the query history writer failed");
        }
    }
}

impl RowSink for ChannelSink {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        if let Some(ex) = &mut self.explain {
            ex.columns = cols.iter().map(|c| c.name.clone()).collect();
            ex.rows.clear();
            return;
        }
        let result_id = self.next_result_id;
        self.next_result_id = self.next_result_id.saturating_add(1);
        self.rows.start(
            result_id,
            RunEvent::ResultStart {
                result_id,
                index: self.index.load(Ordering::SeqCst),
                columns: cols.to_vec(),
            },
        );
    }

    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        match &mut self.explain {
            Some(ex) => ex.rows.extend(batch),
            None => self.rows.push(batch),
        }
    }

    fn result_end(&mut self, summary: &ResultSummary) {
        if self.explain.is_some() {
            return;
        }
        let result_id = self.next_result_id.saturating_sub(1);
        self.rows.end(RunEvent::ResultEnd {
            result_id,
            affected: summary.affected,
            row_count: summary.row_count,
            truncated: summary.truncated,
            elapsed_ms: summary.elapsed_ms,
        });
    }

    fn progress(&mut self, p: ExecProgress) {
        match p {
            ExecProgress::CancelSent => {
                if !self.flags.stopping.swap(true, Ordering::SeqCst) {
                    (self.emit)(RunEvent::Stopping {
                        reason: StopCause::Timeout,
                    });
                }
            }
            ExecProgress::GraceExpired => {
                (self.emit)(RunEvent::Abandoning);
            }
        }
    }
}

impl RunSink for ChannelSink {
    fn statement_start(&mut self, stmt: &PlannedStmt) {
        // A timeout's `stopping` is per statement; a user's stop lasts for the run.
        if !self.flags.user.load(Ordering::SeqCst) {
            self.flags.stopping.store(false, Ordering::SeqCst);
        }
        self.index.store(stmt.index, Ordering::SeqCst);
        let redacted = redact(&stmt.text, self.dialect);
        tracing::debug!(
            target: "dnt::sql",
            connection = %self.history.connection_id,
            sql = %redacted.text,
            "executing"
        );
        self.current = Some(Current {
            sql_text: redacted.text,
            redacted: redacted.changed,
            started_at: utc_millis_now(),
            t0: Instant::now(),
        });
        if let Some(ex) = &mut self.explain {
            ex.columns.clear();
            ex.rows.clear();
        }
        (self.emit)(RunEvent::StatementStart {
            index: stmt.index,
            range: stmt.range,
        });
    }

    fn statement_end(&mut self, stmt: &PlannedStmt, result: &Result<StmtOutcome, AppError>) {
        self.rows.flush();
        match result {
            Ok(_) => {
                if stmt.kind == StatementKind::Ddl {
                    self.ddl_ok = true;
                }
                if let Some(db) = session_effect(&stmt.text, self.dialect).and_then(|e| e.use_db) {
                    self.history.database = Some(db.clone());
                    self.use_db = Some(db);
                }
                if let Some(ex) = &self.explain {
                    (self.emit)(explain_event(stmt.index, ex));
                }
            }
            Err(error) => {
                if error.kind == "connection_lost" && self.tx_now != TxState::Idle {
                    self.lost_in_tx = true;
                }
                (self.emit)(RunEvent::StatementError {
                    index: stmt.index,
                    error: error.clone(),
                });
            }
        }
        self.record(stmt, result);
    }

    fn notice(&mut self, index: u32, message: &str) {
        (self.emit)(RunEvent::Notice {
            index,
            message: message.into(),
        });
    }

    fn tx_state(&mut self, state: TxState) {
        self.tx_now = state;
        (self.emit)(RunEvent::TxState { state });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> (EventSink, Arc<Mutex<Vec<RunEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&seen);
        (
            Arc::new(move |e| {
                out.lock().unwrap().push(e);
                true
            }),
            seen,
        )
    }

    fn row(i: i64) -> Vec<Value> {
        vec![Value::Int(i)]
    }

    #[test]
    fn full_batches_go_at_once_partial_ones_wait() {
        let (emit, seen) = collector();
        let rows = RowStream::new(emit, 3);
        rows.start(
            0,
            RunEvent::ResultStart {
                result_id: 0,
                index: 0,
                columns: vec![],
            },
        );
        rows.push((0..7).map(row).collect());
        // Two full batches out, one row held.
        assert_eq!(seen.lock().unwrap().len(), 3);
        rows.push(vec![row(7)]);
        assert_eq!(seen.lock().unwrap().len(), 3);
        rows.end(RunEvent::Abandoning);
        let seen = seen.lock().unwrap();
        let sizes: Vec<usize> = seen
            .iter()
            .filter_map(|e| match e {
                RunEvent::Rows { rows, .. } => Some(rows.len()),
                _ => None,
            })
            .collect();
        assert_eq!(sizes, [3, 3, 2]);
        assert!(matches!(seen.last(), Some(RunEvent::Abandoning)));
    }

    #[test]
    fn held_rows_flush_after_the_interval() {
        let (emit, seen) = collector();
        let rows = RowStream::new(emit, 500);
        rows.start(
            4,
            RunEvent::ResultStart {
                result_id: 4,
                index: 0,
                columns: vec![],
            },
        );
        rows.push(vec![row(1)]);
        rows.flush_due();
        assert_eq!(seen.lock().unwrap().len(), 1, "not due yet");
        std::thread::sleep(ROWS_EVENT_INTERVAL + Duration::from_millis(5));
        rows.flush_due();
        assert!(matches!(
            seen.lock().unwrap().last(),
            Some(RunEvent::Rows { result_id: 4, rows }) if rows.len() == 1
        ));
    }

    #[test]
    fn user_stop_after_a_timeout_stop_sends_nothing_more() {
        let (emit, seen) = collector();
        let flags = RunFlags::default();
        // A timeout's `stopping` was sent for the running statement.
        flags.stopping.store(true, Ordering::SeqCst);
        flags.user_stop(&emit);
        flags.user_stop(&emit);
        assert!(seen.lock().unwrap().is_empty());
        assert!(
            flags.user.load(Ordering::SeqCst),
            "the user's stop is recorded"
        );

        let flags = RunFlags::default();
        flags.user_stop(&emit);
        flags.user_stop(&emit);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// 10 000 statements' history, sent as fast as a script runs, is written in
    /// batches on a file database: quickly, and in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_writer_batches_10k_rows_in_order() {
        use dnt_core::config::ConnectionConfig;
        use dnt_core::secret::MemorySecretStore;
        use dnt_store::history::{HistoryFilter, HistoryStatus};
        use dnt_store::repo::NewSecrets;

        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(ConnectionRepo::new(
            dnt_store::db::open(&dir.path().join("app.db")).unwrap(),
            Arc::new(MemorySecretStore::default()),
        ));
        let conn = repo
            .create(
                ConnectionConfig {
                    id: Uuid::nil(),
                    ..dnt_core::test_util::sample_config()
                },
                NewSecrets {
                    db_password: None,
                    ssh_password: None,
                    ssh_key_passphrase: None,
                },
            )
            .unwrap()
            .id;
        let started = Instant::now();
        let writer = HistoryWriter::start(Arc::clone(&repo), conn);
        for i in 0..10_000 {
            writer.send(NewHistoryEntry {
                connection_id: conn,
                database: None,
                sql_text: format!("SELECT {i}"),
                redacted: false,
                kind: "read".into(),
                started_at: "2026-10-20T10:00:00.000Z".into(),
                elapsed_ms: Some(1),
                status: HistoryStatus::Ok,
                row_count: Some(1),
                affected: None,
                error_kind: None,
                error_code: None,
            });
        }
        writer.finish().await;
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "took {took:?}");
        let newest = repo
            .history_list(&HistoryFilter {
                connection_id: Some(conn),
                search: None,
                limit: 1000,
            })
            .unwrap();
        let texts: Vec<String> = newest.into_iter().map(|e| e.sql_text).collect();
        let want: Vec<String> = (9_000..10_000)
            .rev()
            .map(|i| format!("SELECT {i}"))
            .collect();
        assert_eq!(texts, want);
    }

    #[test]
    fn started_at_has_fixed_millis() {
        let s = utc_millis_now();
        assert_eq!(s.len(), "2026-10-20T10:00:00.000Z".len(), "{s}");
        assert!(s.ends_with('Z') && s.as_bytes()[19] == b'.', "{s}");
    }

    #[test]
    fn events_serialize_with_tag() {
        let v = serde_json::to_value(RunEvent::Stopping {
            reason: StopCause::User,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "t": "stopping", "reason": "user" }));
        let v = serde_json::to_value(RunEvent::Done {
            stop: None,
            elapsed_ms: 3,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "t": "done", "stop": null, "elapsed_ms": 3 })
        );
    }
}
