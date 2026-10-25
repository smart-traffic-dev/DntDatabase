//! Dump commands (spec §7.1, §7.2): the objects a database offers, the backend's save
//! dialog, the start of a dump job, the external tools found and the start of a tool
//! dump. `dump_start` and `tool_dump_start` accept only a path picked in
//! `dump_pick_path`.

use super::export::file_stem;
use super::guard;
use crate::backup::{
    DumpObjects, DumpRequest, RestorePreview, RestoreRequest, ToolDumpRequest, ToolsList,
};
use crate::file_picks::FileAccess;
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_transfer::tools::PgDumpFormat;
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

/// The save dialog's file name, filter name and filter extension of a dump: a pg_dump
/// archive (`custom` / `tar`) gets `.dump` / `.tar` (already compressed or not, never
/// gzipped by the app); otherwise `.sql` / `.sql.gz` per `compress`.
pub fn dump_file_kind(
    base_name: &str,
    compress: bool,
    format: Option<PgDumpFormat>,
) -> (String, &'static str, &'static str) {
    match format {
        Some(PgDumpFormat::Custom) => (
            format!("{}.dump", file_stem(base_name)),
            "pg_dump custom archive",
            "dump",
        ),
        Some(PgDumpFormat::Tar) => (
            format!("{}.tar", file_stem(base_name)),
            "pg_dump tar archive",
            "tar",
        ),
        _ if compress => (dump_file_name(base_name, true), "Compressed SQL dump", "gz"),
        _ => (dump_file_name(base_name, false), "SQL dump", "sql"),
    }
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
/// `dump_start` / `tool_dump_start`. `format`: a pg_dump tool dump's format (`custom` /
/// `tar` name the file `.dump` / `.tar`); `null` for SQL output. `None`: cancelled.
#[tauri::command]
#[specta::specta]
pub async fn dump_pick_path(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    base_name: String,
    compress: bool,
    format: Option<PgDumpFormat>,
) -> Result<Option<String>, AppError> {
    guard(async move {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (file_name, filter, ext) = dump_file_kind(&base_name, compress, format);
        window
            .dialog()
            .file()
            .set_parent(&window)
            .set_file_name(file_name)
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

pub async fn tools_list_impl(state: &AppState) -> Result<ToolsList, DbError> {
    let repo = std::sync::Arc::clone(&state.repo);
    let configured = tokio::task::spawn_blocking(move || {
        crate::commands::settings::saved_external_tools_dir(&repo)
    })
    .await
    .map_err(|e| DbError::Internal {
        message: format!("reading the external tools folder failed: {e}"),
    })?;
    let path_env = std::env::var_os("PATH");
    Ok(crate::backup::ext_tools::list_tools(configured.as_deref(), path_env.as_deref()).await)
}

pub async fn tool_dump_start_impl(
    state: &AppState,
    request: ToolDumpRequest,
) -> Result<Uuid, DbError> {
    state
        .file_picks
        .check(FileAccess::DumpWrite, &request.path)?;
    state.sessions.tool_dump_start(request).await
}

/// The external tools found (the External tools folder first, then `PATH`) with their
/// version, the missing ones and the folders searched (R25).
#[tauri::command]
#[specta::specta]
pub async fn tools_list(state: State<'_, AppState>) -> Result<ToolsList, AppError> {
    guard(tools_list_impl(&state)).await
}

/// Starts a dump through `mysqldump`, `mariadb-dump` or `pg_dump` (a read: no
/// confirmation, allowed on read-only connections); returns its id. The path must come
/// from `dump_pick_path`.
#[tauri::command]
#[specta::specta]
pub async fn tool_dump_start(
    state: State<'_, AppState>,
    request: ToolDumpRequest,
) -> Result<Uuid, AppError> {
    guard(tool_dump_start_impl(&state, request)).await
}

pub async fn restore_preview_impl(
    state: &AppState,
    request: RestoreRequest,
) -> Result<RestorePreview, DbError> {
    state
        .file_picks
        .check(FileAccess::RestoreRead, &request.path)?;
    state.sessions.restore_preview(request).await
}

pub async fn restore_start_impl(
    state: &AppState,
    plan_id: Uuid,
    confirmed: bool,
    name: Option<String>,
) -> Result<Uuid, DbError> {
    state.sessions.restore_start(plan_id, confirmed, name).await
}

/// Opens the Open dialog for a file to restore from the backend and admits the picked
/// path for `restore_preview`. `None`: cancelled. The restore path is chosen by content,
/// not by extension.
#[tauri::command]
#[specta::specta]
pub async fn restore_pick_file(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<String>, AppError> {
    guard(async {
        let (tx, rx) = tokio::sync::oneshot::channel();
        window
            .dialog()
            .file()
            .set_parent(&window)
            .add_filter("Dumps", &["sql", "gz", "dump", "backup", "tar"])
            .add_filter("All files", &["*"])
            .pick_file(move |picked| {
                let _ = tx.send(picked);
            });
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
            .admit(FileAccess::RestoreRead, path)
            .map(Some)
    })
    .await
}

/// Analyzes a file for a restore (path, statistics or TOC, command line, warnings,
/// confirmation) and stores a single-use plan. Nothing is sent to the target.
#[tauri::command]
#[specta::specta]
pub async fn restore_preview(
    state: State<'_, AppState>,
    request: RestoreRequest,
) -> Result<RestorePreview, AppError> {
    guard(restore_preview_impl(&state, request)).await
}

/// Re-checks the plan and starts the restore job; returns its id. Production needs the
/// database name typed in `name`.
#[tauri::command]
#[specta::specta]
pub async fn restore_start(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirmed: bool,
    name: Option<String>,
) -> Result<Uuid, AppError> {
    guard(restore_start_impl(&state, plan_id, confirmed, name)).await
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

    #[test]
    fn dump_file_kinds() {
        assert_eq!(
            dump_file_kind("shop", false, None),
            ("shop.sql".to_string(), "SQL dump", "sql")
        );
        assert_eq!(
            dump_file_kind("shop", true, Some(PgDumpFormat::Plain)),
            ("shop.sql.gz".to_string(), "Compressed SQL dump", "gz")
        );
        assert_eq!(
            dump_file_kind("shop", true, Some(PgDumpFormat::Custom)).0,
            "shop.dump"
        );
        assert_eq!(
            dump_file_kind("shop", false, Some(PgDumpFormat::Tar)).0,
            "shop.tar"
        );
    }
}
