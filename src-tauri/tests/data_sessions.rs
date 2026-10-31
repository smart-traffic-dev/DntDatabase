//! Data sessions of the table data view against an in-process fake driver: pooled
//! sessions and late cancels, cancel while a session opens, stale pooled sessions.

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, Engine, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, SqlSession, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use dntdatabase::session_manager::SessionManager;
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

/// What the fake driver saw, and the gates the tests script it with.
#[derive(Default)]
struct Log {
    /// Sessions opened so far (the metadata session is number 0).
    sessions: AtomicUsize,
    closes: AtomicUsize,
    /// `(session number, statement)` of every `execute_one`, in order.
    executed: Mutex<Vec<(usize, String)>>,
    /// While set, every connect (after the metadata one) waits for it.
    connect_gate: Mutex<Option<Arc<Notify>>>,
    connect_entered: Notify,
    /// While set, a cancel waits for it before it looks at what runs (a late cancel).
    cancel_gate: Mutex<Option<Arc<Notify>>>,
    /// `GATED` waits for this; `gated_entered` tells the test it started.
    gated: Notify,
    gated_entered: Notify,
    /// Sessions with a lower number lose their connection on their next statement.
    dead_below: AtomicUsize,
    /// Statements that saw a cancel aimed at them.
    cancelled_statements: Mutex<Vec<String>>,
}

struct FakeDriver {
    log: Arc<Log>,
}

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }
    async fn connect(&self, _cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let number = self.log.sessions.fetch_add(1, Ordering::SeqCst);
        let gate = self.log.connect_gate.lock().unwrap().clone();
        if let (Some(gate), true) = (gate, number > 0) {
            self.log.connect_entered.notify_one();
            gate.notified().await;
        }
        Ok(Box::new(FakeSession {
            number,
            info: detect_mysql("8.0.36").unwrap(),
            log: Arc::clone(&self.log),
            running: Arc::default(),
        }))
    }
}

/// The flag of the statement running on a session (a cancel raises it).
type Running = Arc<Mutex<Option<Arc<AtomicBool>>>>;

struct FakeSession {
    number: usize,
    info: ServerInfo,
    log: Arc<Log>,
    running: Running,
}

/// Like the real drivers: the cancel targets what runs on the session when it is SENT.
struct FakeCancel {
    log: Arc<Log>,
    running: Running,
}

#[async_trait]
impl Canceller for FakeCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        let gate = self.log.cancel_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if let Some(flag) = self.running.lock().unwrap().as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

