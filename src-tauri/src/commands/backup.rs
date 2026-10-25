//! Dump commands (spec §7.1): the objects a database offers, the backend's save dialog
//! and the start of a dump job. `dump_start` accepts only a path picked in
//! `dump_pick_path`.

use super::export::file_stem;
use super::guard;
use crate::backup::{DumpObjects, DumpRequest};
use crate::file_picks::FileAccess;
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

pub async fn dump_objects_impl(
    state: &AppState,
    connection_id: Uuid,
    database: String,
    schemas: Vec<String>,
) -> Result<DumpObjects, DbError> {
    state
        .sessions
        .dump_objects(connection_id, database, schemas)
        .await
}

pub async fn dump_start_impl(state: &AppState, request: DumpRequest) -> Result<Uuid, DbError> {
    state
        .file_picks
        .check(FileAccess::DumpWrite, &request.path)?;
    state.sessions.dump_start(request).await
}

/// The default file name of a dump: `<base>.sql`, or `<base>.sql.gz` compressed.
pub fn dump_file_name(base_name: &str, compress: bool) -> String {
    let ext = if compress { "sql.gz" } else { "sql" };
    format!("{}.{ext}", file_stem(base_name))
}

/// The tables, views (PostgreSQL also stand-alone sequences and types) the built-in dump
/// can hold, and the objects it skips with the reason (R22).
#[tauri::command]
#[specta::specta]
pub async fn dump_objects(
    state: State<'_, AppState>,
    connection_id: Uuid,
    database: String,
    schemas: Vec<String>,
) -> Result<DumpObjects, AppError> {
    guard(dump_objects_impl(&state, connection_id, database, schemas)).await
}

/// Opens the save dialog for a dump file from the backend and admits the picked path for
/// `dump_start`. `None`: cancelled.
#[tauri::command]
#[specta::specta]
pub async fn dump_pick_path(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    base_name: String,
    compress: bool,
) -> Result<Option<String>, AppError> {
    guard(async move {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (filter, ext) = if compress {
            ("Compressed SQL dump", "gz")
        } else {
            ("SQL dump", "sql")
        };
        window
            .dialog()
            .file()
            .set_parent(&window)
            .set_file_name(dump_file_name(&base_name, compress))
            .add_filter(filter, &[ext])
            .save_file(move |picked| {
                let _ = tx.send(picked);
            });
        let picked = rx.await.map_err(|_| DbError::Internal {
            message: "the save dialog closed without an answer".into(),
        })?;
        let Some(picked) = picked else {
            return Ok(None);
        };
        let path = picked.into_path().map_err(|e| DbError::Internal {
            message: format!("the save dialog returned no file path: {e}"),
        })?;
        state
            .file_picks
            .admit(FileAccess::DumpWrite, path)
            .map(Some)
    })
    .await
}

/// Starts a dump job (no confirmation: a read, allowed on read-only connections);
/// returns its id.
#[tauri::command]
#[specta::specta]
pub async fn dump_start(
    state: State<'_, AppState>,
    request: DumpRequest,
) -> Result<Uuid, AppError> {
    guard(dump_start_impl(&state, request)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_file_names() {
        assert_eq!(dump_file_name("shop", false), "shop.sql");
        assert_eq!(dump_file_name("shop", true), "shop.sql.gz");
        assert_eq!(dump_file_name("a/b:c", false), "a_b_c.sql");
    }
}
