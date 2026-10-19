//! `SessionManager` against an in-process fake driver (no database needed).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, ExecSummary, Introspector, RowSink, Session,
};
use dnt_core::error::{AppError, ConnectStage, DbError};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, detect_pg, DbKind, ServerInfo};
use dnt_core::test_util::sample_config;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use dntdatabase::cache::NodeKey;
use dntdatabase::session_manager::{ConnState, MetaFuture, SessionManager};
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// What the fake driver saw and did, shared with the test.
#[derive(Default)]
struct Log {
    connects: AtomicUsize,
    closes: AtomicUsize,
    list_calls: AtomicUsize,
    /// `ResolvedConfig::database` of every connect, in order.
    databases: Mutex<Vec<Option<String>>>,
    /// `ResolvedConfig::password` of the last connect.
    last_password: Mutex<Option<String>>,
    /// The next `n` `list_objects` calls fail with `ConnectionLost`.
    lose_next: AtomicUsize,
    /// While set, every connect is refused.
    refuse: AtomicBool,
    /// Every connect takes this long (ms).
    connect_delay_ms: AtomicU64,
    /// Every `list_objects` call takes this long (ms).
    list_delay_ms: AtomicU64,
}

async fn delay(ms: &AtomicU64) {
    let ms = ms.load(Ordering::SeqCst);
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

struct FakeDriver {
    kind: DbKind,
    fail_connect: bool,
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
        *self.log.last_password.lock().unwrap() =
            cfg.password.as_ref().map(|p| p.expose_secret().to_string());
        delay(&self.log.connect_delay_ms).await;
        if self.fail_connect || self.log.refuse.load(Ordering::SeqCst) {
            return Err(DbError::Connect {
                message: "fake refused (10061)".into(),
            });
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
    async fn execute(
        &self,
        _sql: &str,
        _opts: ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<ExecSummary, DbError> {
        sink.result_start(&[]);
        sink.result_end(None, Duration::ZERO);
        Ok(ExecSummary::default())
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoopCancel)
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    fn in_transaction(&self) -> bool {
        false
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
        Ok(vec![DatabaseInfo {
            name: "shop".into(),
        }])
    }
    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_objects(
        &self,
        _scope: &Scope,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        self.log.list_calls.fetch_add(1, Ordering::SeqCst);
        delay(&self.log.list_delay_ms).await;
        let lose = self
            .log
            .lose_next
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if lose.is_ok() {
            return Err(DbError::ConnectionLost {
                in_transaction: false,
            });
        }
        Ok(vec![ObjectSummary {
            name: "orders".into(),
            kind,
            comment: None,
            row_estimate: None,
        }])
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

/// Metadata call bound used by these tests instead of 30 s.
const TEST_METADATA_TIMEOUT: Duration = Duration::from_millis(300);

struct Fixture {
    sm: Arc<SessionManager>,
    repo: Arc<ConnectionRepo>,
    log: Arc<Log>,
    _dir: tempfile::TempDir,
}

fn fixture(kind: DbKind, fail_connect: bool) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let log = Arc::new(Log::default());
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver {
        kind,
        fail_connect,
        log: Arc::clone(&log),
    });
    let dir = tempfile::tempdir().unwrap();
    let sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(kind, driver)]),
    )
    .with_metadata_timeout(TEST_METADATA_TIMEOUT);
    Fixture {
        sm: Arc::new(sm),
        repo,
        log,
        _dir: dir,
    }
}

/// A saved connection; `default_db` is None so no SchemaCache listing interferes with counts.
fn config(kind: DbKind, mode: SecretMode) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        kind,
        default_db: None,
        secret_mode: mode,
        ..sample_config()
    }
}

fn secrets(db_password: Option<&str>) -> NewSecrets {
    NewSecrets {
        db_password: db_password.map(SecretString::from),
        ssh_password: None,
        ssh_key_passphrase: None,
    }
}

