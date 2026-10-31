//! App-side safety paths with a Command session (the in-memory fake Command driver of
//! `dnt_testkit`, registered as PostgreSQL) and the capability gates:
//!
//! - SQL-only code refuses a session that is not SQL, and closes it;
//! - a Command session inside `MULTI` is not idle for the transaction checks;
//! - every capability gate refuses when its capability is off.

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, Engine, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, SqlSession, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::keyspace::Keyspace;
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_pg, Cap, Capabilities, DbKind, NamespaceModel, ServerInfo};
use dnt_core::sql::ddl::ObjectAction;
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::Target;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::fake_command::{FakeCommandDriver, FakeStore};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::backup::dump_objects_impl;
use dntdatabase::commands::data::{data_editable_impl, table_data_impl};
use dntdatabase::commands::import_data::import_target_impl;
use dntdatabase::commands::query::{
    query_close_tab_impl, query_explain_impl, query_plan_impl, query_tx_info_impl,
};
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// The database the connection opens on (the fake Command driver's db 0).
const DB: &str = "0";

/// Sessions opened and closed through a test driver.
#[derive(Default)]
struct Counts {
    opened: AtomicUsize,
    closed: AtomicUsize,
}

impl Counts {
    fn get(&self) -> (usize, usize) {
        (
            self.opened.load(Ordering::SeqCst),
            self.closed.load(Ordering::SeqCst),
        )
    }
}

// ---------------------------------------------------------------------------------
// Command driver: the fake Command driver, counted.

/// [`FakeCommandDriver`] with every capability gate open, so a call gets past its
/// `require_cap` and reaches the SQL-only code behind it. Counts the sessions it opens
/// and closes; while `multi` is set, every new session runs `MULTI` as it opens.
struct CountedCommandDriver {
    inner: FakeCommandDriver,
    counts: Arc<Counts>,
    multi: Arc<AtomicBool>,
}

/// A session of the fake Command driver whose `ServerInfo` says every capability is on.
struct CountedCommandSession {
    inner: Box<dyn Session>,
    info: ServerInfo,
    counts: Arc<Counts>,
}

/// Ignores everything.
struct NoSink;

impl RowSink for NoSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

#[async_trait]
impl Driver for CountedCommandDriver {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }

    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let inner = self.inner.connect(cfg).await?;
        if self.multi.load(Ordering::SeqCst) {
            let command = inner.command()?;
            let multi = command
                .language()
                .parse("MULTI")
                .map_err(|e| DbError::Internal { message: e.message })?;
            command
                .execute_command(&multi, &ExecOptions::default(), &mut NoSink)
                .await?;
        }
        let mut info = inner.server_info().clone();
        let caps = &mut info.capabilities;
        (caps.designer, caps.ddl_edit, caps.object_actions) = (true, true, true);
        (caps.import, caps.dump, caps.seed, caps.explain) = (true, true, true, true);
        self.counts.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountedCommandSession {
            inner,
            info,
            counts: Arc::clone(&self.counts),
        }))
    }
}

#[async_trait]
impl Session for CountedCommandSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    fn engine(&self) -> Engine<'_> {
        self.inner.engine()
    }
    fn introspector(&self) -> &dyn Introspector {
        self.inner.introspector()
    }
    fn keyspace(&self) -> Option<&dyn Keyspace> {
        self.inner.keyspace()
    }
    fn cancel_handle(&self) -> CancelHandle {
        self.inner.cancel_handle()
    }
    async fn ping(&self) -> Result<(), DbError> {
        self.inner.ping().await
    }
    async fn close(self: Box<Self>) {
        self.counts.closed.fetch_add(1, Ordering::SeqCst);
        self.inner.close().await;
    }
}

// ---------------------------------------------------------------------------------
// SQL driver: a PostgreSQL fake whose capabilities the test sets.

struct CapsSqlDriver {
    caps: Arc<Mutex<Capabilities>>,
}

struct CapsSqlSession {
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
impl Driver for CapsSqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    async fn connect(&self, _cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let mut info = detect_pg(170002, "PostgreSQL 17.2");
        info.capabilities = *self.caps.lock().unwrap();
        Ok(Box::new(CapsSqlSession { info }))
    }
}

#[async_trait]
impl Session for CapsSqlSession {
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
impl SqlSession for CapsSqlSession {
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
        Dialect::Postgres
    }
}

#[async_trait]
impl Introspector for CapsSqlSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Ok(vec![DatabaseInfo { name: DB.into() }])
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

// ---------------------------------------------------------------------------------
// Fixture

