//! Favorites (saved queries) and user-defined editor snippets.
//!
//! The store refuses favorite SQL holding a secret literal (see `dnt_store::favorites`),
//! so the error reaches the UI as `AppError { kind: "config", field: "sql" }`.

use super::{blocking, guard};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_store::favorites::{Favorite, FavoriteInput, Snippet, SnippetInput};
use std::sync::Arc;
use tauri::State;
use uuid::Uuid;

pub async fn favorites_list_impl(state: &AppState) -> Result<Vec<Favorite>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.favorites_list()).await
}

pub async fn favorites_create_impl(
    state: &AppState,
    input: FavoriteInput,
) -> Result<Favorite, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.favorite_create(&input)).await
}

pub async fn favorites_update_impl(
    state: &AppState,
    id: Uuid,
    input: FavoriteInput,
) -> Result<Favorite, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.favorite_update(id, &input)).await
}

pub async fn favorites_rename_impl(
    state: &AppState,
    id: Uuid,
    name: String,
) -> Result<Favorite, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.favorite_rename(id, &name)).await
}

pub async fn favorites_move_impl(
    state: &AppState,
    id: Uuid,
    folder: Option<String>,
) -> Result<Favorite, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.favorite_move(id, folder.as_deref())).await
}

pub async fn favorites_delete_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.favorite_delete(id)).await
}

pub async fn snippets_list_impl(state: &AppState) -> Result<Vec<Snippet>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.snippets_list()).await
}

pub async fn snippets_create_impl(
    state: &AppState,
    input: SnippetInput,
) -> Result<Snippet, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.snippet_create(&input)).await
}

pub async fn snippets_update_impl(
    state: &AppState,
    id: Uuid,
    input: SnippetInput,
) -> Result<Snippet, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.snippet_update(id, &input)).await
}

pub async fn snippets_delete_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.snippet_delete(id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn favorites_list(state: State<'_, AppState>) -> Result<Vec<Favorite>, AppError> {
    guard(favorites_list_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn favorites_create(
    state: State<'_, AppState>,
    input: FavoriteInput,
) -> Result<Favorite, AppError> {
    guard(favorites_create_impl(&state, input)).await
}

#[tauri::command]
#[specta::specta]
pub async fn favorites_update(
    state: State<'_, AppState>,
    id: Uuid,
    input: FavoriteInput,
) -> Result<Favorite, AppError> {
    guard(favorites_update_impl(&state, id, input)).await
}

#[tauri::command]
#[specta::specta]
pub async fn favorites_rename(
    state: State<'_, AppState>,
    id: Uuid,
    name: String,
) -> Result<Favorite, AppError> {
    guard(favorites_rename_impl(&state, id, name)).await
}

#[tauri::command]
#[specta::specta]
pub async fn favorites_move(
    state: State<'_, AppState>,
    id: Uuid,
    folder: Option<String>,
) -> Result<Favorite, AppError> {
    guard(favorites_move_impl(&state, id, folder)).await
}

#[tauri::command]
#[specta::specta]
pub async fn favorites_delete(state: State<'_, AppState>, id: Uuid) -> Result<(), AppError> {
    guard(favorites_delete_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn snippets_list(state: State<'_, AppState>) -> Result<Vec<Snippet>, AppError> {
    guard(snippets_list_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn snippets_create(
    state: State<'_, AppState>,
    input: SnippetInput,
) -> Result<Snippet, AppError> {
    guard(snippets_create_impl(&state, input)).await
}

#[tauri::command]
#[specta::specta]
pub async fn snippets_update(
    state: State<'_, AppState>,
    id: Uuid,
    input: SnippetInput,
) -> Result<Snippet, AppError> {
    guard(snippets_update_impl(&state, id, input)).await
}

#[tauri::command]
#[specta::specta]
pub async fn snippets_delete(state: State<'_, AppState>, id: Uuid) -> Result<(), AppError> {
    guard(snippets_delete_impl(&state, id)).await
}
