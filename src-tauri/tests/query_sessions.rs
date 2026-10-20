//! Per-tab query sessions and run plans against an in-process fake driver (no database).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, ExecProgress, Introspector, ResultSummary,
    RowSink, Session, StmtOutcome,
};
use dnt_core::error::{AppError, ConnectStage, DbError};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, detect_pg, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_query::{
    DropKind, DropTarget, RowEstimate, StmtKindLabel, Target, UnboundedReason, UnboundedWrite,
    Verdict, WriteOp,
};
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryFilter, HistoryStatus};
use dnt_store::repo::{ConnectionRepo, NewSecrets, SecretChange, SecretUpdate};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::settings::CONFIRM_UNBOUNDED_KEY;
use dntdatabase::query_sessions::{
    tx_state_after, with_tx_state, PlanView, TabTxInfo, TxAction, TxMode, PREVIEW_CHARS,
};
use dntdatabase::run_events::{EventSink, RunOptions};
use dntdatabase::session_manager::SessionManager;
use secrecy::SecretString;
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Notify;
use uuid::Uuid;

/// What the fake driver saw, and the state the test scripts for it.
#[derive(Default)]
struct Log {
    connects: AtomicUsize,
    closes: AtomicUsize,
    /// `ResolvedConfig::database` of every connect, in order.
    databases: Mutex<Vec<Option<String>>>,
    /// Every statement passed to `execute_one`, in order.
    executed: Mutex<Vec<String>>,
    /// The transaction state every session reports; `COMMIT`/`ROLLBACK` reset it.
    tx: Mutex<Option<TxState>>,
    /// While set, every connect signals `entered` and waits for this gate.
    gate: Mutex<Option<Arc<Notify>>>,
    /// Signalled (one permit) when a connect reaches the gate.
    entered: Notify,
    /// Set by the session's cancel handle; cleared when a statement starts.
    cancelled: AtomicBool,
    /// While set, `COMMIT` / `ROLLBACK` lose the connection.
    lose_on_tx: AtomicBool,
    /// While set, `COMMIT` / `ROLLBACK` time out (the session stays).
    timeout_on_tx: AtomicBool,
    /// Calls of the session's cancel handle.
    cancel_calls: AtomicUsize,
    /// Set once a `FLAKY` statement lost its connection (the next one succeeds).
    flaked: AtomicBool,
    /// While set, `ROLLBACK TO SAVEPOINT …` fails with a server error.
    fail_rollback_to: AtomicBool,
}

impl Log {
    fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
    fn closes(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }
    fn set_tx(&self, state: TxState) {
        *self.tx.lock().unwrap() = Some(state);
    }
    fn executed(&self) -> Vec<String> {
        self.executed.lock().unwrap().clone()
    }
    /// From now on every connect waits at the gate until `open_gate`.
    fn close_gate(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }
    /// Releases the connect waiting at `gate`; later connects pass freely.
    fn open_gate(&self, gate: &Notify) {
        self.gate.lock().unwrap().take();
        gate.notify_one();
    }
}

struct FakeDriver {
    kind: DbKind,
    log: Arc<Log>,
}

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        self.log.connects.fetch_add(1, Ordering::SeqCst);
        self.log
            .databases
            .lock()
            .unwrap()
            .push(cfg.database.clone());
        let gate = self.log.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            self.log.entered.notify_one();
            gate.notified().await;
        }
        let info = match self.kind {
            DbKind::MySql => detect_mysql("8.0.36").unwrap(),
            DbKind::Postgres => detect_pg(170002, "PostgreSQL 17.2"),
        };
        Ok(Box::new(FakeSession {
            info,
            log: Arc::clone(&self.log),
        }))
    }
}

struct FakeSession {
    info: ServerInfo,
    log: Arc<Log>,
}

struct FakeCancel(Arc<Log>);

#[async_trait]
impl Canceller for FakeCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.cancel_calls.fetch_add(1, Ordering::SeqCst);
        self.0.cancelled.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn column(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        db_type: "INT".into(),
        logical: LogicalType::Integer,
        nullable: Some(false),
        origin_table: None,
        origin_column: None,
    }
}

/// Streams `n` rows (`Int(0..n)`) in batches of `batch`.
fn stream_rows(sink: &mut dyn RowSink, n: i64, batch: usize) -> ResultSummary {
    sink.result_start(&[column("n")]);
    let rows: Vec<Vec<Value>> = (0..n).map(|i| vec![Value::Int(i)]).collect();
    for chunk in rows.chunks(batch.max(1)) {
        sink.rows(chunk.to_vec());
    }
    let summary = ResultSummary {
        affected: None,
        row_count: n as u64,
        truncated: false,
        elapsed_ms: 1,
    };
    sink.result_end(&summary);
    summary
}

/// The number after `prefix` in `upper` (`ROWS_1200` → 1200).
fn number_after(upper: &str, prefix: &str) -> Option<i64> {
    let at = upper.find(prefix)? + prefix.len();
    upper[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

impl FakeSession {
    fn stop_requested(&self, opts: &ExecOptions) -> bool {
        opts.stop.is_stopped() || self.log.cancelled.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    /// Scripted by the statement text (upper-cased):
    /// `ROWS_<n>` streams n rows in `batch_size` batches; `TRICKLE` streams 10 batches
    /// of 10 rows; `EXPLAIN …` returns a one-cell plan; `FAIL` is a server error 1146;
    /// `LOSE` loses the connection (`LOSE_TX`: during a transaction; `FLAKY`: only the
    /// first time); `SLOW` streams a row every 20 ms for 10 s until
    /// stopped (then `Cancelled`); `STUCK` ignores a cancel for the whole grace.
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        self.log.executed.lock().unwrap().push(stmt.to_string());
        self.log.cancelled.store(false, Ordering::SeqCst);
        if opts.stop.is_stopped() {
            return Err(DbError::cancelled());
        }
        let upper = stmt.trim().to_ascii_uppercase();
        if upper == "COMMIT" || upper == "ROLLBACK" {
            if self.log.lose_on_tx.load(Ordering::SeqCst) {
                self.log.set_tx(TxState::Idle);
                // A driver that does not know the COMMIT went out: the session layer
                // must still report the outcome as unknown.
                return Err(DbError::ConnectionLost {
                    was_in_tx: true,
                    outcome_unknown: false,
                    message: "server gone".into(),
                });
            }
            if self.log.timeout_on_tx.load(Ordering::SeqCst) {
                return Err(DbError::Timeout {
                    stage: ConnectStage::Execute,
                    outcome_unknown: false,
                    was_in_tx: true,
                });
            }
            self.log.set_tx(TxState::Idle);
        }
        if upper == "BEGIN" || upper == "START TRANSACTION" {
            self.log.set_tx(TxState::Active);
        }
        // MySQL DDL commits the open transaction.
        if self.info.kind == DbKind::MySql && upper.starts_with("CREATE TABLE") {
            self.log.set_tx(TxState::Idle);
        }
        let ok = |summary: ResultSummary| {
            Ok(StmtOutcome {
                results: vec![summary],
                late_completion: false,
            })
        };
        if let Some(n) = number_after(&upper, "ROWS_") {
            return ok(stream_rows(sink, n, opts.batch_size));
        }
        if upper.contains("TRICKLE") {
            sink.result_start(&[column("n")]);
            for b in 0..10 {
                sink.rows((0..10).map(|i| vec![Value::Int(b * 10 + i)]).collect());
            }
            let summary = ResultSummary {
                affected: None,
                row_count: 100,
                truncated: false,
                elapsed_ms: 1,
            };
            sink.result_end(&summary);
            return ok(summary);
        }
        if upper.starts_with("ROLLBACK TO SAVEPOINT")
            && self.log.fail_rollback_to.load(Ordering::SeqCst)
        {
            return Err(DbError::Server {
                code: None,
                sqlstate: Some("3B001".into()),
                message: "savepoint does not exist".into(),
                position: None,
            });
        }
        // Row estimates (`EXPLAIN DELETE …` / `EXPLAIN (FORMAT JSON) UPDATE …`).
        let estimate = [
            "EXPLAIN DELETE",
            "EXPLAIN UPDATE",
            "EXPLAIN (FORMAT JSON) DELETE",
            "EXPLAIN (FORMAT JSON) UPDATE",
        ]
        .iter()
        .any(|p| upper.starts_with(p));
        if estimate {
            if upper.contains("SLOW") {
                // Honours `opts.timeout` as the real drivers do.
                let started = Instant::now();
                let limit = opts.timeout.unwrap_or(Duration::from_secs(10));
                while started.elapsed() < limit {
                    if self.stop_requested(opts) {
                        return Err(DbError::cancelled());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                return Err(DbError::timeout(ConnectStage::Execute));
            }
            if upper.contains("FAIL") {
                return Err(DbError::Server {
                    code: None,
                    sqlstate: Some("42P01".into()),
                    message: "relation does not exist".into(),
                    position: None,
                });
            }
            if self.info.kind == DbKind::Postgres {
                let plan = r#"[{"Plan": {"Node Type": "ModifyTable", "Plan Rows": 0, "Plans": [{"Node Type": "Seq Scan", "Plan Rows": 321}]}}]"#;
                sink.result_start(&[column("QUERY PLAN")]);
                sink.rows(vec![vec![Value::Text(plan.into())]]);
            } else {
                sink.result_start(&[column("table"), column("rows")]);
                sink.rows(vec![
                    vec![Value::Text("other".into()), Value::Int(1)],
                    vec![Value::Text("orders".into()), Value::Int(42)],
                ]);
            }
            let summary = ResultSummary {
                affected: None,
                row_count: 1,
                truncated: false,
                elapsed_ms: 1,
            };
            sink.result_end(&summary);
            return ok(summary);
        }
        if upper.starts_with("EXPLAIN") {
            let plan = if upper.starts_with("EXPLAIN ANALYZE") {
                "-> Rows fetched before execution  (cost=0.00..0.00 rows=1) (actual time=0.001..0.001 rows=1 loops=1)"
            } else if upper.contains("JSONV2") {
                // `explain_json_format_version = 2` (MySQL 8.3+).
                r#"{"query": "/* select#1 */ select 1", "operation": "Rows fetched before execution", "access_type": "rows_fetched_before_execution", "estimated_rows": 1.0}"#
            } else {
                r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "1.00"}, "table": {"table_name": "t", "access_type": "ALL", "rows_examined_per_scan": 1}}}"#
            };
            sink.result_start(&[column("EXPLAIN")]);
            sink.rows(vec![vec![Value::Text(plan.into())]]);
            let summary = ResultSummary {
                affected: None,
                row_count: 1,
                truncated: false,
                elapsed_ms: 1,
            };
            sink.result_end(&summary);
            return ok(summary);
        }
        if upper.contains("FAIL") {
            return Err(DbError::Server {
                code: Some("1146".into()),
                sqlstate: Some("42S02".into()),
                message: "Table 'shop.fail' doesn't exist".into(),
                position: None,
            });
        }
        if upper.contains("PANIC") {
            panic!("the fake driver panics");
        }
        if upper.contains("LOSE_TX") {
            self.log.set_tx(TxState::Idle);
            return Err(DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message: "server gone".into(),
            });
        }
        if upper.contains("FLAKY") && !self.log.flaked.swap(true, Ordering::SeqCst) {
            return Err(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message: "server gone".into(),
            });
        }
        if upper.contains("LOSE") {
            return Err(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message: "server gone".into(),
            });
        }
        if upper.contains("SLOW") {
            sink.result_start(&[column("n")]);
            for i in 0..500 {
                if self.stop_requested(opts) {
                    sink.progress(ExecProgress::CancelSent);
                    return Err(DbError::Cancelled {
                        outcome_unknown: false,
                        was_in_tx: false,
                    });
                }
                sink.rows(vec![vec![Value::Int(i)]]);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let summary = ResultSummary {
                affected: None,
                row_count: 500,
                truncated: false,
                elapsed_ms: 10_000,
            };
            sink.result_end(&summary);
            return ok(summary);
        }
        if upper.contains("STUCK") {
            while !self.stop_requested(opts) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            sink.progress(ExecProgress::CancelSent);
            tokio::time::sleep(opts.cancel_grace).await;
            sink.progress(ExecProgress::GraceExpired);
            return Err(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                message: "the statement did not stop; the connection was replaced".into(),
            });
        }
        let summary = ResultSummary {
            affected: Some(0),
            row_count: 0,
            truncated: false,
            elapsed_ms: 0,
        };
        sink.result_start(&[]);
        sink.result_end(&summary);
        ok(summary)
    }
    fn tx_state(&self) -> TxState {
        self.log.tx.lock().unwrap().unwrap_or(TxState::Idle)
    }
    fn dialect(&self) -> Dialect {
        Dialect::for_kind(self.info.kind)
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(FakeCancel(Arc::clone(&self.log)))
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }
    async fn close(self: Box<Self>) {
        self.log.closes.fetch_add(1, Ordering::SeqCst);
    }
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

