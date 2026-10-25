//! Job commands and job settings, called through their `*_impl` functions with a test
//! `AppState` (no Tauri runtime). Fake runners are submitted through
//! `state.sessions.jobs`.

use async_trait::async_trait;
use dnt_core::error::{AppError, DbError};
use dnt_core::secret::MemorySecretStore;
use dntdatabase::commands::jobs::{
    job_active_count_impl, job_cancel_impl, job_clear_history_impl, job_detail_impl, job_list_impl,
    job_result_path_impl, job_stop_all_impl, DEFAULT_JOB_LIST_LIMIT, MAX_JOB_LIST_LIMIT,
};
use dntdatabase::commands::settings::{
    get_external_tools_dir_impl, get_jobs_per_connection_impl, set_external_tools_dir_impl,
    set_jobs_per_connection_impl,
};
use dntdatabase::jobs::{
    JobCtx, JobDone, JobFailure, JobKind, JobLogLevel, JobRunner, JobSpec, JobState,
    JOBS_PER_CONNECTION_KEY,
};
use dntdatabase::state::{AppPaths, AppState};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn app_state(dir: &std::path::Path) -> AppState {
    let paths = AppPaths::in_dir(dir.join("data"));
    AppState::open(paths, Arc::new(MemorySecretStore::default())).unwrap()
}

fn spec(title: &str) -> JobSpec {
    JobSpec {
        kind: JobKind::Dump,
        connection_id: None,
        title: title.into(),
        params: None,
        rerunnable: false,
    }
}

/// Logs a line, then works until it is stopped.
struct UntilStopped;

#[async_trait]
impl JobRunner for UntilStopped {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        ctx.log(JobLogLevel::Info, "Started");
        while !ctx.is_stopped() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Err(DbError::cancelled().into())
    }
}

/// Finishes at once with `result_path`.
struct DoneWith(Option<String>);

#[async_trait]
impl JobRunner for DoneWith {
    async fn run(self: Box<Self>, _ctx: JobCtx) -> Result<JobDone, JobFailure> {
        Ok(JobDone {
            summary: "Finished".into(),
            result_path: self.0,
        })
    }
}

async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not reached");
}

fn field(e: DbError) -> Option<String> {
    AppError::from(e).field
}

#[tokio::test]
async fn jobs_per_connection_validates_range() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    assert_eq!(get_jobs_per_connection_impl(&state).await.unwrap(), 2);
    for bad in [0, 9] {
        let err = set_jobs_per_connection_impl(&state, bad).await.unwrap_err();
        assert_eq!(field(err).as_deref(), Some("n"));
    }
    assert_eq!(get_jobs_per_connection_impl(&state).await.unwrap(), 2);
    set_jobs_per_connection_impl(&state, 4).await.unwrap();
    assert_eq!(get_jobs_per_connection_impl(&state).await.unwrap(), 4);
    // The job manager reads the same key.
    assert_eq!(
        state
            .repo
            .app_setting(JOBS_PER_CONNECTION_KEY)
            .unwrap()
            .as_deref(),
        Some("4")
    );
}

#[tokio::test]
async fn external_tools_dir_must_be_absolute_existing() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    assert_eq!(get_external_tools_dir_impl(&state).await.unwrap(), None);

    let relative = set_external_tools_dir_impl(&state, Some("tools".into()))
        .await
        .unwrap_err();
    assert_eq!(field(relative).as_deref(), Some("dir"));
    let missing = dir.path().join("missing");
    let err = set_external_tools_dir_impl(&state, Some(missing.to_string_lossy().into()))
        .await
        .unwrap_err();
    assert_eq!(field(err).as_deref(), Some("dir"));
    let file = dir.path().join("file.txt");
    std::fs::write(&file, b"x").unwrap();
    let err = set_external_tools_dir_impl(&state, Some(file.to_string_lossy().into()))
        .await
        .unwrap_err();
    assert_eq!(field(err).as_deref(), Some("dir"));
    assert_eq!(get_external_tools_dir_impl(&state).await.unwrap(), None);

    let tools = dir.path().join("bin");
    std::fs::create_dir(&tools).unwrap();
    let tools = tools.to_string_lossy().into_owned();
    set_external_tools_dir_impl(&state, Some(tools.clone()))
        .await
        .unwrap();
    assert_eq!(
        get_external_tools_dir_impl(&state).await.unwrap(),
        Some(tools)
    );
    // Blank and none clear it.
    set_external_tools_dir_impl(&state, Some("  ".into()))
        .await
        .unwrap();
    assert_eq!(get_external_tools_dir_impl(&state).await.unwrap(), None);
    set_external_tools_dir_impl(&state, None).await.unwrap();
    assert_eq!(get_external_tools_dir_impl(&state).await.unwrap(), None);
}

