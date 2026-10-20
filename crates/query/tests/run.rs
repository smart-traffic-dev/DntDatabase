//! The script runner against a scripted `FakeSession` (no database).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use dnt_core::config::EnvTag;
use dnt_core::driver::{
    CancelHandle, Canceller, ExecOptions, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome,
};
use dnt_core::error::{AppError, ConnectStage, DbError};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::server::{detect_pg, ServerInfo};
use dnt_core::sql::{Dialect, MySqlMode, StatementKind};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::run::{
    error_offset, run, RunCancel, RunReport, RunSink, StopReason, ROLLED_BACK_NOTICE,
};
use dnt_query::{guard, plan, PlannedStmt, ScriptPlan, Target, Verdict};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Fake session

/// What the fake does for one `execute_one` call.
struct FakeStep {
    result: Result<StmtOutcome, DbError>,
    /// The session's transaction state afterwards (`None`: unchanged).
    tx_after: Option<TxState>,
    /// The session's dialect afterwards (`None`: unchanged).
    dialect_after: Option<Dialect>,
    /// Reconnects the driver made during the call.
    reconnects: u64,
}

fn ok() -> FakeStep {
    FakeStep {
        result: Ok(StmtOutcome::default()),
        tx_after: None,
        dialect_after: None,
        reconnects: 0,
    }
}

fn fail(err: DbError) -> FakeStep {
    FakeStep {
        result: Err(err),
        ..ok()
    }
}

impl FakeStep {
    fn tx(mut self, state: TxState) -> Self {
        self.tx_after = Some(state);
        self
    }
    fn dialect(mut self, d: Dialect) -> Self {
        self.dialect_after = Some(d);
        self
    }
    fn reconnects(mut self, n: u64) -> Self {
        self.reconnects = n;
        self
    }
    fn late(mut self) -> Self {
        self.result = Ok(StmtOutcome {
            results: vec![],
            late_completion: true,
        });
        self
    }
}

fn server_err(message: &str) -> DbError {
    DbError::Server {
        code: None,
        sqlstate: Some("42P01".into()),
        message: message.into(),
        position: None,
    }
}

struct FakeSession {
    info: ServerInfo,
    steps: Mutex<VecDeque<FakeStep>>,
    sent: Mutex<Vec<String>>,
    tx: Mutex<TxState>,
    dialect: Mutex<Dialect>,
    reconnects: AtomicU64,
    /// Awaited by the first call before it looks at `opts.stop`.
    gate: Mutex<Option<Arc<Notify>>>,
    cancels: Arc<AtomicUsize>,
}

impl FakeSession {
    fn new(dialect: Dialect, steps: Vec<FakeStep>) -> Self {
        FakeSession {
            info: detect_pg(170002, "PostgreSQL 17.2"),
            steps: Mutex::new(steps.into()),
            sent: Mutex::new(vec![]),
            tx: Mutex::new(TxState::Idle),
            dialect: Mutex::new(dialect),
            reconnects: AtomicU64::new(0),
            gate: Mutex::new(None),
            cancels: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn in_state(self, state: TxState) -> Self {
        *self.tx.lock().unwrap() = state;
        self
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    fn cancel(&self) -> RunCancel {
        RunCancel::new(Arc::new(CountCancel(self.cancels.clone())) as CancelHandle)
    }
}

struct CountCancel(Arc<AtomicUsize>);

#[async_trait]
impl Canceller for CountCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        self.sent.lock().unwrap().push(stmt.to_string());
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if opts.stop.is_stopped() {
            return Err(DbError::cancelled());
        }
        let step = self.steps.lock().unwrap().pop_front().unwrap_or_else(ok);
        if let Some(state) = step.tx_after {
            *self.tx.lock().unwrap() = state;
        }
        if let Some(d) = step.dialect_after {
            *self.dialect.lock().unwrap() = d;
        }
        self.reconnects.fetch_add(step.reconnects, Ordering::SeqCst);
        if let Ok(outcome) = &step.result {
            for r in &outcome.results {
                sink.result_start(&[]);
                sink.result_end(r);
            }
        }
        step.result
    }

    fn tx_state(&self) -> TxState {
        *self.tx.lock().unwrap()
    }

    fn dialect(&self) -> Dialect {
        *self.dialect.lock().unwrap()
    }

    fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::SeqCst)
    }

    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(CountCancel(self.cancels.clone()))
    }

    fn introspector(&self) -> &dyn Introspector {
        self
    }

    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }

    async fn close(self: Box<Self>) {}
}

