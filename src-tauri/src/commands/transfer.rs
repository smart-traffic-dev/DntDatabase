//! Export to `.dntconn` and import from `.dntconn` or Navicat `.ncx` (format detected by
//! content). Importing only writes the repository; the UI reloads the tree afterwards.

use super::{blocking, guard};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_store::dntconn::{dntconn_has_secrets, export_dntconn, parse_dntconn};
use dnt_store::ncx::parse_ncx;
use dnt_store::repo::NodeId;
use dnt_store::transfer::{self, ConflictPolicy, ImportBundle, ImportPreview, ImportResult};
use secrecy::SecretString;
use std::path::Path;
use std::sync::Arc;
use tauri::State;

/// The `format` marker of a `.dntconn` file.
const DNTCONN_FORMAT: &str = "dntdatabase-connections";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Dntconn,
    Ncx,
}

/// `.dntconn` is JSON with `"format": "dntdatabase-connections"`; `.ncx` is XML with a
/// `<Connections` root. Anything else is rejected.
pub fn detect_format(text: &str) -> Result<ImportFormat, DbError> {
    let text = text.trim_start();
    if text.starts_with('{') {
        let format = serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|v| v.get("format")?.as_str().map(str::to_string));
        if format.as_deref() == Some(DNTCONN_FORMAT) {
            return Ok(ImportFormat::Dntconn);
        }
    } else if text.starts_with('<') && text.to_ascii_lowercase().contains("<connections") {
        return Ok(ImportFormat::Ncx);
    }
    Err(DbError::Config {
        field: Some("path".into()),
        message: "The file is neither a DntDatabase export (.dntconn) nor a Navicat export (.ncx)."
            .into(),
    })
}

/// A file the user chose could not be read or written: shown at the file field.
fn path_error(action: &str, path: &str, e: &std::io::Error) -> DbError {
    let path = Path::new(path);
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    DbError::Config {
        field: Some("path".into()),
        message: format!("Could not {action} {name}: {}", e.kind()),
    }
}

/// File contents without a UTF-8 byte order mark.
fn read_file(path: &str) -> Result<String, DbError> {
    let text =
        std::fs::read_to_string(Path::new(path)).map_err(|e| path_error("read", path, &e))?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

fn read_bundle(path: &str, passphrase: Option<String>) -> Result<ImportBundle, DbError> {
    let text = read_file(path)?;
    let passphrase = passphrase.map(SecretString::from);
    match detect_format(&text)? {
        ImportFormat::Dntconn => parse_dntconn(&text, passphrase.as_ref()),
        ImportFormat::Ncx => parse_ncx(&text),
    }
}

/// Writes the selected nodes to `path`; secrets are included (encrypted) only with a
/// passphrase.
pub async fn export_connections_impl(
    state: &AppState,
    ids: Vec<NodeId>,
    passphrase: Option<String>,
    path: String,
) -> Result<(), DbError> {
    let passphrase = passphrase.map(SecretString::from);
    let repo = Arc::clone(&state.repo);
    // Argon2id key derivation, keychain reads and file I/O: on the blocking pool.
    blocking(move || {
        let text = export_dntconn(&repo, &ids, passphrase.as_ref())?;
        std::fs::write(Path::new(&path), text).map_err(|e| path_error("write", &path, &e))?;
        tracing::info!(
            nodes = ids.len(),
            with_secrets = passphrase.is_some(),
            "connections exported"
        );
        Ok(())
    })
    .await
}

pub async fn preview_import_impl(
    state: &AppState,
    path: String,
    passphrase: Option<String>,
) -> Result<ImportPreview, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || {
        let bundle = read_bundle(&path, passphrase)?;
        transfer::preview(&repo, &bundle)
    })
    .await
}

pub async fn apply_import_impl(
    state: &AppState,
    path: String,
    passphrase: Option<String>,
    policy: ConflictPolicy,
) -> Result<ImportResult, DbError> {
    let repo = Arc::clone(&state.repo);
    let result = blocking(move || {
        let bundle = read_bundle(&path, passphrase)?;
        transfer::apply(&repo, bundle, policy)
    })
    .await?;
    // Like `update_connection`: an open runtime would keep using the replaced settings.
    for id in &result.overwritten_ids {
        state.sessions.close(*id).await;
    }
    tracing::info!(
        imported = result.imported,
        overwritten = result.overwritten,
        skipped = result.skipped,
        "connections imported"
    );
    Ok(result)
}

/// Whether the file carries encrypted secrets (`.ncx` never needs a passphrase).
pub async fn import_needs_passphrase_impl(path: String) -> Result<bool, DbError> {
    blocking(move || {
        let text = read_file(&path)?;
        match detect_format(&text)? {
            ImportFormat::Dntconn => dntconn_has_secrets(&text),
            ImportFormat::Ncx => Ok(false),
        }
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn export_connections(
    state: State<'_, AppState>,
    ids: Vec<NodeId>,
    passphrase: Option<String>,
    path: String,
) -> Result<(), AppError> {
    guard(export_connections_impl(&state, ids, passphrase, path)).await
}

#[tauri::command]
#[specta::specta]
pub async fn preview_import(
    state: State<'_, AppState>,
    path: String,
    passphrase: Option<String>,
) -> Result<ImportPreview, AppError> {
    guard(preview_import_impl(&state, path, passphrase)).await
}

#[tauri::command]
#[specta::specta]
pub async fn apply_import(
    state: State<'_, AppState>,
    path: String,
    passphrase: Option<String>,
    policy: ConflictPolicy,
) -> Result<ImportResult, AppError> {
    guard(apply_import_impl(&state, path, passphrase, policy)).await
}

#[tauri::command]
#[specta::specta]
pub async fn import_needs_passphrase(path: String) -> Result<bool, AppError> {
    guard(import_needs_passphrase_impl(path)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_format_by_content() {
        assert_eq!(
            detect_format(r#"  {"format": "dntdatabase-connections", "version": 1}"#).unwrap(),
            ImportFormat::Dntconn
        );
        assert_eq!(
            detect_format("<?xml version=\"1.0\"?>\n<Connections Ver=\"1.5\"></Connections>")
                .unwrap(),
            ImportFormat::Ncx
        );
        for other in [r#"{"format": "other"}"#, "{not json", "plain text", ""] {
            assert!(
                matches!(detect_format(other), Err(DbError::Config { .. })),
                "{other:?}"
            );
        }
    }
}
