//! Connection tree CRUD, connect / disconnect and SSH host key trust.
//!
//! Secrets arrive as plain strings in the IPC payload and are turned into `SecretString`
//! before anything else happens; the input types have no `Debug` so they cannot be logged.

use super::{blocking, guard};
use crate::session_manager::{ConnState, OpenResult};
use crate::state::AppState;
use dnt_core::config::ConnectionConfig;
use dnt_core::conn_string::ParsedConnString;
use dnt_core::error::{AppError, DbError};
use dnt_store::repo::{Group, NewSecrets, NodeId, SecretChange, SecretUpdate, Tree};
use dnt_tunnel::KnownHosts;
use secrecy::SecretString;
use serde::Deserialize;
use specta::Type;
use std::sync::Arc;
use tauri::State;
use uuid::Uuid;

/// Secrets typed by the user (create, test, or the password prompt of `open_connection`).
/// A missing or `null` field means "no secret".
#[derive(Deserialize, Type)]
pub struct SecretsInput {
    #[serde(default)]
    pub db_password: Option<String>,
    #[serde(default)]
    pub ssh_password: Option<String>,
    #[serde(default)]
    pub ssh_key_passphrase: Option<String>,
}

impl From<SecretsInput> for NewSecrets {
    fn from(s: SecretsInput) -> Self {
        NewSecrets {
            db_password: s.db_password.map(SecretString::from),
            ssh_password: s.ssh_password.map(SecretString::from),
            ssh_key_passphrase: s.ssh_key_passphrase.map(SecretString::from),
        }
    }
}

/// `"keep"` | `"clear"` | `{ "set": "<value>" }`.
#[derive(Deserialize, Type, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecretChangeInput {
    #[default]
    Keep,
    Clear,
    Set(String),
}

impl From<SecretChangeInput> for SecretChange {
    fn from(c: SecretChangeInput) -> Self {
        match c {
            SecretChangeInput::Keep => SecretChange::Keep,
            SecretChangeInput::Clear => SecretChange::Clear,
            SecretChangeInput::Set(value) => SecretChange::Set(SecretString::from(value)),
        }
    }
}

/// Per-secret changes of `update_connection`; a missing field keeps the stored secret.
#[derive(Deserialize, Type)]
pub struct SecretsUpdateInput {
    #[serde(default)]
    pub db_password: SecretChangeInput,
    #[serde(default)]
    pub ssh_password: SecretChangeInput,
    #[serde(default)]
    pub ssh_key_passphrase: SecretChangeInput,
}

impl From<SecretsUpdateInput> for SecretUpdate {
    fn from(s: SecretsUpdateInput) -> Self {
        SecretUpdate {
            db_password: s.db_password.into(),
            ssh_password: s.ssh_password.into(),
            ssh_key_passphrase: s.ssh_key_passphrase.into(),
        }
    }
}

// Repository calls hit SQLite and the OS keychain: they run on the blocking pool.

pub async fn list_tree_impl(state: &AppState) -> Result<Tree, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.tree()).await
}

pub async fn create_connection_impl(
    state: &AppState,
    cfg: ConnectionConfig,
    secrets: SecretsInput,
) -> Result<ConnectionConfig, DbError> {
    let secrets = NewSecrets::from(secrets);
    let repo = Arc::clone(&state.repo);
    let cfg = blocking(move || repo.create(cfg, secrets)).await?;
    tracing::info!(connection = %cfg.id, "connection created");
    Ok(cfg)
}

/// Closes the connection's runtime first: it would keep using the old settings.
pub async fn update_connection_impl(
    state: &AppState,
    cfg: ConnectionConfig,
    secrets: SecretsUpdateInput,
) -> Result<ConnectionConfig, DbError> {
    let secrets = SecretUpdate::from(secrets);
    // Invalid settings leave the connection (and its open runtime) untouched.
    cfg.validate()?;
    state.sessions.close(cfg.id).await;
    let repo = Arc::clone(&state.repo);
    let cfg = blocking(move || repo.update(cfg, secrets)).await?;
    tracing::info!(connection = %cfg.id, "connection updated");
    Ok(cfg)
}

pub async fn duplicate_connection_impl(
    state: &AppState,
    id: Uuid,
) -> Result<ConnectionConfig, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.duplicate(id)).await
}

pub async fn delete_connection_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    state.sessions.close(id).await;
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.delete(id)).await?;
    tracing::info!(connection = %id, "connection deleted");
    Ok(())
}

pub async fn create_group_impl(
    state: &AppState,
    name: String,
    parent_id: Option<Uuid>,
) -> Result<Group, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.create_group(&name, parent_id)).await
}

pub async fn rename_group_impl(state: &AppState, id: Uuid, name: String) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.rename_group(id, &name)).await
}

pub async fn delete_group_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.delete_group(id)).await
}

pub async fn move_node_impl(
    state: &AppState,
    node: NodeId,
    parent_id: Option<Uuid>,
    index: u32,
) -> Result<(), DbError> {
    let index = usize::try_from(index).unwrap_or(usize::MAX);
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.move_node(node, parent_id, index)).await
}