#[tokio::test]
async fn job_list_detail_cancel_via_impls() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let jobs = state.sessions.jobs.clone();
    let id = jobs
        .submit(spec("Dump shop"), Box::new(UntilStopped))
        .unwrap();
    wait_for(|| {
        jobs.detail(id)
            .is_ok_and(|d| d.job.state == JobState::Running && !d.log.is_empty())
    })
    .await;
    assert_eq!(job_active_count_impl(&state).await.unwrap(), 1);

    let listed = job_list_impl(&state, None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].title, "Dump shop");
    let detail = job_detail_impl(&state, id).await.unwrap();
    assert_eq!(detail.log[0].message, "Started");

    assert!(job_cancel_impl(&state, id).await.unwrap());
    assert!(jobs.wait(id, Duration::from_secs(5)).await);
    assert!(!job_cancel_impl(&state, id).await.unwrap());
    assert_eq!(job_active_count_impl(&state).await.unwrap(), 0);
    let detail = job_detail_impl(&state, id).await.unwrap();
    assert_eq!(detail.job.state, JobState::Cancelled);
    assert!(!job_cancel_impl(&state, Uuid::new_v4()).await.unwrap());
    assert!(job_detail_impl(&state, Uuid::new_v4()).await.is_err());

    job_clear_history_impl(&state).await.unwrap();
    assert!(job_list_impl(&state, Some(10)).await.unwrap().is_empty());
}

#[tokio::test]
async fn job_list_limit_is_defaulted_and_capped() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let jobs = state.sessions.jobs.clone();
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            jobs.submit(spec(&format!("Job {i}")), Box::new(DoneWith(None)))
                .unwrap(),
        );
    }
    for id in &ids {
        assert!(jobs.wait(*id, Duration::from_secs(5)).await);
    }
    assert_eq!(job_list_impl(&state, Some(2)).await.unwrap().len(), 2);
    assert_eq!(job_list_impl(&state, None).await.unwrap().len(), 3);
    assert_eq!(job_list_impl(&state, Some(0)).await.unwrap().len(), 0);
    assert_eq!(job_list_impl(&state, Some(100_000)).await.unwrap().len(), 3);
    assert_eq!(DEFAULT_JOB_LIST_LIMIT, 200);
    assert_eq!(MAX_JOB_LIST_LIMIT, 500);
}

#[tokio::test]
async fn reveal_refuses_unknown_job_or_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let jobs = state.sessions.jobs.clone();

    assert!(job_result_path_impl(&state, Uuid::new_v4()).await.is_err());

    let none = jobs
        .submit(spec("No file"), Box::new(DoneWith(None)))
        .unwrap();
    assert!(jobs.wait(none, Duration::from_secs(5)).await);
    let err = job_result_path_impl(&state, none).await.unwrap_err();
    assert_eq!(field(err).as_deref(), Some("id"));

    let gone = dir.path().join("gone.sql");
    let missing = jobs
        .submit(
            spec("Missing file"),
            Box::new(DoneWith(Some(gone.to_string_lossy().into()))),
        )
        .unwrap();
    assert!(jobs.wait(missing, Duration::from_secs(5)).await);
    let err = job_result_path_impl(&state, missing).await.unwrap_err();
    assert_eq!(field(err).as_deref(), Some("id"));

    let file = dir.path().join("dump.sql");
    std::fs::write(&file, b"SELECT 1;").unwrap();
    let ok = jobs
        .submit(
            spec("With file"),
            Box::new(DoneWith(Some(file.to_string_lossy().into()))),
        )
        .unwrap();
    assert!(jobs.wait(ok, Duration::from_secs(5)).await);
    assert_eq!(job_result_path_impl(&state, ok).await.unwrap(), file);
}