struct Fixture {
    sm: Arc<SessionManager>,
    repo: Arc<ConnectionRepo>,
    log: Arc<Log>,
    _dir: tempfile::TempDir,
}

fn fixture_with(kind: DbKind, tune: impl FnOnce(SessionManager) -> SessionManager) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let log = Arc::new(Log::default());
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver {
        kind,
        log: Arc::clone(&log),
    });
    let dir = tempfile::tempdir().unwrap();
    let sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(kind, driver)]),
    );
    Fixture {
        sm: Arc::new(tune(sm)),
        repo,
        log,
        _dir: dir,
    }
}

fn fixture(kind: DbKind) -> Fixture {
    fixture_with(kind, |sm| sm)
}

/// A saved connection without a default database (so no SchemaCache load starts).
fn config(kind: DbKind) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        kind,
        name: "Shop DB".into(),
        default_db: None,
        env_tag: None,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ..sample_config()
    }
}

/// Saves `cfg` and opens it; the connect of `open` (the metadata session) is counted.
async fn open(f: &Fixture, cfg: ConnectionConfig) -> Uuid {
    let id = f
        .repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(SecretString::from("pw")),
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    f.sm.open(id, None).await.unwrap();
    id
}

async fn plan(
    f: &Fixture,
    tab: &str,
    conn: Uuid,
    db: Option<&str>,
    text: &str,
) -> Result<dntdatabase::query_sessions::PlanView, DbError> {
    f.sm.query_plan(tab, conn, db.map(str::to_string), text, Target::All)
        .await
}

#[tokio::test]
async fn plan_opens_session_lazily_once() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    assert_eq!(f.log.connects(), 1, "the metadata session");

    let first = plan(&f, "t1", id, None, "SELECT 1; SELECT 2")
        .await
        .unwrap();
    let second = plan(&f, "t1", id, None, "SELECT 3").await.unwrap();
    assert_eq!(f.log.connects(), 2, "one query session for the tab");
    assert_ne!(first.plan_id, second.plan_id);
    assert_eq!(first.stmts.len(), 2);
    assert_eq!(first.stmts[1].index, 1);
    assert_eq!(first.stmts[1].range, (10, 18));
    assert_eq!(first.stmts[1].kind, StmtKindLabel::Read);
    assert_eq!(first.dialect, Dialect::MYSQL);
    assert_eq!(first.tx_state, TxState::Idle);
    assert_eq!(first.connection_name, "Shop DB");
    // Planning sends nothing to the server.
    assert!(f.log.executed().is_empty());

    // Another tab gets its own session.
    plan(&f, "t2", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
}

#[tokio::test]
async fn plan_returns_verdicts_from_connection_config() {
    let f = fixture(DbKind::MySql);
    let prod = open(
        &f,
        ConnectionConfig {
            env_tag: Some(EnvTag::Production),
            ..config(DbKind::MySql)
        },
    )
    .await;
    let view = plan(&f, "t1", prod, None, "SELECT 1; DELETE FROM t")
        .await
        .unwrap();
    let verdicts: Vec<Verdict> = view.stmts.iter().map(|s| s.verdict).collect();
    assert_eq!(verdicts, [Verdict::Allow, Verdict::Confirm]);
    assert_eq!(view.stmts[1].kind, StmtKindLabel::Write);
    assert_eq!(view.env_tag, Some(EnvTag::Production));
    assert!(!view.read_only);

    let ro = open(
        &f,
        ConnectionConfig {
            read_only: true,
            ..config(DbKind::MySql)
        },
    )
    .await;
    let view = plan(&f, "t2", ro, None, "DELETE FROM t").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Block);
    assert!(view.read_only);

    // The guard follows the connection's current config, not the one it was opened with.
    let mut cfg = f.repo.get(prod).unwrap();
    cfg.env_tag = Some(EnvTag::Local);
    let keep = SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    };
    f.repo.update(cfg, keep).unwrap();
    let view = plan(&f, "t1", prod, None, "DELETE FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    assert_eq!(view.env_tag, Some(EnvTag::Local));
}

/// Sets the "Confirm UPDATE/DELETE without WHERE" setting as the Settings dialog does.
fn set_confirm_unbounded(f: &Fixture, on: bool) {
    f.repo
        .set_app_setting(CONFIRM_UNBOUNDED_KEY, if on { "1" } else { "0" })
        .unwrap();
}

#[tokio::test]
async fn unbounded_writes_confirm_on_every_connection() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let prod = open_tagged(&f, Some(EnvTag::Production), false).await;
    let text = "SELECT 1; DELETE FROM orders; UPDATE t SET a = 1 WHERE id = 1";
    let verdicts = |v: &PlanView| v.stmts.iter().map(|s| s.verdict).collect::<Vec<_>>();

    // Setting on (the default): confirmed on Local too, flag and table in the view.
    let view = plan(&f, "t1", local, None, text).await.unwrap();
    assert_eq!(
        verdicts(&view),
        [Verdict::Allow, Verdict::Confirm, Verdict::Allow]
    );
    assert_eq!(
        view.stmts[1].unbounded,
        [UnboundedWrite {
            op: WriteOp::Delete,
            reason: UnboundedReason::NoWhere,
            table: "orders".into(),
        }]
    );
    assert!(view.stmts[0].unbounded.is_empty() && view.stmts[2].unbounded.is_empty());
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
    assert!(f.log.executed().is_empty());

    // Setting off: allowed on Local, still flagged for the UI.
    set_confirm_unbounded(&f, false);
    let view = plan(&f, "t1", local, None, text).await.unwrap();
    assert_eq!(verdicts(&view), [Verdict::Allow; 3]);
    assert_eq!(view.stmts[1].unbounded.len(), 1);

    // Production always confirms, whatever the setting: a DELETE without WHERE (not
    // DDL, so only the Production rule asks) ...
    let view = plan(&f, "t2", prod, None, "DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].unbounded[0].op, WriteOp::Delete);
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
    // Only its row estimate was sent (Production).
    assert_eq!(f.log.executed(), ["EXPLAIN DELETE FROM orders"]);
    // ... and a TRUNCATE.
    let view = plan(&f, "t2", prod, None, "TRUNCATE orders").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].unbounded[0].op, WriteOp::Truncate);
}

/// An export is judged with the same guard context as a run (the unbounded-write
/// setting included): with the setting off a DELETE without WHERE is allowed for a
/// run, yet an export still only reads again.
#[tokio::test]
async fn exports_judge_with_the_run_guard_context() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    for on in [true, false] {
        set_confirm_unbounded(&f, on);
        let view = plan(&f, "t1", local, None, "DELETE FROM orders")
            .await
            .unwrap();
        let mut sink = ExportRows::default();
        let (err, state) = export(&f, view.plan_id, Uuid::new_v4(), &mut sink)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DbError::Config { field: Some(s), .. } if s == "scope"),
            "{on}: {err:?}"
        );
        assert_eq!(state, None);
        let view = plan(&f, "t1", local, None, "SELECT rows_2").await.unwrap();
        export(&f, view.plan_id, Uuid::new_v4(), &mut sink)
            .await
            .unwrap();
    }
    assert!(!f.log.executed().iter().any(|s| s.starts_with("DELETE")));
}

#[tokio::test]
async fn unbounded_setting_turned_off_between_plan_and_run() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let prod = open_tagged(&f, Some(EnvTag::Production), false).await;

    // Local: planned as Confirm, the user confirmed, then the setting went off; the
    // confirmed run goes through once.
    let view = plan(&f, "t1", local, None, "DELETE FROM t").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    set_confirm_unbounded(&f, false);
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(f.log.executed(), ["DELETE FROM t"]);

    // Production: the setting going off never lifts the confirmation.
    set_confirm_unbounded(&f, true);
    let view = plan(&f, "t2", prod, None, "DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    set_confirm_unbounded(&f, false);
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
    // Only the Production plan's row estimate was sent after the Local run.
    assert_eq!(
        f.log.executed(),
        ["DELETE FROM t", "EXPLAIN DELETE FROM orders"]
    );
}

#[tokio::test]
async fn fetch_all_and_retry_of_a_run_keep_the_unbounded_flag() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let view = plan(&f, "t1", local, None, "SELECT 1;\nDELETE FROM t")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;

    // Retry / Fetch all of the taken plan: flagged, confirmed again.
    let again = f.sm.query_plan_statement(view.plan_id, 1).await.unwrap();
    assert_eq!(again.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(again.stmts[0].unbounded, view.stmts[1].unbounded);
    let (emit, _) = collector();
    let err =
        f.sm.query_run(again.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");

    // Setting off: allowed on Local, the flag is still in the view.
    set_confirm_unbounded(&f, false);
    let again = f.sm.query_plan_statement(view.plan_id, 1).await.unwrap();
    assert_eq!(again.stmts[0].verdict, Verdict::Allow);
    assert_eq!(again.stmts[0].unbounded, view.stmts[1].unbounded);
    let all = RunOptions {
        max_rows: 1_000_000,
        continue_on_error: false,
    };
    let (emit, events) = collector();
    f.sm.query_run(again.plan_id, false, all, emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(
        f.log.executed(),
        ["SELECT 1", "DELETE FROM t", "DELETE FROM t"]
    );
}

#[tokio::test]
async fn unbounded_setting_is_read_again_at_run() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    set_confirm_unbounded(&f, false);
    let view = plan(&f, "t1", local, None, "DELETE FROM t").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    // Turned on between plan and run: the run asks again.
    set_confirm_unbounded(&f, true);
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(f.log.executed(), ["DELETE FROM t"]);
}

#[tokio::test]
async fn retry_keeps_the_unbounded_flag() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let view = plan(&f, "t1", local, None, "SELECT 1;\nUPDATE t SET a = 1")
        .await
        .unwrap();
    assert_eq!(view.stmts[1].verdict, Verdict::Confirm);
    let retry = f.sm.query_plan_statement(view.plan_id, 1).await.unwrap();
    assert_eq!(retry.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(retry.stmts[0].unbounded, view.stmts[1].unbounded);
    assert_eq!(retry.stmts[0].unbounded[0].table, "t");
    let (emit, _) = collector();
    let err =
        f.sm.query_run(retry.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
}

#[tokio::test]
async fn unbounded_write_in_a_delimiter_script_is_confirmed() {
    let f = fixture(DbKind::MySql);
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let text = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN DELETE FROM t; END$$\nSELECT 1; DELETE FROM u$$\nDELIMITER ;";
    let view = plan(&f, "t1", local, None, text).await.unwrap();
    let verdicts: Vec<Verdict> = view.stmts.iter().map(|s| s.verdict).collect();
    // The routine body is exempt; the forgotten-delimiter piece is not.
    assert_eq!(verdicts, [Verdict::Allow, Verdict::Confirm]);
    assert_eq!(view.stmts[1].unbounded[0].table, "u");
}

/// Runs plan `id` confirmed, with `names` typed; the error kind when refused.
async fn run_named(f: &Fixture, id: Uuid, names: &[&str]) -> Result<(), String> {
    let (emit, events) = collector();
    let names = names.iter().map(|s| s.to_string()).collect();
    match f.sm.query_run_named(id, true, names, opts(), emit).await {
        Ok(_) => {
            events.done(LONG).await;
            Ok(())
        }
        Err(e) => Err(kind(e)),
    }
}

#[tokio::test]
async fn wrong_typed_name_is_rejected() {
    let f = fixture(DbKind::MySql);
    let prod = open_tagged(&f, Some(EnvTag::Production), false).await;
    let text = "SELECT 1; DELETE FROM orders; DROP TABLE IF EXISTS `Audit`";
    let view = plan(&f, "t1", prod, None, text).await.unwrap();
    let names: Vec<Option<&str>> = view
        .stmts
        .iter()
        .map(|s| s.confirm_name.as_deref())
        .collect();
    assert_eq!(names, [None, Some("orders"), Some("Audit")]);
    assert_eq!(
        view.stmts[2].drops,
        [DropTarget {
            kind: DropKind::Object,
            name: "Audit".into()
        }]
    );
    assert!(view.stmts[0].drops.is_empty() && view.stmts[1].drops.is_empty());

    // Missing, wrong case, partial or extra names: refused, nothing runs, and the plan
    // stays available.
    for typed in [
        &[][..],
        &["orders"][..],
        &["orders", "audit"][..],
        &["ORDERS", "Audit"][..],
        &["orders", "Audit", "extra"][..],
    ] {
        assert_eq!(
            run_named(&f, view.plan_id, typed).await.unwrap_err(),
            "confirmation_required",
            "{typed:?}"
        );
    }
    // `confirmed` without names is refused too.
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, true, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
    // Only the row estimate of planning was sent.
    assert_eq!(f.log.executed(), ["EXPLAIN DELETE FROM orders"]);

    // Every name, in any order: it runs.
    run_named(&f, view.plan_id, &["Audit", "orders"])
        .await
        .unwrap();
    assert_eq!(
        f.log.executed(),
        [
            "EXPLAIN DELETE FROM orders",
            "SELECT 1",
            "DELETE FROM orders",
            "DROP TABLE IF EXISTS `Audit`"
        ]
    );

    // A Production confirmation without a risky statement needs no name, and refuses one.
    let view = plan(&f, "t1", prod, None, "INSERT INTO t VALUES (1)")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].confirm_name, None);
    assert_eq!(
        run_named(&f, view.plan_id, &["t"]).await.unwrap_err(),
        "confirmation_required"
    );
    run_named(&f, view.plan_id, &[]).await.unwrap();

    // Off Production no name is asked: DROP DATABASE is confirmed on Local.
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let view = plan(&f, "t2", local, None, "DROP DATABASE scratch")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].confirm_name, None);
    assert_eq!(view.stmts[0].drops[0].kind, DropKind::Database);
    run_named(&f, view.plan_id, &[]).await.unwrap();
}

