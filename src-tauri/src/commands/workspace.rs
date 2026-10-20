//! Query workspace: autosaved query tabs, `.sql` files and query history.
//!
//! File paths come from the native Open/Save dialogs in the UI. Files are UTF-8: a leading
//! byte order mark is stripped on read and never written.

use super::transfer::path_error;
use super::{blocking, guard};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_store::history::{HistoryEntry, HistoryFilter, HistoryIds};
use dnt_store::tabs::QueryTabRecord;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use tauri::State;

/// Largest `.sql` file the editor opens (the whole text goes over IPC into Monaco).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

const BOM: char = '\u{feff}';

pub async fn tabs_load_impl(state: &AppState) -> Result<Vec<QueryTabRecord>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.tabs_load()).await
}

pub async fn tabs_save_impl(state: &AppState, tab: QueryTabRecord) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.tabs_save(&tab)).await
}

pub async fn tabs_delete_impl(state: &AppState, id: String) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.tabs_delete(&id)).await
}

fn no_path() -> DbError {
    DbError::Config {
        field: Some("path".into()),
        message: "No file was chosen.".into(),
    }
}

fn read_text(path: &str) -> Result<String, DbError> {
    if path.trim().is_empty() {
        return Err(no_path());
    }
    let file = std::fs::File::open(Path::new(path)).map_err(|e| path_error("read", path, &e))?;
    let mut bytes = Vec::new();
    // One byte past the limit tells "too large" apart from "exactly at the limit".
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| path_error("read", path, &e))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(DbError::Config {
            field: Some("path".into()),
            message: format!(
                "The file is larger than {} MB.",
                MAX_FILE_BYTES / (1024 * 1024)
            ),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| DbError::Config {
        field: Some("path".into()),
        message: "The file is not UTF-8 text.".into(),
    })?;
    Ok(match text.strip_prefix(BOM) {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

fn write_text(path: &str, text: &str) -> Result<(), DbError> {
    if path.trim().is_empty() {
        return Err(no_path());
    }
    let text = text.strip_prefix(BOM).unwrap_or(text);
    write_atomic(Path::new(path), text.as_bytes()).map_err(|e| path_error("write", path, &e))
}

/// Writes `bytes` to a temporary file next to `target`, syncs it, then renames it over
/// `target` (which replaces an existing file, on Windows too), so a failed save never
/// leaves a truncated file. The temporary file is removed when any step fails.
fn write_atomic(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = target
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".dnt-tmp-{}", uuid::Uuid::new_v4().simple()));
    let tmp = target.with_file_name(tmp_name);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The UTF-8 text of `path`, without a leading BOM.
pub async fn file_read_impl(path: String) -> Result<String, DbError> {
    blocking(move || read_text(&path)).await
}

/// Writes `text` to `path` as UTF-8 without BOM (a leading U+FEFF is dropped).
pub async fn file_write_impl(path: String, text: String) -> Result<(), DbError> {
    blocking(move || write_text(&path, &text)).await
}

pub async fn history_list_impl(
    state: &AppState,
    filter: HistoryFilter,
) -> Result<Vec<HistoryEntry>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.history_list(&filter)).await
}

pub async fn history_delete_impl(state: &AppState, ids: HistoryIds) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.history_delete(&ids.0)).await
}

pub async fn history_clear_impl(state: &AppState) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.history_clear()).await
}

#[tauri::command]
#[specta::specta]
pub async fn tabs_load(state: State<'_, AppState>) -> Result<Vec<QueryTabRecord>, AppError> {
    guard(tabs_load_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn tabs_save(state: State<'_, AppState>, tab: QueryTabRecord) -> Result<(), AppError> {
    guard(tabs_save_impl(&state, tab)).await
}

#[tauri::command]
#[specta::specta]
pub async fn tabs_delete(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    guard(tabs_delete_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn file_read(path: String) -> Result<String, AppError> {
    guard(file_read_impl(path)).await
}

#[tauri::command]
#[specta::specta]
pub async fn file_write(path: String, text: String) -> Result<(), AppError> {
    guard(file_write_impl(path, text)).await
}

#[tauri::command]
#[specta::specta]
pub async fn history_list(
    state: State<'_, AppState>,
    filter: HistoryFilter,
) -> Result<Vec<HistoryEntry>, AppError> {
    guard(history_list_impl(&state, filter)).await
}

#[tauri::command]
#[specta::specta]
pub async fn history_delete(state: State<'_, AppState>, ids: HistoryIds) -> Result<(), AppError> {
    guard(history_delete_impl(&state, ids)).await
}

#[tauri::command]
#[specta::specta]
pub async fn history_clear(state: State<'_, AppState>) -> Result<(), AppError> {
    guard(history_clear_impl(&state)).await
}
