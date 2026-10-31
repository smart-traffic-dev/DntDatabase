//! Application settings: log level, table page size, the unbounded-write
//! confirmation, the idle-transaction warning delay, the per-connection job limit and
//! the external tools folder (persisted in `app_settings`), and the log folder.

use super::{blocking, guard};
use crate::jobs::{
    saved_jobs_per_connection, JOBS_PER_CONNECTION_KEY, MAX_JOBS_PER_CONNECTION,
    MIN_JOBS_PER_CONNECTION,
};
use crate::logging::{LogHandle, LogLevel};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_store::repo::ConnectionRepo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
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

/// Whether unbounded writes (`dnt_query::unbounded`) are confirmed on Local and untagged
/// connections (Production and Staging always confirm them). On unless `0` is stored; on
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

/// "Confirm UPDATE/DELETE without WHERE" for Local and untagged connections.
#[tauri::command]
#[specta::specta]
pub async fn get_confirm_unbounded_writes(state: State<'_, AppState>) -> Result<bool, AppError> {
    guard(get_confirm_unbounded_writes_impl(&state)).await
}

/// Stores "Confirm UPDATE/DELETE without WHERE"; the next plan uses it. Production and
/// Staging connections confirm unbounded writes whatever it is.
#[tauri::command]
#[specta::specta]
pub async fn set_confirm_unbounded_writes(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), AppError> {
    guard(set_confirm_unbounded_writes_impl(&state, enabled)).await
}

/// `app_settings` key of the minutes after which an open transaction of a query tab
/// that does nothing is warned about (spec §4.2e).
pub const IDLE_TX_WARN_KEY: &str = "idle_tx_warn_minutes";
pub const DEFAULT_IDLE_TX_WARN_MINUTES: u32 = 5;
pub const MIN_IDLE_TX_WARN_MINUTES: u32 = 1;
pub const MAX_IDLE_TX_WARN_MINUTES: u32 = 120;

/// The stored idle-transaction warning delay in minutes; the default when none is
/// stored, it is out of range or it cannot be read.
pub fn saved_idle_tx_warn_minutes(repo: &ConnectionRepo) -> u32 {
    match repo.app_setting(IDLE_TX_WARN_KEY) {
        Ok(Some(v)) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (MIN_IDLE_TX_WARN_MINUTES..=MAX_IDLE_TX_WARN_MINUTES).contains(n))
            .unwrap_or(DEFAULT_IDLE_TX_WARN_MINUTES),
        Ok(None) => DEFAULT_IDLE_TX_WARN_MINUTES,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the idle transaction warning setting");
            DEFAULT_IDLE_TX_WARN_MINUTES
        }
    }
}

pub async fn get_idle_tx_warn_minutes_impl(state: &AppState) -> Result<u32, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(saved_idle_tx_warn_minutes(&repo))).await
}

pub async fn set_idle_tx_warn_minutes_impl(state: &AppState, minutes: u32) -> Result<(), DbError> {
    if !(MIN_IDLE_TX_WARN_MINUTES..=MAX_IDLE_TX_WARN_MINUTES).contains(&minutes) {
        return Err(DbError::Config {
            field: Some("minutes".into()),
            message: format!(
                "the warning delay must be between {MIN_IDLE_TX_WARN_MINUTES} and \
                 {MAX_IDLE_TX_WARN_MINUTES} minutes"
            ),
        });
    }
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.set_app_setting(IDLE_TX_WARN_KEY, &minutes.to_string())).await
}

/// Minutes after which a query tab's open, unused transaction is warned about
/// (default 5).
#[tauri::command]
#[specta::specta]
pub async fn get_idle_tx_warn_minutes(state: State<'_, AppState>) -> Result<u32, AppError> {
    guard(get_idle_tx_warn_minutes_impl(&state)).await
}

/// Stores the idle-transaction warning delay: 1 to 120 minutes, else `config` (field
/// `minutes`).
#[tauri::command]
#[specta::specta]
pub async fn set_idle_tx_warn_minutes(
    state: State<'_, AppState>,
    minutes: u32,
) -> Result<(), AppError> {
    guard(set_idle_tx_warn_minutes_impl(&state, minutes)).await
}

pub async fn get_jobs_per_connection_impl(state: &AppState) -> Result<u32, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(saved_jobs_per_connection(&repo))).await
}

/// Stores the limit (1–8, else `config`, field `n`); queued jobs re-check it at once.
pub async fn set_jobs_per_connection_impl(state: &AppState, n: u32) -> Result<(), DbError> {
    if !(MIN_JOBS_PER_CONNECTION..=MAX_JOBS_PER_CONNECTION).contains(&n) {
        return Err(DbError::Config {
            field: Some("n".into()),
            message: format!(
                "the number of jobs per connection must be between {MIN_JOBS_PER_CONNECTION} \
                 and {MAX_JOBS_PER_CONNECTION}"
            ),
        });
    }
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.set_app_setting(JOBS_PER_CONNECTION_KEY, &n.to_string())).await?;
    state.sessions.jobs.limit_changed();
    Ok(())
}

