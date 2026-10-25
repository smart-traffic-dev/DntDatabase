//! Restore plans and jobs against an in-process fake driver (every statement recorded,
//! failures and delays programmed by statement text, `SET sql_mode` changes the session
//! dialect) and a fake tool spawner (no real process).
use super::*;
use crate::backup::ext_tools::{ReadPipe, ToolChild, WritePipe};
use crate::jobs::{JobDetail, JobState};
use async_trait::async_trait;
use dnt_core::config::ConnectionConfig;
use dnt_core::driver::{CancelHandle, Driver, Introspector};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{ServerInfo, ServerVariant};
use dnt_core::sql::{MySqlMode, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_store::db::open_in_memory;
use dnt_store::history::HistoryFilter;
use dnt_store::repo::{NewSecrets, SecretChange, SecretUpdate};
use dnt_transfer::tools::Invocation;
use dnt_tunnel::KnownHosts;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{watch, Notify};

// ---------------------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------------------

struct Fake {
    server: ServerInfo,
    seen: Mutex<Vec<String>>,
    /// A statement containing the text fails with the error.
    fails: Mutex<Vec<(String, DbError)>>,
    /// A statement containing the text takes this long.
    delay: Mutex<Option<(String, Duration)>>,
    mode: Mutex<MySqlMode>,
}

impl Fake {
    fn mark(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    /// What was sent since `mark`, without the SchemaCache's background metadata read.
    fn since(&self, mark: usize) -> Vec<String> {
        self.seen.lock().unwrap()[mark..]
            .iter()
            .filter(|s| !s.starts_with("SELECT unnest"))
            .cloned()
            .collect()
    }
}

struct FakeDriver(Arc<Fake>);

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DbKind {
        self.0.server.kind
    }
    async fn connect(
        &self,
        _cfg: &dnt_core::config::ResolvedConfig,
    ) -> Result<Box<dyn Session>, DbError> {
        Ok(Box::new(FakeSession {
            fake: Arc::clone(&self.0),
            tx: Mutex::new(TxState::Idle),
        }))
    }
}

struct FakeSession {
    fake: Arc<Fake>,
    tx: Mutex<TxState>,
}

struct NoCancel;

#[async_trait]
impl Canceller for NoCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        Ok(())
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.fake.server
    }
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        if opts.stop.is_stopped() {
            return Err(stopped());
        }
        self.fake.seen.lock().unwrap().push(stmt.to_string());
        let delay = self.fake.delay.lock().unwrap().clone();
        if let Some((_, d)) = delay.filter(|(t, _)| stmt.contains(t.as_str())) {
            tokio::time::sleep(d).await;
        }
        let fail = self
            .fake
            .fails
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| stmt.contains(t.as_str()))
            .map(|(_, e)| e.clone());
        if let Some(e) = fail {
            return Err(e);
        }
        let upper = stmt.to_ascii_uppercase();
        if upper.starts_with("BEGIN") || upper.starts_with("START TRANSACTION") {
            *self.tx.lock().unwrap() = TxState::Active;
        } else if upper == "COMMIT" || upper == "ROLLBACK" {
            *self.tx.lock().unwrap() = TxState::Idle;
        }
        if let Some(m) = upper
            .strip_prefix("SET SQL_MODE = '")
            .and_then(|r| r.strip_suffix('\''))
        {
            *self.fake.mode.lock().unwrap() = dnt_transfer::detect::parse_sql_mode(m);
        }
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
        *self.tx.lock().unwrap()
    }
    fn dialect(&self) -> Dialect {
        match self.fake.server.kind {
            DbKind::MySql => Dialect::MySql(*self.fake.mode.lock().unwrap()),
            DbKind::Postgres => Dialect::Postgres,
        }
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoCancel)
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
            message: "not used".into(),
        })
    }
    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------------------
// Fake tools
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct ToolSeen {
    spawned: AtomicUsize,
    args: Mutex<Vec<Vec<String>>>,
    stdin: Mutex<Vec<u8>>,
    secret_at_spawn: Mutex<Option<String>>,
}

struct FakeSpawner {
    seen: Arc<ToolSeen>,
    /// stdout of `--list`.
    list: Vec<u8>,
    code: i32,
}

struct FakeChild {
    stdin: Option<DuplexStream>,
    stdout: Option<DuplexStream>,
    stderr: Option<DuplexStream>,
    exit: watch::Receiver<Option<i32>>,
    kill: Arc<Notify>,
}