/// The names are checked against the config as it is at run time: a connection tagged
/// Production after the plan asks for them.
#[tokio::test]
async fn typed_names_follow_the_current_config() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Local), false).await;
    set_confirm_unbounded(&f, false);
    let view = plan(&f, "t1", id, None, "DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    let mut cfg = f.repo.get(id).unwrap();
    cfg.env_tag = Some(EnvTag::Production);
    let keep = SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    };
    f.repo.update(cfg, keep).unwrap();
    assert_eq!(
        run_named(&f, view.plan_id, &[]).await.unwrap_err(),
        "confirmation_required"
    );
    run_named(&f, view.plan_id, &["orders"]).await.unwrap();
    // Planned on Local: no row estimate was sent.
    assert_eq!(f.log.executed(), ["DELETE FROM orders"]);
}

#[tokio::test]
async fn estimates_on_staging_and_production_only() {
    let f = fixture(DbKind::MySql);
    let text = "DELETE FROM orders; UPDATE t SET a = 1 WHERE id = 1; TRUNCATE x; SELECT 1";
    for env in [Some(EnvTag::Local), None] {
        let id = open_tagged(&f, env, false).await;
        let view = plan(&f, "t1", id, None, text).await.unwrap();
        assert!(view.stmts.iter().all(|s| s.estimate.is_none()), "{env:?}");
        assert!(f.log.executed().is_empty(), "{env:?}");
    }
    for env in [EnvTag::Staging, EnvTag::Production] {
        let id = open_tagged(&f, Some(env), false).await;
        let view = plan(&f, "t1", id, None, text).await.unwrap();
        let estimates: Vec<Option<RowEstimate>> = view.stmts.iter().map(|s| s.estimate).collect();
        // The `rows` of the target table; bounded writes and TRUNCATE are not estimated.
        assert_eq!(
            estimates,
            [Some(RowEstimate::Rows { n: 42.0 }), None, None, None],
            "{env:?}"
        );
        assert_eq!(f.log.executed(), ["EXPLAIN DELETE FROM orders"]);
        assert_eq!(view.tx_state, TxState::Idle);
        f.log.executed.lock().unwrap().clear();
    }
    // Read-only blocks: nothing is explained.
    let ro = open_tagged(&f, Some(EnvTag::Staging), true).await;
    let view = plan(&f, "t9", ro, None, "DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Block);
    assert_eq!(view.stmts[0].estimate, None);
    assert!(f.log.executed().is_empty());
}

#[tokio::test]
async fn estimate_timeout_is_unavailable() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Staging), false).await;
    let started = Instant::now();
    let view = plan(&f, "t1", id, None, "DELETE FROM slow_t")
        .await
        .unwrap();
    let took = started.elapsed();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Unavailable));
    assert!(
        took >= Duration::from_millis(1900) && took < Duration::from_secs(4),
        "{took:?}"
    );
    // A failed explain is unavailable too, and the plan still runs.
    let view = plan(&f, "t1", id, None, "DELETE FROM fail_t")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Unavailable));
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
}

/// PostgreSQL: an estimate inside an open transaction runs under a savepoint, so a
/// failed explain cannot abort the transaction; none runs in an aborted one.
#[tokio::test]
async fn pg_estimates_keep_the_transaction() {
    let f = fixture(DbKind::Postgres);
    let id = open(
        &f,
        ConnectionConfig {
            env_tag: Some(EnvTag::Production),
            ..config(DbKind::Postgres)
        },
    )
    .await;
    // Idle: a plain explain.
    let view = plan(&f, "t1", id, None, "DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Rows { n: 321.0 }));
    assert_eq!(view.stmts[0].confirm_name.as_deref(), Some("orders"));
    assert_eq!(
        f.log.executed(),
        ["EXPLAIN (FORMAT JSON) DELETE FROM orders"]
    );
    f.log.executed.lock().unwrap().clear();

    f.log.set_tx(TxState::Active);
    let view = plan(&f, "t1", id, None, "UPDATE orders SET a = 1")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Rows { n: 321.0 }));
    assert_eq!(view.tx_state, TxState::Active);
    assert_eq!(
        f.log.executed(),
        [
            "SAVEPOINT dnt_row_estimate",
            "EXPLAIN (FORMAT JSON) UPDATE orders SET a = 1",
            "RELEASE SAVEPOINT dnt_row_estimate",
        ]
    );
    f.log.executed.lock().unwrap().clear();

    let view = plan(&f, "t1", id, None, "DELETE FROM fail_t")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Unavailable));
    assert_eq!(
        f.log.executed(),
        [
            "SAVEPOINT dnt_row_estimate",
            "EXPLAIN (FORMAT JSON) DELETE FROM fail_t",
            "ROLLBACK TO SAVEPOINT dnt_row_estimate",
            "RELEASE SAVEPOINT dnt_row_estimate",
        ]
    );
    f.log.executed.lock().unwrap().clear();

    f.log.set_tx(TxState::Aborted);
    let view = plan(&f, "t1", id, None, "DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Unavailable));
    assert_eq!(view.tx_state, TxState::Aborted);
    assert!(f.log.executed().is_empty());
}

/// An estimate runs in the session as it is before the script: after anything but a
/// plain read earlier in the plan (`USE`, `SET`, a write, …) it is not sent.
#[tokio::test]
async fn estimates_skip_after_an_earlier_session_change() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Staging), false).await;
    for text in [
        "USE x; DELETE FROM t",
        "SET @a = 1; DELETE FROM t",
        "INSERT INTO u VALUES (1); DELETE FROM t",
        "BEGIN; DELETE FROM t",
        "DELETE FROM u; DELETE FROM t",
    ] {
        f.log.executed.lock().unwrap().clear();
        let view = plan(&f, "t1", id, None, text).await.unwrap();
        assert_eq!(
            view.stmts[1].estimate,
            Some(RowEstimate::Unavailable),
            "{text}"
        );
        let sent: Vec<String> = f.log.executed();
        assert!(
            !sent.iter().any(|s| s == "EXPLAIN DELETE FROM t"),
            "{text}: {sent:?}"
        );
    }
    // Plain reads before it do not stop it.
    f.log.executed.lock().unwrap().clear();
    let view = plan(&f, "t1", id, None, "SELECT 1; DELETE FROM orders")
        .await
        .unwrap();
    assert_eq!(view.stmts[1].estimate, Some(RowEstimate::Rows { n: 42.0 }));
    assert_eq!(f.log.executed(), ["EXPLAIN DELETE FROM orders"]);
}

/// PostgreSQL: a transaction just opened has no snapshot yet; an estimate must not take
/// it (a later `SET TRANSACTION` would fail). Once something read in it, estimates run
/// under a savepoint.
#[tokio::test]
async fn pg_fresh_transaction_is_not_explained() {
    let f = fixture(DbKind::Postgres);
    let id = open(
        &f,
        ConnectionConfig {
            env_tag: Some(EnvTag::Production),
            ..config(DbKind::Postgres)
        },
    )
    .await;
    let view = plan(&f, "t1", id, None, "BEGIN").await.unwrap();
    run_named(&f, view.plan_id, &[]).await.unwrap();
    f.log.set_tx(TxState::Active);
    f.log.executed.lock().unwrap().clear();

    for text in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; DELETE FROM t",
        "DELETE FROM t",
    ] {
        let view = plan(&f, "t1", id, None, text).await.unwrap();
        let last = view.stmts.last().unwrap();
        assert_eq!(last.estimate, Some(RowEstimate::Unavailable), "{text}");
        assert_eq!(view.tx_state, TxState::Active);
        assert!(
            f.log.executed().is_empty(),
            "{text}: {:?}",
            f.log.executed()
        );
    }

    // A read in the transaction takes its snapshot: estimates run again.
    let view = plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    run_named(&f, view.plan_id, &[]).await.unwrap();
    f.log.executed.lock().unwrap().clear();
    let view = plan(&f, "t1", id, None, "DELETE FROM t").await.unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Rows { n: 321.0 }));
    assert_eq!(
        f.log.executed(),
        [
            "SAVEPOINT dnt_row_estimate",
            "EXPLAIN (FORMAT JSON) DELETE FROM t",
            "RELEASE SAVEPOINT dnt_row_estimate",
        ]
    );
}

/// A savepoint that cannot be rolled back to leaves the transaction aborted: planning
/// fails with the server's error instead of going on.
#[tokio::test]
async fn failed_savepoint_rollback_fails_the_plan() {
    let f = fixture(DbKind::Postgres);
    let id = open(
        &f,
        ConnectionConfig {
            env_tag: Some(EnvTag::Staging),
            ..config(DbKind::Postgres)
        },
    )
    .await;
    // A session exists and its transaction has been used (not fresh).
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    f.log.fail_rollback_to.store(true, Ordering::SeqCst);
    let err = plan(&f, "t1", id, None, "DELETE FROM fail_t")
        .await
        .unwrap_err();
    let app = AppError::from(err);
    assert_eq!(app.kind, "server", "{app:?}");
    assert!(app.message.contains("may now be aborted"), "{app:?}");
    assert_eq!(
        f.log.executed(),
        [
            "SAVEPOINT dnt_row_estimate",
            "EXPLAIN (FORMAT JSON) DELETE FROM fail_t",
            "ROLLBACK TO SAVEPOINT dnt_row_estimate",
        ]
    );
    // Without the failure the plan goes on, the estimate unavailable.
    f.log.fail_rollback_to.store(false, Ordering::SeqCst);
    let view = plan(&f, "t1", id, None, "DELETE FROM fail_t")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].estimate, Some(RowEstimate::Unavailable));
}

#[tokio::test]
async fn preview_is_first_120_chars() {
    let f = fixture(DbKind::Postgres);
    let id = open(&f, config(DbKind::Postgres)).await;
    let long = format!("SELECT\r\n  'ư',\n{}", "x".repeat(300));
    let view = plan(&f, "t1", id, None, &long).await.unwrap();
    let preview = &view.stmts[0].preview;
    assert_eq!(preview.chars().count(), PREVIEW_CHARS);
    assert!(preview.starts_with("SELECT   'ư', xxx"), "{preview:?}");
    assert!(!preview.contains(['\r', '\n']));

    let view = plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(view.stmts[0].preview, "SELECT 1");
}

#[tokio::test]
async fn plan_on_closed_connection_is_not_open() {
    let f = fixture(DbKind::MySql);
    let err = plan(&f, "t1", Uuid::new_v4(), None, "SELECT 1")
        .await
        .unwrap_err();
    assert_eq!(AppError::from(err).kind, "not_open");

    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.sm.close(id).await;
    let err = plan(&f, "t1", id, None, "SELECT 1").await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "not_open");
}