/// Background jobs that may run at the same time on one connection (default 2).
#[tauri::command]
#[specta::specta]
pub async fn get_jobs_per_connection(state: State<'_, AppState>) -> Result<u32, AppError> {
    guard(get_jobs_per_connection_impl(&state)).await
}

/// Stores the per-connection job limit: 1 to 8, else `config` (field `n`).
#[tauri::command]
#[specta::specta]
pub async fn set_jobs_per_connection(state: State<'_, AppState>, n: u32) -> Result<(), AppError> {
    guard(set_jobs_per_connection_impl(&state, n)).await
}

/// `app_settings` key of the folder searched first for external tools (R25); empty or
/// missing: none.
pub const EXTERNAL_TOOLS_DIR_KEY: &str = "external_tools_dir";

/// The stored external tools folder; `None` when none is stored or it cannot be read.
/// It is not checked again here (the tool lookup skips a folder that no longer exists).
pub fn saved_external_tools_dir(repo: &ConnectionRepo) -> Option<PathBuf> {
    match repo.app_setting(EXTERNAL_TOOLS_DIR_KEY) {
        Ok(Some(v)) if !v.trim().is_empty() => Some(PathBuf::from(v)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the external tools folder setting");
            None
        }
    }
}

pub async fn get_external_tools_dir_impl(state: &AppState) -> Result<Option<String>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(saved_external_tools_dir(&repo).map(|p| p.to_string_lossy().into_owned())))
        .await
}

/// Stores the external tools folder: an absolute path of an existing directory, else
/// `config` (field `dir`). `None` or blank clears it.
///
/// The app runs `pg_dump.exe` and the other tools from this folder, so the UI never
/// names it: only `tools_pick_dir` (the backend's folder dialog) and
/// `clear_external_tools_dir` reach this, never a path sent by the webview.
pub async fn set_external_tools_dir_impl(
    state: &AppState,
    dir: Option<String>,
) -> Result<(), DbError> {
    let dir = dir.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty());
    let repo = Arc::clone(&state.repo);
    blocking(move || {
        let value = match dir {
            None => String::new(),
            Some(d) => {
                let path = Path::new(&d);
                let refuse = |message: &str| DbError::Config {
                    field: Some("dir".into()),
                    message: message.into(),
                };
                if !path.is_absolute() {
                    return Err(refuse("The tools folder must be a full path."));
                }
                if !path.is_dir() {
                    return Err(refuse("The tools folder does not exist."));
                }
                d
            }
        };
        repo.set_app_setting(EXTERNAL_TOOLS_DIR_KEY, &value)
    })
    .await
}

/// The folder searched first for `mysqldump`, `pg_dump` and the other external tools;
/// `None`: only `PATH`.
#[tauri::command]
#[specta::specta]
pub async fn get_external_tools_dir(
    state: State<'_, AppState>,
) -> Result<Option<String>, AppError> {
    guard(get_external_tools_dir_impl(&state)).await
}

/// Opens the folder dialog from the backend and stores the picked folder as the external
/// tools folder; returns it, or `None` when cancelled. The UI cannot name the folder
/// itself: tools run from it, so a compromised webview must not be able to point it at a
/// folder of its choosing.
#[tauri::command]
#[specta::specta]
pub async fn tools_pick_dir(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<String>, AppError> {
    guard(async move {
        let repo = Arc::clone(&state.repo);
        let current = blocking(move || Ok(saved_external_tools_dir(&repo))).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut dialog = window.dialog().file().set_parent(&window);
        if let Some(dir) = current.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        dialog.pick_folder(move |picked| {
            let _ = tx.send(picked);
        });
        let picked = rx.await.map_err(|_| DbError::Internal {
            message: "the folder dialog closed without an answer".into(),
        })?;
        let Some(picked) = picked else {
            return Ok(None);
        };
        let path = picked.into_path().map_err(|e| DbError::Internal {
            message: format!("the folder dialog returned no folder path: {e}"),
        })?;
        let dir = path.to_string_lossy().into_owned();
        set_external_tools_dir_impl(&state, Some(dir.clone())).await?;
        Ok(Some(dir))
    })
    .await
}

/// Clears the external tools folder: only `PATH` is searched.
#[tauri::command]
#[specta::specta]
pub async fn clear_external_tools_dir(state: State<'_, AppState>) -> Result<(), AppError> {
    guard(set_external_tools_dir_impl(&state, None)).await
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
