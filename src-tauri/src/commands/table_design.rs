//! IPC boundary of the table designer. The UI sends `TableDef` drafts, never SQL.
use super::guard;
use crate::state::AppState;
use crate::table_design::{DesignOpen, DesignPreview, DesignRender, DesignReport};
use dnt_core::error::AppError;
use dnt_core::meta::Scope;
use dnt_schema::TableDef;
use tauri::{ipc::Channel, State};
use uuid::Uuid;

#[tauri::command]
#[specta::specta]
pub async fn table_design_open(
    state: State<'_, AppState>,
    connection_id: Uuid,
    scope: Scope,
    table: Option<String>,
) -> Result<DesignOpen, AppError> {
    guard(
        state
            .sessions
            .table_design_open(connection_id, scope, table),
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn table_design_render(
    state: State<'_, AppState>,
    design_id: Uuid,
    draft: TableDef,
) -> Result<DesignRender, AppError> {
    guard(state.sessions.table_design_render(design_id, draft)).await
}

#[tauri::command]
#[specta::specta]
pub async fn table_design_preview(
    state: State<'_, AppState>,
    design_id: Uuid,
    draft: TableDef,
) -> Result<DesignPreview, AppError> {
    guard(state.sessions.table_design_preview(design_id, draft)).await
}

#[tauri::command]
#[specta::specta]
pub async fn table_design_apply(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirmed: bool,
    name: Option<String>,
    operation_id: Uuid,
    channel: Channel<String>,
) -> Result<DesignReport, AppError> {
    guard(state.sessions.table_design_apply(
        plan_id,
        confirmed,
        name,
        operation_id,
        &move |stage| {
            let _ = channel.send(stage.to_string());
        },
    ))
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn table_design_cancel(
    state: State<'_, AppState>,
    operation_id: Uuid,
) -> Result<(), AppError> {
    guard(state.sessions.table_design_cancel(operation_id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn table_design_close(
    state: State<'_, AppState>,
    design_id: Uuid,
) -> Result<(), AppError> {
    guard(state.sessions.table_design_close(design_id)).await
}