#[tokio::test]
async fn switching_database_with_open_tx_is_refused() {
    for kind in [DbKind::MySql, DbKind::Postgres] {
        let f = fixture(kind);
        let id = open(&f, config(kind)).await;
        plan(&f, "t1", id, Some("a"), "SELECT 1").await.unwrap();
        f.log.set_tx(TxState::Active);
        let connects = f.log.connects();
        let err = plan(&f, "t1", id, Some("b"), "SELECT 1").await.unwrap_err();
        assert_eq!(AppError::from(err).kind, "transaction_open", "{kind:?}");
        assert_eq!(f.log.connects(), connects, "{kind:?}");
        assert!(f.log.executed().is_empty(), "{kind:?}");

        // The same database is fine, and the plan reports the open transaction.
        let view = plan(&f, "t1", id, Some("a"), "SELECT 1").await.unwrap();
        assert_eq!(view.tx_state, TxState::Active);
    }
}

#[tokio::test]
async fn mysql_database_switch_runs_use() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, Some("shop"), "SELECT 1").await.unwrap();
    assert_eq!(
        f.log.databases.lock().unwrap().last().cloned().flatten(),
        Some("shop".into())
    );
    plan(&f, "t1", id, Some("other db"), "SELECT 1")
        .await
        .unwrap();
    plan(&f, "t1", id, Some("we`ird"), "SELECT 1")
        .await
        .unwrap();
    // Same database again: nothing to do.
    plan(&f, "t1", id, Some("we`ird"), "SELECT 1")
        .await
        .unwrap();
    assert_eq!(f.log.executed(), ["USE `other db`", "USE `we``ird`"]);
    assert_eq!(f.log.connects(), 2, "metadata + one query session");
    assert_eq!(f.log.closes(), 0);
}

#[tokio::test]
async fn pg_database_switch_reopens_session() {
    let f = fixture(DbKind::Postgres);
    let id = open(&f, config(DbKind::Postgres)).await;
    plan(&f, "t1", id, Some("a"), "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 2);
    plan(&f, "t1", id, Some("b"), "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
    assert_eq!(f.log.closes(), 1, "the old session is closed");
    assert_eq!(
        f.log.databases.lock().unwrap()[1..],
        [Some("a".to_string()), Some("b".to_string())]
    );
    assert!(f.log.executed().is_empty());

    // `None` is the connect database (named after the user on PostgreSQL).
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    let user = sample_config().username;
    assert_eq!(
        f.log.databases.lock().unwrap().last().cloned().flatten(),
        Some(user)
    );
}

#[tokio::test]
async fn close_tab_closes_session() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.sm.query_close_tab("t1", false).await.unwrap();
    assert_eq!(f.log.closes(), 1);
    // The tab's plans are gone with it.
    let err = f.sm.test_take_plan(view.plan_id).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");
    // Closing an unknown (or already closed) tab is a no-op.
    f.sm.query_close_tab("t1", false).await.unwrap();
    f.sm.query_close_tab("nope", false).await.unwrap();
    assert_eq!(f.log.closes(), 1);

    // Planning again opens a fresh session.
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
}

#[tokio::test]
async fn close_tab_with_open_tx_needs_force() {
    let f = fixture(DbKind::Postgres);
    let id = open(&f, config(DbKind::Postgres)).await;
    plan(&f, "t1", id, None, "BEGIN").await.unwrap();
    f.log.set_tx(TxState::Aborted);
    let err = f.sm.query_close_tab("t1", false).await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "transaction_open");
    assert_eq!(f.log.closes(), 0);
    f.sm.query_close_tab("t1", true).await.unwrap();
    assert_eq!(f.log.closes(), 1);
}

#[tokio::test]
async fn close_connection_closes_tab_sessions() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    plan(&f, "t2", id, None, "SELECT 1").await.unwrap();
    f.sm.close(id).await;
    assert_eq!(f.log.closes(), 3, "metadata + two tab sessions");
    let err = f.sm.test_take_plan(view.plan_id).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");
}

#[tokio::test]
async fn tab_moving_to_another_connection_closes_old_session() {
    let f = fixture(DbKind::MySql);
    let a = open(&f, config(DbKind::MySql)).await;
    let b = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", a, None, "SELECT 1").await.unwrap();

    f.log.set_tx(TxState::Active);
    let err = plan(&f, "t1", b, None, "SELECT 1").await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "transaction_open");
    assert_eq!(f.log.closes(), 0);

    f.log.set_tx(TxState::Idle);
    plan(&f, "t1", b, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.closes(), 1, "the session on `a` is closed");
    // Closing `a` now only closes its metadata session.
    f.sm.close(a).await;
    assert_eq!(f.log.closes(), 2);
}

#[tokio::test]
async fn query_tx_rollback_returns_idle() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    // A tab without a session has no transaction.
    assert_eq!(
        f.sm.query_tx("t1", TxAction::Rollback).await.unwrap(),
        TxState::Idle
    );
    assert!(f.log.executed().is_empty());

    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    let state = f.sm.query_tx("t1", TxAction::Rollback).await.unwrap();
    assert_eq!(state, TxState::Idle);
    f.log.set_tx(TxState::Active);
    let state = f.sm.query_tx("t1", TxAction::Commit).await.unwrap();
    assert_eq!(state, TxState::Idle);
    assert_eq!(f.log.executed(), ["ROLLBACK", "COMMIT"]);
}

#[tokio::test]
async fn busy_tab_refuses_plan_and_tx() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let gate = f.log.close_gate();
    let sm = Arc::clone(&f.sm);
    let first = tokio::spawn(async move {
        sm.query_plan("t1", id, None, "SELECT 1", Target::All)
            .await
            .map(|_| ())
    });
    // The first plan holds the tab while it connects.
    f.log.entered.notified().await;
    let err = plan(&f, "t1", id, None, "SELECT 1").await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "busy");
    let err = f.sm.query_tx("t1", TxAction::Rollback).await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "busy");
    // Another tab is not affected.
    f.log.open_gate(&gate);
    plan(&f, "t2", id, None, "SELECT 1").await.unwrap();
    first.await.unwrap().unwrap();
}

#[tokio::test]
async fn expired_plan_is_rejected() {
    let f = fixture_with(DbKind::MySql, |sm| {
        sm.with_plan_ttl(Duration::from_millis(50))
    });
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = f.sm.test_take_plan(view.plan_id).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");
    let err = f.sm.test_take_plan(Uuid::new_v4()).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");
}

#[tokio::test]
async fn taken_plan_is_guarded_and_runs_once() {
    let f = fixture(DbKind::MySql);
    let id = open(
        &f,
        ConnectionConfig {
            env_tag: Some(EnvTag::Production),
            ..config(DbKind::MySql)
        },
    )
    .await;
    let text = "SELECT 0;\nDELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2";
    let view = plan(&f, "t1", id, None, text).await.unwrap();
    assert_eq!(view.stmts.len(), 3);

    let (tab, conn, p) = f.sm.test_take_plan(view.plan_id).unwrap();
    assert_eq!((tab.as_str(), conn), ("t1", id));
    assert!(p.guard.is_some(), "every stored plan is guarded");
    assert_eq!(p.stmts[1].verdict, Verdict::Confirm);
    // A plan runs once.
    let err = f.sm.test_take_plan(view.plan_id).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");

    // Retry / Fetch all re-run one stored statement verbatim (DELIMITER context kept).
    let (_, _, one) = f.sm.test_plan_statement(view.plan_id, 1).unwrap();
    assert_eq!(one.stmts.len(), 1);
    assert_eq!(one.stmts[0].index, 1);
    assert_eq!(
        one.stmts[0].text,
        "CREATE PROCEDURE p() BEGIN SELECT 1; END"
    );
    assert_eq!(one.stmts[0].verdict, Verdict::Confirm);
    assert!(one.guard.is_some());
    let err = f.sm.test_plan_statement(view.plan_id, 3).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");

    // A newer plan of the tab drops its pending (never run) plans, not the one run.
    let pending = plan(&f, "t1", id, None, "SELECT 5").await.unwrap();
    plan(&f, "t1", id, None, "SELECT 6").await.unwrap();
    assert!(f.sm.test_take_plan(pending.plan_id).is_err());
    assert!(f.sm.test_plan_statement(view.plan_id, 0).is_ok());
}

fn kind(e: DbError) -> String {
    AppError::from(e).kind
}

#[tokio::test]
async fn replaced_tunnel_reopens_idle_tab_session() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, Some("shop"), "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 2);
    f.sm.simulate_tunnel_replaced(id);
    let view = plan(&f, "t1", id, Some("shop"), "SELECT 1").await.unwrap();
    assert_eq!(view.tx_state, TxState::Idle);
    assert_eq!(
        f.log.connects(),
        3,
        "a fresh session through the new tunnel"
    );
    assert_eq!(f.log.closes(), 1, "the stale session is closed");
    assert_eq!(
        f.log.databases.lock().unwrap().last().cloned().flatten(),
        Some("shop".into())
    );
    // Not stale any more.
    plan(&f, "t1", id, Some("shop"), "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
}

#[tokio::test]
async fn replaced_tunnel_with_open_tx_reports_lost_once() {
    let f = fixture(DbKind::Postgres);
    let id = open(&f, config(DbKind::Postgres)).await;
    plan(&f, "t1", id, None, "BEGIN").await.unwrap();
    f.log.set_tx(TxState::Active);
    f.sm.simulate_tunnel_replaced(id);
    let err = plan(&f, "t1", id, None, "SELECT 1").await.unwrap_err();
    assert!(
        matches!(&err, DbError::ConnectionLost { was_in_tx: true, outcome_unknown: false, message }
            if message.contains("SSH tunnel")),
        "{err:?}"
    );
    assert_eq!(f.log.closes(), 1);
    // Reported with the tab's state after it: no session, no transaction.
    assert_eq!(tx_state_after(&err), Some(TxState::Idle));
    assert_eq!(with_tx_state(err).tx_state.as_deref(), Some("idle"));
    // The server rolled the transaction back; the next plan reopens.
    f.log.set_tx(TxState::Idle);
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
}

/// `query_run` and `query_export` of a plan made before the tunnel was replaced under
/// an open transaction: `ConnectionLost { was_in_tx }` with the tab's state `Idle`, and
/// a Commit afterwards does not look like it worked (`lost_tx`).
#[tokio::test]
async fn replaced_tunnel_with_open_tx_drops_it_for_runs_and_exports() {
    for export_it in [false, true] {
        let f = fixture(DbKind::MySql);
        let id = open(&f, config(DbKind::MySql)).await;
        let view = plan(&f, "t1", id, None, "SELECT rows_2").await.unwrap();
        f.log.set_tx(TxState::Active);
        f.sm.simulate_tunnel_replaced(id);
        let (err, state) = if export_it {
            let mut sink = ExportRows::default();
            export(&f, view.plan_id, Uuid::new_v4(), &mut sink)
                .await
                .unwrap_err()
        } else {
            let (emit, _events) = collector();
            let err =
                f.sm.query_run(view.plan_id, false, opts(), emit)
                    .await
                    .unwrap_err();
            let state = tx_state_after(&err);
            (err, state)
        };
        assert!(
            matches!(
                &err,
                DbError::ConnectionLost {
                    was_in_tx: true,
                    ..
                }
            ),
            "{export_it}: {err:?}"
        );
        assert_eq!(state, Some(TxState::Idle), "{export_it}");
        assert_eq!(
            f.log.closes(),
            1,
            "{export_it}: the stale session is closed"
        );
        f.log.set_tx(TxState::Idle);
        let err =
            f.sm.query_tx_reported("t1", TxAction::Commit)
                .await
                .unwrap_err();
        assert_eq!(err.kind, "connection_lost", "{export_it}");
        assert_eq!(err.tx_state.as_deref(), Some("idle"), "{export_it}");
        assert!(
            !f.log.executed().iter().any(|s| s == "COMMIT"),
            "{export_it}"
        );
        // Nothing of the plan was sent.
        assert!(f.log.executed().is_empty(), "{export_it}");
    }
}

