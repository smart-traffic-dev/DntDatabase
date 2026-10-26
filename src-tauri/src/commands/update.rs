//! In-app updates (Help > Check for updates): `update_check` asks the release endpoint of
//! `tauri.conf.json` (`latest.json` of the newest GitHub release) whether a newer signed build
//! exists, and keeps it; `update_install` downloads that build, checks its signature and runs
//! its installer, which closes the app and starts the new version.

use super::guard;
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Mutex;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use time::format_description::well_known::Rfc3339;

/// The update `update_check` found, kept for `update_install`.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

/// A newer version than the running one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    /// The release notes, as written in the release.
    pub notes: Option<String>,
    /// RFC 3339 publication date.
    pub date: Option<String>,
}

/// Bytes of the installer downloaded so far, and its size when the server sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct UpdateProgress {
    #[specta(type = f64)]
    pub downloaded: u64,
    #[specta(type = Option<f64>)]
    pub total: Option<u64>,
}

fn unreachable_endpoint(e: impl std::fmt::Display) -> DbError {
    DbError::Connect {
        message: format!("Could not reach the update server: {e}"),
    }
}

/// The installer closes the app: never while a job (export, import, dump, restore) is live.
pub fn install_allowed(active_jobs: u32) -> Result<(), DbError> {
    if active_jobs == 0 {
        return Ok(());
    }
    Err(DbError::Config {
        field: None,
        message: format!(
            "{active_jobs} background job(s) are still running. Wait for them to finish or stop them, then install the update."
        ),
    })
}

/// The newer version, if any; it is kept for `update_install`.
#[tauri::command]
#[specta::specta]
pub async fn update_check(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> Result<Option<UpdateInfo>, AppError> {
    guard(async {
        let update = app
            .updater()
            .map_err(unreachable_endpoint)?
            .check()
            .await
            .map_err(unreachable_endpoint)?;
        let info = update.as_ref().map(|u| UpdateInfo {
            version: u.version.clone(),
            current_version: u.current_version.clone(),
            notes: u.body.clone().filter(|b| !b.trim().is_empty()),
            date: u.date.and_then(|d| d.format(&Rfc3339).ok()),
        });
        *pending.0.lock().unwrap_or_else(|p| p.into_inner()) = update;
        Ok(info)
    })
    .await
}

/// Downloads and installs the update the last `update_check` found, then restarts.
/// Refused while a background job runs; download progress goes through `channel`.
#[tauri::command]
#[specta::specta]
pub async fn update_install(
    app: AppHandle,
    state: State<'_, AppState>,
    pending: State<'_, PendingUpdate>,
    channel: Channel<UpdateProgress>,
) -> Result<(), AppError> {
    guard(async {
        install_allowed(state.sessions.jobs.active_count())?;
        let update = pending
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| DbError::Config {
                field: None,
                message: "No update is ready to install; check for updates again.".into(),
            })?;
        tracing::info!(version = %update.version, "installing an update");
        let mut downloaded = 0u64;
        update
            .download_and_install(
                |chunk, total| {
                    downloaded += chunk as u64;
                    let _ = channel.send(UpdateProgress { downloaded, total });
                },
                || {},
            )
            .await
            .map_err(|e| DbError::Internal {
                message: format!("The update could not be installed: {e}"),
            })?;
        // On Windows the installer has already closed the app; elsewhere start the new build.
        app.restart()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_waits_for_live_jobs() {
        assert!(install_allowed(0).is_ok());
        let err = install_allowed(2).unwrap_err();
        assert!(
            matches!(&err, DbError::Config { message, .. } if message.starts_with("2 background job(s)"))
        );
    }
}
