//! Browsing the tree of a Command connection (the fake Command driver of `dnt_testkit`,
//! registered as Redis), and of SQL connections (unchanged: system names filtered)
//! (R4a Task 4).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, DriverOptions, ResolvedConfig, SecretMode};
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
use dnt_core::server::{detect_mysql, detect_pg, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::fake_command::{collection_key, FakeCommandDriver, FakeStore, FakeValue};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::browse::{
    list_databases_impl, list_objects_impl, list_schemas_impl, refresh_node_impl, table_detail_impl,
};
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use uuid::Uuid;

struct Fixture {
    state: AppState,
    id: Uuid,
    _dir: tempfile::TempDir,
}

/// An `AppState` whose only driver is `driver` (registered as `cfg.kind`), with `cfg`
/// saved and open.
async fn fixture(driver: Arc<dyn Driver>, cfg: ConnectionConfig) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let dir = tempfile::tempdir().unwrap();
    let sessions = SessionManager::new(
        Arc::clone(&repo),
        Arc::clone(&secrets),
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(cfg.kind, driver)]),
    );
    let new_secrets = NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    let id = repo.create(cfg, new_secrets).unwrap().id;
    sessions.open(id, None).await.unwrap();
    let state = AppState {
        repo,
        secrets,
        sessions,
        paths: AppPaths::in_dir(dir.path().join("data")),
        export_paths: ExportPaths::default(),
        file_picks: FilePicks::default(),
        ai: Default::default(),
    };
    Fixture {
        state,
        id,
        _dir: dir,
    }
}

/// The fake Command driver as Redis, with a connection open on db "0".
async fn redis_fixture() -> (Fixture, FakeStore) {
    let driver = FakeCommandDriver::with_kind(DbKind::Redis);
    let store = driver.store();
    let mut cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: "Fake Redis".into(),
        port: 6379,
        default_db: Some("0".into()),
        secret_mode: SecretMode::Saved,
        ..sample_config()
    };
    cfg.options.driver = DriverOptions::Redis { db: 0 };
    (fixture(Arc::new(driver), cfg).await, store)
}

fn db_names(dbs: Vec<DatabaseInfo>) -> Vec<String> {
    dbs.into_iter().map(|d| d.name).collect()
}

fn scope(db: &str) -> Scope {
    Scope {
        database: db.into(),
        schema: None,
    }
}

#[tokio::test]
async fn command_tree_lists_databases_and_objects() {
    let (f, store) = redis_fixture().await;
    store.set("3", b"plain", FakeValue::Str("x".into()));
    store.set("7", b"h:orders", FakeValue::Hash(BTreeMap::new()));

    // The dbs with keys plus the current one, whatever `show_system` says.
    for show_system in [false, true] {
        let dbs = list_databases_impl(&f.state, f.id, show_system)
            .await
            .unwrap();
        assert_eq!(db_names(dbs), ["0", "3", "7"]);
    }
    // No schemas, and no error.
    let schemas = list_schemas_impl(&f.state, f.id, "0".into(), false)
        .await
        .unwrap();
    assert!(schemas.is_empty());

    // `HSET h:users u1 alice` makes collection `users` in db 0.
    store.set(
        "0",
        &collection_key("users"),
        FakeValue::Hash(BTreeMap::from([("u1".into(), "alice".into())])),
    );
    store.set("0", b"h:", FakeValue::Str("not a hash".into()));
    let objects = list_objects_impl(&f.state, f.id, scope("0"), ObjectKind::Collection)
        .await
        .unwrap();
    let names: Vec<_> = objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["users"]);
    assert_eq!(objects[0].kind, ObjectKind::Collection);
    // Other folders are empty; another db lists its own collections.
    let tables = list_objects_impl(&f.state, f.id, scope("0"), ObjectKind::Table)
        .await
        .unwrap();
    assert!(tables.is_empty());
    let other = list_objects_impl(&f.state, f.id, scope("7"), ObjectKind::Collection)
        .await
        .unwrap();
    assert_eq!(other[0].name, "orders");

    let users = ObjectRef {
        database: "0".into(),
        schema: None,
        name: "users".into(),
        kind: ObjectKind::Collection,
    };
    let detail = table_detail_impl(&f.state, f.id, users.clone())
        .await
        .unwrap();
    assert_eq!(detail.object, users);
    let columns: Vec<_> = detail.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(columns, ["_id", "value"]);
    let pk = detail.primary_key.expect("`_id` is the primary key");
    assert!(pk.primary);
    assert_eq!(pk.columns, ["_id"]);

    // A collection that is not there is a server error.
    let missing = ObjectRef {
        name: "nope".into(),
        ..users
    };
    let err = table_detail_impl(&f.state, f.id, missing)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Server { .. }), "{err:?}");

    // A refresh drops the cached listing.
    store.set(
        "0",
        &collection_key("audit"),
        FakeValue::Hash(BTreeMap::new()),
    );
    refresh_node_impl(&f.state, f.id, vec!["db:0".into()])
        .await
        .unwrap();
    let objects = list_objects_impl(&f.state, f.id, scope("0"), ObjectKind::Collection)
        .await
        .unwrap();
    let names: Vec<_> = objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["audit", "users"]);
}

