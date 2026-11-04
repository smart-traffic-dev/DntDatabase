//! Query tabs on a Command connection: the fake Command driver of `dnt_testkit`,
//! registered as Redis (R4a Task 2: planning; Task 3: running).

use async_trait::async_trait;
use dnt_core::command::CommandLanguage;
use dnt_core::config::{ConnectionConfig, DriverOptions, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{CancelHandle, Driver, Engine, Introspector, Session};
use dnt_core::error::{AppError, DbError};
use dnt_core::keyspace::Keyspace;
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{DbKind, EngineFamily, ServerInfo};
use dnt_core::sql::Dialect;
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::Value;
use dnt_query::{StmtKindLabel, Target, Verdict};
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryEntry, HistoryFilter, HistoryOrigin};
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
use dntdatabase::run_events::{EventSink, RunOptions};
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use serde_json::Value as Json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

// ---------------------------------------------------------------------------------
// Running (R4a Task 3)

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

    /// Waits for `done`.
    async fn done(&self) -> Json {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(done) = self.of("done").pop() {
                    return done;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no done: {:?}", self.tags()))
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

fn opts() -> RunOptions {
    RunOptions {
        max_rows: 10_000,
        continue_on_error: false,
    }
}

/// Starts plan `id`, confirmed, with `names` typed.
async fn start(f: &Fixture, id: Uuid, names: &[&str]) -> Result<(Uuid, Arc<Events>), DbError> {
    let (emit, events) = collector();
    let names = names.iter().map(|s| s.to_string()).collect();
    let run = f
        .state
        .sessions
        .query_run_named(id, true, names, opts(), emit)
        .await?;
    Ok((run, events))
}

/// Runs plan `id` to its end; the `done` event.
async fn run(f: &Fixture, id: Uuid, names: &[&str]) -> Result<Json, DbError> {
    let (_, events) = start(f, id, names).await?;
    Ok(events.done().await)
}

fn history(f: &Fixture) -> Vec<HistoryEntry> {
    let mut rows = f
        .state
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(f.id),
            search: None,
            origin: None,
            limit: 1000,
        })
        .unwrap();
    rows.sort_by_key(|r| r.id);
    rows
}

#[tokio::test]
async fn command_run_streams_rows_and_history() {
    let f = fixture(None, false).await;
    let view = plan_on(&f, "0", "SET a 1\nGET a").await.unwrap();
    let (_, events) = start(&f, view.plan_id, &[]).await.unwrap();
    let done = events.done().await;
    assert_eq!(done["stop"], Json::Null);
    assert_eq!(
        events.tags(),
        [
            "statement_start",
            "result_start",
            "result_end",
            "statement_start",
            "result_start",
            "result_stored",
            "rows_stored",
            "result_end",
            "done"
        ]
    );
    let starts = events.of("statement_start");
    assert_eq!(
        starts[0],
        serde_json::json!({ "t": "statement_start", "index": 0, "range": [0, 7] })
    );
    assert_eq!(starts[1]["index"], 1);
    assert_eq!(events.of("result_end")[0]["affected"], 1);
    // GET's rows are in the result store.
    let token: Uuid = events.of("result_stored")[0]["token"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let page = f.state.sessions.results().page(token, 0, 10).unwrap();
    assert_eq!(page.rows, [[Value::Text("1".into())]]);
    assert_eq!(f.store.executed(), ["SET", "GET"]);

    let rows = history(&f);
    assert_eq!(rows.len(), 2);
    let kinds: Vec<&str> = rows.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["write", "read"]);
    let texts: Vec<&str> = rows.iter().map(|r| r.sql_text.as_str()).collect();
    assert_eq!(texts, ["SET a 1", "GET a"]);
    for r in &rows {
        assert_eq!(r.origin, HistoryOrigin::Query);
        assert_eq!(r.database.as_deref(), Some("0"));
        assert!(!r.redacted);
    }
}

/// Collects formatted log output.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every log line of this test binary, from every test (installed on first use). A
/// global subscriber, not a scoped one: the interest cache of a callsite that another
/// test (on another thread, without a subscriber) reaches first is global too.
fn captured_logs() -> &'static Captured {
    static LOGS: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        logs
    })
}