struct Fixture {
    state: AppState,
    id: Uuid,
    _dir: tempfile::TempDir,
}

/// An `AppState` whose only driver is `driver` (PostgreSQL), with one saved, open
/// connection on database [`DB`].
async fn fixture(driver: Arc<dyn Driver>) -> Fixture {
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
        HashMap::from([(DbKind::Postgres, driver)]),
    );
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Postgres,
        port: 5432,
        default_db: Some(DB.into()),
        secret_mode: SecretMode::Saved,
        ..sample_config()
    };
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
    };
    Fixture {
        state,
        id,
        _dir: dir,
    }
}

struct CommandFixture {
    f: Fixture,
    store: FakeStore,
    counts: Arc<Counts>,
    multi: Arc<AtomicBool>,
}

async fn command_fixture() -> CommandFixture {
    let inner = FakeCommandDriver::new();
    let store = inner.store();
    let counts = Arc::new(Counts::default());
    let multi = Arc::new(AtomicBool::new(false));
    let driver = Arc::new(CountedCommandDriver {
        inner,
        counts: Arc::clone(&counts),
        multi: Arc::clone(&multi),
    });
    CommandFixture {
        f: fixture(driver).await,
        store,
        counts,
        multi,
    }
}

fn table() -> ObjectRef {
    ObjectRef {
        database: DB.into(),
        schema: Some("public".into()),
        name: "items".into(),
        kind: ObjectKind::Table,
    }
}

fn scope() -> Scope {
    Scope {
        database: DB.into(),
        schema: Some("public".into()),
    }
}

fn truncate() -> ObjectAction {
    ObjectAction::TruncateTable { object: table() }
}

#[track_caller]
fn assert_unsupported<T: std::fmt::Debug>(r: Result<T, DbError>, expected: &str) {
    match r {
        Err(DbError::Unsupported { feature, .. }) if feature == expected => {}
        other => panic!("expected Unsupported {{ feature: {expected:?} }}, got {other:?}"),
    }
}