fn save(f: &Fixture, cfg: ConnectionConfig, pw: Option<&str>) -> Uuid {
    f.repo.create(cfg, secrets(pw)).unwrap().id
}

fn key(segments: &[&str]) -> NodeKey {
    NodeKey(segments.iter().map(|s| s.to_string()).collect())
}

fn tables_scope(db: &str) -> Scope {
    Scope {
        database: db.into(),
        schema: None,
    }
}

fn assert_send<T: Send>(_: &T) {}

/// A browse call listing `kind` objects of `db`.
fn list(
    db: &str,
    kind: ObjectKind,
) -> impl for<'a> Fn(&'a dyn Introspector) -> MetaFuture<'a, Vec<ObjectSummary>> + Sync {
    let scope = tables_scope(db);
    move |i| {
        let scope = scope.clone();
        Box::pin(async move { i.list_objects(&scope, kind).await })
    }
}

async fn list_tables(f: &Fixture, id: Uuid, db: Option<&str>, k: &[&str]) -> Vec<ObjectSummary> {
    f.sm.browse(
        id,
        db,
        key(k),
        list(db.unwrap_or("shop"), ObjectKind::Table),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn cache_hits_and_refresh_invalidates_prefix() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    let k = ["db:shop", "folder:Table"];

    let first = list_tables(&f, id, Some("shop"), &k).await;
    let second = list_tables(&f, id, Some("shop"), &k).await;
    assert_eq!(first, second);
    assert_eq!(first[0].name, "orders");
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 1);

    // A refresh of an unrelated prefix keeps the entry.
    f.sm.refresh(id, &key(&["db:other"]));
    list_tables(&f, id, Some("shop"), &k).await;
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 1);

    f.sm.refresh(id, &key(&["db:shop"]));
    // Browse futures are `Send` (Tauri commands run them on the multi-threaded runtime).
    let call =
        f.sm.browse(id, Some("shop"), key(&k), list("shop", ObjectKind::Table));
    assert_send(&call);
    call.await.unwrap();
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 2);
    // Only the metadata session opened by `open` was used.
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ask_mode_requires_prompt() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Ask), None);
    let err = f.sm.open(id, None).await.unwrap_err();
    assert_eq!(err.stage(), Some(ConnectStage::ResolveSecrets));
    assert!(
        matches!(err.unstaged(), DbError::Secret { message } if message == "password_required"),
        "{err:?}"
    );
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 0);
    assert_eq!(f.sm.state(id), ConnState::Failed);

    let res = f.sm.open(id, Some(secrets(Some("typed")))).await.unwrap();
    assert_eq!(res.info.kind, DbKind::MySql);
    assert_eq!(
        f.log.last_password.lock().unwrap().as_deref(),
        Some("typed")
    );
    assert_eq!(f.sm.state(id), ConnState::Connected);
    // The prompted secret is not stored.
    let store_has = f.repo.tree().unwrap().connections[0].has_db_password;
    assert!(!store_has);
}

#[tokio::test]
async fn saved_mode_uses_stored_password() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("stored"));
    let res = f.sm.open(id, None).await.unwrap();
    assert_eq!(
        f.log.last_password.lock().unwrap().as_deref(),
        Some("stored")
    );
    let stages: Vec<_> = res.timings.iter().map(|t| t.stage).collect();
    // No SSH: no OpenTunnel stage.
    assert_eq!(
        stages,
        [
            ConnectStage::ResolveSecrets,
            ConnectStage::ConnectDb,
            ConnectStage::DetectServer
        ]
    );
    let summary = &f.repo.tree().unwrap().connections[0];
    assert!(summary.last_connected_at.is_some());
}

