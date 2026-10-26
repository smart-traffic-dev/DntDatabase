//! IPC boundary for typed connection-tree actions.
use super::guard;
use crate::object_actions::{ActionPreview, ActionReport, NamespaceOptions};
use crate::state::AppState;
use dnt_core::error::AppError;
use dnt_core::sql::ddl::ObjectAction;
use tauri::{ipc::Channel, State};
use uuid::Uuid;

#[tauri::command]
#[specta::specta]
pub async fn object_action_preview(
    state: State<'_, AppState>,
    connection_id: Uuid,
    action: ObjectAction,
) -> Result<ActionPreview, AppError> {
    guard(state.sessions.object_action_preview(connection_id, action)).await
}
#[tauri::command]
#[specta::specta]
pub async fn object_action_render(
    state: State<'_, AppState>,
    connection_id: Uuid,
    action: ObjectAction,
) -> Result<String, AppError> {
    guard(async { state.sessions.object_action_render(connection_id, &action) }).await
}
#[tauri::command]
#[specta::specta]
pub async fn namespace_options(
    state: State<'_, AppState>,
    connection_id: Uuid,
    database: String,
) -> Result<NamespaceOptions, AppError> {
    guard(state.sessions.namespace_options(connection_id, database)).await
}
#[tauri::command]
#[specta::specta]
pub async fn object_action_apply(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirmed: bool,
    name: Option<String>,
    operation_id: Uuid,
    channel: Channel<String>,
) -> Result<ActionReport, AppError> {
    guard(state.sessions.object_action_apply(
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
pub async fn object_action_cancel(
    state: State<'_, AppState>,
    operation_id: Uuid,
) -> Result<(), AppError> {
    guard(state.sessions.object_action_cancel(operation_id)).await
}
