//! `SessionManager` against an in-process fake driver (no database needed).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome,
};
use dnt_core::error::{AppError, ConnectStage, DbError};
use dnt_core::meta::{
    ColumnBrief, DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope,
    TableColumns, TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, detect_pg, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use dntdatabase::cache::{NodeKey, SchemaCache};
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
    columns_calls: AtomicUsize,
    detail_calls: AtomicUsize,
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
    /// Every `list_columns` call takes this long (ms).
    columns_delay_ms: AtomicU64,
    /// `Scope::database` of every `list_columns` call, in order.
    column_scopes: Mutex<Vec<String>>,
    /// `list_columns` calls that ran to the end (were not dropped mid-call).
    columns_done: AtomicUsize,
    /// While set, `list_columns` fails.
    fail_columns: AtomicBool,
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
    async fn execute_one(
        &self,
        _stmt: &str,
        _kind: StatementKind,
        _opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let summary = ResultSummary {
            affected: None,
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
        TxState::Idle
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
                was_in_tx: false,
                outcome_unknown: false,
                message: "Lost connection to MySQL server during query".into(),
            });
        }
        Ok(vec![ObjectSummary {
            name: "orders".into(),
            kind,
            comment: None,
            row_estimate: None,
        }])
    }
    async fn list_columns(&self, scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        self.log.columns_calls.fetch_add(1, Ordering::SeqCst);
        self.log
            .column_scopes
            .lock()
            .unwrap()
            .push(scope.database.clone());
        delay(&self.log.columns_delay_ms).await;
        self.log.columns_done.fetch_add(1, Ordering::SeqCst);
        if self.log.fail_columns.load(Ordering::SeqCst) {
            return Err(DbError::Server {
                code: Some("1142".into()),
                sqlstate: None,
                message: "columns denied".into(),
                position: None,
            });
        }
        Ok(vec![TableColumns {
            table: "orders".into(),
            columns: vec![
                ColumnBrief {
                    name: "id".into(),
                    data_type: "int".into(),
                },
                ColumnBrief {
                    name: "total".into(),
                    data_type: "decimal(12,2)".into(),
                },
            ],
        }])
    }
    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        self.log.detail_calls.fetch_add(1, Ordering::SeqCst);
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

/// How long a failed SchemaCache load is remembered in these tests instead of 10 s.
const TEST_SCHEMA_ERROR_TTL: Duration = Duration::from_millis(200);

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
    .with_metadata_timeout(TEST_METADATA_TIMEOUT)
    .with_schema_error_ttl(TEST_SCHEMA_ERROR_TTL);
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
async fn schema_cache_loads_with_bulk_column_listing_only() {
    let f = fixture(DbKind::MySql, false);
    let mut cfg = config(DbKind::MySql, SecretMode::Saved);
    cfg.default_db = Some("shop".into());
    let id = save(&f, cfg, Some("pw"));
    f.sm.open(id, None).await.unwrap();
    // `open` starts loading the home database in the background; this call waits for
    // that load instead of starting a second one.
    let cache = f.sm.schema_cache_for(id, "shop").await.unwrap();
    assert_eq!(cache.database, "shop");
    let orders = cache
        .tables
        .iter()
        .find(|t| t.name == "orders" && t.kind == ObjectKind::Table)
        .unwrap();
    let columns: Vec<(&str, &str)> = orders
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.data_type.as_str()))
        .collect();
    assert_eq!(columns, [("id", "int"), ("total", "decimal(12,2)")]);
    // One column query per scope, never a per-table detail.
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.log.detail_calls.load(Ordering::SeqCst), 0);
    // Tables and views listed: one list_objects call each.
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn schema_cache_is_per_database_and_lazy() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    // No default database: nothing is loaded until asked for.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 0);

    let a = f.sm.schema_cache_for(id, "a").await.unwrap();
    assert_eq!(a.database, "a");
    let b = f.sm.schema_cache_for(id, "b").await.unwrap();
    assert_eq!(b.database, "b");
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 2);
    // A repeat is served from the cache.
    assert_eq!(f.sm.schema_cache_for(id, "a").await.unwrap(), a);
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 2);
    assert_eq!(*f.log.column_scopes.lock().unwrap(), ["a", "b"]);

    // Concurrent first requests for one database share a single load.
    f.log.columns_delay_ms.store(50, Ordering::SeqCst);
    let (x, y) = tokio::join!(
        f.sm.schema_cache_for(id, "c"),
        f.sm.schema_cache_for(id, "c")
    );
    assert_eq!(x.unwrap(), y.unwrap());
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn refresh_schema_cache_reloads_in_background() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    f.sm.schema_cache_for(id, "a").await.unwrap();
    f.sm.schema_cache_for(id, "b").await.unwrap();
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 2);

    f.sm.refresh_schema_cache(id, "a");
    wait_until(|| f.log.columns_calls.load(Ordering::SeqCst) == 3).await;
    // The reload is what the next call gets: no further load.
    f.sm.schema_cache_for(id, "a").await.unwrap();
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 3);

    // A tree refresh of a database reloads that database's cache too (and only it).
    f.sm.refresh(id, &key(&["db:b", "folder:Table"]));
    wait_until(|| f.log.columns_calls.load(Ordering::SeqCst) == 4).await;
    assert_eq!(
        f.log
            .column_scopes
            .lock()
            .unwrap()
            .last()
            .map(String::as_str),
        Some("b")
    );
    // A whole-connection refresh reloads every loaded database.
    f.sm.refresh(id, &key(&[]));
    wait_until(|| f.log.columns_calls.load(Ordering::SeqCst) == 6).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 6);
}

