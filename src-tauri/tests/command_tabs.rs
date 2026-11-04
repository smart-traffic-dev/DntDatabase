//! Query tabs on a Command connection: the fake Command driver of `dnt_testkit`,
//! registered as Redis (R4a Task 2: planning).

use async_trait::async_trait;
use dnt_core::command::CommandLanguage;
use dnt_core::config::{ConnectionConfig, DriverOptions, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{CancelHandle, Driver, Engine, Introspector, Session};
use dnt_core::error::DbError;
use dnt_core::keyspace::Keyspace;
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{DbKind, EngineFamily, ServerInfo};
use dnt_core::sql::Dialect;
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_query::{StmtKindLabel, Target, Verdict};
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets, SecretChange, SecretUpdate};
use dnt_testkit::fake_command::{FakeCommandDriver, FakeStore};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::query::{
    completion_objects_impl, completion_scope_impl, query_plan_impl, query_plan_statement_impl,
    query_tx_impl, query_tx_mode_impl, statement_ranges_impl,
};
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::query_sessions::{PlanView, TxAction, TxMode};
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The connection's name (the name a Production confirmation of an unknown command asks).
const NAME: &str = "Fake Redis";

/// Sessions opened (with the database each was opened on) and closed.
#[derive(Default)]
struct Opens {
    databases: Mutex<Vec<Option<String>>>,
    closed: AtomicUsize,
}

impl Opens {
    fn opened(&self) -> usize {
        self.databases.lock().unwrap().len()
    }
    fn last_database(&self) -> Option<String> {
        self.databases.lock().unwrap().last().cloned().flatten()
    }
    fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }
}

/// [`FakeCommandDriver`] as Redis, counting the sessions it opens and closes.
struct CountedDriver {
    inner: FakeCommandDriver,
    opens: Arc<Opens>,
}

struct CountedSession {
    inner: Box<dyn Session>,
    opens: Arc<Opens>,
}

#[async_trait]
impl Driver for CountedDriver {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }

    fn command_language(&self) -> Option<Arc<dyn CommandLanguage>> {
        self.inner.command_language()
    }

    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let inner = self.inner.connect(cfg).await?;
        self.opens
            .databases
            .lock()
            .unwrap()
            .push(cfg.database.clone());
        Ok(Box::new(CountedSession {
            inner,
            opens: Arc::clone(&self.opens),
        }))
    }
}

#[async_trait]
impl Session for CountedSession {
    fn server_info(&self) -> &ServerInfo {
        self.inner.server_info()
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
        self.opens.closed.fetch_add(1, Ordering::SeqCst);
        self.inner.close().await;
    }
}

struct Fixture {
    state: AppState,
    id: Uuid,
    store: FakeStore,
    opens: Arc<Opens>,
    _dir: tempfile::TempDir,
}

/// An `AppState` with the counted fake registered as Redis and one saved, open Redis
/// connection on db "0", tagged `env` and read-only as `read_only`.
async fn fixture(env: Option<EnvTag>, read_only: bool) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let dir = tempfile::tempdir().unwrap();
    let inner = FakeCommandDriver::with_kind(DbKind::Redis);
    let store = inner.store();
    let opens = Arc::new(Opens::default());
    let driver: Arc<dyn Driver> = Arc::new(CountedDriver {
        inner,
        opens: Arc::clone(&opens),
    });
    let sessions = SessionManager::new(
        Arc::clone(&repo),
        Arc::clone(&secrets),
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(DbKind::Redis, driver)]),
    );
    let mut cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: NAME.into(),
        port: 6379,
        default_db: Some("0".into()),
        secret_mode: SecretMode::Saved,
        env_tag: env,
        read_only,
        ..sample_config()
    };
    cfg.options.driver = DriverOptions::Redis { db: 0 };
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
        store,
        opens,
        _dir: dir,
    }
}

async fn plan_on(f: &Fixture, db: &str, text: &str) -> Result<PlanView, DbError> {
    query_plan_impl(
        &f.state,
        "tab".into(),
        f.id,
        Some(db.into()),
        text.into(),
        Target::All,
    )
    .await
}