#[tokio::test]
async fn stop_all_stops_every_job() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let jobs = state.sessions.jobs.clone();
    let a = jobs.submit(spec("A"), Box::new(UntilStopped)).unwrap();
    let b = jobs.submit(spec("B"), Box::new(UntilStopped)).unwrap();
    wait_for(|| job_count(&state) == 2).await;
    job_stop_all_impl(&state).await.unwrap();
    assert_eq!(job_active_count_impl(&state).await.unwrap(), 0);
    for id in [a, b] {
        assert_eq!(
            job_detail_impl(&state, id).await.unwrap().job.state,
            JobState::Cancelled
        );
    }
}

fn job_count(state: &AppState) -> u32 {
    state.sessions.jobs.active_count()
}

#[tokio::test]
async fn startup_recovers_interrupted_jobs_and_cleans_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let state = app_state(dir.path());
        let id = state
            .sessions
            .jobs
            .submit(spec("Interrupted"), Box::new(UntilStopped))
            .unwrap();
        state.sessions.jobs.stored(id).await;
        wait_for(|| {
            state
                .sessions
                .jobs
                .detail(id)
                .is_ok_and(|d| d.job.state == JobState::Running)
        })
        .await;
        // Give the "running" row time to reach the store.
        tokio::time::sleep(Duration::from_millis(100)).await;
        id
    };
    let leftover = dir.path().join("data").join("job-tmp").join("old");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("secret.cnf"), b"x").unwrap();

    let state = app_state(dir.path());
    assert!(!leftover.exists());
    let detail = job_detail_impl(&state, id).await.unwrap();
    assert_eq!(detail.job.state, JobState::Failed);
    assert!(detail.job.outcome_unknown);
}

