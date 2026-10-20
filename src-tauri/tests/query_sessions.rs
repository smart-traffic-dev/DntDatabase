//! Per-tab query sessions and run plans against an in-process fake driver (no database).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, ExecProgress, Introspector, ResultSummary,
    RowSink, Session, StmtOutcome,
};
use dnt_core::error::{AppError, DbError};
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
use dnt_query::{StmtKindLabel, Target, Verdict};
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryFilter, HistoryStatus};
use dnt_store::repo::{ConnectionRepo, NewSecrets, SecretChange, SecretUpdate};
use dnt_tunnel::KnownHosts;
use dntdatabase::query_sessions::{TxAction, PREVIEW_CHARS};
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
    /// `LOSE` loses the connection; `SLOW` streams a row every 20 ms for 10 s until
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
                return Err(DbError::ConnectionLost {
                    was_in_tx: true,
                    outcome_unknown: upper == "COMMIT",
                    message: "server gone".into(),
                });
            }
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
    let view = plan(&f, "t1", prod, None, "DELETE FROM t").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    assert_eq!(view.env_tag, Some(EnvTag::Local));
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
    let err = f.sm.take_plan(view.plan_id).unwrap_err();
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
    let err = f.sm.take_plan(view.plan_id).unwrap_err();
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
    let err = f.sm.take_plan(view.plan_id).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");
    let err = f.sm.take_plan(Uuid::new_v4()).unwrap_err();
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

    let (tab, conn, p) = f.sm.take_plan(view.plan_id).unwrap();
    assert_eq!((tab.as_str(), conn), ("t1", id));
    assert!(p.guard.is_some(), "every stored plan is guarded");
    assert_eq!(p.stmts[1].verdict, Verdict::Confirm);
    // A plan runs once.
    let err = f.sm.take_plan(view.plan_id).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");

    // Retry / Fetch all re-run one stored statement verbatim (DELIMITER context kept).
    let (_, _, one) = f.sm.plan_statement(view.plan_id, 1).unwrap();
    assert_eq!(one.stmts.len(), 1);
    assert_eq!(one.stmts[0].index, 1);
    assert_eq!(
        one.stmts[0].text,
        "CREATE PROCEDURE p() BEGIN SELECT 1; END"
    );
    assert_eq!(one.stmts[0].verdict, Verdict::Confirm);
    assert!(one.guard.is_some());
    let err = f.sm.plan_statement(view.plan_id, 3).unwrap_err();
    assert_eq!(AppError::from(err).kind, "plan_expired");

    // A newer plan of the tab drops its pending (never run) plans, not the one run.
    let pending = plan(&f, "t1", id, None, "SELECT 5").await.unwrap();
    plan(&f, "t1", id, None, "SELECT 6").await.unwrap();
    assert!(f.sm.take_plan(pending.plan_id).is_err());
    assert!(f.sm.plan_statement(view.plan_id, 0).is_ok());
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
    // The server rolled the transaction back; the next plan reopens.
    f.log.set_tx(TxState::Idle);
    plan(&f, "t1", id, None, "SELECT 1").await.unwrap();
    assert_eq!(f.log.connects(), 3);
}

#[tokio::test]
async fn stored_statement_on_another_database_is_expired() {
    for k in [DbKind::MySql, DbKind::Postgres] {
        let f = fixture(k);
        let id = open(&f, config(k)).await;
        let view = plan(&f, "t1", id, Some("a"), "SELECT 1; SELECT 2")
            .await
            .unwrap();
        f.sm.take_plan(view.plan_id).unwrap();
        assert!(f.sm.plan_statement(view.plan_id, 1).is_ok());
        let pending = plan(&f, "t1", id, Some("b"), "SELECT 3").await.unwrap();
        let err = f.sm.plan_statement(view.plan_id, 1).unwrap_err();
        assert_eq!(kind(err), "plan_expired", "{k:?}");
        // Back on `a`: a plan made on `b` does not run there.
        plan(&f, "t1", id, Some("a"), "SELECT 4").await.unwrap();
        assert_eq!(
            kind(f.sm.take_plan(pending.plan_id).unwrap_err()),
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
    f.sm.take_plan(view.plan_id).unwrap();
    plan(&f, "t1", b, None, "SELECT 1").await.unwrap();
    assert_eq!(
        kind(f.sm.plan_statement(view.plan_id, 0).unwrap_err()),
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
    f.sm.take_plan(first.plan_id).unwrap();
    let second = plan(&f, "t1", id, None, "SELECT 2").await.unwrap();
    // A plan of another tab does not affect t1's.
    let other = plan(&f, "t2", id, None, "SELECT 9").await.unwrap();
    f.sm.take_plan(other.plan_id).unwrap();
    assert!(f.sm.plan_statement(first.plan_id, 0).is_ok());
    f.sm.take_plan(second.plan_id).unwrap();
    assert_eq!(
        kind(f.sm.plan_statement(first.plan_id, 0).unwrap_err()),
        "plan_expired",
        "an older taken plan of the tab is dropped"
    );

    let pending = plan(&f, "t1", id, None, "SELECT 3").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The latest taken plans outlive the TTL; a pending plan does not.
    let (_, _, one) = f.sm.plan_statement(second.plan_id, 0).unwrap();
    assert_eq!(one.stmts[0].text, "SELECT 2");
    assert!(f.sm.plan_statement(other.plan_id, 0).is_ok());
    assert_eq!(
        kind(f.sm.take_plan(pending.plan_id).unwrap_err()),
        "plan_expired"
    );

    f.sm.query_close_tab("t1", false).await.unwrap();
    assert!(f.sm.plan_statement(second.plan_id, 0).is_err());
    assert!(f.sm.plan_statement(other.plan_id, 0).is_ok());
}

#[tokio::test]
async fn plan_statement_carries_only_the_statement() {
    let f = fixture(DbKind::MySql);
    let id = open(&f, config(DbKind::MySql)).await;
    let text = format!("SELECT 1;\n{}\nSELECT 2", "-- filler\n".repeat(1000));
    let view = plan(&f, "t1", id, None, &text).await.unwrap();
    let (_, _, one) = f.sm.plan_statement(view.plan_id, 1).unwrap();
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
        }),
        events,
    )
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
    assert!(f.log.executed().is_empty(), "execute_one is never called");
    assert!(events.list().is_empty());
    // The plan is still there: confirmed, it runs.
    f.sm.query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    assert_eq!(f.log.executed().len(), 3);
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
    let view = plan(&f, "t1", id, None, "DELETE FROM t").await.unwrap();
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
    let view = plan(&f, "t1", id, None, "UPDATE stuck SET a = 1")
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
        sm.with_cancel_grace(Duration::from_millis(20))
    });
    let id = open(&f, config(DbKind::MySql)).await;
    let view = plan(&f, "t1", id, None, "SELECT stuck").await.unwrap();
    let (emit, events) = collector();
    f.sm.query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    // The driver's own cancel (as a statement timeout does).
    f.log.cancelled.store(true, Ordering::SeqCst);
    events.done(LONG).await;
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
    let view = plan(&f, "t1", id, None, "SELECT 1; DELETE FROM t")
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
