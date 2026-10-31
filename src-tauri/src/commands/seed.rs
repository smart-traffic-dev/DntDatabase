//! Data seeder commands (data seeder spec §5.1): the target with inferred generators, the
//! live preview, the plan and the start.

use super::guard;
use crate::import_data::ImportConfirm;
use crate::seed_data::{SeedPreview, SeedRequest, SeedSample, SeedTarget};
use crate::state::AppState;
use dnt_core::error::AppError;
use dnt_core::meta::ObjectRef;
use tauri::State;
use uuid::Uuid;

/// The table's columns with the generator inferred for each, read on a short-lived session.
#[tauri::command]
#[specta::specta]
pub async fn seed_target(
    state: State<'_, AppState>,
    connection_id: Uuid,
    table: ObjectRef,
) -> Result<SeedTarget, AppError> {
    guard(state.sessions.seed_target(connection_id, table)).await
}

/// The first rows exactly as the job generates them; column problems come back as issues.
#[tauri::command]
#[specta::specta]
pub async fn seed_preview(
    state: State<'_, AppState>,
    request: SeedRequest,
) -> Result<SeedSample, AppError> {
    guard(state.sessions.seed_preview(request)).await
}

/// Reads and checks everything and returns the reviewed plan (read-only connections refused).
#[tauri::command]
#[specta::specta]
pub async fn seed_prepare(
    state: State<'_, AppState>,
    request: SeedRequest,
) -> Result<SeedPreview, AppError> {
    guard(state.sessions.seed_prepare(request)).await
}

/// Re-checks the plan and starts the seed job; returns its id.
#[tauri::command]
#[specta::specta]
pub async fn seed_start(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirm: ImportConfirm,
) -> Result<Uuid, AppError> {
    guard(state.sessions.seed_start(plan_id, confirm)).await
}
