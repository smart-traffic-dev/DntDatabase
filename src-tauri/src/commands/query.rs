//! Query editor commands. For now: autocomplete data (spec §7.3).
//!
//! `completion_scope` returns the SchemaCache of the database a query tab is on (loaded on
//! first use, one load per database at a time). `completion_objects` lists the tables and
//! views after `db.` (MySQL) / `schema.` (PostgreSQL) through the tree's browse cache.

use super::guard;
use crate::cache::{CompletionObject, SchemaCache};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use tauri::State;
use uuid::Uuid;

pub async fn completion_scope_impl(
    state: &AppState,
    conn_id: Uuid,
    database: String,
) -> Result<SchemaCache, DbError> {
    state.sessions.schema_cache_for(conn_id, &database).await
}

pub async fn completion_objects_impl(
    state: &AppState,
    conn_id: Uuid,
    database: String,
    schema: Option<String>,
) -> Result<Vec<CompletionObject>, DbError> {
    state
        .sessions
        .completion_objects(conn_id, &database, schema.as_deref())
        .await
}

/// The SchemaCache of `database` for autocomplete. The whole scope comes back in one
/// payload: the UI must cache it per (tab connection, database) and fetch it again only
/// after a refresh or when the tab changes database, never per keystroke. Errors: an
/// empty `database` is a `config` error; a failed load is returned again (without a new
/// load) for 10 s.
#[tauri::command]
#[specta::specta]
pub async fn completion_scope(
    state: State<'_, AppState>,
    conn_id: Uuid,
    database: String,
) -> Result<SchemaCache, AppError> {
    guard(completion_scope_impl(&state, conn_id, database)).await
}

/// Tables and views after `db.` (MySQL: `schema` must be null) or `schema.` (PostgreSQL:
/// `schema` is required). Shares the object tree's cache.
#[tauri::command]
#[specta::specta]
pub async fn completion_objects(
    state: State<'_, AppState>,
    conn_id: Uuid,
    database: String,
    schema: Option<String>,
) -> Result<Vec<CompletionObject>, AppError> {
    guard(completion_objects_impl(&state, conn_id, database, schema)).await
}