#[tokio::test]
async fn stage_reported_on_failure() {
    let f = fixture(DbKind::MySql, true);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    let err = f.sm.open(id, None).await.unwrap_err();
    assert!(
        matches!(err.unstaged(), DbError::Connect { message } if message == "fake refused (10061)"),
        "{err:?}"
    );
    let timings = f.sm.last_timings(id).unwrap();
    let stages: Vec<_> = timings.iter().map(|t| t.stage).collect();
    assert_eq!(stages, [ConnectStage::ResolveSecrets]);
    // The error names the failing stage and carries the completed stages for the UI.
    let app = AppError::from(err);
    assert_eq!(app.kind, "connect");
    assert_eq!(app.stage.as_deref(), Some("connect_db"));
    assert_eq!(app.timings, Some(timings));
    assert!(app.hint.is_some());
    assert_eq!(f.sm.state(id), ConnState::Failed);
    // A failed open does not count as connected.
    assert!(f.repo.tree().unwrap().connections[0]
        .last_connected_at
        .is_none());
}

#[tokio::test]
async fn pg_metadata_session_per_database() {
    let f = fixture(DbKind::Postgres, false);
    let id = save(&f, config(DbKind::Postgres, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    list_tables(
        &f,
        id,
        Some("a"),
        &["db:a", "schema:public", "folder:Table"],
    )
    .await;
    list_tables(
        &f,
        id,
        Some("b"),
        &["db:b", "schema:public", "folder:Table"],
    )
    .await;
    // Same database again: the session is reused.
    list_tables(&f, id, Some("a"), &["db:a", "schema:other", "folder:Table"]).await;
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 3);
    assert_eq!(
        *f.log.databases.lock().unwrap(),
        [None, Some("a".to_string()), Some("b".to_string())]
    );
}

#[tokio::test]
async fn lost_metadata_session_is_reopened_once() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();

    f.log.lose_next.store(1, Ordering::SeqCst);
    list_tables(&f, id, Some("shop"), &["db:shop", "folder:Table"]).await;
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 2);
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.sm.state(id), ConnState::Connected);

    // Only one retry: a second loss is reported.
    f.log.lose_next.store(2, Ordering::SeqCst);
    let k = key(&["db:shop", "folder:View"]);
    let err =
        f.sm.browse(id, Some("shop"), k, list("shop", ObjectKind::View))
            .await
            .unwrap_err();
    assert!(matches!(err, DbError::ConnectionLost { .. }), "{err:?}");
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn failed_reopen_marks_failed_until_a_call_succeeds() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();

    f.log.lose_next.store(1, Ordering::SeqCst);
    f.log.refuse.store(true, Ordering::SeqCst);
    let k = key(&["db:shop", "folder:Table"]);
    let err =
        f.sm.browse(id, Some("shop"), k.clone(), list("shop", ObjectKind::Table))
            .await
            .unwrap_err();
    assert!(matches!(err, DbError::Connect { .. }), "{err:?}");
    assert_eq!(f.sm.state(id), ConnState::Failed);

    // The server is back: the next call opens a fresh session.
    f.log.refuse.store(false, Ordering::SeqCst);
    list_tables(&f, id, Some("shop"), &["db:shop", "folder:Table"]).await;
    assert_eq!(f.sm.state(id), ConnState::Connected);
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn close_drops_runtime() {
    let f = fixture(DbKind::Postgres, false);
    let id = save(&f, config(DbKind::Postgres, SecretMode::Saved), Some("pw"));
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
    f.sm.open(id, None).await.unwrap();
    list_tables(&f, id, Some("a"), &["db:a", "folder:Table"]).await;
    assert_eq!(f.sm.state(id), ConnState::Connected);
    assert_eq!(f.sm.open_connections(), [id]);

    f.sm.close(id).await;
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
    assert!(f.sm.open_connections().is_empty());
    assert!(f.sm.schema_cache(id).is_none());
    // Both metadata sessions (connect database and `a`) were closed.
    assert_eq!(f.log.closes.load(Ordering::SeqCst), 2);
    let k = key(&["db:a", "folder:Table"]);
    let err =
        f.sm.browse(id, Some("a"), k, list("a", ObjectKind::Table))
            .await
            .unwrap_err();
    assert!(matches!(err, DbError::NotOpen), "{err:?}");
}

