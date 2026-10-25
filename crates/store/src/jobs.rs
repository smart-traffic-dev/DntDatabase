//! Persisted background jobs (`jobs`, migration 7).
//!
//! The store keeps one row per job: what it was (`kind`, `title`, `params_json` for a
//! re-run), where it stands (`state`, timestamps) and how it ended (`summary`,
//! `error_json`, `outcome_unknown`, the persisted log). `kind` and `state` are plain
//! strings here; the app layer owns their enums and the table's CHECK constraints
//! refuse anything else.

use crate::db::internal;
use crate::repo::{parse_uuid, ConnectionRepo};
use dnt_core::error::DbError;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

/// One `jobs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: Uuid,
    /// `import`, `import_dry_run`, `dump`, `tool_dump`, `restore`, `tool_restore` or `export`.
    pub kind: String,
    /// `None` once the connection was deleted.
    pub connection_id: Option<Uuid>,
    pub title: String,
    /// What a re-run needs (never a secret).
    pub params_json: Option<String>,
    pub rerunnable: bool,
    /// `queued`, `running`, `done`, `failed` or `cancelled`.
    pub state: String,
    /// UTC RFC 3339 with fixed millisecond precision (ordering compares the strings).
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub result_path: Option<String>,
    pub summary: Option<String>,
    pub error_json: Option<String>,
    /// No answer came for the last write: it may or may not have been applied.
    pub outcome_unknown: bool,
    /// JSON array of the persisted log lines.
    pub log_json: String,
    /// Log lines not persisted.
    pub dropped_lines: u32,
}

const JOB_COLUMNS: &str = "id, kind, connection_id, title, params_json, rerunnable, state, \
    created_at, started_at, finished_at, result_path, summary, error_json, outcome_unknown, \
    log_json, dropped_lines";

/// A job that has ended (the only ones clearing and pruning remove).
const FINISHED: &str = "state IN ('done','failed','cancelled')";

struct RawJob {
    id: String,
    kind: String,
    connection_id: Option<String>,
    title: String,
    params_json: Option<String>,
    rerunnable: bool,
    state: String,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    result_path: Option<String>,
    summary: Option<String>,
    error_json: Option<String>,
    outcome_unknown: bool,
    log_json: String,
    dropped_lines: i64,
}

impl RawJob {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawJob {
            id: r.get(0)?,
            kind: r.get(1)?,
            connection_id: r.get(2)?,
            title: r.get(3)?,
            params_json: r.get(4)?,
            rerunnable: r.get(5)?,
            state: r.get(6)?,
            created_at: r.get(7)?,
            started_at: r.get(8)?,
            finished_at: r.get(9)?,
            result_path: r.get(10)?,
            summary: r.get(11)?,
            error_json: r.get(12)?,
            outcome_unknown: r.get(13)?,
            log_json: r.get(14)?,
            dropped_lines: r.get(15)?,
        })
    }

    fn into_record(self) -> Result<JobRecord, DbError> {
        Ok(JobRecord {
            id: parse_uuid(&self.id)?,
            kind: self.kind,
            // A corrupt connection id detaches the job rather than hiding it.
            connection_id: self.connection_id.and_then(|c| parse_uuid(&c).ok()),
            title: self.title,
            params_json: self.params_json,
            rerunnable: self.rerunnable,
            state: self.state,
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            result_path: self.result_path,
            summary: self.summary,
            error_json: self.error_json,
            outcome_unknown: self.outcome_unknown,
            log_json: self.log_json,
            dropped_lines: u32::try_from(self.dropped_lines.max(0)).unwrap_or(u32::MAX),
        })
    }
}