/// Review Focus 2: a secret typed in a Command console never reaches the History or
/// the log.
#[tokio::test]
async fn command_history_is_redacted() {
    let logs = captured_logs();

    let f = fixture(Some(EnvTag::Local), false).await;
    let view = plan_on(&f, "0", "CONFIG SET requirepass hunter2")
        .await
        .unwrap();
    let done = run(&f, view.plan_id, &[]).await.unwrap();
    assert_eq!(done["stop"], Json::Null);
    assert_eq!(f.store.executed(), ["CONFIG"]);

    let rows = history(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sql_text, "CONFIG SET requirepass ***");
    assert!(rows[0].redacted);
    assert_eq!(rows[0].kind, "admin");

    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("CONFIG SET requirepass ***"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
}

/// Review Focus 1: on Production, the names typed are checked against what the plan
/// asks for when judged again at run time; a refused run sends nothing.
#[tokio::test]
async fn command_run_rechecks_confirm_names() {
    let f = fixture(Some(EnvTag::Production), false).await;
    let view = plan_on(&f, "0", "FLUSHDB").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].confirm_name.as_deref(), Some("0"));
    let done = run(&f, view.plan_id, &["0"]).await.unwrap();
    assert_eq!(done["stop"], Json::Null);
    assert_eq!(f.store.executed(), ["FLUSHDB"]);

    // A wrong, missing or extra name: the error SQL gives, and nothing is sent.
    let view = plan_on(&f, "0", "FLUSHDB").await.unwrap();
    for typed in [&["1"][..], &[][..], &["0", "1"][..]] {
        let e = run(&f, view.plan_id, typed).await.unwrap_err();
        assert!(
            matches!(e, DbError::ConfirmationRequired { count: 1 }),
            "{typed:?}: {e:?}"
        );
        assert_eq!(AppError::from(e).kind, "confirmation_required");
    }
    // Not confirmed at all.
    let (emit, _) = collector();
    let e = f
        .state
        .sessions
        .query_run(view.plan_id, false, opts(), emit)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::ConfirmationRequired { .. }), "{e:?}");
    assert_eq!(f.store.executed(), ["FLUSHDB"]);

    // The name asked for changed between plan and run (the connection was renamed):
    // the name typed for the plan no longer runs it.
    let view = plan_on(&f, "0", "NUKE").await.unwrap();
    assert_eq!(view.stmts[0].confirm_name.as_deref(), Some(NAME));
    let mut cfg = f.state.repo.get(f.id).unwrap();
    cfg.name = "Renamed".into();
    f.state
        .repo
        .update(
            cfg,
            SecretUpdate {
                db_password: SecretChange::Keep,
                ssh_password: SecretChange::Keep,
                ssh_key_passphrase: SecretChange::Keep,
            },
        )
        .unwrap();
    let e = run(&f, view.plan_id, &[NAME]).await.unwrap_err();
    assert!(
        matches!(e, DbError::ConfirmationRequired { count: 1 }),
        "{e:?}"
    );
    assert_eq!(f.store.executed(), ["FLUSHDB"]);
    // The plan stays available: with the new name it runs (the server refuses NUKE).
    let done = run(&f, view.plan_id, &["Renamed"]).await.unwrap();
    assert_eq!(done["stop"]["kind"], "error");
    assert_eq!(f.store.executed(), ["FLUSHDB", "NUKE"]);

    // The session is on another db than the one the name was typed for (a reconnect
    // landed elsewhere): the run stops before the command is sent.
    let view = plan_on(&f, "0", "FLUSHDB").await.unwrap();
    f.store.force_db("3");
    let done = run(&f, view.plan_id, &["0"]).await.unwrap();
    assert_eq!(done["stop"]["kind"], "database_changed", "{done}");
    assert_eq!(done["stop"]["expected"], "0");
    assert_eq!(done["stop"]["actual"], "3");
    assert_eq!(f.store.executed(), ["FLUSHDB", "NUKE"]);
}