#[async_trait]
impl Introspector for FakeSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_objects(
        &self,
        _scope: &Scope,
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Ok(vec![])
    }
    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }
    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Err(DbError::Internal {
            message: "n/a".into(),
        })
    }
    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------
// Recording sink

#[derive(Default)]
struct Rec {
    events: Vec<String>,
    notices: Vec<(u32, String)>,
    tx: Vec<TxState>,
    /// Cancelled (to completion) inside `statement_end` of this statement.
    cancel_at_end: Option<(u32, RunCancel)>,
    /// Notified at `statement_start` of statement 0.
    started: Option<Arc<Notify>>,
}

impl RowSink for Rec {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.events.push(format!("result_start {}", cols.len()));
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.events.push(format!("rows {}", batch.len()));
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.events
            .push(format!("result_end {}", summary.row_count));
    }
}

impl RunSink for Rec {
    fn statement_start(&mut self, stmt: &PlannedStmt) {
        self.events.push(format!("start {}", stmt.index));
        if stmt.index == 0 {
            if let Some(n) = &self.started {
                n.notify_one();
            }
        }
    }
    fn statement_end(&mut self, stmt: &PlannedStmt, result: &Result<StmtOutcome, AppError>) {
        self.events.push(format!(
            "end {} {}",
            stmt.index,
            if result.is_ok() { "ok" } else { "err" }
        ));
        if let Some((index, cancel)) = &self.cancel_at_end {
            if *index == stmt.index {
                block_on(cancel.cancel()).unwrap();
            }
        }
    }
    fn notice(&mut self, index: u32, message: &str) {
        self.notices.push((index, message.to_string()));
    }
    fn tx_state(&mut self, state: TxState) {
        self.tx.push(state);
    }
}

/// Polls `f` to completion on this thread (for futures that never pend for real).
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers

const PG: Dialect = Dialect::Postgres;
const MY: Dialect = Dialect::MYSQL;
const NBE: Dialect = Dialect::MySql(MySqlMode {
    ansi_quotes: false,
    no_backslash_escapes: true,
});
const ANSI: Dialect = Dialect::MySql(MySqlMode {
    ansi_quotes: true,
    no_backslash_escapes: false,
});

fn all(text: &str, dialect: Dialect) -> ScriptPlan {
    plan(text, Target::All, dialect)
}

async fn go(
    session: &FakeSession,
    plan: &ScriptPlan,
    continue_on_error: bool,
    sink: &mut Rec,
) -> RunReport {
    let cancel = session.cancel();
    run(
        session,
        plan,
        &ExecOptions::default(),
        continue_on_error,
        &cancel,
        sink,
    )
    .await
}

fn err_of(report: &RunReport, i: usize) -> &AppError {
    report.stmts[i].result.as_ref().unwrap_err()
}

// ---------------------------------------------------------------------------
// Stop rules

#[tokio::test]
async fn stops_at_first_error() {
    let s = FakeSession::new(PG, vec![ok(), fail(server_err("boom")), ok()]);
    let p = all("SELECT 1; SELECT 2; SELECT 3", PG);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent(), ["SELECT 1", "SELECT 2"]);
    assert_eq!(r.stop, Some(StopReason::Error { index: 1 }));
    assert_eq!(r.stmts.len(), 2);
    assert_eq!(r.stmts[1].index, 1);
    let e = err_of(&r, 1);
    assert_eq!(e.kind, "server");
    assert_eq!(e.tx_state.as_deref(), Some("idle"));
}

#[tokio::test]
async fn continue_on_error_runs_rest() {
    let s = FakeSession::new(PG, vec![ok(), fail(server_err("boom")), ok()]);
    let p = all("SELECT 1; SELECT 2; SELECT 3", PG);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 3);
    assert_eq!(r.stop, None);
    assert!(r.stmts[1].result.is_err());
    assert!(r.stmts[2].result.is_ok());
}

#[tokio::test]
async fn error_in_transaction_always_stops() {
    let s = FakeSession::new(
        PG,
        vec![
            ok().tx(TxState::Active),
            fail(server_err("boom")).tx(TxState::Active),
        ],
    );
    let p = all("BEGIN; SELECT 2; SELECT 3", PG);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 2);
    assert_eq!(r.stop, Some(StopReason::Error { index: 1 }));
    assert_eq!(err_of(&r, 1).tx_state.as_deref(), Some("active"));
}