impl Spawner for FakeSpawner {
    fn spawn(&self, inv: &Invocation) -> std::io::Result<Box<dyn ToolChild>> {
        self.seen.spawned.fetch_add(1, Ordering::SeqCst);
        let args: Vec<String> = inv
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if let dnt_transfer::tools::SecretFileSpec::PgPass(p) = &inv.secret {
            *self.seen.secret_at_spawn.lock().unwrap() = std::fs::read_to_string(p).ok();
        }
        let listing = args.first().map(String::as_str) == Some("--list");
        self.seen.args.lock().unwrap().push(args);
        let (stdin_parent, mut stdin_child) = tokio::io::duplex(64 * 1024);
        let (mut stdout_child, stdout_parent) = tokio::io::duplex(64 * 1024);
        let (stderr_child, stderr_parent) = tokio::io::duplex(1024);
        let (exit_tx, exit_rx) = watch::channel(None);
        let kill = Arc::new(Notify::new());
        let killed = Arc::clone(&kill);
        let seen = Arc::clone(&self.seen);
        let list = if listing {
            self.list.clone()
        } else {
            Vec::new()
        };
        let code = self.code;
        let read_stdin = inv.stdin_file;
        tokio::spawn(async move {
            let work = async {
                stdout_child.write_all(&list).await.unwrap();
                if read_stdin {
                    let mut buf = Vec::new();
                    stdin_child.read_to_end(&mut buf).await.unwrap();
                    seen.stdin.lock().unwrap().extend(buf);
                }
            };
            let code = tokio::select! {
                _ = work => code,
                _ = killed.notified() => 1,
            };
            drop(stdout_child);
            drop(stderr_child);
            drop(stdin_child);
            let _ = exit_tx.send(Some(code));
        });
        Ok(Box::new(FakeChild {
            stdin: Some(stdin_parent),
            stdout: Some(stdout_parent),
            stderr: Some(stderr_parent),
            exit: exit_rx,
            kill,
        }))
    }
}

#[async_trait]
impl ToolChild for FakeChild {
    fn take_stdin(&mut self) -> Option<WritePipe> {
        self.stdin.take().map(|p| Box::pin(p) as WritePipe)
    }
    fn take_stdout(&mut self) -> Option<ReadPipe> {
        self.stdout.take().map(|p| Box::pin(p) as ReadPipe)
    }
    fn take_stderr(&mut self) -> Option<ReadPipe> {
        self.stderr.take().map(|p| Box::pin(p) as ReadPipe)
    }
    async fn wait(&mut self) -> std::io::Result<Option<i32>> {
        Ok(*self.exit.wait_for(Option::is_some).await.unwrap())
    }
    async fn kill(&mut self) -> std::io::Result<()> {
        self.kill.notify_one();
        Ok(())
    }
}

fn tool_info(tool: Tool) -> ToolInfo {
    ToolInfo {
        tool,
        path: format!(r"C:\pg\bin\{}.exe", tool.name()),
        version: format!("{} (PostgreSQL) 17.0", tool.name()),
        mariadb: false,
    }
}

// ---------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------

const PASSWORD: &str = "Sup3r$ecret!";
const DB: &str = "shop";
const PG_SQL: &str = "SET client_encoding = 'UTF8';\nCREATE TABLE public.t (id integer);\n\
                      INSERT INTO public.t VALUES (1);\n";

struct Fx {
    sm: Arc<SessionManager>,
    fake: Arc<Fake>,
    repo: Arc<ConnectionRepo>,
    conn: Uuid,
    tools: Arc<ToolSeen>,
    dir: tempfile::TempDir,
}