fn one_row(sink: &mut dyn RowSink) -> Result<StmtOutcome, DbError> {
    sink.result_start(&[ColumnMeta {
        name: "n".into(),
        db_type: "INT".into(),
        logical: LogicalType::Integer,
        nullable: Some(false),
        origin_table: None,
        origin_column: None,
    }]);
    sink.rows(vec![vec![Value::Int(1)]]);
    let summary = ResultSummary {
        affected: None,
        row_count: 1,
        truncated: false,
        elapsed_ms: 1,
    };
    sink.result_end(&summary);
    Ok(StmtOutcome {
        results: vec![summary],
        late_completion: false,
    })
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    fn engine(&self) -> Engine<'_> {
        Engine::Sql(self)
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(FakeCancel {
            log: Arc::clone(&self.log),
            running: Arc::clone(&self.running),
        })
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
impl SqlSession for FakeSession {
    /// `GATED` waits for the test's gate and then succeeds (even when cancelled meanwhile:
    /// it completed first); `SLOWISH` runs 300 ms unless a cancel reaches it; anything
    /// else returns one row.
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        self.log
            .executed
            .lock()
            .unwrap()
            .push((self.number, stmt.to_string()));
        if opts.stop.is_stopped() {
            return Err(DbError::cancelled());
        }
        if self.number < self.log.dead_below.load(Ordering::SeqCst) {
            return Err(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message: "server gone".into(),
            });
        }
        let flag = Arc::new(AtomicBool::new(false));
        *self.running.lock().unwrap() = Some(Arc::clone(&flag));
        let result = match stmt {
            "GATED" => {
                self.log.gated_entered.notify_one();
                self.log.gated.notified().await;
                one_row(sink)
            }
            "SLOWISH" => {
                let mut result = None;
                for _ in 0..30 {
                    if flag.load(Ordering::SeqCst) {
                        self.log
                            .cancelled_statements
                            .lock()
                            .unwrap()
                            .push(stmt.to_string());
                        result = Some(Err(DbError::cancelled()));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result.unwrap_or_else(|| one_row(sink))
            }
            _ => one_row(sink),
        };
        *self.running.lock().unwrap() = None;
        result
    }
    fn tx_state(&self) -> TxState {
        TxState::Idle
    }
    fn dialect(&self) -> Dialect {
        Dialect::for_kind(self.info.kind.require_sql().unwrap())
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
    sm: SessionManager,
    log: Arc<Log>,
    id: Uuid,
    _dir: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let log = Arc::new(Log::default());
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver {
        log: Arc::clone(&log),
    });
    let dir = tempfile::tempdir().unwrap();
    let sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(DbKind::MySql, driver)]),
    );
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::MySql,
        default_db: None,
        env_tag: None,
        secret_mode: SecretMode::Saved,
        ..sample_config()
    };
    let id = repo
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
    sm.open(id, None).await.unwrap();
    Fixture {
        sm,
        log,
        id,
        _dir: dir,
    }
}

const DB: Option<&str> = Some("shop");

/// A request superseded just as it completes must not hand its session to the next
/// request: the late cancel sent for it would stop that one.
#[tokio::test]
async fn a_late_cancel_never_hits_the_next_request_on_a_pooled_session() {
    let f = fixture().await;
    let late = Arc::new(Notify::new());
    *f.log.cancel_gate.lock().unwrap() = Some(Arc::clone(&late));

    // A runs on session 1; B (same key) supersedes it, its cancel held back; A then
    // completes normally.
    let (a, ()) = tokio::join!(f.sm.data_read(f.id, DB, "tab-1:page", "GATED", 10), async {
        f.log.gated_entered.notified().await;
        let b =
            f.sm.data_read(f.id, DB, "tab-1:page", "QUICK", 10)
                .await
                .unwrap();
        assert_eq!(b.rows.len(), 1);
        f.log.gated.notify_one();
    });
    assert!(a.is_ok(), "A completed before the cancel reached it: {a:?}");

    // C (another tab) takes a pooled session while A's cancel is finally sent.
    let (c, ()) = tokio::join!(
        f.sm.data_read(f.id, DB, "tab-2:page", "SLOWISH", 10),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            late.notify_waiters();
            late.notify_one();
        }
    );
    assert!(c.is_ok(), "C was cancelled by A's late cancel: {c:?}");
    assert!(f.log.cancelled_statements.lock().unwrap().is_empty());
    // A's session (1) was closed, not pooled: C ran on B's (2).
    let executed = f.log.executed.lock().unwrap().clone();
    assert_eq!(
        executed,
        [
            (1, "GATED".to_string()),
            (2, "QUICK".to_string()),
            (2, "SLOWISH".to_string())
        ]
    );
}

/// A cancel while the request's session is still opening drops the request before it
/// sends anything; its connect future is dropped and the next request opens a fresh one.
#[tokio::test]
async fn a_cancel_during_open_drops_the_request() {
    let f = fixture().await;
    let gate = Arc::new(Notify::new());
    *f.log.connect_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let (r, ()) = tokio::join!(f.sm.data_read(f.id, DB, "tab-1:count", "COUNT", 1), async {
        f.log.connect_entered.notified().await;
        assert!(f.sm.data_cancel(f.id, "tab-1:count"), "it was pending");
        f.log.connect_gate.lock().unwrap().take();
        gate.notify_one();
    });
    assert!(matches!(r, Err(DbError::Cancelled { .. })), "{r:?}");
    assert!(
        f.log.executed.lock().unwrap().is_empty(),
        "nothing was sent"
    );
    // The cancelled connect never produces a session to pool; the next read opens again.
    f.sm.data_read(f.id, DB, "tab-1:count", "COUNT", 1)
        .await
        .unwrap();
    assert_eq!(f.log.sessions.load(Ordering::SeqCst), 3);
    assert!(!f.sm.data_cancel(f.id, "tab-1:count"), "nothing left");
}