#[tokio::test]
async fn timeout_with_was_in_tx_stops() {
    // The driver already reset the state (e.g. MySQL rolled back): was_in_tx alone stops.
    let timeout = DbError::Timeout {
        stage: ConnectStage::Execute,
        outcome_unknown: false,
        was_in_tx: true,
    };
    let s = FakeSession::new(MY, vec![ok(), fail(timeout).tx(TxState::Idle)]);
    let p = all("SELECT 1; SELECT 2; SELECT 3", MY);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 2);
    assert_eq!(r.stop, Some(StopReason::Error { index: 1 }));
    let e = err_of(&r, 1);
    assert_eq!(e.kind, "timeout");
    assert!(!e.retryable);
}

#[tokio::test]
async fn connection_lost_always_stops() {
    let lost = DbError::ConnectionLost {
        was_in_tx: false,
        outcome_unknown: false,
        message: "gone".into(),
    };
    let s = FakeSession::new(PG, vec![ok(), fail(lost)]);
    let p = all("SELECT 1; SELECT 2; SELECT 3", PG);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 2);
    assert_eq!(r.stop, Some(StopReason::Error { index: 1 }));
    assert_eq!(err_of(&r, 1).kind, "connection_lost");
}

#[tokio::test]
async fn outcome_unknown_always_stops() {
    let unknown = DbError::Timeout {
        stage: ConnectStage::Execute,
        outcome_unknown: true,
        was_in_tx: false,
    };
    let s = FakeSession::new(PG, vec![ok(), fail(unknown)]);
    let p = all("SELECT 1; UPDATE t SET a = 1; SELECT 3", PG);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 2);
    assert_eq!(r.stop, Some(StopReason::Error { index: 1 }));
    // Never reported as not applied.
    assert!(err_of(&r, 1).outcome_unknown);
}

#[tokio::test]
async fn cancelled_error_stops_cancelled_and_keeps_outcome_unknown() {
    let killed = DbError::Cancelled {
        outcome_unknown: true,
        was_in_tx: false,
    };
    let s = FakeSession::new(MY, vec![fail(killed)]);
    let p = all("CALL p(); SELECT 2", MY);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 1);
    assert_eq!(r.stop, Some(StopReason::Cancelled));
    let e = err_of(&r, 0);
    assert_eq!(e.kind, "cancelled");
    assert!(e.outcome_unknown);
}

// ---------------------------------------------------------------------------
// Transaction state

#[tokio::test]
async fn aborted_tx_blocks_other_statements() {
    let s = FakeSession::new(PG, vec![]).in_state(TxState::Aborted);
    let p = all("SELECT 1; ROLLBACK", PG);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert!(s.sent().is_empty());
    assert!(r.stmts.is_empty());
    assert_eq!(r.stop, Some(StopReason::TxAborted));
}

#[tokio::test]
async fn aborted_tx_mid_script_stops_before_next() {
    let s = FakeSession::new(
        PG,
        vec![
            ok().tx(TxState::Active),
            fail(server_err("boom")).tx(TxState::Aborted),
        ],
    );
    // continue_on_error does not help: the error left a transaction open.
    let p = all("BEGIN; SELECT x; SELECT 3", PG);
    let mut rec = Rec::default();
    let r = go(&s, &p, true, &mut rec).await;
    assert_eq!(r.stop, Some(StopReason::Error { index: 1 }));
    assert_eq!(err_of(&r, 1).tx_state.as_deref(), Some("aborted"));
    assert_eq!(rec.tx, [TxState::Active, TxState::Aborted]);
}

#[tokio::test]
async fn rollback_to_allowed_when_aborted() {
    let s = FakeSession::new(PG, vec![ok().tx(TxState::Active), ok()]).in_state(TxState::Aborted);
    let p = all("ROLLBACK TO s; SELECT 1", PG);
    let mut rec = Rec::default();
    let r = go(&s, &p, false, &mut rec).await;
    assert_eq!(s.sent(), ["ROLLBACK TO s", "SELECT 1"]);
    assert_eq!(r.stop, None);
    assert_eq!(rec.tx, [TxState::Active]);
}

