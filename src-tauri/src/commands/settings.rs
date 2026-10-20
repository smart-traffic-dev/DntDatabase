//! Application settings: log level (persisted in `app_settings`) and the log folder.

use super::{blocking, guard};
use crate::logging::{LogHandle, LogLevel};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_store::repo::ConnectionRepo;
use std::sync::Arc;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

/// `app_settings` key of the log level (`info` | `debug`).
pub const LOG_LEVEL_KEY: &str = "log_level";

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
    }
}

/// The stored log level; `Info` when none is stored or it cannot be read.
pub fn saved_log_level(repo: &ConnectionRepo) -> LogLevel {
    match repo.app_setting(LOG_LEVEL_KEY) {
        Ok(Some(v)) if v == level_name(LogLevel::Debug) => LogLevel::Debug,
        Ok(_) => LogLevel::Info,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the saved log level");
            LogLevel::Info
        }
    }
}

pub async fn get_log_level_impl(state: &AppState) -> Result<LogLevel, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(saved_log_level(&repo))).await
}

/// Applies `level` and stores it so the next start uses it too.
pub async fn set_log_level_impl(
    state: &AppState,
    log: &LogHandle,
    level: LogLevel,
) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.set_app_setting(LOG_LEVEL_KEY, level_name(level))).await?;
    log.set_level(level)
}

#[tauri::command]
#[specta::specta]
pub async fn get_log_level(state: State<'_, AppState>) -> Result<LogLevel, AppError> {
    guard(get_log_level_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_log_level(
    state: State<'_, AppState>,
    log: State<'_, LogHandle>,
    level: LogLevel,
) -> Result<(), AppError> {
    guard(set_log_level_impl(&state, &log, level)).await
}

/// Opens the log folder in the system file manager.
#[tauri::command]
#[specta::specta]
pub async fn open_log_folder(app: AppHandle, state: State<'_, AppState>) -> Result<(), AppError> {
    let dir = state.paths.logs_dir.to_string_lossy().into_owned();
    guard(async move {
        app.opener()
            .open_path(dir, None::<&str>)
            .map_err(|e| DbError::Internal {
                message: format!("cannot open the log folder: {e}"),
            })
    })
    .await
}