async fn fixture(kind: DbKind, env: Option<EnvTag>, read_only: bool, tools: &[Tool]) -> Fx {
    let server = match kind {
        DbKind::MySql => ServerInfo {
            kind,
            variant: ServerVariant::MySql8,
            version: (8, 0, 36),
            version_string: "8.0.36".into(),
        },
        DbKind::Postgres => ServerInfo {
            kind,
            variant: ServerVariant::Postgres,
            version: (17, 0, 0),
            version_string: "17.0".into(),
        },
    };
    let fake = Arc::new(Fake {
        server,
        seen: Mutex::new(Vec::new()),
        fails: Mutex::new(Vec::new()),
        delay: Mutex::new(None),
        mode: Mutex::new(MySqlMode::default()),
    });
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver(Arc::clone(&fake)));
    let dir = tempfile::tempdir().unwrap();
    let sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(kind, driver)]),
    );
    sm.jobs.set_tmp_root(dir.path().join("job-tmp"));
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind,
        default_db: None,
        env_tag: env,
        read_only,
        ..sample_config()
    };
    let conn = repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(secrecy::SecretString::from(PASSWORD.to_string())),
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    let seen = Arc::new(ToolSeen::default());
    *sm.restore_plans.fixed_tools.lock().unwrap() = Some(Arc::new(FixedTools {
        tools: tools.iter().map(|t| tool_info(*t)).collect(),
        spawner: Arc::new(FakeSpawner {
            seen: Arc::clone(&seen),
            list:
                b";\n; Archive created at 2026-10-25\n;\n215; 1259 16390 TABLE public t postgres\n"
                    .to_vec(),
            code: 0,
        }),
    }));
    let sm = Arc::new(sm);
    sm.open(conn, None).await.unwrap();
    Fx {
        sm,
        fake,
        repo,
        conn,
        tools: seen,
        dir,
    }
}

impl Fx {
    fn file(&self, name: &str, content: &[u8]) -> String {
        let p = self.dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn request(&self, path: &str) -> RestoreRequest {
        RestoreRequest {
            connection_id: self.conn,
            database: DB.into(),
            path: path.into(),
            single_transaction: true,
            clean: false,
            no_owner: true,
        }
    }

    async fn wait(&self, id: Uuid) -> JobDetail {
        assert!(self.sm.jobs.wait(id, Duration::from_secs(10)).await);
        self.sm.jobs.detail(id).unwrap()
    }

    /// Preview + start with the confirmation; the finished job.
    async fn restore(&self, path: &str) -> (RestorePreview, JobDetail) {
        let preview = self.sm.restore_preview(self.request(path)).await.unwrap();
        let id = self
            .sm
            .restore_start(preview.plan_id, true, Some(DB.into()))
            .await
            .unwrap();
        (preview, self.wait(id).await)
    }

    fn set_read_only(&self, read_only: bool) {
        let mut cfg = self.repo.get(self.conn).unwrap();
        cfg.read_only = read_only;
        self.repo
            .update(
                cfg,
                SecretUpdate {
                    db_password: SecretChange::Keep,
                    ssh_password: SecretChange::Keep,
                    ssh_key_passphrase: SecretChange::Keep,
                },
            )
            .unwrap();
    }

    fn restore_history(&self) -> Vec<dnt_store::history::HistoryEntry> {
        self.repo
            .history_list(&HistoryFilter {
                connection_id: Some(self.conn),
                search: None,
                origin: Some(HistoryOrigin::Restore),
                limit: 100,
            })
            .unwrap()
    }

    fn spawned(&self) -> usize {
        self.tools.spawned.load(Ordering::SeqCst)
    }
}

fn copy_file() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/p5/pg_dump_plain_copy.sql"),
    )
    .unwrap()
}

fn archive() -> Vec<u8> {
    let mut b = b"PGDMP\x01\x0e\x00".to_vec();
    b.extend(vec![1u8; 2000]);
    b
}