fn verdicts(view: &PlanView) -> Vec<Verdict> {
    view.stmts.iter().map(|s| s.verdict).collect()
}

#[tokio::test]
async fn command_plan_has_labels_and_verdicts() {
    let f = fixture(Some(EnvTag::Production), false).await;
    let view = plan_on(&f, "0", "SET a 1\nGET a\nNUKE").await.unwrap();
    let labels: Vec<_> = view.stmts.iter().map(|s| s.kind).collect();
    assert_eq!(
        labels,
        [
            StmtKindLabel::Write,
            StmtKindLabel::Read,
            StmtKindLabel::Admin
        ]
    );
    assert_eq!(
        verdicts(&view),
        [Verdict::Confirm, Verdict::Allow, Verdict::Confirm]
    );
    assert_eq!(view.stmts[2].confirm_name.as_deref(), Some(NAME));
    assert_eq!(view.stmts[0].confirm_name, None);
    assert_eq!(view.family, EngineFamily::Command);
    assert_eq!(view.dialect, None);
    assert_eq!(view.connection_name, NAME);
    assert_eq!(view.env_tag, Some(EnvTag::Production));
    assert_eq!(view.tx_state, TxState::Idle);
    let ranges: Vec<_> = view.stmts.iter().map(|s| s.range).collect();
    assert_eq!(ranges, [(0, 7), (8, 13), (14, 18)]);
    assert_eq!(view.stmts[1].preview, "GET a");
    for s in &view.stmts {
        assert!(s.unbounded.is_empty() && s.drops.is_empty());
        assert_eq!(s.estimate, None);
        assert!(!s.implicit_commit);
    }
    // Planning sends nothing.
    assert!(f.store.executed().is_empty());
}

#[tokio::test]
async fn command_plan_blocks_on_read_only() {
    let f = fixture(None, true).await;
    let view = plan_on(&f, "0", "SET a 1\nGET a\nFLUSHDB").await.unwrap();
    assert_eq!(
        verdicts(&view),
        [Verdict::Block, Verdict::Allow, Verdict::Block]
    );
    assert!(view.read_only);
    // A command that does not parse is blocked on any connection.
    let f = fixture(None, false).await;
    let view = plan_on(&f, "0", "GET \"open").await.unwrap();
    assert_eq!(verdicts(&view), [Verdict::Block]);
    assert_eq!(view.stmts[0].kind, StmtKindLabel::Other);
}

#[tokio::test]
async fn command_tab_switches_db_with_a_fresh_session() {
    let f = fixture(Some(EnvTag::Production), false).await;
    assert_eq!(f.opens.opened(), 1, "the metadata session");

    let first = plan_on(&f, "0", "FLUSHDB").await.unwrap();
    assert_eq!(f.opens.opened(), 2, "the tab's session");
    assert_eq!(f.opens.last_database().as_deref(), Some("0"));
    // FLUSHDB drops the session's current db: its name is the one to type.
    assert_eq!(first.stmts[0].confirm_name.as_deref(), Some("0"));

    // The same db again: the session is kept.
    plan_on(&f, "0", "GET a").await.unwrap();
    assert_eq!(f.opens.opened(), 2);

    let closed = f.opens.closed();
    let second = plan_on(&f, "2", "FLUSHDB").await.unwrap();
    assert_eq!(f.opens.opened(), 3, "a fresh session on db 2");
    assert_eq!(f.opens.last_database().as_deref(), Some("2"));
    assert_eq!(f.opens.closed(), closed + 1, "the old session is closed");
    assert_eq!(second.stmts[0].confirm_name.as_deref(), Some("2"));
    // No switch command was sent.
    assert!(f.store.executed().is_empty());
}

