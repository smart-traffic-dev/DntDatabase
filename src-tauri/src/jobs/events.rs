//! The app's job event sink: every [`JobEvent`] becomes the Tauri app event
//! [`JOB_PROGRESS_EVENT`] (R4). Installed in `main.rs` `setup`.

use super::manager::JobEvents;
use super::model::JobEvent;
use tauri::{AppHandle, Emitter, Runtime};

/// Name of the app event carrying a [`JobEvent`].
pub const JOB_PROGRESS_EVENT: &str = "job://progress";

/// Emits job events to every webview of the app.
pub struct TauriJobEvents<R: Runtime>(pub AppHandle<R>);

impl<R: Runtime> JobEvents for TauriJobEvents<R> {
    fn emit(&self, event: &JobEvent) {
        if let Err(e) = self.0.emit(JOB_PROGRESS_EVENT, event) {
            // The UI also polls `job_list` while a job is live, so a lost event only
            // delays the panel.
            tracing::debug!(error = %e, "a job event was not delivered");
        }
    }
}