#[tokio::test]
async fn failed_test_reports_stage_and_partial_timings() {
    let f = fixture(DbKind::MySql, true);
    let cfg = config(DbKind::MySql, SecretMode::Saved);
    let err = f.sm.test(&cfg, secrets(Some("pw"))).await.unwrap_err();
    let app = AppError::from(err);
    assert_eq!(app.kind, "connect");
    assert_eq!(app.stage.as_deref(), Some("connect_db"));
    let stages: Vec<_> = app.timings.unwrap().iter().map(|t| t.stage).collect();
    assert_eq!(stages, [ConnectStage::ResolveSecrets]);
}

#[tokio::test]
async fn test_connection_opens_and_closes_without_persisting() {
    let f = fixture(DbKind::MySql, false);
    let mut cfg = config(DbKind::MySql, SecretMode::Saved);
    cfg.id = Uuid::new_v4();
    let res = f.sm.test(&cfg, secrets(Some("try"))).await.unwrap();
    assert_eq!(res.info.kind, DbKind::MySql);
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 1);
    assert_eq!(f.log.closes.load(Ordering::SeqCst), 1);
    assert_eq!(f.sm.state(cfg.id), ConnState::Disconnected);
    assert!(f.repo.tree().unwrap().connections.is_empty());
}

#[tokio::test]
async fn test_connection_falls_back_to_stored_secrets_and_typed_ones_win() {
    let f = fixture(DbKind::MySql, false);
    let id = save(
        &f,
        config(DbKind::MySql, SecretMode::Saved),
        Some("stored-pw"),
    );
    let cfg = f.repo.get(id).unwrap();

    f.sm.test(&cfg, secrets(None)).await.unwrap();
    assert_eq!(
        f.log.last_password.lock().unwrap().as_deref(),
        Some("stored-pw")
    );

    f.sm.test(&cfg, secrets(Some("typed-pw"))).await.unwrap();
    assert_eq!(
        f.log.last_password.lock().unwrap().as_deref(),
        Some("typed-pw")
    );

    // Ask mode and unsaved (nil id) configs have nothing to fall back to.
    let mut ask = cfg.clone();
    ask.secret_mode = SecretMode::Ask;
    f.sm.test(&ask, secrets(None)).await.unwrap();
    assert_eq!(f.log.last_password.lock().unwrap().as_deref(), None);
    let mut fresh = cfg;
    fresh.id = Uuid::nil();
    f.sm.test(&fresh, secrets(None)).await.unwrap();
    assert_eq!(f.log.last_password.lock().unwrap().as_deref(), None);
}

#[tokio::test]
async fn ask_mode_prompt_without_password_is_rejected() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Ask), None);
    let err = f.sm.open(id, Some(secrets(None))).await.unwrap_err();
    assert!(
        matches!(err.unstaged(), DbError::Secret { message } if message == "password_required"),
        "{err:?}"
    );
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ssh_password_auth_without_password_is_rejected() {
    let f = fixture(DbKind::MySql, false);
    let mut cfg = config(DbKind::MySql, SecretMode::Saved);
    cfg.ssh = Some(dnt_core::test_util::sample_ssh());
    let id = save(&f, cfg, Some("pw"));
    let err = f.sm.open(id, None).await.unwrap_err();
    assert!(
        matches!(err.unstaged(), DbError::Secret { message } if message == "password_required"),
        "{err:?}"
    );
}

/// Polls `cond` for up to 2 s.
async fn wait_until(cond: impl Fn() -> bool) {
    let started = Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "condition not met"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn close_during_slow_connect_ends_disconnected() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.log.connect_delay_ms.store(200, Ordering::SeqCst);
    let sm = Arc::clone(&f.sm);
    let open = tokio::spawn(async move { sm.open(id, None).await });
    wait_until(|| f.log.connects.load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        f.sm.state(id),
        ConnState::Connecting {
            stage: ConnectStage::ConnectDb
        }
    );

    f.sm.close(id).await;
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
    let err = open.await.unwrap().unwrap_err();
    assert!(matches!(err, DbError::Cancelled), "{err:?}");
    // The superseded open closed the session it got and installed nothing.
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
    assert!(f.sm.open_connections().is_empty());
    assert_eq!(f.log.closes.load(Ordering::SeqCst), 1);
    assert!(f.repo.tree().unwrap().connections[0]
        .last_connected_at
        .is_none());
}