#[tokio::test]
async fn end_and_abort_allowed_when_aborted() {
    for text in ["END", "ABORT", "ROLLBACK", "COMMIT"] {
        let s = FakeSession::new(PG, vec![ok().tx(TxState::Idle)]).in_state(TxState::Aborted);
        let r = go(&s, &all(text, PG), false, &mut Rec::default()).await;
        assert_eq!(s.sent(), [text]);
        assert_eq!(r.stop, None, "{text}");
    }
}

#[tokio::test]
async fn commit_when_aborted_notices_rollback() {
    let s = FakeSession::new(PG, vec![ok().tx(TxState::Idle)]).in_state(TxState::Aborted);
    let mut rec = Rec::default();
    let r = go(&s, &all("COMMIT", PG), false, &mut rec).await;
    assert_eq!(r.stop, None);
    assert_eq!(rec.notices, [(0, ROLLED_BACK_NOTICE.to_string())]);
    assert_eq!(ROLLED_BACK_NOTICE, "Transaction was rolled back");
    assert_eq!(rec.tx, [TxState::Idle]);
}

#[tokio::test]
async fn commit_when_active_has_no_notice() {
    let s = FakeSession::new(PG, vec![ok().tx(TxState::Idle)]).in_state(TxState::Active);
    let mut rec = Rec::default();
    go(&s, &all("COMMIT", PG), false, &mut rec).await;
    assert!(rec.notices.is_empty());
}

#[tokio::test]
async fn tx_state_emitted_on_change_only() {
    let s = FakeSession::new(
        PG,
        vec![ok().tx(TxState::Active), ok(), ok().tx(TxState::Idle)],
    );
    let mut rec = Rec::default();
    go(&s, &all("BEGIN; SELECT 1; COMMIT", PG), false, &mut rec).await;
    assert_eq!(rec.tx, [TxState::Active, TxState::Idle]);
}

// ---------------------------------------------------------------------------
// Late completion and cancel

#[tokio::test]
async fn late_completion_stops() {
    let s = FakeSession::new(PG, vec![ok().late(), ok()]);
    let r = go(
        &s,
        &all("SELECT 1; SELECT 2", PG),
        false,
        &mut Rec::default(),
    )
    .await;
    assert_eq!(s.sent(), ["SELECT 1"]);
    assert_eq!(r.stop, Some(StopReason::LateCompletion));
    assert!(r.stmts[0].result.as_ref().unwrap().late_completion);
}

