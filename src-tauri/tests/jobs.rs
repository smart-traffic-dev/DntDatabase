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