/// A raw `WHERE` read may leave session state a ROLLBACK does not undo (a MySQL
/// `GET_LOCK`, a PostgreSQL advisory lock): its session is closed, never pooled.
#[tokio::test]
async fn a_raw_where_session_is_never_pooled() {
    use dntdatabase::data_sessions::{DataRequest, RawRead};
    let f = fixture().await;
    let raw = DataRequest {
        raw: Some(RawRead(Arc::new(|_| Ok("SELECT RAW".to_string())))),
        max_rows: 10,
        ..DataRequest::default()
    };
    let page = f.sm.data_read_request(f.id, DB, "k", &raw).await.unwrap();
    assert_eq!(page.rows.len(), 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        f.log.closes.load(Ordering::SeqCst),
        1,
        "closed after its read"
    );
    // The next read opens a fresh session.
    f.sm.data_read(f.id, DB, "k", "QUICK", 10).await.unwrap();
    let executed = f.log.executed.lock().unwrap().clone();
    assert_eq!(
        executed,
        [
            (1, "START TRANSACTION READ ONLY".to_string()),
            (1, "SELECT RAW".to_string()),
            (1, "ROLLBACK".to_string()),
            (2, "QUICK".to_string()),
        ]
    );
    // A plain read's session is pooled as before.
    f.sm.data_read(f.id, DB, "k", "QUICK", 10).await.unwrap();
    assert_eq!(f.log.sessions.load(Ordering::SeqCst), 3);
}

/// A pooled session that went stale is replaced once, transparently.
#[tokio::test]
async fn a_stale_pooled_session_is_retried_on_a_fresh_one() {
    let f = fixture().await;
    f.sm.data_read(f.id, DB, "k", "QUICK", 10).await.unwrap();
    // Session 1 is pooled; the server drops it.
    f.log.dead_below.store(2, Ordering::SeqCst);
    let page = f.sm.data_read(f.id, DB, "k", "QUICK", 10).await.unwrap();
    assert_eq!(page.rows.len(), 1);
    let executed = f.log.executed.lock().unwrap().clone();
    assert_eq!(
        executed,
        [
            (1, "QUICK".to_string()),
            (1, "QUICK".to_string()),
            (2, "QUICK".to_string())
        ]
    );
    // Only one retry: a fresh session that fails too ends the request.
    f.log.dead_below.store(usize::MAX, Ordering::SeqCst);
    let err =
        f.sm.data_read(f.id, DB, "k", "QUICK", 10)
            .await
            .unwrap_err();
    assert!(matches!(err, DbError::ConnectionLost { .. }), "{err:?}");
}

#[tokio::test]
#[ignore = "manual backend performance observation; see backend-core-performance-review"]
async fn review_probe_concurrent_reads() {
    let f = fixture().await;
    let reads = async {
        let jobs = (0..24).map(|i| {
            let sm = &f.sm;
            let id = f.id;
            async move {
                sm.data_read(id, DB, &format!("review-{i}"), "SLOWISH", 10)
                    .await
            }
        });
        for result in futures_util::future::join_all(jobs).await {
            result.unwrap();
        }
    };
    let observe = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let opened = f.log.sessions.load(Ordering::SeqCst);
        println!(
            "REVIEW: 24 concurrent reads, physical sessions opened including metadata = {opened}"
        );
        assert!(opened > 0);
    };
    tokio::join!(reads, observe);
}