#[tokio::test]
async fn browse_waiting_during_close_does_not_reconnect() {
    let f = fixture(DbKind::Postgres, false);
    let id = save(&f, config(DbKind::Postgres, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    f.log.list_delay_ms.store(150, Ordering::SeqCst);

    // `a` holds the metadata link (connect database); `b` waits for it and would open a
    // second session for database `b`.
    let sm = Arc::clone(&f.sm);
    let a = tokio::spawn(async move {
        sm.browse(id, None, key(&["db:root"]), list("root", ObjectKind::Table))
            .await
    });
    wait_until(|| f.log.list_calls.load(Ordering::SeqCst) == 1).await;
    let sm = Arc::clone(&f.sm);
    let b = tokio::spawn(async move {
        sm.browse(id, Some("b"), key(&["db:b"]), list("b", ObjectKind::Table))
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    f.sm.close(id).await;
    a.await.unwrap().unwrap();
    let err = b.await.unwrap().unwrap_err();
    assert!(matches!(err, DbError::ConnectionLost { .. }), "{err:?}");
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 1);
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
}

#[tokio::test]
async fn hanging_metadata_call_times_out_and_close_is_prompt() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    f.log.list_delay_ms.store(60_000, Ordering::SeqCst);

    let started = Instant::now();
    let err =
        f.sm.browse(
            id,
            Some("shop"),
            key(&["db:shop"]),
            list("shop", ObjectKind::Table),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::Timeout {
                stage: ConnectStage::Execute
            }
        ),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // No retry, and the stuck session was dropped rather than closed.
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.log.closes.load(Ordering::SeqCst), 0);

    // Close while another call hangs: it waits at most for the call bound.
    let sm = Arc::clone(&f.sm);
    let hung = tokio::spawn(async move {
        sm.browse(
            id,
            Some("shop"),
            key(&["db:shop"]),
            list("shop", ObjectKind::Table),
        )
        .await
    });
    wait_until(|| f.log.list_calls.load(Ordering::SeqCst) == 2).await;
    let started = Instant::now();
    f.sm.close(id).await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
    let err = hung.await.unwrap().unwrap_err();
    assert!(matches!(err, DbError::Timeout { .. }), "{err:?}");
}

#[tokio::test]
async fn refresh_during_browse_does_not_cache_the_stale_result() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    f.log.list_delay_ms.store(100, Ordering::SeqCst);
    let k = ["db:shop", "folder:Table"];

    let sm = Arc::clone(&f.sm);
    let call = tokio::spawn(async move {
        sm.browse(id, Some("shop"), key(&k), list("shop", ObjectKind::Table))
            .await
    });
    wait_until(|| f.log.list_calls.load(Ordering::SeqCst) == 1).await;
    f.sm.refresh(id, &key(&["db:shop"]));
    call.await.unwrap().unwrap();

    f.log.list_delay_ms.store(0, Ordering::SeqCst);
    list_tables(&f, id, Some("shop"), &k).await;
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn aborted_open_resets_pending() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.log.connect_delay_ms.store(5_000, Ordering::SeqCst);
    let sm = Arc::clone(&f.sm);
    let open = tokio::spawn(async move { sm.open(id, None).await });
    wait_until(|| f.log.connects.load(Ordering::SeqCst) == 1).await;
    assert!(matches!(f.sm.state(id), ConnState::Connecting { .. }));

    open.abort();
    assert!(open.await.unwrap_err().is_cancelled());
    assert_eq!(f.sm.state(id), ConnState::Disconnected);
}
