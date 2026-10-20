//! Per-tab query sessions and run plans against an in-process fake driver (no database).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome,
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
use dnt_query::{StmtKindLabel, Target, Verdict};
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets, SecretChange, SecretUpdate};
use dnt_tunnel::KnownHosts;
use dntdatabase::query_sessions::{TxAction, PREVIEW_CHARS};
use dntdatabase::session_manager::SessionManager;
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    /// Every connect takes this long (ms).
    connect_delay_ms: AtomicU64,
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
        let ms = self.log.connect_delay_ms.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
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

struct NoopCancel;

#[async_trait]
impl Canceller for NoopCancel {
    async fn cancel(&self) -> Result<(), DbError> {
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
        _opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        self.log.executed.lock().unwrap().push(stmt.to_string());
        let upper = stmt.trim().to_ascii_uppercase();
        if upper == "COMMIT" || upper == "ROLLBACK" {
            self.log.set_tx(TxState::Idle);
        }
        let summary = ResultSummary {
            affected: Some(0),
            row_count: 0,
            truncated: false,
            elapsed_ms: 0,
        };
        sink.result_start(&[]);
        sink.result_end(&summary);
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: false,
        })
    }
    fn tx_state(&self) -> TxState {
        self.log.tx.lock().unwrap().unwrap_or(TxState::Idle)
    }
    fn dialect(&self) -> Dialect {
        Dialect::for_kind(self.info.kind)
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoopCancel)
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
    f.log.connect_delay_ms.store(300, Ordering::SeqCst);
    let sm = Arc::clone(&f.sm);
    let first = tokio::spawn(async move {
        sm.query_plan("t1", id, None, "SELECT 1", Target::All)
            .await
            .map(|_| ())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = plan(&f, "t1", id, None, "SELECT 1").await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "busy");
    let err = f.sm.query_tx("t1", TxAction::Rollback).await.unwrap_err();
    assert_eq!(AppError::from(err).kind, "busy");
    // Another tab is not affected.
    f.log.connect_delay_ms.store(0, Ordering::SeqCst);
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