#[tokio::test]
async fn stored_statement_on_another_database_is_expired() {
    for k in [DbKind::MySql, DbKind::Postgres] {
        let f = fixture(k);
        let id = open(&f, config(k)).await;
        let view = plan(&f, "t1", id, Some("a"), "SELECT 1; SELECT 2")
            .await
            .unwrap();
        f.sm.test_take_plan(view.plan_id).unwrap();
        assert!(f.sm.test_plan_statement(view.plan_id, 1).is_ok());
        let pending = plan(&f, "t1", id, Some("b"), "SELECT 3").await.unwrap();
        let err = f.sm.test_plan_statement(view.plan_id, 1).unwrap_err();
        assert_eq!(kind(err), "plan_expired", "{k:?}");
        // Back on `a`: a plan made on `b` does not run there.
        plan(&f, "t1", id, Some("a"), "SELECT 4").await.unwrap();
        assert_eq!(
            kind(f.sm.test_take_plan(pending.plan_id).unwrap_err()),
            "plan_expired"
        );
    }
}

#[tokio::test]
async fn plan_of_tab_moved_to_another_connection_is_expired() {
    let f = fixture(DbKind::MySql);
    let a = open(&f, config(DbKind::MySql)).await;
    let b = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", a, None, "SELECT 1").await.unwrap();
    f.sm.test_take_plan(view.plan_id).unwrap();
    plan(&f, "t1", b, None, "SELECT 1").await.unwrap();
    assert_eq!(
        kind(f.sm.test_plan_statement(view.plan_id, 0).unwrap_err()),
        "plan_expired"
    );
}

#[tokio::test]
async fn only_latest_taken_plan_is_kept_without_ttl() {
    let f = fixture_with(DbKind::MySql, |sm| {
        sm.with_plan_ttl(Duration::from_millis(50))
    });
    let id = open(&f, config(DbKind::MySql)).await;
    let first = plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.sm.test_take_plan(first.plan_id).unwrap();
    let second = plan(&f, "t1", id, None, "SELECT 2").await.unwrap();
    // A plan of another tab does not affect t1's.
    let other = plan(&f, "t2", id, None, "SELECT 9").await.unwrap();
    f.sm.test_take_plan(other.plan_id).unwrap();
    assert!(f.sm.test_plan_statement(first.plan_id, 0).is_ok());
    f.sm.test_take_plan(second.plan_id).unwrap();
    assert_eq!(
        kind(f.sm.test_plan_statement(first.plan_id, 0).unwrap_err()),
        "plan_expired",
        "an older taken plan of the tab is dropped"
    );

    let pending = plan(&f, "t1", id, None, "SELECT 3").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The latest taken plans outlive the TTL; a pending plan does not.
    let (_, _, one) = f.sm.test_plan_statement(second.plan_id, 0).unwrap();
    assert_eq!(one.stmts[0].text, "SELECT 2");
    assert!(f.sm.test_plan_statement(other.plan_id, 0).is_ok());
    assert_eq!(
        kind(f.sm.test_take_plan(pending.plan_id).unwrap_err()),
        "plan_expired"
    );

    f.sm.query_close_tab("t1", false).await.unwrap();
    assert!(f.sm.test_plan_statement(second.plan_id, 0).is_err());
    assert!(f.sm.test_plan_statement(other.plan_id, 0).is_ok());
}

#[tokio::test]
async fn plan_statement_carries_only_the_statement() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let text = format!("SELECT 1;\n{}\nSELECT 2", "-- filler\n".repeat(1000));
    let view = plan(&f, "t1", id, None, &text).await.unwrap();
    let (_, _, one) = f.sm.test_plan_statement(view.plan_id, 1).unwrap();
    assert_eq!(one.source, "SELECT 2");
    assert_eq!(one.stmts[0].source_range, (0, 8));
    assert_eq!(one.stmts[0].range, view.stmts[1].range);
    assert_eq!(one.base, view.stmts[1].range.0);
}

#[tokio::test]
async fn plan_finishing_after_close_is_not_stored() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let gate = f.log.close_gate();
    let sm = Arc::clone(&f.sm);
    let planning =
        tokio::spawn(async move { sm.query_plan("t1", id, None, "SELECT 1", Target::All).await });
    f.log.entered.notified().await;
    let sm = Arc::clone(&f.sm);
    let closing = tokio::spawn(async move { sm.close(id).await });
    while !f.sm.open_connections().is_empty() {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    f.log.open_gate(&gate);
    let err = planning.await.unwrap().unwrap_err();
    assert_eq!(kind(err), "connection_lost");
    closing.await.unwrap();
    assert_eq!(
        f.log.closes(),
        2,
        "metadata + the tab session opened meanwhile"
    );
}

#[tokio::test]
async fn tab_id_is_validated() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    for tab in [String::new(), "x".repeat(65)] {
        let err = plan(&f, &tab, id, None, "SELECT 1").await.unwrap_err();
        assert_eq!(kind(err), "config");
        let err = f.sm.query_tx(&tab, TxAction::Commit).await.unwrap_err();
        assert_eq!(kind(err), "config");
        let err = f.sm.query_close_tab(&tab, true).await.unwrap_err();
        assert_eq!(kind(err), "config");
    }
    plan(&f, &"é".repeat(64), id, None, "SELECT 1")
        .await
        .unwrap();
    assert_eq!(f.log.connects(), 2);
}

// ---- Runs (Task 15) ----

/// Collects a run's events as JSON (the wire shape).
#[derive(Default)]
struct Events(Mutex<Vec<Json>>);

impl Events {
    fn list(&self) -> Vec<Json> {
        self.0.lock().unwrap().clone()
    }

    fn tags(&self) -> Vec<String> {
        self.list()
            .iter()
            .map(|e| e["t"].as_str().unwrap().to_string())
            .collect()
    }

    fn of(&self, tag: &str) -> Vec<Json> {
        self.list().into_iter().filter(|e| e["t"] == tag).collect()
    }

    /// Waits for `done`; panics after `within`.
    async fn done(&self, within: Duration) -> Json {
        tokio::time::timeout(within, async {
            loop {
                if let Some(done) = self.of("done").pop() {
                    return done;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no done within {within:?}: {:?}", self.tags()))
    }
}

fn collector() -> (EventSink, Arc<Events>) {
    let events = Arc::new(Events::default());
    let out = Arc::clone(&events);
    (
        Arc::new(move |e| {
            out.0.lock().unwrap().push(serde_json::to_value(e).unwrap());
            true
        }),
        events,
    )
}

fn history(f: &Fixture, id: Uuid) -> Vec<dnt_store::history::HistoryEntry> {
    let mut rows = f
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(id),
            search: None,
            limit: 1000,
        })
        .unwrap();
    rows.sort_by_key(|r| r.id);
    rows
}

fn opts() -> RunOptions {
    RunOptions {
        max_rows: 10_000,
        continue_on_error: false,
    }
}

async fn open_tagged(f: &Fixture, env_tag: Option<EnvTag>, read_only: bool) -> Uuid {
    open(
        f,
        ConnectionConfig {
            env_tag,
            read_only,
            ..config(DbKind::MySql)
        },
    )
    .await
}

/// Plans `text` on tab `t1` and runs it; returns the events once the run is done.
async fn run_text(f: &Fixture, id: Uuid, text: &str, opts: RunOptions) -> Arc<Events> {
    let view = plan(f, "t1", id, None, text).await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, true, opts, emit)
        .await
        .unwrap();
    events.done(Duration::from_secs(5)).await;
    events
}

const LONG: Duration = Duration::from_secs(5);

#[tokio::test]
async fn run_without_confirmation_is_refused_on_production() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Production), false).await;
    let view = plan(
        &f,
        "t1",
        id,
        None,
        "SELECT 1; DELETE FROM t; UPDATE t SET a = 1",
    )
    .await
    .unwrap();
    let (emit, events) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit.clone())
            .await
            .unwrap_err();
    assert!(
        matches!(err, DbError::ConfirmationRequired { count: 2 }),
        "{err:?}"
    );
    // Only the row estimates of planning were sent.
    // Only the DELETE's row estimate was sent (the UPDATE comes after a write).
    assert_eq!(f.log.executed(), ["EXPLAIN DELETE FROM t"]);
    assert!(events.list().is_empty());
    // Confirmed without the typed table name: still refused.
    let err =
        f.sm.query_run(view.plan_id, true, opts(), emit.clone())
            .await
            .unwrap_err();
    assert!(
        matches!(err, DbError::ConfirmationRequired { count: 2 }),
        "{err:?}"
    );
    // The plan is still there: confirmed with the name, it runs.
    f.sm.query_run_named(view.plan_id, true, vec!["t".into()], opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(f.log.executed().len(), 4);
}

#[tokio::test]
async fn blocked_plan_is_refused_even_if_confirmed() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Production), true).await;
    let view = plan(&f, "t1", id, None, "SELECT 1; DELETE FROM t")
        .await
        .unwrap();
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, true, opts(), emit)
            .await
            .unwrap_err();
    assert!(matches!(err, DbError::Blocked { count: 1 }), "{err:?}");
    assert!(f.log.executed().is_empty());
}

#[tokio::test]
async fn guard_is_rechecked_at_run() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Local), false).await;
    let view = plan(&f, "t1", id, None, "DELETE FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    let mut cfg = f.repo.get(id).unwrap();
    cfg.env_tag = Some(EnvTag::Production);
    let keep = SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    };
    f.repo.update(cfg.clone(), keep).unwrap();
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit.clone())
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");

    // Read-only now: blocked even when confirmed.
    cfg.read_only = true;
    let keep = SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    };
    f.repo.update(cfg, keep).unwrap();
    let err =
        f.sm.query_run(view.plan_id, true, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "blocked");
    assert!(f.log.executed().is_empty());
}

#[tokio::test]
async fn second_run_on_busy_tab_is_busy() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT slow").await.unwrap();
    let (emit, events) = collector();
    let run_id =
        f.sm.query_run(view.plan_id, false, opts(), emit.clone())
            .await
            .unwrap();
    // The run holds the tab: running (or planning, or Retry) again is refused.
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "busy");
    let err = plan(&f, "t1", id, None, "SELECT 1").await.unwrap_err();
    assert_eq!(kind(err), "busy");
    let err =
        f.sm.query_plan_statement(view.plan_id, 0)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "busy");
    f.sm.query_cancel(run_id).await.unwrap();
    events.done(LONG).await;
    // Done: the plan ran once.
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "plan_expired");
}

#[tokio::test]
async fn events_follow_contract() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let events = run_text(&f, id, "SELECT rows_1200", opts()).await;
    assert_eq!(
        events.tags(),
        [
            "statement_start",
            "result_start",
            "rows",
            "rows",
            "rows",
            "result_end",
            "done"
        ]
    );
    let list = events.list();
    assert_eq!(
        list[0],
        json!({ "t": "statement_start", "index": 0, "range": [0, 16] })
    );
    assert_eq!(list[1]["result_id"], 0);
    assert_eq!(list[1]["index"], 0);
    assert_eq!(list[1]["columns"][0]["name"], "n");
    let sizes: Vec<usize> = events
        .of("rows")
        .iter()
        .map(|e| e["rows"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [500, 500, 200]);
    assert_eq!(list[5]["row_count"], 1200);
    assert_eq!(list[5]["truncated"], false);
    assert_eq!(list[6]["stop"], Json::Null);
    assert!(list[6]["elapsed_ms"].is_number());
}

#[tokio::test]
async fn partial_batches_are_coalesced() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let events = run_text(&f, id, "SELECT trickle", opts()).await;
    let rows = events.of("rows");
    assert!((1..=2).contains(&rows.len()), "{} rows events", rows.len());
    let all: Vec<i64> = rows
        .iter()
        .flat_map(|e| e["rows"].as_array().unwrap().clone())
        .map(|r| r[0]["v"].as_i64().unwrap())
        .collect();
    assert_eq!(all, (0..100).collect::<Vec<_>>());
}

#[tokio::test]
async fn cancel_emits_stopping_once_and_done_cancelled() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT slow; SELECT 2")
        .await
        .unwrap();
    let (emit, events) = collector();
    let run_id =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let t0 = Instant::now();
    f.sm.query_cancel(run_id).await.unwrap();
    f.sm.query_cancel(run_id).await.unwrap();
    let done = events.done(LONG).await;
    assert!(t0.elapsed() < Duration::from_secs(1));
    assert_eq!(done["stop"], json!({ "kind": "cancelled" }));
    let stopping = events.of("stopping");
    assert_eq!(stopping, [json!({ "t": "stopping", "reason": "user" })]);
    let error = &events.of("statement_error")[0]["error"];
    assert_eq!(error["kind"], "cancelled");
    assert_eq!(error["tx_state"], "idle");
    // The second statement was never sent.
    assert_eq!(f.log.executed(), ["SELECT slow"]);
    // Unknown or finished runs: no-op.
    f.sm.query_cancel(run_id).await.unwrap();
    f.sm.query_cancel(Uuid::new_v4()).await.unwrap();
}

