//! Query workspace: autosaved query tabs, `.sql` files and query history.
//!
//! File paths come from the Open / Save dialogs the BACKEND opens (`file_pick_open`,
//! `file_pick_save`): `file_read` / `file_write` only take a path picked there, or the
//! file of a query tab as stored in the app database (see [`crate::file_picks`]). Files
//! are UTF-8: a leading byte order mark is stripped on read and never written.

use super::export::file_stem;
use super::transfer::path_error;
use super::{blocking, guard};
use crate::file_picks::{FileAccess, FileKind};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_store::history::{HistoryEntry, HistoryFilter, HistoryIds};
use dnt_store::tabs::QueryTabRecord;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use tauri::State;
use tauri_plugin_dialog::DialogExt;

/// Largest `.sql` file the editor opens (the whole text goes over IPC into Monaco).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

const BOM: char = '\u{feff}';

/// The stored query tabs. The `.sql` file of each, read from the app database (never
/// from the UI), is admitted for `file_read` / `file_write` again: Ctrl+S on a restored
/// tab saves to its file without a new dialog.
pub async fn tabs_load_impl(state: &AppState) -> Result<Vec<QueryTabRecord>, DbError> {
    let repo = Arc::clone(&state.repo);
    let tabs = blocking(move || repo.tabs_load()).await?;
    for path in tabs.iter().filter_map(|t| t.file_path.as_deref()) {
        // A relative path stored by hand is simply not admitted.
        let _ = state.file_picks.admit(FileAccess::Sql, path.into());
    }
    Ok(tabs)
}

/// Saves a query tab. Its `file_path` must be admitted (picked in the backend's
/// dialog, or restored by `tabs_load`), or be the one already stored for this tab:
/// the UI cannot plant a path for a later `tabs_load` to admit.
pub async fn tabs_save_impl(state: &AppState, tab: QueryTabRecord) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    if let Some(path) = tab.file_path.clone() {
        if state.file_picks.check(FileAccess::Sql, &path).is_err() {
            let id = tab.id.clone();
            let stored = Arc::clone(&repo);
            let same = blocking(move || {
                Ok(stored
                    .tabs_load()?
                    .into_iter()
                    .any(|t| t.id == id && t.file_path.as_deref() == Some(path.as_str())))
            })
            .await?;
            if !same {
                return Err(DbError::Config {
                    field: Some("file_path".into()),
                    message: "Choose the tab's file in the file dialog.".into(),
                });
            }
        }
    }
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

/// Refuses an empty `path` ("No file was chosen.") and one not admitted for `.sql`
/// files (see [`crate::file_picks`]).
fn admitted_sql(state: &AppState, path: &str) -> Result<(), DbError> {
    if path.trim().is_empty() {
        return Err(no_path());
    }
    state.file_picks.check(FileAccess::Sql, path)
}

/// The UTF-8 text of `path`, without a leading BOM. Only an admitted path.
pub async fn file_read_impl(state: &AppState, path: String) -> Result<String, DbError> {
    admitted_sql(state, &path)?;
    blocking(move || read_text(&path)).await
}

/// Writes `text` to `path` as UTF-8 without BOM (a leading U+FEFF is dropped). Only an
/// admitted path.
pub async fn file_write_impl(state: &AppState, path: String, text: String) -> Result<(), DbError> {
    admitted_sql(state, &path)?;
    blocking(move || write_text(&path, &text)).await
}

/// The file dialog of `kind` (`save`: a save dialog, named `default_name` when given),
/// opened by the backend; the picked path is admitted for what `kind` / `save` grant
/// (see [`FileAccess::of`]) and returned. `None`: the dialog was cancelled.
async fn pick_file(
    window: &tauri::WebviewWindow,
    state: &AppState,
    kind: FileKind,
    save: bool,
    default_name: Option<String>,
) -> Result<Option<String>, DbError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = window.dialog().file().set_parent(window);
    dialog = match (kind, save) {
        (FileKind::Sql, _) => dialog.add_filter("SQL", &["sql"]),
        (FileKind::Connections, false) => dialog
            .add_filter("DntDatabase connections", &["dntconn"])
            .add_filter("Navicat connections", &["ncx"])
            .add_filter("All files", &["*"]),
        (FileKind::Connections, true) => dialog.add_filter("DntDatabase connections", &["dntconn"]),
    };
    if let Some(name) = default_name.filter(|n| !n.trim().is_empty()) {
        dialog = dialog.set_file_name(file_stem(&name));
    }
    let answer = move |picked| {
        let _ = tx.send(picked);
    };
    if save {
        dialog.save_file(answer);
    } else {
        dialog.pick_file(answer);
    }
    let picked = rx.await.map_err(|_| DbError::Internal {
        message: "the file dialog closed without an answer".into(),
    })?;
    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| DbError::Internal {
        message: format!("the file dialog returned no file path: {e}"),
    })?;
    state
        .file_picks
        .admit(FileAccess::of(kind, save), path)
        .map(Some)
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

/// Opens the Open dialog for a `kind` file (from the backend: the UI never names a
/// path itself) and returns the picked path, admitted for `file_read` / `file_write`
/// (`sql`) or the connection import commands (`connections`). `None`: cancelled.
#[tauri::command]
#[specta::specta]
pub async fn file_pick_open(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    kind: FileKind,
) -> Result<Option<String>, AppError> {
    guard(pick_file(&window, &state, kind, false, None)).await
}

/// Opens the Save dialog for a `kind` file, named `default_name` by default, and
/// returns the picked path, admitted for `file_write` (`sql`) or `export_connections`
/// (`connections`). `None`: cancelled.
#[tauri::command]
#[specta::specta]
pub async fn file_pick_save(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    kind: FileKind,
    default_name: Option<String>,
) -> Result<Option<String>, AppError> {
    guard(pick_file(&window, &state, kind, true, default_name)).await
}

/// The text of a `.sql` file: only one picked through `file_pick_open` /
/// `file_pick_save` or stored with a query tab (`config` error, field `path`, else).
#[tauri::command]
#[specta::specta]
pub async fn file_read(state: State<'_, AppState>, path: String) -> Result<String, AppError> {
    guard(file_read_impl(&state, path)).await
}

/// Writes a `.sql` file: only one picked through `file_pick_open` / `file_pick_save`
/// or stored with a query tab (`config` error, field `path`, else).
#[tauri::command]
#[specta::specta]
pub async fn file_write(
    state: State<'_, AppState>,
    path: String,
    text: String,
) -> Result<(), AppError> {
    guard(file_write_impl(&state, path, text)).await
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
