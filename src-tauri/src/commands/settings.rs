//! Application settings: log level and the log folder.

use super::guard;
use crate::logging::{LogHandle, LogLevel};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

pub async fn set_log_level_impl(log: &LogHandle, level: LogLevel) -> Result<(), DbError> {
    log.set_level(level)
}

#[tauri::command]
#[specta::specta]
pub async fn set_log_level(log: State<'_, LogHandle>, level: LogLevel) -> Result<(), AppError> {
    guard(set_log_level_impl(&log, level)).await
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