fn is_columns_denied(r: &Result<SchemaCache, DbError>) -> bool {
    matches!(r, Err(DbError::Server { message, .. }) if message == "columns denied")
}

#[tokio::test]
async fn failed_schema_load_is_shared_and_remembered_for_the_ttl() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    f.log.fail_columns.store(true, Ordering::SeqCst);
    f.log.columns_delay_ms.store(50, Ordering::SeqCst);

    // Five concurrent callers share one failing load.
    let results =
        futures_util::future::join_all((0..5).map(|_| f.sm.schema_cache_for(id, "a"))).await;
    assert!(results.iter().all(is_columns_denied), "{results:?}");
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 1);

    // Within the TTL the error is returned without a new load.
    let again = f.sm.schema_cache_for(id, "a").await;
    assert!(is_columns_denied(&again), "{again:?}");
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 1);

    // After the TTL the next caller retries; a success replaces the error.
    tokio::time::sleep(TEST_SCHEMA_ERROR_TTL + Duration::from_millis(50)).await;
    f.log.fail_columns.store(false, Ordering::SeqCst);
    let cache = f.sm.schema_cache_for(id, "a").await.unwrap();
    assert_eq!(cache.database, "a");
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 2);
    f.sm.schema_cache_for(id, "a").await.unwrap();
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_aborts_the_inflight_load() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    f.log.columns_delay_ms.store(150, Ordering::SeqCst);

    let sm = Arc::clone(&f.sm);
    let first = tokio::spawn(async move { sm.schema_cache_for(id, "a").await });
    wait_until(|| f.log.columns_calls.load(Ordering::SeqCst) == 1).await;
    f.sm.refresh_schema_cache(id, "a");
    // The waiting caller follows the reload instead of failing.
    assert_eq!(first.await.unwrap().unwrap().database, "a");
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The first load never finished: it was aborted, not left to compete for the link.
    assert_eq!(f.log.columns_done.load(Ordering::SeqCst), 1);
    // Its session was dropped mid-call (it may be mid-protocol), not reused.
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn completion_inputs_are_validated() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    let is_config = |e: &DbError, field: &str| matches!(e, DbError::Config { field: Some(f), .. } if f == field);
    let err = f.sm.schema_cache_for(id, "").await.unwrap_err();
    assert!(is_config(&err, "database"), "{err:?}");
    let err = f.sm.completion_objects(id, "", None).await.unwrap_err();
    assert!(is_config(&err, "database"), "{err:?}");
    // MySQL has no schemas below a database.
    let err =
        f.sm.completion_objects(id, "shop", Some("public"))
            .await
            .unwrap_err();
    assert!(is_config(&err, "schema"), "{err:?}");
    assert_eq!(f.log.columns_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 0);

    // PostgreSQL: objects are listed per schema, like the tree.
    let f = fixture(DbKind::Postgres, false);
    let id = save(&f, config(DbKind::Postgres, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    let err = f.sm.completion_objects(id, "shop", None).await.unwrap_err();
    assert!(is_config(&err, "schema"), "{err:?}");
    let err =
        f.sm.completion_objects(id, "", Some("public"))
            .await
            .unwrap_err();
    assert!(is_config(&err, "database"), "{err:?}");
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn completion_objects_uses_browse_cache() {
    let f = fixture(DbKind::MySql, false);
    let id = save(&f, config(DbKind::MySql, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    let first = f.sm.completion_objects(id, "shop", None).await.unwrap();
    let second = f.sm.completion_objects(id, "shop", None).await.unwrap();
    assert_eq!(first, second);
    let kinds: Vec<ObjectKind> = first.iter().map(|o| o.kind).collect();
    assert_eq!(kinds, [ObjectKind::Table, ObjectKind::View]);
    assert!(first
        .iter()
        .all(|o| o.name == "orders" && o.schema.is_none()));
    // One listing per kind, shared with the tree (same folder keys).
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 2);
    list_tables(&f, id, Some("shop"), &["db:shop", "folder:Table"]).await;
    assert_eq!(f.log.list_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn pg_completion_objects_list_materialized_views_of_the_schema() {
    let f = fixture(DbKind::Postgres, false);
    let id = save(&f, config(DbKind::Postgres, SecretMode::Saved), Some("pw"));
    f.sm.open(id, None).await.unwrap();
    let objects =
        f.sm.completion_objects(id, "shop", Some("sales"))
            .await
            .unwrap();
    let kinds: Vec<ObjectKind> = objects.iter().map(|o| o.kind).collect();
    assert_eq!(
        kinds,
        [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView
        ]
    );
    assert!(objects.iter().all(|o| o.schema.as_deref() == Some("sales")));
    // Listed on the metadata session of database `shop`.
    assert_eq!(
        f.log.databases.lock().unwrap().last().cloned().flatten(),
        Some("shop".to_string())
    );
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
    let err = f.sm.schema_cache_for(id, "a").await.unwrap_err();
    assert!(matches!(err, DbError::NotOpen), "{err:?}");
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
async fn test_falls_back_to_stored_secrets_only_for_the_same_server_and_user() {
    let f = fixture(DbKind::MySql, false);
    let id = save(
        &f,
        config(DbKind::MySql, SecretMode::Saved),
        Some("stored-pw"),
    );
    let stored = f.repo.get(id).unwrap();
    type Edit = (&'static str, fn(&mut ConnectionConfig));
    let edits: [Edit; 3] = [
        ("host", |c| c.host = "evil.example.com".into()),
        ("port", |c| c.port = 3307),
        ("username", |c| c.username = "someone".into()),
    ];
    for (what, edit) in edits {
        let mut cfg = stored.clone();
        edit(&mut cfg);
        f.sm.test(&cfg, secrets(None)).await.unwrap();
        assert_eq!(
            f.log.last_password.lock().unwrap().as_deref(),
            None,
            "changed {what}: the stored password must not be sent"
        );
        // A typed password is still used.
        f.sm.test(&cfg, secrets(Some("typed"))).await.unwrap();
        assert_eq!(
            f.log.last_password.lock().unwrap().as_deref(),
            Some("typed")
        );
    }
    // Other edits (name, database, timeouts) keep the fallback.
    let mut cfg = stored.clone();
    cfg.name = "renamed".into();
    cfg.default_db = Some("other".into());
    f.sm.test(&cfg, secrets(None)).await.unwrap();
    assert_eq!(
        f.log.last_password.lock().unwrap().as_deref(),
        Some("stored-pw")
    );
}

#[tokio::test]
async fn test_falls_back_to_stored_ssh_secret_only_for_the_same_ssh_server_and_user() {
    let f = fixture(DbKind::MySql, false);
    let mut cfg = config(DbKind::MySql, SecretMode::Saved);
    cfg.options.ssh_timeout_s = 1;
    cfg.ssh = Some(dnt_core::config::SshConfig {
        // Nothing listens there: with the stored password the test fails at OpenTunnel.
        host: "127.0.0.1".into(),
        port: 9,
        ..dnt_core::test_util::sample_ssh()
    });
    let id = f
        .repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(SecretString::from("pw")),
                ssh_password: Some(SecretString::from("ssh-pw")),
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    let stored = f.repo.get(id).unwrap();

    let err = f.sm.test(&stored, secrets(None)).await.unwrap_err();
    assert_eq!(err.stage(), Some(ConnectStage::OpenTunnel), "{err:?}");

    type Edit = (&'static str, fn(&mut dnt_core::config::SshConfig));
    let edits: [Edit; 3] = [
        ("ssh.host", |s| s.host = "localhost".into()),
        ("ssh.port", |s| s.port = 10),
        ("ssh.user", |s| s.user = "other".into()),
    ];
    for (what, edit) in edits {
        let mut cfg = stored.clone();
        edit(cfg.ssh.as_mut().unwrap());
        let err = f.sm.test(&cfg, secrets(None)).await.unwrap_err();
        assert_eq!(err.stage(), Some(ConnectStage::ResolveSecrets), "{what}");
        assert!(
            matches!(err.unstaged(), DbError::Secret { message } if message == "password_required"),
            "changed {what}: {err:?}"
        );
    }

    // Only the database endpoint changed: the bastion still gets its stored password
    // (the test fails at OpenTunnel, not at ResolveSecrets).
    let mut cfg = stored.clone();
    cfg.host = "10.0.0.9".into();
    let err = f.sm.test(&cfg, secrets(None)).await.unwrap_err();
    assert_eq!(err.stage(), Some(ConnectStage::OpenTunnel), "{err:?}");
}

#[tokio::test]
async fn test_does_not_send_stored_password_when_ssh_is_turned_off() {
    let f = fixture(DbKind::MySql, false);
    let mut cfg = config(DbKind::MySql, SecretMode::Saved);
    cfg.ssh = Some(dnt_core::test_util::sample_ssh());
    let id = f
        .repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(SecretString::from("stored-pw")),
                ssh_password: Some(SecretString::from("ssh-pw")),
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    // Without the tunnel the same host:port is another server (seen from this machine).
    let mut cfg = f.repo.get(id).unwrap();
    cfg.ssh = None;
    f.sm.test(&cfg, secrets(None)).await.unwrap();
    assert_eq!(f.log.last_password.lock().unwrap().as_deref(), None);
}

#[tokio::test]
async fn test_does_not_send_stored_ssh_secret_when_ssh_is_turned_on() {
    let f = fixture(DbKind::MySql, false);
    let id = f
        .repo
        .create(
            config(DbKind::MySql, SecretMode::Saved),
            NewSecrets {
                db_password: Some(SecretString::from("stored-pw")),
                ssh_password: Some(SecretString::from("ssh-pw")),
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    let mut cfg = f.repo.get(id).unwrap();
    cfg.ssh = Some(dnt_core::test_util::sample_ssh());
    let err = f.sm.test(&cfg, secrets(None)).await.unwrap_err();
    assert_eq!(err.stage(), Some(ConnectStage::ResolveSecrets), "{err:?}");
    assert_eq!(AppError::from(err).kind, "password_required");
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn saved_encrypted_ssh_key_without_passphrase_asks_for_it() {
    let f = fixture(DbKind::MySql, false);
    let mut cfg = config(DbKind::MySql, SecretMode::Saved);
    cfg.ssh = Some(dnt_core::config::SshConfig {
        auth: dnt_core::config::SshAuth::Key,
        key_path: Some(dnt_testkit::testdata("ssh/id_ed25519_enc")),
        ..dnt_core::test_util::sample_ssh()
    });
    let id = save(&f, cfg, Some("pw"));
    let err = f.sm.open(id, None).await.unwrap_err();
    let app = AppError::from(err);
    assert_eq!(app.kind, "password_required", "{app:?}");
    assert_eq!(app.stage.as_deref(), Some("open_tunnel"));
    assert_eq!(f.log.connects.load(Ordering::SeqCst), 0);
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
    assert!(matches!(err, DbError::Cancelled { .. }), "{err:?}");
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
                stage: ConnectStage::Execute,
                outcome_unknown: false,
                was_in_tx: false,
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