#[tokio::test]
async fn grace_expired_emits_abandoning() {
    let f = fixture_with(DbKind::MySql, |sm| {
        sm.with_cancel_grace(Duration::from_millis(50))
    });
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "UPDATE stuck SET a = 1 WHERE id = 1")
        .await
        .unwrap();
    let (emit, events) = collector();
    let run_id =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.sm.query_cancel(run_id).await.unwrap();
    let done = events.done(LONG).await;
    let tags = events.tags();
    let stopping = tags.iter().position(|t| t == "stopping").unwrap();
    let abandoning = tags.iter().position(|t| t == "abandoning").unwrap();
    assert!(stopping < abandoning, "{tags:?}");
    assert_eq!(tags.iter().filter(|t| *t == "stopping").count(), 1);
    // A statement that may have been applied is reported as such.
    let error = &events.of("statement_error")[0]["error"];
    assert_eq!(error["kind"], "connection_lost");
    assert_eq!(error["outcome_unknown"], true);
    assert_eq!(done["stop"], json!({ "kind": "error", "index": 0 }));
}

#[tokio::test]
async fn timeout_progress_emits_stopping_timeout() {
    // A `CancelSent` the user did not ask for is a timeout.
    let f = fixture_with(DbKind::MySql, |sm| {
        sm.with_cancel_grace(Duration::from_millis(300))
    });
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT stuck; SELECT 2")
        .await
        .unwrap();
    let (emit, events) = collector();
    let run_id =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    // The driver's own cancel (as a statement timeout does).
    f.log.cancelled.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Stop pressed while the timeout is stopping: no second `stopping`, but the user's
    // stop is recorded and ends the run.
    f.sm.query_cancel(run_id).await.unwrap();
    events.done(LONG).await;
    assert_eq!(f.log.executed(), ["SELECT stuck"]);
    assert_eq!(
        events.of("stopping"),
        [json!({ "t": "stopping", "reason": "timeout" })]
    );
}

#[tokio::test]
async fn history_row_per_sent_statement() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let events = run_text(
        &f,
        id,
        "SELECT rows_3; SELECT * FROM fail; SELECT 3",
        opts(),
    )
    .await;
    assert_eq!(
        events.done(LONG).await["stop"],
        json!({ "kind": "error", "index": 1 })
    );
    let rows = f
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(id),
            search: None,
            limit: 100,
        })
        .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let ok = rows.iter().find(|r| r.status == HistoryStatus::Ok).unwrap();
    assert_eq!(ok.sql_text, "SELECT rows_3");
    assert_eq!(ok.kind, "read");
    assert_eq!(ok.row_count, Some(3));
    assert!(!ok.redacted);
    assert_eq!(ok.started_at.len(), "2026-10-20T10:00:00.000Z".len());
    let err = rows
        .iter()
        .find(|r| r.status == HistoryStatus::Error)
        .unwrap();
    assert_eq!(err.sql_text, "SELECT * FROM fail");
    assert_eq!(err.error_kind.as_deref(), Some("server"));
    assert_eq!(err.error_code.as_deref(), Some("1146"));
}

#[tokio::test]
async fn history_is_redacted_and_cancel_is_recorded() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    run_text(
        &f,
        id,
        "CREATE USER 'u'@'%' IDENTIFIED BY 'hunter2-fake'",
        opts(),
    )
    .await;
    let view = plan(&f, "t1", id, None, "SELECT slow").await.unwrap();
    let (emit, events) = collector();
    let run_id =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.sm.query_cancel(run_id).await.unwrap();
    events.done(LONG).await;
    let rows = f
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(id),
            search: None,
            limit: 100,
        })
        .unwrap();
    let created = rows.iter().find(|r| r.kind == "ddl").unwrap();
    assert!(created.redacted);
    assert!(!created.sql_text.contains("hunter2-fake"), "{created:?}");
    let cancelled = rows.iter().find(|r| r.kind == "read").unwrap();
    assert_eq!(cancelled.status, HistoryStatus::Cancelled);
}

#[tokio::test]
async fn close_connection_during_run_ends_run() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT slow; SELECT 2")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let t0 = Instant::now();
    f.sm.close(id).await;
    let done = events.done(Duration::from_secs(2)).await;
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(done["stop"], json!({ "kind": "cancelled" }));
    assert_eq!(f.log.executed(), ["SELECT slow"]);
    // The metadata session and the tab's session are closed.
    tokio::time::timeout(LONG, async {
        while f.log.closes() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn close_tab_during_run_stops_it() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT slow").await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let t0 = Instant::now();
    f.sm.query_close_tab("t1", false).await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1), "close did not wait");
    events.done(LONG).await;
    assert_eq!(f.log.closes(), 1, "the tab's session is closed");
    // The tab starts over with a fresh session.
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
}

#[tokio::test]
async fn retry_runs_the_stored_statement_verbatim() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let text = "SELECT 0;\nDELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2";
    let view = plan(&f, "t1", id, None, text).await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;

    let retry = f.sm.query_plan_statement(view.plan_id, 1).await.unwrap();
    assert_eq!(retry.stmts.len(), 1);
    assert_eq!(retry.stmts[0].index, 1);
    assert_eq!(retry.stmts[0].range, view.stmts[1].range);
    let (emit, events) = collector();
    f.sm.query_run(retry.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(
        events.of("statement_start")[0]["index"],
        1,
        "the statement keeps its index"
    );
    let executed = f.log.executed();
    assert_eq!(
        executed.last().unwrap(),
        "CREATE PROCEDURE p() BEGIN SELECT 1; END"
    );
}

#[tokio::test]
async fn retry_is_judged_with_the_current_config() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, None, false).await;
    let view = plan(&f, "t1", id, None, "SELECT 1; DELETE FROM t WHERE id = 1")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    let mut cfg = f.repo.get(id).unwrap();
    cfg.env_tag = Some(EnvTag::Production);
    let keep = SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    };
    f.repo.update(cfg, keep).unwrap();
    let retry = f.sm.query_plan_statement(view.plan_id, 1).await.unwrap();
    assert_eq!(retry.stmts[0].verdict, Verdict::Confirm);
    let (emit, _) = collector();
    let err =
        f.sm.query_run(retry.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
}

#[tokio::test]
async fn use_moves_the_tracked_database() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, Some("a"), "USE b").await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    // The UI still shows `a`: the next plan must switch back.
    plan(&f, "t1", id, Some("a"), "SELECT 1").await.unwrap();
    assert_eq!(f.log.executed(), ["USE b", "USE `a`"]);
}

#[tokio::test]
async fn lost_connection_drops_the_tab_session() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let events = run_text(&f, id, "SELECT lose", opts()).await;
    assert_eq!(
        events.of("statement_error")[0]["error"]["kind"],
        "connection_lost"
    );
    assert_eq!(f.log.connects(), 2);
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3, "the next plan opens a fresh session");
}

#[tokio::test]
async fn failed_tx_reports_tx_state() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    f.log.lose_on_tx.store(true, Ordering::SeqCst);
    let err =
        f.sm.query_tx_reported("t1", TxAction::Commit)
            .await
            .unwrap_err();
    assert_eq!(err.kind, "connection_lost");
    assert!(err.outcome_unknown);
    assert_eq!(err.tx_state.as_deref(), Some("idle"));
    f.log.lose_on_tx.store(false, Ordering::SeqCst);
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3, "the dead session was dropped");
    // Busy has no transaction state to report.
    let err =
        f.sm.query_tx_reported("", TxAction::Commit)
            .await
            .unwrap_err();
    assert_eq!((err.kind.as_str(), err.tx_state), ("config", None));
}

#[tokio::test]
async fn explain_ddl_is_not_available() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let err =
        f.sm.query_explain("t1", id, None, "CREATE TABLE t (a INT)", 0, false)
            .await
            .unwrap_err();
    let app = AppError::from(err);
    assert_eq!(app.kind, "config");
    assert_eq!(app.field.as_deref(), Some("explain"));
    assert!(app.message.contains("Explain is not available"), "{app:?}");
    assert!(f.log.executed().is_empty());
}

#[tokio::test]
async fn explain_analyze_of_a_write_is_refused() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let err =
        f.sm.query_explain("t1", id, None, "DELETE FROM t", 0, true)
            .await
            .unwrap_err();
    let app = AppError::from(err);
    assert_eq!(app.kind, "config");
    assert!(
        app.message.contains("only available for read statements"),
        "{app:?}"
    );
    // Plain Explain of the DELETE is fine (it does not execute it).
    let view =
        f.sm.query_explain("t1", id, None, "DELETE FROM t", 0, false)
            .await
            .unwrap();
    assert_eq!(view.stmts[0].kind, StmtKindLabel::Write);
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
}

#[tokio::test]
async fn explain_analyze_needs_confirmation_on_production() {
    let f = fixture(DbKind::MySql);
    let id = open_tagged(&f, Some(EnvTag::Production), false).await;
    let text = "SELECT 1;\nSELECT * FROM t WHERE a = 1";
    let view =
        f.sm.query_explain("t1", id, None, text, 15, true)
            .await
            .unwrap();
    assert_eq!(view.stmts.len(), 1);
    assert_eq!(view.stmts[0].kind, StmtKindLabel::Read);
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].range, (10, 37));
    let (emit, events) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit.clone())
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");
    assert!(f.log.executed().is_empty());

    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    let done = events.done(LONG).await;
    assert_eq!(done["stop"], Json::Null);
    assert_eq!(
        f.log.executed(),
        ["EXPLAIN ANALYZE SELECT * FROM t WHERE a = 1"]
    );
    assert_eq!(
        events.tags(),
        ["statement_start", "explain", "done"],
        "rows are collected, not streamed"
    );
    let explain = &events.of("explain")[0];
    assert_eq!(explain["index"], 0);
    assert_eq!(explain["plan"]["kind"], "tree");
}

#[tokio::test]
async fn explain_json_v2_gives_a_clear_error() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view =
        f.sm.query_explain("t1", id, None, "SELECT jsonv2", 0, false)
            .await
            .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    let error = &events.of("statement_error")[0]["error"];
    assert_eq!(error["kind"], "internal");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("explain_json_format_version"),
        "{error}"
    );

    // Format version 1 parses into a tree.
    let view =
        f.sm.query_explain("t1", id, None, "SELECT * FROM t", 0, false)
            .await
            .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(events.of("explain")[0]["plan"]["kind"], "tree");
}

// ---- Fix round 1 ----

/// Plans statement `index` of `plan_id` again (Retry / Fetch all) and runs it.
async fn rerun(f: &Fixture, plan_id: Uuid, index: u32, opts: RunOptions) -> Arc<Events> {
    let view = f.sm.query_plan_statement(plan_id, index).await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts, emit)
        .await
        .unwrap();
    events.done(LONG).await;
    events
}

#[tokio::test]
async fn retries_and_fetch_all_from_one_run_all_succeed() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 0; SELECT 1; SELECT rows_7")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;

    // Two Retries and a Fetch all, each from the original run.
    let first = rerun(&f, view.plan_id, 0, opts()).await;
    assert_eq!(first.of("statement_start")[0]["index"], 0);
    let second = rerun(&f, view.plan_id, 1, opts()).await;
    assert_eq!(second.of("statement_start")[0]["index"], 1);
    let all = RunOptions {
        max_rows: 1_000_000,
        continue_on_error: false,
    };
    let fetched = rerun(&f, view.plan_id, 2, all).await;
    assert_eq!(fetched.of("result_end")[0]["row_count"], 7);
    // Retry of a retried statement works too, and the original is still there.
    rerun(&f, view.plan_id, 0, opts()).await;
    assert_eq!(
        f.log.executed(),
        [
            "SELECT 0",
            "SELECT 1",
            "SELECT rows_7",
            "SELECT 0",
            "SELECT 1",
            "SELECT rows_7",
            "SELECT 0"
        ]
    );

    // A new run of the tab (a user plan) replaces the original.
    let next = plan(&f, "t1", id, None, "SELECT 9").await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(next.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    let err =
        f.sm.query_plan_statement(view.plan_id, 0)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "plan_expired");
    rerun(&f, next.plan_id, 0, opts()).await;
}