fn log_text(d: &JobDetail) -> String {
    d.log
        .iter()
        .map(|l| l.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

// ---------------------------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------------------------

/// Read-only connections: the preview refuses all three paths before reading anything
/// from the server or spawning a tool, and a connection made read-only after the preview
/// refuses the start.
#[tokio::test(flavor = "multi_thread")]
async fn read_only_blocks_every_restore_path() {
    let all = [Tool::Psql, Tool::PgRestore];
    let fx = fixture(DbKind::Postgres, None, true, &all).await;
    let files = [
        fx.file("plain.sql", PG_SQL.as_bytes()),
        fx.file("copy.sql", &copy_file()),
        fx.file("archive.dump", &archive()),
    ];
    let mark = fx.fake.mark();
    for f in &files {
        let e = fx.sm.restore_preview(fx.request(f)).await.unwrap_err();
        assert!(matches!(e, DbError::Blocked { .. }), "{f}: {e:?}");
    }
    assert!(fx.fake.since(mark).is_empty());
    assert_eq!(fx.spawned(), 0);

    let fx = fixture(DbKind::Postgres, None, false, &all).await;
    let files = [
        (
            fx.file("plain.sql", PG_SQL.as_bytes()),
            RestorePath::AppRunner,
        ),
        (fx.file("copy.sql", &copy_file()), RestorePath::Psql),
        (fx.file("archive.dump", &archive()), RestorePath::PgRestore),
    ];
    for (f, want) in &files {
        let preview = fx.sm.restore_preview(fx.request(f)).await.unwrap();
        assert_eq!(preview.path, *want, "{f}");
        let spawned = fx.spawned();
        let mark = fx.fake.mark();
        fx.set_read_only(true);
        let e = fx
            .sm
            .restore_start(preview.plan_id, true, None)
            .await
            .unwrap_err();
        assert!(matches!(e, DbError::Blocked { .. }), "{f}: {e:?}");
        assert!(fx.fake.since(mark).is_empty(), "{f}");
        assert_eq!(fx.spawned(), spawned, "{f}: nothing spawned");
        fx.set_read_only(false);
    }
    assert!(fx.restore_history().is_empty());
}

/// A queued job whose connection became read-only sends nothing.
#[tokio::test(flavor = "multi_thread")]
async fn read_only_switch_mid_run_blocks_the_next_statement() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let f = fx.file(
        "my.sql",
        b"CREATE TABLE t (id int);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\n",
    );
    *fx.fake.delay.lock().unwrap() = Some(("CREATE TABLE".into(), Duration::from_millis(400)));
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    fx.set_read_only(true);
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Failed, "{:?}", d.job);
    assert_eq!(d.job.error.as_ref().unwrap().kind, "blocked");
    assert_eq!(fx.fake.since(mark), ["CREATE TABLE t (id int)"]);
    assert_eq!(fx.restore_history().len(), 1);
}

/// The guard judges each statement as a one-statement plan of its own text: a read-only
/// context blocks a write, and a text holding two statements is not re-split.
#[test]
fn block_verdict_stops_before_sending() {
    let d = Dialect::MySql(MySqlMode::default());
    let mut plan = one_statement_plan("INSERT INTO t VALUES (1)", d);
    guard_with(&mut plan, guard_ctx(None, true));
    assert_eq!(plan.stmts.len(), 1);
    assert_eq!(plan.stmts[0].verdict, Verdict::Block);
    let text = "SELECT 1; DELETE FROM t";
    let mut plan = one_statement_plan(text, d);
    guard_with(&mut plan, guard_ctx(None, true));
    assert_eq!(plan.stmts.len(), 1);
    assert_eq!(plan.stmts[0].text, text);
    assert_eq!(plan.stmts[0].verdict, Verdict::Block);
    let mut plan = one_statement_plan("SELECT 1", d);
    guard_with(&mut plan, guard_ctx(Some(EnvTag::Production), false));
    assert_eq!(plan.stmts[0].verdict, Verdict::Allow);
    let mut plan = one_statement_plan("DROP TABLE t", d);
    guard_with(&mut plan, guard_ctx(Some(EnvTag::Production), false));
    assert_eq!(plan.stmts[0].verdict, Verdict::Confirm);
}

/// The job stops at a `Block` before sending: the read-only check of the job runs before
/// the guard and the guard itself is fed the connection's read-only flag.
#[tokio::test(flavor = "multi_thread")]
async fn block_verdict_job_sends_nothing_more() {
    let fx = fixture(DbKind::Postgres, None, false, &[]).await;
    let f = fx.file("plain.sql", PG_SQL.as_bytes());
    *fx.fake.delay.lock().unwrap() = Some(("BEGIN".into(), Duration::from_millis(300)));
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    fx.set_read_only(true);
    let d = fx.wait(id).await;
    assert_eq!(d.job.error.as_ref().unwrap().kind, "blocked", "{:?}", d.job);
    assert_eq!(fx.fake.since(mark), ["BEGIN", "ROLLBACK"]);
    assert!(d.job.summary.unwrap().contains("rolled back"));
}