#[tokio::test]
async fn cancel_between_statements() {
    let s = FakeSession::new(PG, vec![ok(), ok()]);
    let p = all("SELECT 1; SELECT 2", PG);
    let cancel = s.cancel();
    let mut rec = Rec {
        cancel_at_end: Some((0, cancel.clone())),
        ..Rec::default()
    };
    let r = run(&s, &p, &ExecOptions::default(), false, &cancel, &mut rec).await;
    assert_eq!(s.sent(), ["SELECT 1"]);
    assert_eq!(r.stop, Some(StopReason::Cancelled));
    assert_eq!(r.stmts.len(), 1);
    assert!(cancel.is_cancelled());
    assert_eq!(s.cancels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stop_signal_reaches_execute_one() {
    let s = FakeSession::new(PG, vec![ok(), ok()]);
    let gate = Arc::new(Notify::new());
    *s.gate.lock().unwrap() = Some(gate.clone());
    let started = Arc::new(Notify::new());
    let p = all("SELECT 1; SELECT 2", PG);
    let cancel = s.cancel();
    let mut rec = Rec {
        started: Some(started.clone()),
        ..Rec::default()
    };
    // The caller's own signal is replaced by the run's.
    let opts = ExecOptions::default();
    let control = async {
        started.notified().await;
        cancel.cancel().await.unwrap();
        gate.notify_one();
    };
    let (r, ()) = tokio::join!(run(&s, &p, &opts, false, &cancel, &mut rec), control);
    assert_eq!(s.sent(), ["SELECT 1"]);
    assert_eq!(r.stop, Some(StopReason::Cancelled));
    assert_eq!(err_of(&r, 0).kind, "cancelled");
    assert!(!opts.stop.is_stopped());
    assert_eq!(s.cancels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_before_run_sends_nothing() {
    let s = FakeSession::new(PG, vec![]);
    let cancel = s.cancel();
    cancel.cancel().await.unwrap();
    let r = run(
        &s,
        &all("SELECT 1", PG),
        &ExecOptions::default(),
        false,
        &cancel,
        &mut Rec::default(),
    )
    .await;
    assert!(s.sent().is_empty());
    assert_eq!(r.stop, Some(StopReason::Cancelled));
}

#[tokio::test]
async fn sink_sees_statement_and_result_events_in_order() {
    let summary = ResultSummary {
        affected: None,
        row_count: 3,
        truncated: false,
        elapsed_ms: 1,
    };
    let one = FakeStep {
        result: Ok(StmtOutcome {
            results: vec![summary],
            late_completion: false,
        }),
        ..ok()
    };
    let s = FakeSession::new(PG, vec![one, fail(server_err("boom"))]);
    let mut rec = Rec::default();
    go(&s, &all("SELECT 1; SELECT 2", PG), false, &mut rec).await;
    assert_eq!(
        rec.events,
        [
            "start 0",
            "result_start 0",
            "result_end 3",
            "end 0 ok",
            "start 1",
            "end 1 err"
        ]
    );
}

// ---------------------------------------------------------------------------
// Reconnects

#[tokio::test]
async fn silent_reconnects_are_counted_and_noticed() {
    let s = FakeSession::new(PG, vec![ok().reconnects(1), ok(), ok().reconnects(1)]);
    let mut rec = Rec::default();
    let r = go(
        &s,
        &all("SELECT 1; SELECT 2; SELECT 3", PG),
        false,
        &mut rec,
    )
    .await;
    assert_eq!(r.stop, None);
    assert_eq!(r.reconnects, 2);
    let at: Vec<u32> = rec.notices.iter().map(|(i, _)| *i).collect();
    assert_eq!(at, [0, 2]);
    assert!(rec.notices[1].1.contains("2"), "{:?}", rec.notices);
}

#[tokio::test]
async fn reconnect_behind_connection_lost_is_not_noticed_twice() {
    let lost = DbError::ConnectionLost {
        was_in_tx: true,
        outcome_unknown: false,
        message: "gone".into(),
    };
    let s = FakeSession::new(PG, vec![fail(lost).reconnects(1)]);
    let mut rec = Rec::default();
    let r = go(&s, &all("SELECT 1", PG), false, &mut rec).await;
    assert_eq!(r.reconnects, 1);
    assert!(rec.notices.is_empty());
}

// ---------------------------------------------------------------------------
// sql_mode / dialect changes

#[tokio::test]
async fn dialect_changed_resplits() {
    let text = r"SET sql_mode='NO_BACKSLASH_ESCAPES'; SELECT 'a\'; SELECT 2'";
    let p = all(text, MY);
    assert_eq!(p.stmts.len(), 2);
    let s = FakeSession::new(MY, vec![ok().dialect(NBE)]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 1);
    assert_eq!(r.stop, Some(StopReason::DialectChanged { index: 1 }));
}

#[tokio::test]
async fn dialect_change_with_same_boundaries_continues() {
    let p = all(r#"SET sql_mode='ANSI_QUOTES'; SELECT "a;b"; SELECT 2"#, MY);
    let s = FakeSession::new(MY, vec![ok().dialect(ANSI)]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 3);
    assert_eq!(r.stop, None);
}

#[tokio::test]
async fn dialect_changed_before_first_statement() {
    // A reconnect between planning and running reset the session's sql_mode.
    let p = all(r"SELECT 'a\'; SELECT 2'", MY);
    let s = FakeSession::new(NBE, vec![]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert!(s.sent().is_empty());
    assert_eq!(r.stop, Some(StopReason::DialectChanged { index: 0 }));
}

#[tokio::test]
async fn dialect_changed_by_reconnect_mid_script() {
    // No SET sql_mode in the script: the silent reconnect of stmt 0 reset the mode.
    let p = all(r"SELECT 1; SELECT 'a\'; SELECT 2'", NBE);
    assert_eq!(p.stmts.len(), 3);
    let s = FakeSession::new(NBE, vec![ok().reconnects(1).dialect(MY)]);
    let mut rec = Rec::default();
    let r = go(&s, &p, false, &mut rec).await;
    assert_eq!(s.sent(), ["SELECT 1"]);
    assert_eq!(r.stop, Some(StopReason::DialectChanged { index: 1 }));
    assert_eq!(rec.notices.len(), 1);
}

#[tokio::test]
async fn dialect_change_with_cursor_plan_checks_only_its_statement() {
    let text = "SELECT 1; SELECT 2; SELECT 3";
    let p = plan(text, Target::Cursor(12), MY);
    assert_eq!(p.stmts.len(), 1);
    let s = FakeSession::new(ANSI, vec![]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent(), ["SELECT 2"]);
    assert_eq!(r.stop, None);
}

#[tokio::test]
async fn dialect_change_inside_delimiter_block_continues() {
    let text = "DELIMITER $$\nSET sql_mode='ANSI_QUOTES'$$\nSELECT 2$$\nSELECT 3$$\nDELIMITER ;";
    let p = all(text, MY);
    assert_eq!(p.stmts.len(), 3);
    let s = FakeSession::new(MY, vec![ok().dialect(ANSI)]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 3);
    assert_eq!(r.stop, None);
}

/// Under the default mode the backslash escapes the quote, so the string runs to the
/// end and the `SET` has one session assignment (Allow on Production). Under
/// `NO_BACKSLASH_ESCAPES` the string ends at `\'` and `GLOBAL` is a second, global
/// assignment (Confirm); the boundaries and the kind (`Other`) are the same.
const VERDICT_FLIP: &str =
    r"SET sql_mode='NO_BACKSLASH_ESCAPES'; SET @x = 'a\', GLOBAL max_connections = 1 -- '";

#[tokio::test]
async fn dialect_change_that_changes_a_verdict_stops() {
    let mut p = all(VERDICT_FLIP, MY);
    guard(&mut p, Some(EnvTag::Production), false, false);
    assert_eq!(p.stmts.len(), 2);
    assert_eq!(p.stmts[1].verdict, Verdict::Allow);
    let s = FakeSession::new(MY, vec![ok().dialect(NBE)]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 1);
    assert_eq!(r.stop, Some(StopReason::DialectChanged { index: 1 }));
}

#[tokio::test]
async fn dialect_change_with_same_verdicts_continues() {
    // Same script, not on Production: every verdict stays Allow.
    let mut p = all(VERDICT_FLIP, MY);
    guard(&mut p, Some(EnvTag::Local), false, false);
    let s = FakeSession::new(MY, vec![ok().dialect(NBE)]);
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 2);
    assert_eq!(r.stop, None);
}

#[tokio::test]
async fn dialect_rechecked_after_a_failed_statement() {
    // The failing statement still changed the session's mode (a driver re-reads a
    // stale mode inside `execute_one`); continue_on_error must not skip the check.
    let p = all(
        r"SET sql_mode = 'NO_BACKSLASH_ESCAPES,X'; SELECT 'a\'; SELECT 2'",
        MY,
    );
    assert_eq!(p.stmts.len(), 2);
    let s = FakeSession::new(MY, vec![fail(server_err("bad mode")).dialect(NBE)]);
    let r = go(&s, &p, true, &mut Rec::default()).await;
    assert_eq!(s.sent().len(), 1);
    assert!(r.stmts[0].result.is_err());
    assert_eq!(r.stop, Some(StopReason::DialectChanged { index: 1 }));
}

#[tokio::test]
async fn cancel_during_last_statement_that_completes_ok_gives_no_stop() {
    let s = FakeSession::new(PG, vec![ok(), ok()]);
    let p = all("SELECT 1; SELECT 2", PG);
    let cancel = s.cancel();
    let mut rec = Rec {
        cancel_at_end: Some((1, cancel.clone())),
        ..Rec::default()
    };
    let r = run(&s, &p, &ExecOptions::default(), false, &cancel, &mut rec).await;
    assert_eq!(s.sent().len(), 2);
    assert_eq!(r.stop, None);
    assert!(cancel.is_cancelled());
}

// ---------------------------------------------------------------------------
// Error offsets

fn stmt(range: (u32, u32), text: &str) -> PlannedStmt {
    PlannedStmt {
        index: 0,
        range,
        text: text.into(),
        kind: StatementKind::Read,
        verdict: Verdict::Allow,
        source_range: (0, text.len()),
    }
}

/// `error_offset` reads the raw driver error (never the mapped `AppError`, whose
/// `position` is already an editor offset).
fn pg_err(position: u32) -> DbError {
    DbError::Server {
        code: None,
        sqlstate: Some("42703".into()),
        message: "column does not exist".into(),
        position: Some(position),
    }
}

fn my_err(message: &str) -> DbError {
    DbError::Server {
        code: Some("1064".into()),
        sqlstate: Some("42000".into()),
        message: message.into(),
        position: None,
    }
}

#[test]
fn error_offset_ignores_errors_without_a_place() {
    let s = stmt((3, 14), "SELECT * FROM");
    assert_eq!(error_offset(&s, &DbError::cancelled()), None);
    let staged = DbError::at_stage(ConnectStage::Execute, vec![], pg_err(2));
    assert_eq!(error_offset(&s, &staged), Some(4));
}

const MY_SYNTAX: &str = "You have an error in your SQL syntax; check the manual that \
                         corresponds to your MySQL server version for the right syntax to use";

#[test]
fn error_offset_pg_position() {
    let s = stmt((10, 26), "SELECT ưx FROM t");
    assert_eq!(error_offset(&s, &pg_err(8)), Some(17));
}

#[test]
fn error_offset_pg_position_after_emoji_counts_utf16() {
    // 😀 is one char for PostgreSQL, two UTF-16 units for the editor.
    let s = stmt((0, 20), "SELECT '😀', x FROM t");
    assert_eq!(error_offset(&s, &pg_err(13)), Some(13));
    // Past the end clamps to the end of the statement.
    assert_eq!(error_offset(&s, &pg_err(999)), Some(21));
}

#[test]
fn error_offset_mysql_near_line() {
    let s = stmt((5, 30), "SELECT 1\nFROM nowhere x y");
    let e = my_err(&format!("{MY_SYNTAX} near 'x y' at line 2"));
    assert_eq!(error_offset(&s, &e), Some(27));
}

#[test]
fn error_offset_mysql_snippet_not_found_gives_line_start() {
    let s = stmt((5, 30), "SELECT 1\nFROM nowhere x y");
    let e = my_err(&format!("{MY_SYNTAX} near 'zzz' at line 2"));
    assert_eq!(error_offset(&s, &e), Some(14));
}

#[test]
fn error_offset_mysql_long_multiline_snippet_uses_first_20_chars() {
    let text = "SELECT 1\nFROM t WHERE a = 1 AND b =\n 2 GARBAGE";
    let s = stmt((0, 46), text);
    let snippet = "a = 1 AND b =\n 2 GARBAGE";
    let e = my_err(&format!("{MY_SYNTAX} near '{snippet}' at line 2"));
    assert_eq!(
        error_offset(&s, &e),
        Some(text.find("a = 1").unwrap() as u32)
    );
}

#[test]
fn error_offset_mysql_quote_in_snippet() {
    let text = "SELECT 'it''s' FROM";
    let s = stmt((0, 19), text);
    let e = my_err(&format!("{MY_SYNTAX} near ''s' FROM' at line 1"));
    assert_eq!(error_offset(&s, &e), Some(11));
}

#[test]
fn error_offset_mysql_empty_snippet_points_at_end() {
    let s = stmt((3, 14), "SELECT * FROM");
    let e = my_err(&format!("{MY_SYNTAX} near '' at line 1"));
    assert_eq!(error_offset(&s, &e), Some(16));
}

#[test]
fn error_offset_none_without_position_or_near() {
    let s = stmt((3, 14), "SELECT * FROM");
    assert_eq!(error_offset(&s, &my_err("Table 't' doesn't exist")), None);
    let e = my_err(&format!("{MY_SYNTAX} near 'x' at line 9"));
    assert_eq!(error_offset(&s, &e), None);
}

#[tokio::test]
async fn statement_error_position_is_an_editor_offset() {
    let text = "SELECT 1;\nSELECT ưx FROM t";
    let p = all(text, PG);
    assert_eq!(p.stmts[1].range, (10, 26));
    let s = FakeSession::new(
        PG,
        vec![
            ok(),
            fail(DbError::Server {
                code: None,
                sqlstate: Some("42703".into()),
                message: "column \"ưx\" does not exist".into(),
                position: Some(8),
            }),
        ],
    );
    let r = go(&s, &p, false, &mut Rec::default()).await;
    assert_eq!(err_of(&r, 1).position, Some(17));
}

#[test]
fn stop_reason_serde_shape() {
    assert_eq!(
        serde_json::to_value(StopReason::Error { index: 2 }).unwrap(),
        serde_json::json!({"kind": "error", "index": 2})
    );
    assert_eq!(
        serde_json::to_value(StopReason::TxAborted).unwrap(),
        serde_json::json!({"kind": "tx_aborted"})
    );
    assert_eq!(
        serde_json::to_value(StopReason::DialectChanged { index: 1 }).unwrap(),
        serde_json::json!({"kind": "dialect_changed", "index": 1})
    );
}
