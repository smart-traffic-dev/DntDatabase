//! `JobRecord` ↔ model, and what of a job's log and error the store keeps (R6).
//!
//! The store keeps the first [`PERSIST_HEAD`] and last [`PERSIST_TAIL`] log lines within
//! [`PERSIST_LOG_BYTES`]. Lines and errors carrying a server message are stored as
//! `Server error <code>` only: History never stores server messages, and neither do jobs
//! (the full text is shown while the app runs).

use super::manager::{PERSIST_LOG_BYTES, PERSIST_LOG_LINES};
use super::model::{JobDetail, JobKind, JobLogLine, JobParams, JobProgress, JobState, JobSummary};
use dnt_core::error::AppError;
use dnt_store::jobs::JobRecord;

const PERSIST_HEAD: usize = PERSIST_LOG_LINES / 2;
const PERSIST_TAIL: usize = PERSIST_LOG_LINES - PERSIST_HEAD;

/// What is stored for a server message.
pub(super) fn server_error_text(code: Option<&str>) -> String {
    match code {
        Some(code) if !code.is_empty() => format!("Server error {code}"),
        _ => "Server error".into(),
    }
}

/// The error as stored: a server error loses its server text.
pub(super) fn persisted_error(e: &AppError) -> AppError {
    let mut e = e.clone();
    if e.kind == "server" {
        e.message = server_error_text(e.server_code.as_deref());
        e.detail = None;
        e.hint = None;
        e.position = None;
    }
    e
}

fn persisted_line(l: &JobLogLine) -> JobLogLine {
    if l.server {
        JobLogLine::new(
            l.at.clone(),
            l.level,
            server_error_text(l.server_code.as_deref()),
        )
    } else {
        JobLogLine::new(l.at.clone(), l.level, l.message.clone())
    }
}

/// The stored log of `kept` (the lines memory kept, in order; `dropped` more were not
/// kept): a JSON array of at most the first 250 + last 250 lines, at most
/// [`PERSIST_LOG_BYTES`] long, and the number of lines it leaves out.
pub(super) fn persisted_log(kept: &[JobLogLine], dropped: u32) -> (String, u32) {
    let n = kept.len();
    let head_n = n.min(PERSIST_HEAD);
    let tail_n = (n - head_n).min(PERSIST_TAIL);
    let encode = |l: &JobLogLine| serde_json::to_string(&persisted_line(l)).unwrap_or_default();
    let head: Vec<String> = kept[..head_n].iter().map(encode).collect();
    let tail: Vec<String> = kept[n - tail_n..].iter().rev().map(encode).collect();

    // Fill from both ends alternately while the lines fit (brackets + one comma each).
    let budget = PERSIST_LOG_BYTES.saturating_sub(2);
    let mut used = 0usize;
    let (mut h, mut t) = (0usize, 0usize);
    let (mut head_open, mut tail_open) = (true, true);
    while head_open || tail_open {
        if head_open {
            match head.get(h) {
                Some(line) if used + line.len() < budget => {
                    used += line.len() + 1;
                    h += 1;
                }
                _ => head_open = false,
            }
        }
        if tail_open {
            match tail.get(t) {
                Some(line) if used + line.len() < budget => {
                    used += line.len() + 1;
                    t += 1;
                }
                _ => tail_open = false,
            }
        }
    }
    let lines: Vec<&str> = head[..h]
        .iter()
        .chain(tail[..t].iter().rev())
        .map(String::as_str)
        .collect();
    let json = format!("[{}]", lines.join(","));
    let left_out = u32::try_from(n - h - t).unwrap_or(u32::MAX);
    (json, dropped.saturating_add(left_out))
}

/// The stored row of a job.
pub(super) fn to_record(
    job: &JobSummary,
    params: Option<&JobParams>,
    kept: &[JobLogLine],
    dropped: u32,
) -> JobRecord {
    let (log_json, dropped_lines) = persisted_log(kept, dropped);
    JobRecord {
        id: job.id,
        kind: job.kind.as_str().into(),
        connection_id: job.connection_id,
        title: job.title.clone(),
        params_json: params.and_then(|p| serde_json::to_string(p).ok()),
        rerunnable: job.rerunnable,
        state: job.state.as_str().into(),
        created_at: job.created_at.clone(),
        started_at: job.started_at.clone(),
        finished_at: job.finished_at.clone(),
        result_path: job.result_path.clone(),
        summary: job.summary.clone(),
        error_json: job
            .error
            .as_ref()
            .and_then(|e| serde_json::to_string(&persisted_error(e)).ok()),
        outcome_unknown: job.outcome_unknown,
        log_json,
        dropped_lines,
    }
}

/// A stored job; `None` when its kind or state is not one this version knows.
pub(super) fn from_record(r: JobRecord) -> Option<JobDetail> {
    let kind = JobKind::parse(&r.kind)?;
    let state = JobState::parse(&r.state)?;
    let job = JobSummary {
        id: r.id,
        kind,
        connection_id: r.connection_id,
        title: r.title,
        state,
        progress: JobProgress::default(),
        created_at: r.created_at,
        started_at: r.started_at,
        finished_at: r.finished_at,
        result_path: r.result_path,
        summary: r.summary,
        error: r
            .error_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        outcome_unknown: r.outcome_unknown,
        rerunnable: r.rerunnable,
    };
    Some(JobDetail {
        job,
        log: serde_json::from_str(&r.log_json).unwrap_or_default(),
        dropped_lines: r.dropped_lines,
        params: r
            .params_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
    })
}