#[tokio::test]
async fn dead_listener_stops_the_run() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT slow; SELECT 2")
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    // The window is gone: no event is delivered.
    let emit: EventSink = Arc::new(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        false
    });
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    // The run stops and frees the tab well before `slow` would end (10 s).
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match plan(&f, "t1", id, None, "SELECT 1").await {
                Ok(_) => return,
                Err(e) => assert_eq!(kind(e), "busy"),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the run did not stop");
    assert_eq!(f.log.executed(), ["SELECT slow"]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "nothing is sent after a failure"
    );
}

#[tokio::test]
async fn failed_done_send_never_cancels() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 1; SELECT rows_3")
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let out = Arc::clone(&events);
    // Every event but `done` is delivered (the window closes right at the end).
    let emit: EventSink = Arc::new(move |e| {
        let json = serde_json::to_value(e).unwrap();
        let done = json["t"] == "done";
        out.0.lock().unwrap().push(json);
        !done
    });
    let run_id =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
    assert_eq!(events.done(LONG).await["stop"], Json::Null);
    // The tab's next statements run undisturbed: no late cancel was sent.
    let next = run_text(&f, id, "SELECT 3; SELECT 4", opts()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.log.cancel_calls.load(Ordering::SeqCst), 0);
    assert_eq!(next.done(LONG).await["stop"], Json::Null);
    // A Stop of the ended run sends nothing either.
    f.sm.query_cancel(run_id).await.unwrap();
    assert_eq!(f.log.cancel_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn history_is_written_in_statement_order() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let stmts: Vec<String> = (0..60).map(|i| format!("SELECT {i} AS n{i}")).collect();
    run_text(&f, id, &stmts.join(";\n"), opts()).await;
    let texts: Vec<String> = history(&f, id).into_iter().map(|r| r.sql_text).collect();
    assert_eq!(texts, stmts);
}

#[tokio::test]
async fn panicking_run_ends_and_drops_the_session() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 1; SELECT panic")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    let done = events.done(LONG).await;
    assert_eq!(done["stop"], json!({ "kind": "error", "index": 1 }));
    assert_eq!(events.of("statement_error")[0]["error"]["kind"], "internal");
    assert_eq!(
        events.of("statement_error")[0]["error"]["outcome_unknown"],
        true,
        "the panicking statement was sent and never ended: it may have been applied"
    );
    assert_eq!(
        f.log.closes(),
        1,
        "the session in an unknown state is closed"
    );
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3, "the next plan opens a fresh session");
    // The statement that ran before the panic is in the history.
    assert_eq!(history(&f, id).len(), 1);
}

// ---- Final review fixes ----

/// Production: a forgotten `DELIMITER ;` leaves `SELECT …; DELETE …;` as one planned
/// statement, which the server runs as two. The DELETE makes it Confirm (read-only:
/// Block), and Explain refuses it.
#[tokio::test]
async fn forgotten_delimiter_cannot_bypass_the_guard() {
    let text = "DELIMITER $$\nSELECT * FROM t; DELETE FROM t WHERE id = 1;";
    let f = fixture(DbKind::MySql);
    let prod = open_tagged(&f, Some(EnvTag::Production), false).await;
    let view = plan(&f, "t1", prod, None, text).await.unwrap();
    assert_eq!(view.stmts.len(), 1);
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    // The confirm dialog shows what really runs: a write.
    assert_eq!(view.stmts[0].kind, StmtKindLabel::Write);
    let (emit, _) = collector();
    let err =
        f.sm.query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap_err();
    assert_eq!(kind(err), "confirmation_required");

    let ro = open_tagged(&f, None, true).await;
    let view = plan(&f, "t2", ro, None, text).await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Block);

    for analyze in [false, true] {
        let err =
            f.sm.query_explain("t1", prod, None, text, 20, analyze)
                .await
                .unwrap_err();
        let app = AppError::from(err);
        assert_eq!(
            (app.kind.as_str(), app.field.as_deref()),
            ("config", Some("explain"))
        );
    }
    assert!(f.log.executed().is_empty());

    // A mysqldump procedure block stays one Ddl statement, allowed off Production.
    let dump = "DELIMITER $$\nCREATE DEFINER=`root`@`localhost` PROCEDURE `p`()\nBEGIN\n  SELECT 1;\n  DELETE FROM t;\nEND$$\nDELIMITER ;";
    let view = plan(&f, "t1", prod, None, dump).await.unwrap();
    assert_eq!(
        (view.stmts.len(), view.stmts[0].kind, view.stmts[0].verdict),
        (1, StmtKindLabel::Ddl, Verdict::Confirm)
    );
    let local = open_tagged(&f, Some(EnvTag::Local), false).await;
    let view = plan(&f, "t3", local, None, dump).await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
}

#[tokio::test]
async fn retry_after_a_lost_connection_reopens_the_session() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, Some("shop"), "SELECT 1; SELECT flaky")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    let error = &events.of("statement_error")[0]["error"];
    assert_eq!(error["kind"], "connection_lost");
    assert_eq!(error["retryable"], true);
    assert_eq!(f.log.closes(), 1, "the dead session is closed");
    assert_eq!(f.log.connects(), 2);

    // Retry of the lost statement: planned and run on a reopened session, on the
    // tab's database.
    let retried = rerun(&f, view.plan_id, 1, opts()).await;
    assert_eq!(retried.of("statement_start")[0]["index"], 1);
    assert_eq!(retried.done(LONG).await["stop"], Json::Null);
    assert_eq!(f.log.connects(), 3, "one fresh session");
    assert_eq!(
        f.log.databases.lock().unwrap().last().cloned().flatten(),
        Some("shop".into())
    );
    assert_eq!(
        f.log.executed(),
        ["SELECT 1", "SELECT flaky", "SELECT flaky"]
    );
    // Fetch all / Retry of the other statement still works, on the same session.
    rerun(&f, view.plan_id, 0, opts()).await;
    assert_eq!(f.log.connects(), 3);
}

#[tokio::test]
async fn retry_after_a_lost_transaction_is_refused() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT 1; SELECT lose_tx")
        .await
        .unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    let error = &events.of("statement_error")[0]["error"];
    assert_eq!(error["kind"], "connection_lost");
    assert_eq!(error["retryable"], false);
    // The run's statements belonged to the lost transaction: none runs again.
    for index in [0, 1] {
        let err =
            f.sm.query_plan_statement(view.plan_id, index)
                .await
                .unwrap_err();
        assert_eq!(kind(err), "plan_expired");
    }
    assert_eq!(f.log.connects(), 2);
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3, "the next plan opens a fresh session");
}

#[tokio::test]
async fn tx_action_after_the_session_died_in_a_transaction_is_connection_lost() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT panic").await.unwrap();
    f.log.set_tx(TxState::Active);
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(f.log.closes(), 1, "the session was dropped");
    // Its transaction is gone: a Commit must not look like it worked.
    let err =
        f.sm.query_tx_reported("t1", TxAction::Commit)
            .await
            .unwrap_err();
    assert_eq!(err.kind, "connection_lost");
    assert_eq!(err.tx_state.as_deref(), Some("idle"));
    // Reported once; nothing is sent.
    assert_eq!(
        f.sm.query_tx("t1", TxAction::Rollback).await.unwrap(),
        TxState::Idle
    );
    assert!(!f
        .log
        .executed()
        .iter()
        .any(|s| s == "COMMIT" || s == "ROLLBACK"));
}

/// Collects an export's rows; panics on its first batch when `panic` is set.
#[derive(Default)]
struct ExportRows {
    rows: usize,
    panic: bool,
}

impl RowSink for ExportRows {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.panic {
            panic!("the export sink panics");
        }
        self.rows += batch.len();
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn export(
    f: &Fixture,
    plan_id: Uuid,
    run_id: Uuid,
    sink: &mut ExportRows,
) -> Result<(), (DbError, Option<TxState>)> {
    f.sm.query_export(plan_id, 0, run_id, sink, &Notify::new())
        .await
}

/// A panic while an export runs (in the driver or in its sink) is caught as a run's
/// is: the run id is released, the session (in an unknown state) dropped, and the tab
/// works again.
#[tokio::test]
async fn a_panicking_export_releases_its_run_and_drops_the_session() {
    for in_driver in [true, false] {
        let f = fixture(DbKind::MySql);
        let id = open(&f, config(DbKind::MySql)).await;
        let text = if in_driver {
            "SELECT panic"
        } else {
            "SELECT rows_3"
        };
        let view = plan(&f, "t1", id, None, text).await.unwrap();
        f.log.set_tx(TxState::Active);
        let run_id = Uuid::new_v4();
        let mut sink = ExportRows {
            panic: !in_driver,
            ..ExportRows::default()
        };
        let (err, state) = export(&f, view.plan_id, run_id, &mut sink)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Internal { .. }), "{err:?}");
        assert_eq!(state, Some(TxState::Idle));
        assert_eq!(f.log.closes(), 1, "the session was dropped");
        // Its transaction is gone: a Commit must not look like it worked.
        let err =
            f.sm.query_tx_reported("t1", TxAction::Commit)
                .await
                .unwrap_err();
        assert_eq!(err.kind, "connection_lost");
        f.log.set_tx(TxState::Idle);

        // The run id is free again, the tab is not busy, and the next export reopens
        // the session.
        let view = plan(&f, "t1", id, None, "SELECT rows_4").await.unwrap();
        let mut sink = ExportRows::default();
        export(&f, view.plan_id, run_id, &mut sink).await.unwrap();
        assert_eq!(sink.rows, 4);
    }
}

#[tokio::test]
async fn an_export_refuses_a_run_id_in_use() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let slow = plan(&f, "t1", id, None, "SELECT slow").await.unwrap();
    let other = plan(&f, "t2", id, None, "SELECT rows_2").await.unwrap();
    let run_id = Uuid::new_v4();
    let mut a = ExportRows::default();
    let mut b = ExportRows::default();
    let (first, ()) = tokio::join!(export(&f, slow.plan_id, run_id, &mut a), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (err, state) = export(&f, other.plan_id, run_id, &mut b).await.unwrap_err();
        assert!(
            matches!(&err, DbError::Config { field: Some(f), .. } if f == "run_id"),
            "{err:?}"
        );
        assert_eq!(state, None);
        // The run registered under the id is the first export's: this stops it.
        f.sm.query_cancel(run_id).await.unwrap();
    });
    assert!(
        matches!(first, Err((DbError::Cancelled { .. }, _))),
        "{first:?}"
    );
    assert_eq!(b.rows, 0);
    assert!(
        !f.log.executed().iter().any(|s| s == "SELECT rows_2"),
        "nothing was sent for the refused export"
    );
}

// ---- Manual commit mode (Task 13, spec §4.2d–e) ----

/// The statements sent, without PostgreSQL's `search_path` probe after a run.
fn sent(f: &Fixture) -> Vec<String> {
    f.log
        .executed()
        .into_iter()
        .filter(|s| !s.contains("current_schemas"))
        .collect()
}

async fn manual(f: &Fixture, tab: &str) {
    assert_eq!(
        f.sm.query_tx_mode(tab, TxMode::Manual).await.unwrap(),
        TxState::Idle
    );
}

