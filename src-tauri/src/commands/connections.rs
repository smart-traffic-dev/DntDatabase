//! Connection tree CRUD, connect / disconnect and SSH host key trust.
//!
//! Secrets arrive as plain strings in the IPC payload and are turned into `SecretString`
//! before anything else happens; the input types have no `Debug` so they cannot be logged.

use super::guard;
use crate::session_manager::{ConnState, OpenResult};
use crate::state::AppState;
use dnt_core::config::ConnectionConfig;
use dnt_core::error::{AppError, DbError};
use dnt_store::repo::{Group, NewSecrets, NodeId, SecretChange, SecretUpdate, Tree};
use dnt_tunnel::KnownHosts;
use secrecy::SecretString;
use serde::Deserialize;
use specta::Type;
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

pub async fn list_tree_impl(state: &AppState) -> Result<Tree, DbError> {
    state.repo.tree()
}

pub async fn create_connection_impl(
    state: &AppState,
    cfg: ConnectionConfig,
    secrets: SecretsInput,
) -> Result<ConnectionConfig, DbError> {
    let secrets = NewSecrets::from(secrets);
    let cfg = state.repo.create(cfg, secrets)?;
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
    let cfg = state.repo.update(cfg, secrets)?;
    tracing::info!(connection = %cfg.id, "connection updated");
    Ok(cfg)
}

pub async fn duplicate_connection_impl(
    state: &AppState,
    id: Uuid,
) -> Result<ConnectionConfig, DbError> {
    state.repo.duplicate(id)
}

pub async fn delete_connection_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    state.sessions.close(id).await;
    state.repo.delete(id)?;
    tracing::info!(connection = %id, "connection deleted");
    Ok(())
}

pub async fn create_group_impl(
    state: &AppState,
    name: String,
    parent_id: Option<Uuid>,
) -> Result<Group, DbError> {
    state.repo.create_group(&name, parent_id)
}

pub async fn rename_group_impl(state: &AppState, id: Uuid, name: String) -> Result<(), DbError> {
    state.repo.rename_group(id, &name)
}

pub async fn delete_group_impl(state: &AppState, id: Uuid) -> Result<(), DbError> {
    state.repo.delete_group(id)
}

pub async fn move_node_impl(
    state: &AppState,
    node: NodeId,
    parent_id: Option<Uuid>,
    index: u32,
) -> Result<(), DbError> {
    let index = usize::try_from(index).unwrap_or(usize::MAX);
    state.repo.move_node(node, parent_id, index)
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
    KnownHosts::new(state.paths.known_hosts.clone()).trust(
        &host,
        port,
        &fingerprint,
        &key_openssh,
    )?;
    tracing::info!(%host, port, %fingerprint, "SSH host key trusted");
    Ok(())
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
