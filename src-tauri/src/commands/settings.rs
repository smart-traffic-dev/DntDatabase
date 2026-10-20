//! Application settings: log level, table page size and the unbounded-write
//! confirmation (persisted in `app_settings`), and the log folder.

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

/// `app_settings` key of the table data view's default page size.
pub const TABLE_PAGE_SIZE_KEY: &str = "table_page_size";
pub const DEFAULT_TABLE_PAGE_SIZE: u32 = 100;
pub const MIN_TABLE_PAGE_SIZE: u32 = 10;
pub const MAX_TABLE_PAGE_SIZE: u32 = 10_000;

/// The stored page size; the default when none is stored, it is out of range or it
/// cannot be read.
pub fn saved_table_page_size(repo: &ConnectionRepo) -> u32 {
    match repo.app_setting(TABLE_PAGE_SIZE_KEY) {
        Ok(Some(v)) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (MIN_TABLE_PAGE_SIZE..=MAX_TABLE_PAGE_SIZE).contains(n))
            .unwrap_or(DEFAULT_TABLE_PAGE_SIZE),
        Ok(None) => DEFAULT_TABLE_PAGE_SIZE,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the saved table page size");
            DEFAULT_TABLE_PAGE_SIZE
        }
    }
}

pub async fn get_table_page_size_impl(state: &AppState) -> Result<u32, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(saved_table_page_size(&repo))).await
}

pub async fn set_table_page_size_impl(state: &AppState, size: u32) -> Result<(), DbError> {
    if !(MIN_TABLE_PAGE_SIZE..=MAX_TABLE_PAGE_SIZE).contains(&size) {
        return Err(DbError::Config {
            field: Some("size".into()),
            message: format!(
                "the page size must be between {MIN_TABLE_PAGE_SIZE} and {MAX_TABLE_PAGE_SIZE}"
            ),
        });
    }
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.set_app_setting(TABLE_PAGE_SIZE_KEY, &size.to_string())).await
}

#[tauri::command]
#[specta::specta]
pub async fn get_table_page_size(state: State<'_, AppState>) -> Result<u32, AppError> {
    guard(get_table_page_size_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn set_table_page_size(state: State<'_, AppState>, size: u32) -> Result<(), AppError> {
    guard(set_table_page_size_impl(&state, size)).await
}

/// `app_settings` key of "Confirm UPDATE/DELETE without WHERE" (`1` | `0`).
pub const CONFIRM_UNBOUNDED_KEY: &str = "confirm_unbounded_writes";

/// Whether unbounded writes (`dnt_query::unbounded`) are confirmed on connections that
/// are not Production (Production always confirms them). On unless `0` is stored; on
/// when it cannot be read.
pub fn saved_confirm_unbounded(repo: &ConnectionRepo) -> bool {
    match repo.app_setting(CONFIRM_UNBOUNDED_KEY) {
        Ok(Some(v)) => v != "0",
        Ok(None) => true,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the unbounded-write setting");
            true
        }
    }
}

pub async fn get_confirm_unbounded_writes_impl(state: &AppState) -> Result<bool, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(saved_confirm_unbounded(&repo))).await
}

pub async fn set_confirm_unbounded_writes_impl(
    state: &AppState,
    enabled: bool,
) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    let value = if enabled { "1" } else { "0" };
    blocking(move || repo.set_app_setting(CONFIRM_UNBOUNDED_KEY, value)).await
}

/// "Confirm UPDATE/DELETE without WHERE" for non-Production connections.
#[tauri::command]
#[specta::specta]
pub async fn get_confirm_unbounded_writes(state: State<'_, AppState>) -> Result<bool, AppError> {
    guard(get_confirm_unbounded_writes_impl(&state)).await
}

/// Stores "Confirm UPDATE/DELETE without WHERE"; the next plan uses it. Production
/// connections confirm unbounded writes whatever it is.
#[tauri::command]
#[specta::specta]
pub async fn set_confirm_unbounded_writes(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), AppError> {
    guard(set_confirm_unbounded_writes_impl(&state, enabled)).await
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
