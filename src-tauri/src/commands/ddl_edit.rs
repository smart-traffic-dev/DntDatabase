//! IPC boundary of DDL editing. The UI sends the edited text and a strategy, never
//! statements.
use super::guard;
use crate::ddl_edit::{DdlEditOpen, DdlEditPreview};
use crate::state::AppState;
use dnt_core::error::AppError;
use dnt_core::meta::{ObjectKind, Scope};
use dnt_core::sql::ddl_edit::Strategy;
use tauri::State;
use uuid::Uuid;

#[tauri::command]
#[specta::specta]
pub async fn ddl_edit_open(
    state: State<'_, AppState>,
    connection_id: Uuid,
    scope: Scope,
    kind: ObjectKind,
    name: Option<String>,
    table: Option<String>,
) -> Result<DdlEditOpen, AppError> {
    guard(
        state
            .sessions
            .ddl_edit_open(connection_id, scope, kind, name, table),
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn ddl_edit_preview(
    state: State<'_, AppState>,
    edit_id: Uuid,
    text: String,
    strategy: Option<Strategy>,
) -> Result<DdlEditPreview, AppError> {
    guard(state.sessions.ddl_edit_preview(edit_id, text, strategy)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ddl_edit_cancel(
    state: State<'_, AppState>,
    operation_id: Uuid,
) -> Result<(), AppError> {
    guard(state.sessions.ddl_edit_cancel(operation_id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ddl_edit_close(state: State<'_, AppState>, edit_id: Uuid) -> Result<(), AppError> {
    guard(state.sessions.ddl_edit_close(edit_id)).await
}