#[tokio::test]
async fn command_statement_ranges_follow_the_language() {
    let f = fixture(None, false).await;
    // One command per line; blank and `#` lines are skipped; ranges are UTF-16.
    let text = "GET 😀\n\n# note\n  SET b 1\nSELECT 2; GET c";
    let ranges = statement_ranges_impl(&f.state, text, Dialect::MYSQL, Some(f.id));
    assert_eq!(ranges, [(0, 6), (17, 24), (25, 40)]);
    // Without a connection, the dialect splits it.
    let sql = statement_ranges_impl(&f.state, "SELECT 1; SELECT 2", Dialect::MYSQL, None);
    assert_eq!(sql, [(0, 8), (10, 18)]);
    // An unknown connection falls back to the dialect too.
    let unknown = statement_ranges_impl(
        &f.state,
        "SELECT 1; SELECT 2",
        Dialect::MYSQL,
        Some(Uuid::new_v4()),
    );
    assert_eq!(unknown, sql);
}

#[tokio::test]
async fn command_tx_buttons_are_refused() {
    let f = fixture(None, false).await;
    plan_on(&f, "0", "GET a").await.unwrap();
    for action in [TxAction::Commit, TxAction::Rollback] {
        let e = query_tx_impl(&f.state, "tab".into(), action)
            .await
            .unwrap_err();
        assert_eq!(e.kind, "unsupported", "{action:?}");
        assert_eq!(e.detail.as_deref(), Some("transactions"));
    }
    let r = query_tx_mode_impl(&f.state, "tab".into(), TxMode::Manual).await;
    assert!(
        matches!(&r, Err(DbError::Unsupported { feature, .. }) if feature == "transactions"),
        "{r:?}"
    );
    let state = query_tx_mode_impl(&f.state, "tab".into(), TxMode::Auto)
        .await
        .unwrap();
    assert_eq!(state, TxState::Idle);
    assert!(f.store.executed().is_empty());
}

#[tokio::test]
async fn command_completion_is_empty() {
    let f = fixture(None, false).await;
    let scope = completion_scope_impl(&f.state, f.id, "0".into())
        .await
        .unwrap();
    assert_eq!(scope.database, "0");
    assert!(scope.tables.is_empty());
    let objects = completion_objects_impl(&f.state, f.id, "0".into(), None)
        .await
        .unwrap();
    assert!(objects.is_empty());
}

/// Retry / Fetch all of a Command plan: one command, judged again with the current
/// config.
#[tokio::test]
async fn command_plan_statement_is_one_guarded_command() {
    let f = fixture(Some(EnvTag::Production), false).await;
    let view = plan_on(&f, "0", "GET a\nSET a 1\nNUKE").await.unwrap();
    let one = query_plan_statement_impl(&f.state, view.plan_id, 2)
        .await
        .unwrap();
    assert_eq!(one.family, EngineFamily::Command);
    assert_eq!(one.dialect, None);
    assert_eq!(one.stmts.len(), 1);
    assert_eq!(one.stmts[0].index, 2);
    assert_eq!(one.stmts[0].range, view.stmts[2].range);
    assert_eq!(one.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(one.stmts[0].confirm_name.as_deref(), Some(NAME));

    // The config changed since the plan: the statement is judged with the new one.
    let keep = || SecretUpdate {
        db_password: SecretChange::Keep,
        ssh_password: SecretChange::Keep,
        ssh_key_passphrase: SecretChange::Keep,
    };
    let mut cfg = f.state.repo.get(f.id).unwrap();
    cfg.read_only = true;
    f.state.repo.update(cfg, keep()).unwrap();
    let view = plan_on(&f, "0", "GET a\nSET a 1").await.unwrap();
    let mut cfg = f.state.repo.get(f.id).unwrap();
    cfg.read_only = false;
    cfg.env_tag = None;
    f.state.repo.update(cfg, keep()).unwrap();
    let one = query_plan_statement_impl(&f.state, view.plan_id, 1)
        .await
        .unwrap();
    assert_eq!(view.stmts[1].verdict, Verdict::Block);
    assert_eq!(one.stmts[0].verdict, Verdict::Allow);
    assert!(f.store.executed().is_empty());
}