pub async fn test_connection_impl(
    state: &AppState,
    cfg: ConnectionConfig,
    secrets: SecretsInput,
) -> Result<OpenResult, DbError> {
    state.sessions.test(&cfg, secrets.into()).await
}

pub async fn open_connection_impl(
    state: &AppState,
    id: Uuid,
    prompt: Option<SecretsInput>,
) -> Result<OpenResult, DbError> {
    let prompt = prompt.map(NewSecrets::from);
    let opened = state.sessions.open(id, prompt).await;
    match &opened {
        Ok(r) => tracing::info!(connection = %id, version = %r.info.version_string, "connected"),
        // DbError messages never carry secrets.
        Err(e) => tracing::info!(connection = %id, error = %e, "connect failed"),
    }
    opened
}

pub async fn close_connection_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    state.sessions.close(id).await;
    Ok(())
}

pub async fn connection_state_impl(state: &AppState, id: Uuid) -> Result<ConnState, DbError> {
    Ok(state.sessions.state(id))
}

/// Trusts the host key the user confirmed after an `ssh_host_key_unknown` error.
pub async fn trust_host_key_impl(
    state: &AppState,
    host: String,
    port: u16,
    fingerprint: String,
    key_openssh: String,
) -> Result<(), DbError> {
    let known_hosts = KnownHosts::new(state.paths.known_hosts.clone());
    blocking(move || {
        known_hosts.trust(&host, port, &fingerprint, &key_openssh)?;
        tracing::info!(%host, port, %fingerprint, "SSH host key trusted");
        Ok(())
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn list_tree(state: State<'_, AppState>) -> Result<Tree, AppError> {
    guard(list_tree_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn create_connection(
    state: State<'_, AppState>,
    cfg: ConnectionConfig,
    secrets: SecretsInput,
) -> Result<ConnectionConfig, AppError> {
    guard(create_connection_impl(&state, cfg, secrets)).await
}

#[tauri::command]
#[specta::specta]
pub async fn update_connection(
    state: State<'_, AppState>,
    cfg: ConnectionConfig,
    secrets: SecretsUpdateInput,
) -> Result<ConnectionConfig, AppError> {
    guard(update_connection_impl(&state, cfg, secrets)).await
}

#[tauri::command]
#[specta::specta]
pub async fn duplicate_connection(
    state: State<'_, AppState>,
    id: Uuid,
) -> Result<ConnectionConfig, AppError> {
    guard(duplicate_connection_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn delete_connection(state: State<'_, AppState>, id: Uuid) -> Result<(), AppError> {
    guard(delete_connection_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn create_group(
    state: State<'_, AppState>,
    name: String,
    parent_id: Option<Uuid>,
) -> Result<Group, AppError> {
    guard(create_group_impl(&state, name, parent_id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn rename_group(
    state: State<'_, AppState>,
    id: Uuid,
    name: String,
) -> Result<(), AppError> {
    guard(rename_group_impl(&state, id, name)).await
}

#[tauri::command]
#[specta::specta]
pub async fn delete_group(state: State<'_, AppState>, id: Uuid) -> Result<(), AppError> {
    guard(delete_group_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn move_node(
    state: State<'_, AppState>,
    node: NodeId,
    parent_id: Option<Uuid>,
    index: u32,
) -> Result<(), AppError> {
    guard(move_node_impl(&state, node, parent_id, index)).await
}

#[tauri::command]
#[specta::specta]
pub async fn test_connection(
    state: State<'_, AppState>,
    cfg: ConnectionConfig,
    secrets: SecretsInput,
) -> Result<OpenResult, AppError> {
    guard(test_connection_impl(&state, cfg, secrets)).await
}

#[tauri::command]
#[specta::specta]
pub async fn open_connection(
    state: State<'_, AppState>,
    id: Uuid,
    prompt: Option<SecretsInput>,
) -> Result<OpenResult, AppError> {
    guard(open_connection_impl(&state, id, prompt)).await
}

#[tauri::command]
#[specta::specta]
pub async fn close_connection(state: State<'_, AppState>, id: Uuid) -> Result<(), AppError> {
    guard(close_connection_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn connection_state(state: State<'_, AppState>, id: Uuid) -> Result<ConnState, AppError> {
    guard(connection_state_impl(&state, id)).await
}

#[tauri::command]
#[specta::specta]
pub async fn trust_host_key(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    fingerprint: String,
    key_openssh: String,
) -> Result<(), AppError> {
    guard(trust_host_key_impl(
        &state,
        host,
        port,
        fingerprint,
        key_openssh,
    ))
    .await
}

/// Reads a pasted connection string into the dialog's fields (nothing is stored or logged).
#[tauri::command]
#[specta::specta]
pub async fn parse_connection_string(input: String) -> Result<ParsedConnString, AppError> {
    guard(async move { dnt_core::conn_string::parse_connection_string(&input) }).await
}
