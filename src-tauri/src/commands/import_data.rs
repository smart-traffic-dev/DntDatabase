//! Data import commands (spec §6.5): the backend's file dialog, the file preview, the
//! target read, the pure mapping helpers, the dry run, the plan and the start. Every
//! command that reads the file accepts only a path picked in `import_pick_file`.

use super::{blocking, guard};
use crate::file_picks::FileAccess;
use crate::import_data::{
    DryRunRequest, ImportConfirm, ImportPreview, ImportRequest, ImportTarget,
};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::ObjectRef;
use dnt_transfer::convert::TargetColumn;
use dnt_transfer::mapping::{
    auto_map, check_mapping, ImportMode, KeyChoice, Mapping, MappingReport,
};
use dnt_transfer::read::{inspect, Inspection, SourceOptions};
use std::path::PathBuf;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

fn file_error(message: String) -> DbError {
    DbError::Config {
        field: Some("file".into()),
        message,
    }
}

/// The first 100 records of an admitted file.
pub async fn import_inspect_impl(
    state: &AppState,
    path: String,
    source: SourceOptions,
) -> Result<Inspection, DbError> {
    state.file_picks.check(FileAccess::ImportRead, &path)?;
    blocking(move || inspect(&PathBuf::from(path), &source).map_err(|e| file_error(e.to_string())))
        .await
}

pub async fn import_target_impl(
    state: &AppState,
    connection_id: Uuid,
    table: ObjectRef,
) -> Result<ImportTarget, DbError> {
    state.sessions.import_target(connection_id, table).await
}

pub async fn import_dry_run_impl(
    state: &AppState,
    request: DryRunRequest,
) -> Result<Uuid, DbError> {
    state
        .file_picks
        .check(FileAccess::ImportRead, &request.request.path)?;
    state.sessions.import_dry_run(request).await
}

pub async fn import_prepare_impl(
    state: &AppState,
    request: ImportRequest,
) -> Result<ImportPreview, DbError> {
    state
        .file_picks
        .check(FileAccess::ImportRead, &request.path)?;
    state.sessions.import_prepare(request).await
}

pub async fn import_start_impl(
    state: &AppState,
    plan_id: Uuid,
    confirm: ImportConfirm,
) -> Result<Uuid, DbError> {
    state.sessions.import_start(plan_id, confirm).await
}

/// Opens the Open dialog for a data file (CSV/TSV/TXT, JSON/JSONL/NDJSON, XLSX) from the
/// backend and admits the picked path for the import commands. `None`: cancelled.
#[tauri::command]
#[specta::specta]
pub async fn import_pick_file(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Option<String>, AppError> {
    guard(async {
        let (tx, rx) = tokio::sync::oneshot::channel();
        window
            .dialog()
            .file()
            .set_parent(&window)
            .add_filter(
                "Data files",
                &["csv", "tsv", "txt", "json", "jsonl", "ndjson", "xlsx"],
            )
            .add_filter("CSV / TSV", &["csv", "tsv", "txt"])
            .add_filter("JSON / JSON Lines", &["json", "jsonl", "ndjson"])
            .add_filter("Excel workbook", &["xlsx"])
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
            .admit(FileAccess::ImportRead, path)
            .map(Some)
    })
    .await
}

/// The file's columns and first 100 records (only a file picked in `import_pick_file`).
#[tauri::command]
#[specta::specta]
pub async fn import_inspect(
    state: State<'_, AppState>,
    path: String,
    source: SourceOptions,
) -> Result<Inspection, AppError> {
    guard(import_inspect_impl(&state, path, source)).await
}

/// The target table's columns (with the driver's types), upsert keys and transaction
/// policy, read on a short-lived dedicated session.
#[tauri::command]
#[specta::specta]
pub async fn import_target(
    state: State<'_, AppState>,
    connection_id: Uuid,
    table: ObjectRef,
) -> Result<ImportTarget, AppError> {
    guard(import_target_impl(&state, connection_id, table)).await
}

/// The automatic mapping of source columns to target columns (no database).
#[tauri::command]
#[specta::specta]
pub async fn import_auto_map(
    source: Vec<String>,
    targets: Vec<TargetColumn>,
) -> Result<Vec<Mapping>, AppError> {
    Ok(auto_map(&source, &targets))
}

/// The mapping's errors and warnings for `mode` (no database).
#[tauri::command]
#[specta::specta]
pub async fn import_check_mapping(
    mapping: Vec<Mapping>,
    targets: Vec<TargetColumn>,
    keys: Vec<KeyChoice>,
    mode: ImportMode,
    override_identity: bool,
) -> Result<MappingReport, AppError> {
    Ok(check_mapping(
        &mapping,
        &targets,
        &keys,
        &mode,
        override_identity,
    ))
}

/// Starts a dry-run job; returns its id.
#[tauri::command]
#[specta::specta]
pub async fn import_dry_run(
    state: State<'_, AppState>,
    request: DryRunRequest,
) -> Result<Uuid, AppError> {
    guard(import_dry_run_impl(&state, request)).await
}

/// Reads the target and the file and returns the reviewed plan.
#[tauri::command]
#[specta::specta]
pub async fn import_prepare(
    state: State<'_, AppState>,
    request: ImportRequest,
) -> Result<ImportPreview, AppError> {
    guard(import_prepare_impl(&state, request)).await
}

/// Re-checks the plan and starts the import job; returns its id.
#[tauri::command]
#[specta::specta]
pub async fn import_start(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirm: ImportConfirm,
) -> Result<Uuid, AppError> {
    guard(import_start_impl(&state, plan_id, confirm)).await
}