impl ConnectionRepo {
    /// Stores a new job. A `connection_id` that does not exist (any more) is stored as NULL.
    pub fn job_insert(&self, r: &JobRecord) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            &format!(
                "INSERT INTO jobs ({JOB_COLUMNS}) VALUES (?1, ?2, \
                 (SELECT id FROM connections WHERE id = ?3), ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            params![
                r.id.to_string(),
                r.kind,
                r.connection_id.map(|c| c.to_string()),
                r.title,
                r.params_json,
                r.rerunnable,
                r.state,
                r.created_at,
                r.started_at,
                r.finished_at,
                r.result_path,
                r.summary,
                r.error_json,
                r.outcome_unknown,
                r.log_json,
                r.dropped_lines,
            ],
        )
        .map_err(|e| internal("save job", e))?;
        Ok(())
    }

    /// Writes every field of job `r.id` except `kind`, `connection_id` and `created_at`,
    /// which never change after the insert (the connection may have been deleted since).
    /// Updating a job that is not stored (cleared meanwhile) is not an error.
    pub fn job_update(&self, r: &JobRecord) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET title = ?2, params_json = ?3, rerunnable = ?4, state = ?5, \
             started_at = ?6, finished_at = ?7, result_path = ?8, summary = ?9, \
             error_json = ?10, outcome_unknown = ?11, log_json = ?12, dropped_lines = ?13 \
             WHERE id = ?1",
            params![
                r.id.to_string(),
                r.title,
                r.params_json,
                r.rerunnable,
                r.state,
                r.started_at,
                r.finished_at,
                r.result_path,
                r.summary,
                r.error_json,
                r.outcome_unknown,
                r.log_json,
                r.dropped_lines,
            ],
        )
        .map_err(|e| internal("update job", e))?;
        Ok(())
    }

    pub fn job_get(&self, id: Uuid) -> Result<Option<JobRecord>, DbError> {
        let raw = {
            let conn = self.lock()?;
            conn.query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                params![id.to_string()],
                RawJob::from_row,
            )
            .optional()
            .map_err(|e| internal("read job", e))?
        };
        raw.map(RawJob::into_record).transpose()
    }

    /// At most `limit` jobs, newest created first.
    pub fn job_list(&self, limit: u32) -> Result<Vec<JobRecord>, DbError> {
        let raw = {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at DESC, rowid DESC LIMIT ?1"
                ))
                .map_err(|e| internal("list jobs", e))?;
            let rows = stmt
                .query_map(params![limit], RawJob::from_row)
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|e| internal("list jobs", e))?;
            rows
        };
        raw.into_iter().map(RawJob::into_record).collect()
    }

    /// Startup recovery (R7): every job left `queued` or `running` becomes `failed` with
    /// `summary`, `outcome_unknown` and `finished_at = now`. Returns how many changed.
    pub fn job_mark_interrupted(&self, summary: &str, now: &str) -> Result<u32, DbError> {
        let conn = self.lock()?;
        let n = conn
            .execute(
                "UPDATE jobs SET state = 'failed', outcome_unknown = 1, summary = ?1, \
                 finished_at = ?2 WHERE state IN ('queued','running')",
                params![summary, now],
            )
            .map_err(|e| internal("mark interrupted jobs", e))?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Deletes every finished job; queued and running ones stay. Returns how many went.
    pub fn job_clear_finished(&self) -> Result<u32, DbError> {
        let conn = self.lock()?;
        let n = conn
            .execute(&format!("DELETE FROM jobs WHERE {FINISHED}"), [])
            .map_err(|e| internal("clear finished jobs", e))?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Keeps the newest `keep` finished jobs (by creation) and deletes older finished
    /// ones; queued and running jobs are never pruned.
    pub fn job_prune(&self, keep: u32) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            &format!(
                "DELETE FROM jobs WHERE {FINISHED} AND rowid NOT IN \
                 (SELECT rowid FROM jobs WHERE {FINISHED} \
                  ORDER BY created_at DESC, rowid DESC LIMIT ?1)"
            ),
            params![keep],
        )
        .map_err(|e| internal("prune jobs", e))?;
        Ok(())
    }
}
