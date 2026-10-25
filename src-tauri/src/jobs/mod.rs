//! N5 job framework: long transfers (import, dump, restore, tracked exports) run as
//! background jobs with progress, Stop, a log, events and persistence (spec §4).
//!
//! - [`model`]: the types the UI sees (`JobSummary`, `JobDetail`, `JobEvent`, …).
//!   `JobParams` carries each kind's request as a `serde_json::Value` (specta's
//!   `serde_json` feature exports it as `JsonValue`), so this module does not depend on
//!   the import/backup modules; the UI casts the payload per kind.
//! - [`manager`]: [`JobManager`] (per-connection run limit, FIFO queue, Stop, events).
//! - `persist`: `JobRecord` ↔ model and the R6 log trimming.

pub mod manager;
pub mod model;
mod persist;

#[cfg(test)]
mod tests;

pub use manager::{
    jobs_per_connection, JobCtx, JobDone, JobEvents, JobFailure, JobManager, JobRunner, JobSpec,
    TrackedJob, DEFAULT_JOBS_PER_CONNECTION, EVENT_INTERVAL, INTERRUPTED_SUMMARY,
    JOBS_PER_CONNECTION_KEY, KEEP_FINISHED, MAX_JOBS_PER_CONNECTION, MAX_LIVE_LOG_LINES,
    MIN_JOBS_PER_CONNECTION, PERSIST_LOG_BYTES, PERSIST_LOG_LINES,
};
pub use model::{
    JobDetail, JobEvent, JobKind, JobLogLevel, JobLogLine, JobParams, JobProgress, JobState,
    JobSummary, ProgressUnit,
};