#[tokio::test(flavor = "multi_thread")]
async fn production_requires_database_name() {
    let fx = fixture(DbKind::MySql, Some(EnvTag::Production), false, &[]).await;
    let f = fx.file("my.sql", b"CREATE TABLE t (id int);\n");
    for (confirmed, name) in [
        (true, None),
        (true, Some("Shop")),
        (true, Some("shop ")),
        (false, Some(DB)),
    ] {
        let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
        assert_eq!(preview.confirm, ConfirmLevel::TypeName { name: DB.into() });
        let mark = fx.fake.mark();
        let e = fx
            .sm
            .restore_start(preview.plan_id, confirmed, name.map(str::to_string))
            .await
            .unwrap_err();
        assert!(
            matches!(e, DbError::ConfirmationRequired { .. }),
            "{name:?}: {e:?}"
        );
        assert!(fx.fake.since(mark).is_empty());
    }
    let (_, d) = fx.restore(&f).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);

    let fx = fixture(DbKind::MySql, Some(EnvTag::Staging), false, &[]).await;
    let f = fx.file("my.sql", b"CREATE TABLE t (id int);\n");
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    assert_eq!(preview.confirm, ConfirmLevel::Click);
    let e = fx
        .sm
        .restore_start(preview.plan_id, false, None)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::ConfirmationRequired { .. }));
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    assert_eq!(fx.wait(id).await.job.state, JobState::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_changed_file_is_stale() {
    let fx = fixture(DbKind::Postgres, None, false, &[Tool::Psql]).await;
    for (name, content) in [
        ("plain.sql", PG_SQL.as_bytes().to_vec()),
        ("copy.sql", copy_file()),
    ] {
        let f = fx.file(name, &content);
        let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
        let mut changed = content.clone();
        changed.extend_from_slice(b"SELECT 1;\n");
        std::fs::write(&f, &changed).unwrap();
        let mark = fx.fake.mark();
        let spawned = fx.spawned();
        let e = fx
            .sm
            .restore_start(preview.plan_id, true, None)
            .await
            .unwrap_err();
        assert!(matches!(e, DbError::PlanStale { .. }), "{name}: {e:?}");
        assert!(fx.fake.since(mark).is_empty(), "{name}: zero statements");
        assert_eq!(fx.spawned(), spawned, "{name}: nothing spawned");
    }
}

/// A pg_dump plain file with `COPY … FROM stdin` needs psql: without psql the preview
/// refuses with the reason, nothing is executed and nothing is spawned; with psql it runs
/// through `psql -f -` with the file on stdin (decompressed when gzip).
#[tokio::test(flavor = "multi_thread")]
async fn copy_file_needs_psql() {
    let fx = fixture(DbKind::Postgres, None, false, &[]).await;
    let f = fx.file("copy.sql", &copy_file());
    let mark = fx.fake.mark();
    let e = fx.sm.restore_preview(fx.request(&f)).await.unwrap_err();
    match &e {
        DbError::Unsupported { feature, .. } => assert_eq!(feature, PSQL_MISSING),
        e => panic!("{e:?}"),
    }
    let app = AppError::from(e);
    assert_eq!(app.kind, "unsupported");
    assert!(fx.fake.since(mark).is_empty(), "nothing executed");
    assert_eq!(fx.spawned(), 0, "nothing spawned");
    // An archive without pg_restore: likewise.
    let a = fx.file("a.dump", &archive());
    let e = fx.sm.restore_preview(fx.request(&a)).await.unwrap_err();
    assert!(
        matches!(&e, DbError::Unsupported { feature, .. } if feature == PG_RESTORE_MISSING),
        "{e:?}"
    );
    assert_eq!(fx.spawned(), 0);

    let fx = fixture(DbKind::Postgres, None, false, &[Tool::Psql]).await;
    let content = copy_file();
    let f = fx.file("copy.sql.gz", &gzip(&content));
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    assert_eq!(preview.path, RestorePath::Psql);
    assert!(preview.compressed);
    assert!(preview.transactional);
    let cmd = preview.command.clone().unwrap();
    assert!(
        cmd.contains("-f -") && cmd.contains("--single-transaction"),
        "{cmd}"
    );
    assert!(cmd.contains("ON_ERROR_STOP=1"), "{cmd}");
    assert!(!cmd.contains(PASSWORD), "{cmd}");
    assert_eq!(preview.stats.as_ref().unwrap().copy_blocks, 1);
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}\n{}", d.job, log_text(&d));
    assert_eq!(d.job.kind, JobKind::ToolRestore);
    assert!(
        fx.fake.since(mark).is_empty(),
        "the app sent nothing itself"
    );
    assert_eq!(fx.spawned(), 1);
    assert_eq!(
        *fx.tools.stdin.lock().unwrap(),
        content,
        "decompressed on stdin"
    );
    let secret = fx.tools.secret_at_spawn.lock().unwrap().clone().unwrap();
    assert!(secret.contains("Sup3r$ecret!"), "the pgpass file holds it");
    for args in fx.tools.args.lock().unwrap().iter() {
        assert!(args.iter().all(|a| !a.contains(PASSWORD)), "{args:?}");
    }
    assert!(!log_text(&d).contains(PASSWORD));
    assert_eq!(fx.restore_history().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_restore_archive_has_toc_and_runs() {
    let fx = fixture(DbKind::Postgres, None, false, &[Tool::PgRestore]).await;
    let f = fx.file("a.dump", &archive());
    let mut req = fx.request(&f);
    req.clean = true;
    let preview = fx.sm.restore_preview(req).await.unwrap();
    assert_eq!(preview.path, RestorePath::PgRestore);
    assert_eq!(preview.toc, ["215; 1259 16390 TABLE public t postgres"]);
    assert!(preview.stats.is_none());
    let cmd = preview.command.clone().unwrap();
    assert!(
        cmd.contains("--clean --if-exists") && cmd.contains("--exit-on-error"),
        "{cmd}"
    );
    assert!(preview.warnings.iter().any(|w| w.contains("--clean")));
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(fx.spawned(), 2, "--list, then the restore");
    // A gzip-wrapped archive is refused (pg_restore reads the file itself).
    let g = fx.file("a.dump.gz", &gzip(&archive()));
    let e = fx.sm.restore_preview(fx.request(&g)).await.unwrap_err();
    assert!(matches!(e, DbError::Config { .. }), "{e:?}");
    // A PostgreSQL archive into MySQL is refused.
    let my = fixture(DbKind::MySql, None, false, &[Tool::PgRestore]).await;
    let a = my.file("a.dump", &archive());
    assert!(my.sm.restore_preview(my.request(&a)).await.is_err());
    assert_eq!(my.spawned(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_failure_outcomes() {
    let fx = fixture(DbKind::Postgres, None, false, &[Tool::Psql]).await;
    *fx.sm.restore_plans.fixed_tools.lock().unwrap() = Some(Arc::new(FixedTools {
        tools: vec![tool_info(Tool::Psql)],
        spawner: Arc::new(FakeSpawner {
            seen: Arc::clone(&fx.tools),
            list: vec![],
            code: 3,
        }),
    }));
    let f = fx.file("copy.sql", &copy_file());
    for (single, unknown) in [(true, false), (false, true)] {
        let mut req = fx.request(&f);
        req.single_transaction = single;
        let preview = fx.sm.restore_preview(req).await.unwrap();
        assert_eq!(preview.transactional, single);
        let id = fx
            .sm
            .restore_start(preview.plan_id, true, None)
            .await
            .unwrap();
        let d = fx.wait(id).await;
        assert_eq!(d.job.state, JobState::Failed);
        assert_eq!(d.job.outcome_unknown, unknown, "{:?}", d.job);
        let s = d.job.summary.unwrap();
        if single {
            assert!(s.contains("rolled back"), "{s}");
        } else {
            assert!(s.contains("partially restored"), "{s}");
        }
    }
    let h = fx.restore_history();
    assert_eq!(h.len(), 2);
    assert!(h.iter().any(|e| e.outcome_unknown));
}

#[tokio::test(flavor = "multi_thread")]
async fn database_statements_are_refused_at_preview() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    for content in [
        "CREATE DATABASE other;\nUSE other;\nCREATE TABLE t (id int);\n",
        "DROP DATABASE shop;\n",
        "USE other;\n",
        "SELECT 1\\G\n",
    ] {
        let f = fx.file("bad.sql", content.as_bytes());
        let mark = fx.fake.mark();
        let e = fx.sm.restore_preview(fx.request(&f)).await.unwrap_err();
        assert!(
            matches!(&e, DbError::Config { field: Some(f), .. } if f == "file"),
            "{content}: {e:?}"
        );
        assert!(fx.fake.since(mark).is_empty());
    }
    let f = fx.file("ok.sql", b"USE shop;\nCREATE TABLE t (id int);\n");
    let (_, d) = fx.restore(&f).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    // A PostgreSQL dump into MySQL.
    let f = fx.file("pg.sql", &copy_file());
    assert!(fx.sm.restore_preview(fx.request(&f)).await.is_err());
}

// ---------------------------------------------------------------------------------------
// App runner
// ---------------------------------------------------------------------------------------

/// A `SET sql_mode` changes how the rest of the file is split (R33): with backslash
/// escapes the two INSERTs would be one statement.
#[tokio::test(flavor = "multi_thread")]
async fn dialect_change_feeds_splitter() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let f = fx.file(
        "mode.sql",
        b"SET sql_mode = 'NO_BACKSLASH_ESCAPES';\nINSERT INTO t VALUES ('a\\');\n\
          INSERT INTO t VALUES ('b');\n",
    );
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    assert_eq!(preview.stats.as_ref().unwrap().statements, 3);
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        fx.fake.since(mark),
        [
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
            "INSERT INTO t VALUES ('a\\')",
            "INSERT INTO t VALUES ('b')"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_single_transaction_wraps_and_hash_mismatch_rolls_back() {
    let fx = fixture(DbKind::Postgres, None, false, &[]).await;
    let f = fx.file("plain.sql", PG_SQL.as_bytes());
    let mark = fx.fake.mark();
    let (preview, d) = fx.restore(&f).await;
    assert!(preview.transactional);
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        fx.fake.since(mark),
        [
            "BEGIN",
            "SET client_encoding = 'UTF8'",
            "CREATE TABLE public.t (id integer)",
            "INSERT INTO public.t VALUES (1)",
            "COMMIT"
        ]
    );
    // The file changes after the start's check (while BEGIN runs): what was read does
    // not hash to the plan's SHA-256, so the transaction is rolled back.
    *fx.fake.delay.lock().unwrap() = Some(("BEGIN".into(), Duration::from_millis(300)));
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&f, PG_SQL.replace("(1)", "(2)")).unwrap();
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Failed, "{:?}", d.job);
    assert_eq!(d.job.error.as_ref().unwrap().kind, "plan_stale");
    let sent = fx.fake.since(mark);
    assert_eq!(sent.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(
        sent.last().map(String::as_str),
        Some("ROLLBACK"),
        "{sent:?}"
    );
    assert!(!sent.iter().any(|s| s == "COMMIT"));
    assert!(!d.job.outcome_unknown);
    assert!(d.job.summary.unwrap().contains("changed while"));
    // A file with its own transaction control runs without the wrapper.
    *fx.fake.delay.lock().unwrap() = None;
    let f = fx.file(
        "tx.sql",
        b"BEGIN;\nCREATE TABLE t (id int);\nCOMMIT;\nVACUUM t;\n",
    );
    let mark = fx.fake.mark();
    let (preview, d) = fx.restore(&f).await;
    assert!(!preview.transactional);
    assert!(preview
        .warnings
        .iter()
        .any(|w| w.contains("partial restore")));
    assert_eq!(d.job.state, JobState::Done);
    assert_eq!(
        fx.fake.since(mark),
        ["BEGIN", "CREATE TABLE t (id int)", "COMMIT", "VACUUM t"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_failure_in_transaction_rolls_back() {
    let fx = fixture(DbKind::Postgres, None, false, &[]).await;
    fx.fake.fails.lock().unwrap().push((
        "INSERT".into(),
        DbError::Server {
            code: None,
            sqlstate: Some("23505".into()),
            message: "duplicate key".into(),
            position: None,
        },
    ));
    let f = fx.file("plain.sql", PG_SQL.as_bytes());
    let mark = fx.fake.mark();
    let (_, d) = fx.restore(&f).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert_eq!(d.job.error.as_ref().unwrap().kind, "server");
    assert_eq!(fx.fake.since(mark).last().unwrap(), "ROLLBACK");
    assert!(d.job.summary.unwrap().contains("nothing was restored"));
    assert_eq!(fx.restore_history().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_restore_not_transactional_warning() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let f = fx.file(
        "my.sql",
        b"-- MySQL dump\nDROP TABLE IF EXISTS t;\nCREATE TABLE t (id int);\n\
          INSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\n",
    );
    fx.fake.fails.lock().unwrap().push((
        "VALUES (2)".into(),
        DbError::Server {
            code: Some("1062".into()),
            sqlstate: None,
            message: "dup".into(),
            position: None,
        },
    ));
    let mark = fx.fake.mark();
    let (preview, d) = fx.restore(&f).await;
    assert!(!preview.transactional);
    assert!(preview
        .warnings
        .iter()
        .any(|w| w.contains("not transactional")));
    assert!(preview.warnings.iter().any(|w| w.contains("drops 1")));
    let sent = fx.fake.since(mark);
    assert!(!sent
        .iter()
        .any(|s| s.starts_with("BEGIN") || s.starts_with("START")));
    assert_eq!(d.job.state, JobState::Failed);
    let s = d.job.summary.unwrap();
    assert!(
        s.contains("3 statements ran") && s.contains("partially restored"),
        "{s}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_is_single_use() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let f = fx.file("my.sql", b"CREATE TABLE t (id int);\n");
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    fx.wait(id).await;
    let e = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::PlanExpired), "{e:?}");
    // A refused start uses the plan up too.
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    assert!(fx
        .sm
        .restore_start(preview.plan_id, false, None)
        .await
        .is_err());
    assert!(matches!(
        fx.sm.restore_start(preview.plan_id, true, None).await,
        Err(DbError::PlanExpired)
    ));
    // Closing the connection drops its plans.
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    fx.sm.close(fx.conn).await;
    assert!(matches!(
        fx.sm.restore_start(preview.plan_id, true, None).await,
        Err(DbError::PlanExpired)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn history_entry_once_per_job() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let f = fx.file(
        "my.sql",
        b"CREATE TABLE t (id int);\nINSERT INTO t VALUES (1);\nINSERT INTO t VALUES (2);\n",
    );
    let (preview, d) = fx.restore(&f).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    let h = fx.restore_history();
    assert_eq!(h.len(), 1);
    let sha = &preview.file.sha256[..12];
    assert!(
        h[0].sql_text.starts_with("-- Restore my.sql ("),
        "{}",
        h[0].sql_text
    );
    assert!(h[0].sql_text.contains(sha));
    assert!(h[0].sql_text.contains("into shop"));
    assert!(h[0].sql_text.contains("3 statements"));
    assert!(
        !h[0].sql_text.contains("INSERT"),
        "no statement of the file"
    );
    // A failed job: one entry too (not one per statement), with the error.
    fx.fake.fails.lock().unwrap().push((
        "VALUES (2)".into(),
        DbError::Server {
            code: Some("1062".into()),
            sqlstate: None,
            message: "dup".into(),
            position: None,
        },
    ));
    let (_, d) = fx.restore(&f).await;
    assert_eq!(d.job.state, JobState::Failed);
    let h = fx.restore_history();
    assert_eq!(h.len(), 2);
    assert_eq!(
        h.iter()
            .filter(|e| e.error_code.as_deref() == Some("1062"))
            .count(),
        1
    );
    // A start refused before any job runs writes none.
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    assert!(fx
        .sm
        .restore_start(preview.plan_id, false, None)
        .await
        .is_err());
    assert_eq!(fx.restore_history().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_the_restore_between_statements() {
    let fx = fixture(DbKind::Postgres, None, false, &[]).await;
    *fx.fake.delay.lock().unwrap() = Some(("CREATE TABLE".into(), Duration::from_millis(400)));
    let f = fx.file("plain.sql", PG_SQL.as_bytes());
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(fx.sm.jobs.cancel(id).await);
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Cancelled, "{:?}", d.job);
    let sent = fx.fake.since(mark);
    assert!(!sent.iter().any(|s| s.starts_with("INSERT")), "{sent:?}");
    assert!(!sent.iter().any(|s| s == "COMMIT"));
    assert_eq!(
        sent.last().map(String::as_str),
        Some("ROLLBACK"),
        "{sent:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_and_bom_files_restore_through_the_app_runner() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let mut text = b"\xEF\xBB\xBF".to_vec();
    text.extend_from_slice(b"CREATE TABLE t (id int);\nINSERT INTO t VALUES (1);\n");
    let f = fx.file("my.sql.gz", &gzip(&text));
    let mark = fx.fake.mark();
    let (preview, d) = fx.restore(&f).await;
    assert!(preview.compressed);
    assert_eq!(preview.path, RestorePath::AppRunner);
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        fx.fake.since(mark),
        ["CREATE TABLE t (id int)", "INSERT INTO t VALUES (1)"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dialect_changed_since_preview_is_stale() {
    let fx = fixture(DbKind::MySql, None, false, &[]).await;
    let f = fx.file("my.sql", b"CREATE TABLE t (id int);\n");
    let preview = fx.sm.restore_preview(fx.request(&f)).await.unwrap();
    fx.fake.mode.lock().unwrap().ansi_quotes = true;
    let e = fx
        .sm
        .restore_start(preview.plan_id, true, None)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
}