// ---------------------------------------------------------------------------------
// SQL: a fake whose listings hold system names.

struct ListingSqlDriver {
    kind: DbKind,
}

struct ListingSqlSession {
    info: ServerInfo,
}

struct NoopCancel;

#[async_trait]
impl Canceller for NoopCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        Ok(())
    }
}

#[async_trait]
impl Driver for ListingSqlDriver {
    fn kind(&self) -> DbKind {
        self.kind
    }

    async fn connect(&self, _cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let info = match self.kind {
            DbKind::MySql => detect_mysql("8.0.36")?,
            DbKind::Postgres => detect_pg(170002, "PostgreSQL 17.2"),
            DbKind::Mongo | DbKind::Redis => unreachable!("a SQL fake"),
        };
        Ok(Box::new(ListingSqlSession { info }))
    }
}

#[async_trait]
impl Session for ListingSqlSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    fn engine(&self) -> Engine<'_> {
        Engine::Sql(self)
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
    async fn close(self: Box<Self>) {}
}

#[async_trait]
impl SqlSession for ListingSqlSession {
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
        match self.info.kind {
            DbKind::MySql => Dialect::MySql(Default::default()),
            DbKind::Postgres => Dialect::Postgres,
            DbKind::Mongo | DbKind::Redis => unreachable!("a SQL fake"),
        }
    }
}

#[async_trait]
impl Introspector for ListingSqlSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Ok(["app", "information_schema", "mysql", "sys"]
            .into_iter()
            .map(|name| DatabaseInfo { name: name.into() })
            .collect())
    }
    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(["public", "information_schema", "pg_catalog", "sales"]
            .into_iter()
            .map(|name| SchemaInfo { name: name.into() })
            .collect())
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

async fn sql_fixture(kind: DbKind) -> Fixture {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind,
        default_db: Some("app".into()),
        ..sample_config()
    };
    fixture(Arc::new(ListingSqlDriver { kind }), cfg).await
}

fn schema_names(schemas: Vec<SchemaInfo>) -> Vec<String> {
    schemas.into_iter().map(|s| s.name).collect()
}

#[tokio::test]
async fn sql_tree_unchanged() {
    // MySQL hides its system databases unless asked.
    let f = sql_fixture(DbKind::MySql).await;
    let dbs = list_databases_impl(&f.state, f.id, false).await.unwrap();
    assert_eq!(db_names(dbs), ["app"]);
    let dbs = list_databases_impl(&f.state, f.id, true).await.unwrap();
    assert_eq!(db_names(dbs), ["app", "information_schema", "mysql", "sys"]);

    // PostgreSQL lists every database, and hides its system schemas unless asked.
    let f = sql_fixture(DbKind::Postgres).await;
    let dbs = list_databases_impl(&f.state, f.id, false).await.unwrap();
    assert_eq!(dbs.len(), 4);
    let schemas = list_schemas_impl(&f.state, f.id, "app".into(), false)
        .await
        .unwrap();
    assert_eq!(schema_names(schemas), ["public", "sales"]);
    let schemas = list_schemas_impl(&f.state, f.id, "app".into(), true)
        .await
        .unwrap();
    assert_eq!(
        schema_names(schemas),
        ["public", "information_schema", "pg_catalog", "sales"]
    );
}
