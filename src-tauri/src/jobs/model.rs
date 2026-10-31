//! Job types the UI sees (spec §4.1). Every `u64` that crosses IPC is an `f64`.

use dnt_core::error::AppError;
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Import,
    ImportDryRun,
    Dump,
    ToolDump,
    Restore,
    ToolRestore,
    Export,
    /// The data seeder.
    Seed,
}

impl JobKind {
    pub const ALL: [JobKind; 8] = [
        JobKind::Import,
        JobKind::ImportDryRun,
        JobKind::Dump,
        JobKind::ToolDump,
        JobKind::Restore,
        JobKind::ToolRestore,
        JobKind::Export,
        JobKind::Seed,
    ];

    /// The stored name (`jobs.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Import => "import",
            JobKind::ImportDryRun => "import_dry_run",
            JobKind::Dump => "dump",
            JobKind::ToolDump => "tool_dump",
            JobKind::Restore => "restore",
            JobKind::ToolRestore => "tool_restore",
            JobKind::Export => "export",
            JobKind::Seed => "seed",
        }
    }

    pub fn parse(s: &str) -> Option<JobKind> {
        JobKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub const ALL: [JobState; 5] = [
        JobState::Queued,
        JobState::Running,
        JobState::Done,
        JobState::Failed,
        JobState::Cancelled,
    ];

    /// The stored name (`jobs.state`).
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<JobState> {
        JobState::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Queued or running.
    pub fn is_live(self) -> bool {
        matches!(self, JobState::Queued | JobState::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    Rows,
    Bytes,
    Statements,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct JobProgress {
    /// What the job is doing ("Reading file", "Rows", …); empty before it reports.
    pub phase: String,
    pub done: f64,
    pub total: Option<f64>,
    pub unit: ProgressUnit,
}

impl Default for JobProgress {
    fn default() -> Self {
        JobProgress {
            phase: String::new(),
            done: 0.0,
            total: None,
            unit: ProgressUnit::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobLogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct JobLogLine {
    /// RFC 3339 UTC with milliseconds.
    pub at: String,
    pub level: JobLogLevel,
    pub message: String,
    /// The message carries a server message (R6): persisted as `Server error <code>`.
    #[serde(skip)]
    pub(crate) server: bool,
    #[serde(skip)]
    pub(crate) server_code: Option<String>,
}

impl JobLogLine {
    pub fn new(at: String, level: JobLogLevel, message: String) -> Self {
        JobLogLine {
            at,
            level,
            message,
            server: false,
            server_code: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct JobSummary {
    pub id: Uuid,
    pub kind: JobKind,
    pub connection_id: Option<Uuid>,
    pub title: String,
    pub state: JobState,
    pub progress: JobProgress,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub result_path: Option<String>,
    pub summary: Option<String>,
    pub error: Option<AppError>,
    pub outcome_unknown: bool,
    pub rerunnable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct JobDetail {
    pub job: JobSummary,
    pub log: Vec<JobLogLine>,
    /// Log lines not kept (memory keeps the first and last 1,000; the store 250 + 250).
    pub dropped_lines: u32,
    pub params: Option<JobParams>,
}

/// Payload of the `job://progress` app event.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct JobEvent {
    pub job: JobSummary,
    /// Log lines added since the previous event of this job.
    pub new_log: Vec<JobLogLine>,
}

/// The request a job was started with, for "Run again". The payload is the request type
/// of the kind (ImportRequest, DryRunRequest, DumpRequest, ToolDumpRequest,
/// RestoreRequest, SeedRequest) carried as a JSON value, so the job framework does not depend on the
/// import/backup modules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum JobParams {
    Import(serde_json::Value),
    ImportDryRun(serde_json::Value),
    Dump(serde_json::Value),
    ToolDump(serde_json::Value),
    Restore(serde_json::Value),
    ToolRestore(serde_json::Value),
    Seed(serde_json::Value),
}