#[tokio::test]
async fn command_run_blocked_plan_is_refused() {
    let f = fixture(None, true).await;
    let view = plan_on(&f, "0", "GET a\nSET a 1").await.unwrap();
    let e = run(&f, view.plan_id, &[]).await.unwrap_err();
    assert!(matches!(e, DbError::Blocked { count: 1 }), "{e:?}");
    assert!(f.store.executed().is_empty());
    assert!(history(&f).is_empty());

    // Read-only switched on after the plan: judged again, blocked.
    let f = fixture(None, false).await;
    let view = plan_on(&f, "0", "SET a 1").await.unwrap();
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    let mut cfg = f.state.repo.get(f.id).unwrap();
    cfg.read_only = true;
    f.state
        .repo
        .update(
            cfg,
            SecretUpdate {
                db_password: SecretChange::Keep,
                ssh_password: SecretChange::Keep,
                ssh_key_passphrase: SecretChange::Keep,
            },
        )
        .unwrap();
    let e = run(&f, view.plan_id, &[]).await.unwrap_err();
    assert!(matches!(e, DbError::Blocked { count: 1 }), "{e:?}");
    assert!(f.store.executed().is_empty());
}

#[tokio::test]
async fn command_run_reports_database_change() {
    let f = fixture(Some(EnvTag::Production), false).await;
    let view = plan_on(&f, "0", "SELECT 2\nSET a 1").await.unwrap();
    let done = run(&f, view.plan_id, &[]).await.unwrap();
    assert_eq!(done["stop"], Json::Null);
    assert!(f.store.get("2", b"a").is_some(), "SET ran on db 2");
    assert!(f.store.get("0", b"a").is_none());
    let rows = history(&f);
    let dbs: Vec<Option<&str>> = rows.iter().map(|r| r.database.as_deref()).collect();
    assert_eq!(dbs, [Some("0"), Some("2")], "SET ran on db 2");

    // The tab is on db 2 now: planning on it keeps the session, and the plan's
    // db-relative targets are db 2's.
    let opened = f.opens.opened();
    let next = plan_on(&f, "2", "FLUSHDB").await.unwrap();
    assert_eq!(f.opens.opened(), opened, "no fresh session");
    assert_eq!(next.stmts[0].confirm_name.as_deref(), Some("2"));
    // The plan made before the switch is gone with it.
    let e = run(&f, view.plan_id, &[]).await.unwrap_err();
    assert!(matches!(e, DbError::PlanExpired), "{e:?}");
}

#[tokio::test]
async fn command_cancel_stops_a_blocked_command() {
    let f = fixture(None, false).await;
    let view = plan_on(&f, "0", "GET a\nGET b").await.unwrap();
    f.store.block_next();
    let (run_id, events) = start(&f, view.plan_id, &[]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), f.store.wait_blocked())
        .await
        .expect("the command blocks");
    f.state.sessions.query_cancel(run_id).await.unwrap();
    let done = events.done().await;
    assert_eq!(done["stop"]["kind"], "cancelled", "{:?}", events.tags());
    assert_eq!(events.of("stopping")[0]["reason"], "user");
    assert!(f.store.executed().is_empty(), "GET b is never sent");
    let rows = history(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].error_kind.as_deref(), Some("cancelled"));
}

/// The test helpers hand out Command plans too.
#[tokio::test]
async fn command_plans_are_taken_and_cut_by_the_test_helpers() {
    let f = fixture(None, false).await;
    let view = plan_on(&f, "0", "GET a\nSET a 1").await.unwrap();
    let (_, _, one) = f
        .state
        .sessions
        .test_plan_statement(view.plan_id, 1)
        .unwrap();
    let one = one.into_command().unwrap();
    assert_eq!(one.stmts.len(), 1);
    assert_eq!(one.stmts[0].text, "SET a 1");
    let (tab, conn, taken) = f.state.sessions.test_take_plan(view.plan_id).unwrap();
    assert_eq!((tab.as_str(), conn), ("tab", f.id));
    let taken = taken.into_command().unwrap();
    assert!(taken.guard.is_some());
    assert_eq!(taken.stmts.len(), 2);
}
