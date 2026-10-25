//! Background job commands (spec §4.3): list, detail, Stop, the close guard's count and
//! stop-all, "Open folder" of a job's result and clearing finished jobs. The job
//! manager's store reads are synchronous, so they run through [`blocking`].

use super::{blocking, guard};
use crate::jobs::{JobDetail, JobSummary};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use uuid::Uuid;

/// Jobs `job_list` returns when no limit is given.
pub const DEFAULT_JOB_LIST_LIMIT: u32 = 200;
/// Most jobs `job_list` returns.
pub const MAX_JOB_LIST_LIMIT: u32 = 500;
/// How long `job_stop_all` waits for the jobs to end (R8).
pub const STOP_ALL_WAIT: Duration = Duration::from_secs(5);

/// Live jobs (newest first), then finished ones (newest first); `limit` defaults to
/// [`DEFAULT_JOB_LIST_LIMIT`] and is capped at [`MAX_JOB_LIST_LIMIT`].
pub async fn job_list_impl(
    state: &AppState,
    limit: Option<u32>,
) -> Result<Vec<JobSummary>, DbError> {
    let limit = limit
        .unwrap_or(DEFAULT_JOB_LIST_LIMIT)
        .min(MAX_JOB_LIST_LIMIT);
    let jobs = state.sessions.jobs.clone();
    blocking(move || jobs.list(limit)).await
}

pub async fn job_detail_impl(state: &AppState, id: Uuid) -> Result<JobDetail, DbError> {
    let jobs = state.sessions.jobs.clone();
    blocking(move || jobs.detail(id)).await
}

/// Stops a queued or running job; `false` when it is not live.
pub async fn job_cancel_impl(state: &AppState, id: Uuid) -> Result<bool, DbError> {
    Ok(state.sessions.jobs.cancel(id).await)
}

/// Queued + running jobs, tracked exports included.
pub async fn job_active_count_impl(state: &AppState) -> Result<u32, DbError> {
    Ok(state.sessions.jobs.active_count())
}

/// Stops every job and waits at most [`STOP_ALL_WAIT`] for them to end.
pub async fn job_stop_all_impl(state: &AppState) -> Result<(), DbError> {
    state.sessions.jobs.stop_all(STOP_ALL_WAIT).await;
    Ok(())
}

/// Deletes the finished jobs (store and memory); live jobs stay.
pub async fn job_clear_history_impl(state: &AppState) -> Result<(), DbError> {
    let jobs = state.sessions.jobs.clone();
    blocking(move || jobs.clear_finished()).await
}

/// The result file of job `id` that "Open folder" may show: only a path the job itself
/// recorded, and only while that file exists; else `config` (field `id`).
pub async fn job_result_path_impl(state: &AppState, id: Uuid) -> Result<PathBuf, DbError> {
    let jobs = state.sessions.jobs.clone();
    blocking(move || {
        let refuse = |message: &str| DbError::Config {
            field: Some("id".into()),
            message: message.into(),
        };
        let detail = jobs.detail(id)?;
        let path = detail
            .job
            .result_path
            .map(PathBuf::from)
            .ok_or_else(|| refuse("This job has no result file."))?;
        if !path.is_file() {
            return Err(refuse("The result file is no longer there."));
        }
        Ok(path)
    })
    .await
}

/// Live jobs first, then finished ones; `limit` default 200, at most 500.
#[tauri::command]
#[specta::specta]
pub async fn job_list(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<JobSummary>, AppError> {
    guard(job_list_impl(&state, limit)).await
}

/// A job with its log and the parameters it was started with.
#[tauri::command]
#[specta::specta]
pub async fn job_detail(state: State<'_, AppState>, id: Uuid) -> Result<JobDetail, AppError> {
    guard(job_detail_impl(&state, id)).await
}

/// Stops a job; `false` when it is not queued or running.
#[tauri::command]
#[specta::specta]
pub async fn job_cancel(state: State<'_, AppState>, id: Uuid) -> Result<bool, AppError> {
    guard(job_cancel_impl(&state, id)).await
}

/// Queued and running jobs (status bar, close guard).
#[tauri::command]
#[specta::specta]
pub async fn job_active_count(state: State<'_, AppState>) -> Result<u32, AppError> {
    guard(job_active_count_impl(&state)).await
}

/// Stops every job and waits up to 5 seconds for them to end (close guard).
#[tauri::command]
#[specta::specta]
pub async fn job_stop_all(state: State<'_, AppState>) -> Result<(), AppError> {
    guard(job_stop_all_impl(&state)).await
}

/// Shows a job's result file in the system file manager, only when the job recorded it
/// and the file exists; nothing is opened or run.
#[tauri::command]
#[specta::specta]
pub async fn job_reveal_result(
    app: AppHandle,
    state: State<'_, AppState>,
    id: Uuid,
) -> Result<(), AppError> {
    guard(async move {
        let path = job_result_path_impl(&state, id).await?;
        app.opener()
            .reveal_item_in_dir(path)
            .map_err(|e| DbError::Internal {
                message: format!("cannot open the folder: {e}"),
            })
    })
    .await
}

/// Deletes the finished jobs; queued and running ones stay.
#[tauri::command]
#[specta::specta]
pub async fn job_clear_history(state: State<'_, AppState>) -> Result<(), AppError> {
    guard(job_clear_history_impl(&state)).await
}