/// Waits (bounded) until every session opened since `before` is closed again: some
/// paths close a refused session in a spawned task.
async fn assert_all_closed_since(counts: &Counts, before: (usize, usize), what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (opened, closed) = counts.get();
        let (new_opened, new_closed) = (opened - before.0, closed - before.1);
        if new_opened == new_closed {
            assert!(new_opened > 0, "{what}: no session was opened");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{what}: opened {new_opened} sessions, closed {new_closed}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------
// Tests

/// Every SQL-only entry point refuses the Command session with "SQL on PostgreSQL" and
/// closes every session it opened for it; nothing reaches the fake.
#[tokio::test]
async fn sql_paths_refuse_a_command_session() {
    let c = command_fixture().await;
    let (state, id) = (&c.f.state, c.f.id);
    const SQL: &str = "SQL on PostgreSQL";

    let before = c.counts.get();
    let r = table_data_impl(state, id, table(), 100, 0, vec![], None, "data".into()).await;
    assert_unsupported(r, SQL);
    assert_all_closed_since(&c.counts, before, "table_data_impl").await;

    // The tab keeps its session (as any tab does) until the tab is closed.
    let before = c.counts.get();
    let r = query_plan_impl(
        state,
        "tab".into(),
        id,
        Some(DB.into()),
        "GET a".into(),
        Target::All,
    )
    .await;
    assert_unsupported(r, SQL);
    query_close_tab_impl(state, "tab".into(), false)
        .await
        .unwrap();
    assert_all_closed_since(&c.counts, before, "query_plan_impl").await;

    let before = c.counts.get();
    let r = state
        .sessions
        .table_design_open(id, scope(), Some("items".into()))
        .await;
    assert_unsupported(r, SQL);
    assert_all_closed_since(&c.counts, before, "table_design_open").await;

    let before = c.counts.get();
    assert_unsupported(import_target_impl(state, id, table()).await, SQL);
    assert_all_closed_since(&c.counts, before, "import_target_impl").await;

    let before = c.counts.get();
    let r = dump_objects_impl(state, id, DB.into(), vec!["public".into()]).await;
    assert_unsupported(r, SQL);
    assert_all_closed_since(&c.counts, before, "dump_objects_impl").await;

    assert_eq!(c.store.executed(), Vec::<String>::new());
    // Closing the connection closes the rest (its metadata session).
    state.sessions.close(id).await;
    let (opened, closed) = c.counts.get();
    assert_eq!(opened, closed, "sessions left open after close");
}

/// A tab whose Command session ran `MULTI` is in a transaction: `query_tx_info` says
/// Active and DDL (`check_ddl_idle`) is refused while it is open.
#[tokio::test]
async fn tab_with_command_session_is_not_idle() {
    let c = command_fixture().await;
    let (state, id) = (&c.f.state, c.f.id);
    let plan = |tab: &'static str| {
        query_plan_impl(
            state,
            tab.into(),
            id,
            Some(DB.into()),
            "GET a".into(),
            Target::All,
        )
    };

    // A tab with an idle Command session (planning refuses it; the tab keeps it).
    assert_unsupported(plan("idle").await, "SQL on PostgreSQL");
    let info = query_tx_info_impl(state, "idle".into()).await.unwrap();
    assert_eq!(info.state, TxState::Idle);
    query_close_tab_impl(state, "idle".into(), false)
        .await
        .unwrap();

    // The tab's session runs MULTI as it opens.
    c.multi.store(true, Ordering::SeqCst);
    assert_unsupported(plan("multi").await, "SQL on PostgreSQL");
    c.multi.store(false, Ordering::SeqCst);
    assert_eq!(c.store.executed(), ["MULTI"]);
    let info = query_tx_info_impl(state, "multi".into()).await.unwrap();
    assert_eq!(info.state, TxState::Active);
    let r = state.sessions.object_action_preview(id, truncate()).await;
    assert!(matches!(r, Err(DbError::TransactionOpen)), "{r:?}");

    // Once the tab is closed, nothing holds a transaction: the action goes on to its
    // own session, which is not SQL.
    query_close_tab_impl(state, "multi".into(), true)
        .await
        .unwrap();
    let r = state.sessions.object_action_preview(id, truncate()).await;
    assert_unsupported(r, "SQL on PostgreSQL");
    assert_eq!(c.store.executed(), ["MULTI"]);
}

/// The capabilities of a PostgreSQL server with every feature but `off`.
fn caps_without(off: Cap) -> Capabilities {
    let mut caps = Capabilities::sql_full(NamespaceModel::DbSchema);
    let flag = match off {
        Cap::DataEdit => &mut caps.data_edit,
        Cap::Designer => &mut caps.designer,
        Cap::DdlEdit => &mut caps.ddl_edit,
        Cap::ObjectActions => &mut caps.object_actions,
        Cap::Import => &mut caps.import,
        Cap::Dump => &mut caps.dump,
        Cap::Seed => &mut caps.seed,
        Cap::Explain => &mut caps.explain,
    };
    *flag = false;
    caps
}

/// One representative entry point gated on `cap`.
async fn call_gated(state: &AppState, id: Uuid, cap: Cap) -> Result<(), DbError> {
    let sm = &state.sessions;
    match cap {
        Cap::Designer => sm
            .table_design_open(id, scope(), Some("items".into()))
            .await
            .map(drop),
        Cap::DdlEdit => sm
            .ddl_edit_open(id, scope(), ObjectKind::View, Some("v".into()), None)
            .await
            .map(drop),
        Cap::ObjectActions => sm.object_action_preview(id, truncate()).await.map(drop),
        Cap::Import => import_target_impl(state, id, table()).await.map(drop),
        Cap::Dump => dump_objects_impl(state, id, DB.into(), vec!["public".into()])
            .await
            .map(drop),
        Cap::Seed => sm.seed_target(id, table()).await.map(drop),
        Cap::Explain => query_explain_impl(
            state,
            "tab".into(),
            id,
            Some(DB.into()),
            "SELECT 1".into(),
            0,
            false,
        )
        .await
        .map(drop),
        Cap::DataEdit => data_editable_impl(state, id, table()).await.map(drop),
    }
}

/// With exactly one capability off, its gate refuses with that capability's label.
#[tokio::test]
async fn every_cap_gate_refuses_when_off() {
    let caps = Arc::new(Mutex::new(Capabilities::sql_full(NamespaceModel::DbSchema)));
    let f = fixture(Arc::new(CapsSqlDriver {
        caps: Arc::clone(&caps),
    }))
    .await;
    for cap in Cap::ALL {
        f.state.sessions.close(f.id).await;
        *caps.lock().unwrap() = caps_without(cap);
        f.state.sessions.open(f.id, None).await.unwrap();
        let r = call_gated(&f.state, f.id, cap).await;
        match r {
            Err(DbError::Unsupported { feature, .. }) if feature == cap.label() => {}
            other => panic!(
                "{cap:?}: expected Unsupported {:?}, got {other:?}",
                cap.label()
            ),
        }
    }
}
