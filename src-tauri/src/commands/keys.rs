//! The key browser of a Command connection (spec §5.2, §7.5): one page of keys, one
//! page of a key's value, and key edits.
//!
//! Scans and values are read through `Session::keyspace()` of a data session on the
//! requested db (the data view's pool), so the user's console tab is never switched.
//! A key edit is built by the driver (`DataAccess::build_key_edit`), judged and stored
//! as a collection edit is ([`SessionManager::key_edit_preview`]), and applied by
//! `data_apply` through the shared edit store: once, never retried, with History rows
//! redacted by the connection's language.
//!
//! [`SessionManager::key_edit_preview`]: crate::session_manager::SessionManager::key_edit_preview

use super::guard;
use crate::edit_plans::EditPreview;
use crate::session_manager::config_error;
use crate::state::AppState;
use dnt_core::data_access::KeyEdit;
use dnt_core::error::{AppError, DbError};
use dnt_core::keyspace::{KeyName, KeyPage, KeyValueView, Keyspace, ScanRequest, ValueRange};
use dnt_core::server::{check_cap, Cap, Capabilities, EngineFamily};
use tauri::State;
use uuid::Uuid;

/// Most keys one scan asks for (`ScanRequest::count` is clamped to `1..=MAX_SCAN_COUNT`).
pub const MAX_SCAN_COUNT: u32 = 1000;

fn no_key_browser() -> DbError {
    DbError::Unsupported {
        feature: "key browser".into(),
        version: String::new(),
    }
}

/// The capabilities of connection `id`, when it is a Command connection (else
/// `Unsupported { feature: "key browser" }`).
fn command_caps(state: &AppState, id: Uuid) -> Result<Capabilities, DbError> {
    let caps = state.sessions.capabilities(id)?;
    match caps.family {
        EngineFamily::Command => Ok(caps),
        EngineFamily::Sql => Err(no_key_browser()),
    }
}

/// `Unsupported { feature: "key browser" }` unless connection `id` browses keys
/// (`capabilities.paged_objects`).
fn require_key_browser(state: &AppState, id: Uuid) -> Result<(), DbError> {
    if command_caps(state, id)?.paged_objects {
        Ok(())
    } else {
        Err(no_key_browser())
    }
}

fn require_db(db: &str) -> Result<(), DbError> {
    if db.is_empty() {
        Err(config_error("database", "a database is required"))
    } else {
        Ok(())
    }
}

/// Runs `read` on the keyspace of a data session for `db` of connection `id`; the
/// session goes back to the pool unless the read failed.
async fn on_keyspace<T>(
    state: &AppState,
    id: Uuid,
    db: &str,
    read: impl for<'a> FnOnce(
        &'a dyn Keyspace,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, DbError>> + Send + 'a>,
    >,
) -> Result<T, DbError> {
    require_key_browser(state, id)?;
    require_db(db)?;
    let sessions = &state.sessions;
    let rt = sessions.runtime(id)?;
    let taken = sessions.take_data_session(&rt, Some(db)).await?;
    let result = match taken.session.keyspace() {
        Some(keyspace) => read(keyspace).await,
        None => Err(no_key_browser()),
    };
    let clean = result.is_ok();
    sessions.return_data_session(&rt, taken, clean);
    result
}

/// One page of the keys of `db` (`req.count` clamped to 1..=1000).
pub async fn keys_scan_impl(
    state: &AppState,
    id: Uuid,
    db: String,
    req: ScanRequest,
) -> Result<KeyPage, DbError> {
    let req = ScanRequest {
        count: req.count.clamp(1, MAX_SCAN_COUNT),
        ..req
    };
    let at = db.clone();
    on_keyspace(state, id, &db, move |k| {
        Box::pin(async move { k.scan(&at, &req).await })
    })
    .await
}

/// One page (`range`) of the value of `key` in `db`, with its TTL and memory use.
pub async fn key_value_impl(
    state: &AppState,
    id: Uuid,
    db: String,
    key: KeyName,
    range: ValueRange,
) -> Result<KeyValueView, DbError> {
    let at = db.clone();
    on_keyspace(state, id, &db, move |k| {
        Box::pin(async move { k.key_value(&at, &key, &range).await })
    })
    .await
}

/// The plan of one key edit in `db`; `data_apply(plan_id, confirmed)` runs it.
pub async fn key_edit_preview_impl(
    state: &AppState,
    id: Uuid,
    db: String,
    edit: KeyEdit,
) -> Result<EditPreview, DbError> {
    check_cap(&command_caps(state, id)?, Cap::DataEdit)?;
    state.sessions.key_edit_preview(id, &db, &edit).await
}

/// One page of the keys of `db` matching `req` (a glob `pattern`, a `key_type`).
/// `next_cursor: null`: the scan is complete.
#[tauri::command]
#[specta::specta]
pub async fn keys_scan(
    state: State<'_, AppState>,
    id: Uuid,
    db: String,
    req: ScanRequest,
) -> Result<KeyPage, AppError> {
    guard(keys_scan_impl(&state, id, db, req)).await
}

/// One page of the value of `key` in `db`.
#[tauri::command]
#[specta::specta]
pub async fn key_value(
    state: State<'_, AppState>,
    id: Uuid,
    db: String,
    key: KeyName,
    range: ValueRange,
) -> Result<KeyValueView, AppError> {
    guard(key_value_impl(&state, id, db, key, range)).await
}

/// Previews one key edit in `db`: the commands it sends and their verdict. Apply it
/// with `data_apply`.
#[tauri::command]
#[specta::specta]
pub async fn key_edit_preview(
    state: State<'_, AppState>,
    id: Uuid,
    db: String,
    edit: KeyEdit,
) -> Result<EditPreview, AppError> {
    guard(key_edit_preview_impl(&state, id, db, edit)).await
}