/// Tracked streaming exports (R3) against an in-process fake driver: Stop from the Jobs
/// panel reaches each export's own cancel path.
mod tracked_exports {
    use super::*;
    use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode};
    use dnt_core::driver::{
        CancelHandle, Canceller, CellMode, Driver, ExecOptions, Introspector, ResultSummary,
        RowSink, Session, StmtOutcome,
    };
    use dnt_core::meta::{
        DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
        TableDetail,
    };
    use dnt_core::secret::SecretStore;
    use dnt_core::server::{detect_mysql, DbKind, ServerInfo};
    use dnt_core::sql::{Dialect, StatementKind};
    use dnt_core::test_util::sample_config;
    use dnt_core::tx::TxState;
    use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
    use dnt_query::Target as Pick;
    use dnt_store::repo::{ConnectionRepo, NewSecrets};
    use dnt_tunnel::KnownHosts;
    use dntdatabase::commands::export::{export_query_impl, export_table_impl, Progress};
    use dntdatabase::commands::query::query_plan_impl;
    use dntdatabase::export_format::{ExportFormat, ExportOptions};
    use dntdatabase::export_paths::ExportPaths;
    use dntdatabase::file_picks::FilePicks;
    use dntdatabase::session_manager::SessionManager;
    use secrecy::SecretString;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// What the fake server saw.
    #[derive(Default)]
    struct Log {
        /// Calls of a session's cancel handle.
        cancel_calls: AtomicUsize,
        /// Set by a cancel handle; cleared when a statement starts.
        cancelled: AtomicBool,
        /// Statements that mention `slow` and started streaming.
        slow_started: AtomicUsize,
    }

    struct FakeDriver(Arc<Log>);

    #[async_trait]
    impl Driver for FakeDriver {
        fn kind(&self) -> DbKind {
            DbKind::MySql
        }
        async fn connect(&self, _cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
            Ok(Box::new(FakeSession {
                info: detect_mysql("8.0.36").unwrap(),
                log: Arc::clone(&self.0),
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
            self.0.cancel_calls.fetch_add(1, Ordering::SeqCst);
            self.0.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn column() -> ColumnMeta {
        ColumnMeta {
            name: "n".into(),
            db_type: "INT".into(),
            logical: LogicalType::Integer,
            nullable: Some(false),
            origin_table: None,
            origin_column: None,
        }
    }

    fn send(sink: &mut dyn RowSink, opts: &ExecOptions, row: Vec<Value>) {
        match opts.cells {
            CellMode::Preview => sink.rows(vec![row]),
            CellMode::Full { .. } => sink.full_rows(vec![row
                .into_iter()
                .map(|v| FullValue::try_from_value(v).unwrap())
                .collect()]),
        }
    }

    #[async_trait]
    impl Session for FakeSession {
        fn server_info(&self) -> &ServerInfo {
            &self.info
        }
        /// A statement mentioning `slow` streams a row every 10 ms for 10 s unless it is
        /// stopped or cancelled (then `Cancelled`); anything else returns no result.
        async fn execute_one(
            &self,
            stmt: &str,
            _kind: StatementKind,
            opts: &ExecOptions,
            sink: &mut dyn RowSink,
        ) -> Result<StmtOutcome, DbError> {
            self.log.cancelled.store(false, Ordering::SeqCst);
            let summary = |row_count| ResultSummary {
                affected: None,
                row_count,
                truncated: false,
                elapsed_ms: 1,
            };
            if !stmt.to_ascii_lowercase().contains("slow") {
                sink.result_start(&[]);
                sink.result_end(&summary(0));
                return Ok(StmtOutcome {
                    results: vec![summary(0)],
                    late_completion: false,
                });
            }
            self.log.slow_started.fetch_add(1, Ordering::SeqCst);
            sink.result_start(&[column()]);
            for i in 0..1000 {
                if opts.stop.is_stopped() || self.log.cancelled.load(Ordering::SeqCst) {
                    return Err(DbError::Cancelled {
                        outcome_unknown: false,
                        was_in_tx: false,
                    });
                }
                send(sink, opts, vec![Value::Int(i)]);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            sink.result_end(&summary(1000));
            Ok(StmtOutcome {
                results: vec![summary(1000)],
                late_completion: false,
            })
        }
        fn tx_state(&self) -> TxState {
            TxState::Idle
        }
        fn dialect(&self) -> Dialect {
            Dialect::for_kind(DbKind::MySql)
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
        /// No detail: the export reads without a stable order (as for a view).
        async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
            Err(DbError::Internal {
                message: "n/a".into(),
            })
        }
        async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
            Ok(String::new())
        }
    }

    /// An app state whose MySQL driver is the fake, with one open connection.
    async fn fake_state(dir: &std::path::Path) -> (Arc<AppState>, Uuid, Arc<Log>) {
        let paths = AppPaths::in_dir(dir.join("data"));
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
        let repo = Arc::new(ConnectionRepo::new(
            dnt_store::db::open(&paths.db_file).unwrap(),
            Arc::clone(&secrets),
        ));
        let log = Arc::new(Log::default());
        let driver: Arc<dyn Driver> = Arc::new(FakeDriver(Arc::clone(&log)));
        let sessions = SessionManager::new(
            Arc::clone(&repo),
            Arc::clone(&secrets),
            KnownHosts::new(paths.known_hosts.clone()),
            HashMap::from([(DbKind::MySql, driver)]),
        )
        .with_result_root(paths.data_dir.join("query-results"))
        .unwrap();
        let cfg = ConnectionConfig {
            id: Uuid::nil(),
            kind: DbKind::MySql,
            name: "Shop".into(),
            default_db: None,
            env_tag: None,
            read_only: false,
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
        sessions.open(id, None).await.unwrap();
        let state = AppState {
            repo,
            secrets,
            sessions,
            paths,
            export_paths: ExportPaths::default(),
            file_picks: FilePicks::default(),
        };
        (Arc::new(state), id, log)
    }

    fn csv() -> ExportOptions {
        ExportOptions {
            format: ExportFormat::Csv,
            delimiter: ",".into(),
            header: true,
            quote_all: false,
            bom: false,
            table_name: String::new(),
            formula_guard: false,
        }
    }

    fn no_progress() -> Progress {
        Box::new(|_| {})
    }

    /// The live tracked export, once its statement streams rows.
    async fn running_export(state: &AppState, log: &Log) -> Uuid {
        for _ in 0..1000 {
            let live = state.sessions.jobs.list(10).unwrap();
            if let Some(job) = live.first() {
                if job.state == JobState::Running
                    && log.slow_started.load(Ordering::SeqCst) > 0
                    && job.progress.done > 0.0
                {
                    assert_eq!(job.kind, JobKind::Export);
                    return job.id;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no running tracked export");
    }

    /// Stop from the Jobs panel cancels a table export like `table_data_cancel(id, key)`:
    /// the statement ends cancelled, no file is left and the job is cancelled.
    #[tokio::test(flavor = "multi_thread")]
    async fn job_stop_cancels_table_export_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id, log) = fake_state(dir.path()).await;
        let path = dir.path().join("slow.csv");
        let export = {
            let state = Arc::clone(&state);
            let path = path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                let obj = ObjectRef {
                    database: "shop".into(),
                    schema: None,
                    name: "slow".into(),
                    kind: ObjectKind::Table,
                };
                export_table_impl(
                    &state,
                    id,
                    obj,
                    vec![],
                    None,
                    csv(),
                    path,
                    "export:slow".into(),
                    no_progress(),
                )
                .await
            })
        };
        let job = running_export(&state, &log).await;
        let detail = job_detail_impl(&state, job).await.unwrap();
        assert_eq!(detail.job.title, "Export slow to slow.csv");
        assert_eq!(detail.job.connection_id, Some(id));

        assert!(job_cancel_impl(&state, job).await.unwrap());
        let err = tokio::time::timeout(Duration::from_secs(5), export)
            .await
            .expect("the export stops")
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, DbError::Cancelled { .. }), "{err:?}");
        assert!(state.sessions.jobs.wait(job, Duration::from_secs(5)).await);
        let detail = job_detail_impl(&state, job).await.unwrap();
        assert_eq!(detail.job.state, JobState::Cancelled);
        assert_eq!(detail.job.result_path, None);
        assert!(!path.exists());
    }

    /// Stop from the Jobs panel stops a query export like `query_cancel(run_id)`: the
    /// run's canceller reaches the tab session's statement.
    #[tokio::test(flavor = "multi_thread")]
    async fn job_stop_cancels_query_export_run() {
        let dir = tempfile::tempdir().unwrap();
        let (state, id, log) = fake_state(dir.path()).await;
        let plan = query_plan_impl(
            &state,
            "tab-1".into(),
            id,
            None,
            "SELECT n FROM slow".into(),
            Pick::All,
        )
        .await
        .unwrap();
        let path = dir.path().join("q.csv");
        let export = {
            let state = Arc::clone(&state);
            let path = path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                export_query_impl(
                    &state,
                    plan.plan_id,
                    0,
                    csv(),
                    path,
                    Uuid::new_v4(),
                    no_progress(),
                )
                .await
            })
        };
        let job = running_export(&state, &log).await;
        let detail = job_detail_impl(&state, job).await.unwrap();
        assert_eq!(detail.job.title, "Export query result to q.csv");
        assert_eq!(detail.job.connection_id, Some(id));

        assert!(job_cancel_impl(&state, job).await.unwrap());
        let err = tokio::time::timeout(Duration::from_secs(5), export)
            .await
            .expect("the export stops")
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, DbError::Cancelled { .. }), "{err:?}");
        // The run's canceller (the one `query_cancel` uses) reached the session.
        assert!(log.cancel_calls.load(Ordering::SeqCst) >= 1);
        assert!(state.sessions.jobs.wait(job, Duration::from_secs(5)).await);
        let detail = job_detail_impl(&state, job).await.unwrap();
        assert_eq!(detail.job.state, JobState::Cancelled);
        assert!(!path.exists());
    }
}