#[tokio::test]
async fn manual_mode_begins_before_first_write() {
    for (kind, begin) in [
        (DbKind::MySql, "START TRANSACTION"),
        (DbKind::Postgres, "BEGIN"),
    ] {
        let f = fixture(kind);
        let id = open(&f, config(kind)).await;
        manual(&f, "t1").await;
        let info = f.sm.query_tx_info("t1").await.unwrap();
        assert_eq!(
            info,
            TabTxInfo {
                state: TxState::Idle,
                since_ms: None,
                statements: 0
            }
        );
        let events = run_text(&f, id, "SELECT 1; UPDATE t SET a = 1 WHERE id = 1", opts()).await;
        assert_eq!(events.of("done")[0]["stop"], Json::Null, "{kind:?}");
        assert_eq!(
            sent(&f),
            ["SELECT 1", begin, "UPDATE t SET a = 1 WHERE id = 1"],
            "{kind:?}: the transaction begins right before the first write"
        );
        let info = f.sm.query_tx_info("t1").await.unwrap();
        assert_eq!(info.state, TxState::Active, "{kind:?}");
        assert_eq!(info.statements, 1, "{kind:?}");
        assert!(info.since_ms.is_some_and(|ms| ms > 0.0), "{kind:?}");

        // In the open transaction: no second BEGIN, the count goes on.
        run_text(&f, id, "DELETE FROM t WHERE id = 2", opts()).await;
        assert_eq!(sent(&f).len(), 4, "{kind:?}");
        let later = f.sm.query_tx_info("t1").await.unwrap();
        assert_eq!(later.statements, 2, "{kind:?}");
        assert_eq!(later.since_ms, info.since_ms, "{kind:?}");

        // Commit ends it; the next write begins again.
        assert_eq!(
            f.sm.query_tx("t1", TxAction::Commit).await.unwrap(),
            TxState::Idle
        );
        let info = f.sm.query_tx_info("t1").await.unwrap();
        assert_eq!(
            (info.state, info.since_ms, info.statements),
            (TxState::Idle, None, 0)
        );
        run_text(&f, id, "INSERT INTO t VALUES (1)", opts()).await;
        assert_eq!(
            sent(&f)[5..],
            [begin, "INSERT INTO t VALUES (1)"],
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn manual_mode_keeps_the_users_transaction_control() {
    let f = fixture(DbKind::Postgres);
    let id = open(&f, config(DbKind::Postgres)).await;
    manual(&f, "t1").await;
    // The user's own BEGIN opens the transaction: nothing is added.
    run_text(
        &f,
        id,
        "SELECT 1; BEGIN; UPDATE t SET a = 1 WHERE id = 1",
        opts(),
    )
    .await;
    assert_eq!(
        sent(&f),
        ["SELECT 1", "BEGIN", "UPDATE t SET a = 1 WHERE id = 1"]
    );
    let info = f.sm.query_tx_info("t1").await.unwrap();
    assert_eq!(info.state, TxState::Active);
    assert_eq!(info.statements, 1, "counted from the user's BEGIN");
}

#[tokio::test]
async fn manual_mode_reads_do_not_begin() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    manual(&f, "t1").await;
    run_text(&f, id, "SELECT 1; SELECT 2", opts()).await;
    assert_eq!(sent(&f), ["SELECT 1", "SELECT 2"]);
    assert_eq!(f.sm.query_tx_info("t1").await.unwrap().state, TxState::Idle);
    // Auto mode (the default) never begins.
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    run_text(&f, id, "UPDATE t SET a = 1 WHERE id = 1", opts()).await;
    assert_eq!(sent(&f), ["UPDATE t SET a = 1 WHERE id = 1"]);
}

#[tokio::test]
async fn switching_to_auto_with_open_tx_fails() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    // A tab without a session takes either mode.
    manual(&f, "t1").await;
    run_text(&f, id, "UPDATE t SET a = 1 WHERE id = 1", opts()).await;
    let err = f.sm.query_tx_mode("t1", TxMode::Auto).await.unwrap_err();
    assert!(matches!(err, DbError::TransactionOpen), "{err:?}");
    // Still Manual: the next write does not begin again, nor autocommit.
    run_text(&f, id, "UPDATE t SET a = 2 WHERE id = 1", opts()).await;
    assert_eq!(sent(&f).len(), 3);
    f.sm.query_tx("t1", TxAction::Rollback).await.unwrap();
    assert_eq!(
        f.sm.query_tx_mode("t1", TxMode::Auto).await.unwrap(),
        TxState::Idle
    );
    run_text(&f, id, "UPDATE t SET a = 3 WHERE id = 1", opts()).await;
    assert_eq!(
        sent(&f).last().map(String::as_str),
        Some("UPDATE t SET a = 3 WHERE id = 1")
    );
    assert_eq!(sent(&f).len(), 5, "auto mode adds no BEGIN");
    // Bad tab ids are refused.
    let err = f.sm.query_tx_mode("", TxMode::Manual).await.unwrap_err();
    assert_eq!(kind(err), "config");
}

#[tokio::test]
async fn mysql_ddl_in_manual_tx_is_flagged_implicit_commit() {
    let text = "UPDATE t SET a = 1 WHERE id = 1; CREATE TABLE u (a INT); SELECT 1";
    for (kind, mode, want) in [
        (DbKind::MySql, TxMode::Manual, [false, true, false]),
        (DbKind::MySql, TxMode::Auto, [false, false, false]),
        // PostgreSQL DDL is transactional.
        (DbKind::Postgres, TxMode::Manual, [false, false, false]),
    ] {
        let f = fixture(kind);
        let id = open(&f, config(kind)).await;
        f.sm.query_tx_mode("t1", mode).await.unwrap();
        let view = plan(&f, "t1", id, None, text).await.unwrap();
        let got: Vec<bool> = view.stmts.iter().map(|s| s.implicit_commit).collect();
        assert_eq!(got, want, "{kind:?} {mode:?}");
        let (_, _, p) = f.sm.test_take_plan(view.plan_id).unwrap();
        let got: Vec<bool> = p.stmts.iter().map(|s| s.implicit_commit).collect();
        assert_eq!(got, want, "{kind:?} {mode:?}: on the stored plan too");
    }
    // In Auto mode, a transaction the user opened is committed by DDL too.
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    let view = plan(&f, "t1", id, None, "DROP TABLE u").await.unwrap();
    assert!(view.stmts[0].implicit_commit);
}

#[tokio::test]
async fn commit_lost_reports_outcome_unknown() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();

    // COMMIT sent, connection lost: it may have committed.
    f.log.set_tx(TxState::Active);
    f.log.lose_on_tx.store(true, Ordering::SeqCst);
    let err =
        f.sm.query_tx_reported("t1", TxAction::Commit)
            .await
            .unwrap_err();
    assert_eq!(err.kind, "connection_lost");
    assert!(err.outcome_unknown, "{err:?}");
    assert!(!err.retryable);
    assert!(!err.message.contains("rolled back"), "{}", err.message);
    assert_eq!(err.tx_state.as_deref(), Some("idle"));

    // ROLLBACK lost: the server rolls back anyway; the outcome is known.
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    let err =
        f.sm.query_tx_reported("t1", TxAction::Rollback)
            .await
            .unwrap_err();
    assert_eq!(err.kind, "connection_lost");
    assert!(!err.outcome_unknown, "{err:?}");
    f.log.lose_on_tx.store(false, Ordering::SeqCst);

    // COMMIT timed out: unknown as well.
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    f.log.timeout_on_tx.store(true, Ordering::SeqCst);
    let err =
        f.sm.query_tx_reported("t1", TxAction::Commit)
            .await
            .unwrap_err();
    assert_eq!(err.kind, "timeout");
    assert!(err.outcome_unknown, "{err:?}");
    // The session (still `Active` in the fake) is dropped: no Rollback can look like it
    // undid a COMMIT that may have landed.
    assert_eq!(err.tx_state.as_deref(), Some("idle"));
    assert_eq!(f.sm.query_tx_info("t1").await.unwrap().state, TxState::Idle);
    f.log.timeout_on_tx.store(false, Ordering::SeqCst);
    let sent_before = f.log.executed().len();
    assert_eq!(
        f.sm.query_tx("t1", TxAction::Rollback).await.unwrap(),
        TxState::Idle
    );
    assert_eq!(f.log.executed().len(), sent_before, "nothing to roll back");
    // The tab keeps its database and reopens on its next use.
    let connects = f.log.connects();
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), connects + 1);
}

#[tokio::test]
async fn closing_a_tab_forgets_its_mode() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    manual(&f, "t1").await;
    f.sm.query_close_tab("t1", false).await.unwrap();
    run_text(&f, id, "UPDATE t SET a = 1 WHERE id = 1", opts()).await;
    assert_eq!(sent(&f), ["UPDATE t SET a = 1 WHERE id = 1"]);
}

#[tokio::test]
async fn manual_mysql_ddl_commits_and_the_next_write_begins_again() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    manual(&f, "t1").await;
    let text = "UPDATE a SET x = 1 WHERE id = 1; CREATE TABLE x (a INT); \
                UPDATE b SET x = 1 WHERE id = 1";
    let view = plan(&f, "t1", id, None, text).await.unwrap();
    let flags: Vec<bool> = view.stmts.iter().map(|s| s.implicit_commit).collect();
    assert_eq!(flags, [false, true, false]);
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(
        sent(&f),
        [
            "START TRANSACTION",
            "UPDATE a SET x = 1 WHERE id = 1",
            "CREATE TABLE x (a INT)",
            "START TRANSACTION",
            "UPDATE b SET x = 1 WHERE id = 1"
        ]
    );
    // `a` was committed by the DDL; `b` waits in a new open transaction.
    let info = f.sm.query_tx_info("t1").await.unwrap();
    assert_eq!(info.state, TxState::Active);
    assert_eq!(info.statements, 1);
}

#[tokio::test]
async fn manual_commit_mid_run_begins_again() {
    for (kind, begin) in [
        (DbKind::MySql, "START TRANSACTION"),
        (DbKind::Postgres, "BEGIN"),
    ] {
        let f = fixture(kind);
        let id = open(&f, config(kind)).await;
        manual(&f, "t1").await;
        let text = "UPDATE a SET x = 1 WHERE id = 1; COMMIT; UPDATE b SET x = 1 WHERE id = 1";
        run_text(&f, id, text, opts()).await;
        assert_eq!(
            sent(&f),
            [
                begin,
                "UPDATE a SET x = 1 WHERE id = 1",
                "COMMIT",
                begin,
                "UPDATE b SET x = 1 WHERE id = 1"
            ],
            "{kind:?}"
        );
        let info = f.sm.query_tx_info("t1").await.unwrap();
        assert_eq!(
            (info.state, info.statements),
            (TxState::Active, 1),
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn manual_vacuum_and_set_do_not_begin() {
    let f = fixture(DbKind::Postgres);
    let id = open(&f, config(DbKind::Postgres)).await;
    manual(&f, "t1").await;
    run_text(&f, id, "VACUUM", opts()).await;
    run_text(&f, id, "SET work_mem = '64MB'", opts()).await;
    assert_eq!(sent(&f), ["VACUUM", "SET work_mem = '64MB'"]);

    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    manual(&f, "t1").await;
    run_text(&f, id, "SET @x = 1", opts()).await;
    assert_eq!(sent(&f), ["SET @x = 1"]);
    assert_eq!(f.sm.query_tx_info("t1").await.unwrap().state, TxState::Idle);
}

#[tokio::test]
async fn manual_lost_tx_blocks_runs_until_acknowledged() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    manual(&f, "t1").await;
    run_text(&f, id, "UPDATE t SET a = 1 WHERE id = 1", opts()).await;
    let events = run_text(&f, id, "UPDATE t SET a = 2 WHERE lose_tx = 1", opts()).await;
    assert_eq!(
        events.of("statement_error")[0]["error"]["kind"],
        "connection_lost"
    );
    let before = sent(&f).len();

    // A new plan reopens the session, but the run is refused: nothing is sent, and
    // no new transaction begins.
    let view = plan(&f, "t1", id, None, "UPDATE t SET a = 3 WHERE id = 1")
        .await
        .unwrap();
    let (emit, events) = collector();
    let err =
        f.sm.query_run(view.plan_id, true, opts(), emit.clone())
            .await
            .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(with_tx_state(err).tx_state.as_deref(), Some("idle"));
    assert_eq!(sent(&f).len(), before);
    assert!(events.list().is_empty());

    // Rolling back acknowledges the loss (nothing is sent); then the tab runs again.
    let ack =
        f.sm.query_tx_reported("t1", TxAction::Rollback)
            .await
            .unwrap_err();
    assert_eq!(ack.kind, "connection_lost");
    assert!(!ack.outcome_unknown);
    assert_eq!(ack.tx_state.as_deref(), Some("idle"));
    assert_eq!(sent(&f).len(), before);
    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(
        sent(&f)[before..],
        ["START TRANSACTION", "UPDATE t SET a = 3 WHERE id = 1"]
    );

    // An Auto tab forgets the loss once its session reopens (unchanged behaviour).
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    f.log.set_tx(TxState::Active);
    run_text(&f, id, "UPDATE t SET a = 2 WHERE lose_tx = 1", opts()).await;
    run_text(&f, id, "UPDATE t SET a = 3 WHERE id = 1", opts()).await;
    assert_eq!(
        sent(&f).last().map(String::as_str),
        Some("UPDATE t SET a = 3 WHERE id = 1")
    );
}
