//! Query history (`query_history`, migration 2): one row per statement sent to a server.
//!
//! The store never redacts: `NewHistoryEntry::sql_text` must already have gone through
//! `dnt_query::redact` (the caller records whether it changed in `redacted`). Server error
//! messages are never stored, only `error_kind` and the error code / SQLSTATE (spec §8.1).

use crate::db::internal;
use crate::repo::{parse_uuid, ConnectionRepo};
use dnt_core::error::DbError;
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

/// Rows kept: older ones are pruned right after each insert (or batch of inserts).
pub const HISTORY_KEEP: i64 = 10_000;

/// Upper bound of `HistoryFilter::limit`.
pub const HISTORY_LIST_MAX: u32 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
    Ok,
    Error,
    /// The statement was stopped. Not "not applied": the stop may have come after it
    /// took effect (the error's `outcome_unknown` is not stored), so the UI must not
    /// show such a row as not applied.
    Cancelled,
}

impl HistoryStatus {
    fn as_str(self) -> &'static str {
        match self {
            HistoryStatus::Ok => "ok",
            HistoryStatus::Error => "error",
            HistoryStatus::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Result<Self, DbError> {
        match s {
            "ok" => Ok(HistoryStatus::Ok),
            "error" => Ok(HistoryStatus::Error),
            "cancelled" => Ok(HistoryStatus::Cancelled),
            other => Err(DbError::Internal {
                message: format!("invalid history status {other:?} in database"),
            }),
        }
    }
}

/// A statement to record. `sql_text` is the **already redacted** text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct NewHistoryEntry {
    pub connection_id: Uuid,
    pub database: Option<String>,
    pub sql_text: String,
    pub redacted: bool,
    pub kind: String,
    pub started_at: String,
    #[specta(type = Option<f64>)]
    pub elapsed_ms: Option<u64>,
    pub status: HistoryStatus,
    #[specta(type = Option<f64>)]
    pub row_count: Option<u64>,
    #[specta(type = Option<f64>)]
    pub affected: Option<u64>,
    pub error_kind: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct HistoryEntry {
    #[specta(type = f64)]
    pub id: i64,
    pub connection_id: Uuid,
    pub database: Option<String>,
    pub sql_text: String,
    pub redacted: bool,
    pub kind: String,
    pub started_at: String,
    #[specta(type = Option<f64>)]
    pub elapsed_ms: Option<u64>,
    pub status: HistoryStatus,
    #[specta(type = Option<f64>)]
    pub row_count: Option<u64>,
    #[specta(type = Option<f64>)]
    pub affected: Option<u64>,
    pub error_kind: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct HistoryFilter {
    pub connection_id: Option<Uuid>,
    /// Substring of the SQL text; `%`, `_` and `\` match literally.
    pub search: Option<String>,
    /// At most [`HISTORY_LIST_MAX`]; larger values are clamped.
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct HistoryIds(#[specta(type = Vec<f64>)] pub Vec<i64>);

/// SQLite stores integers as i64; counts beyond that saturate.
fn to_sql_u64(v: Option<u64>) -> Option<i64> {
    v.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

fn from_sql_u64(v: Option<i64>) -> Option<u64> {
    v.map(|v| u64::try_from(v).unwrap_or(0))
}

/// `s` with the LIKE wildcards and the escape character escaped (for `ESCAPE '\'`).
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

const HISTORY_COLUMNS: &str = "id, connection_id, database, sql_text, redacted, kind, \
    started_at, elapsed_ms, status, row_count, affected, error_kind, error_code";

impl ConnectionRepo {
    /// Records one statement (with already redacted SQL) and prunes everything beyond the
    /// newest [`HISTORY_KEEP`] rows in the same transaction. Returns the new row id.
    ///
    /// `started_at` must be UTC RFC 3339 with fixed millisecond precision
    /// (`%Y-%m-%dT%H:%M:%S%.3fZ`, e.g. `2026-10-20T10:00:00.000Z`): `history_list` orders
    /// by comparing the strings, which is chronological only in that one fixed format.
    pub fn history_insert(&self, e: &NewHistoryEntry) -> Result<i64, DbError> {
        self.history_insert_keep(e, HISTORY_KEEP)
    }

    /// [`Self::history_insert`] with an explicit bound. Public only for the integration
    /// tests; use `history_insert`.
    #[doc(hidden)]
    pub fn history_insert_keep(&self, e: &NewHistoryEntry, keep: i64) -> Result<i64, DbError> {
        let ids = self.history_insert_batch_keep(std::slice::from_ref(e), keep)?;
        ids.first().copied().ok_or_else(|| DbError::Internal {
            message: "insert history entry: no row id".into(),
        })
    }

    /// Records `entries` in order in ONE transaction, then prunes once (see
    /// [`Self::history_insert`] for `started_at`). Returns the new row ids, in order.
    /// A run's history writer sends whatever statements are queued as one batch, so a
    /// long script costs one commit per batch, not one per statement.
    pub fn history_insert_batch(&self, entries: &[NewHistoryEntry]) -> Result<Vec<i64>, DbError> {
        self.history_insert_batch_keep(entries, HISTORY_KEEP)
    }

    /// [`Self::history_insert_batch`] with an explicit bound. Public only for the
    /// integration tests; use `history_insert_batch`.
    #[doc(hidden)]
    pub fn history_insert_batch_keep(
        &self,
        entries: &[NewHistoryEntry],
        keep: i64,
    ) -> Result<Vec<i64>, DbError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        self.transaction(|tx| {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO query_history (connection_id, database, sql_text, redacted, \
                     kind, started_at, elapsed_ms, status, row_count, affected, error_kind, \
                     error_code) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(|err| internal("insert history entry", err))?;
            let mut ids = Vec::with_capacity(entries.len());
            for e in entries {
                stmt.execute(params![
                    e.connection_id.to_string(),
                    e.database,
                    e.sql_text,
                    e.redacted,
                    e.kind,
                    e.started_at,
                    to_sql_u64(e.elapsed_ms),
                    e.status.as_str(),
                    to_sql_u64(e.row_count),
                    to_sql_u64(e.affected),
                    e.error_kind,
                    e.error_code,
                ])
                .map_err(|err| internal("insert history entry", err))?;
                ids.push(tx.last_insert_rowid());
            }
            drop(stmt);
            prune(tx, keep)?;
            Ok(ids)
        })
    }

    /// Newest first (`started_at DESC, id DESC`).
    pub fn history_list(&self, f: &HistoryFilter) -> Result<Vec<HistoryEntry>, DbError> {
        let limit = f.limit.min(HISTORY_LIST_MAX);
        let pattern = f
            .search
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", escape_like(s)));
        let raw = {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {HISTORY_COLUMNS} FROM query_history \
                     WHERE (?1 IS NULL OR connection_id = ?1) \
                       AND (?2 IS NULL OR sql_text LIKE ?2 ESCAPE '\\') \
                     ORDER BY started_at DESC, id DESC LIMIT ?3"
                ))
                .map_err(|e| internal("list history", e))?;
            let rows = stmt
                .query_map(
                    params![f.connection_id.map(|c| c.to_string()), pattern, limit],
                    RawEntry::from_row,
                )
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|e| internal("list history", e))?;
            rows
        };
        raw.into_iter().map(RawEntry::into_entry).collect()
    }

    pub fn history_delete(&self, ids: &[i64]) -> Result<(), DbError> {
        if ids.is_empty() {
            return Ok(());
        }
        self.transaction(|tx| {
            // Bounded batches keep each statement under SQLite's parameter limit.
            for chunk in ids.chunks(500) {
                let marks = vec!["?"; chunk.len()].join(", ");
                tx.execute(
                    &format!("DELETE FROM query_history WHERE id IN ({marks})"),
                    params_from_iter(chunk),
                )
                .map_err(|e| internal("delete history entries", e))?;
            }
            Ok(())
        })
    }

    pub fn history_clear(&self) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM query_history", [])
            .map_err(|e| internal("clear history", e))?;
        Ok(())
    }
}

/// Deletes every row older than the newest `keep` (ids grow with insertion order).
fn prune(conn: &Connection, keep: i64) -> Result<(), DbError> {
    conn.execute(
        "DELETE FROM query_history WHERE id <= \
         (SELECT id FROM query_history ORDER BY id DESC LIMIT 1 OFFSET ?1)",
        params![keep.max(0)],
    )
    .map_err(|e| internal("prune history", e))?;
    Ok(())
}

struct RawEntry {
    id: i64,
    connection_id: String,
    database: Option<String>,
    sql_text: String,
    redacted: bool,
    kind: String,
    started_at: String,
    elapsed_ms: Option<i64>,
    status: String,
    row_count: Option<i64>,
    affected: Option<i64>,
    error_kind: Option<String>,
    error_code: Option<String>,
}

impl RawEntry {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawEntry {
            id: r.get(0)?,
            connection_id: r.get(1)?,
            database: r.get(2)?,
            sql_text: r.get(3)?,
            redacted: r.get(4)?,
            kind: r.get(5)?,
            started_at: r.get(6)?,
            elapsed_ms: r.get(7)?,
            status: r.get(8)?,
            row_count: r.get(9)?,
            affected: r.get(10)?,
            error_kind: r.get(11)?,
            error_code: r.get(12)?,
        })
    }

    fn into_entry(self) -> Result<HistoryEntry, DbError> {
        Ok(HistoryEntry {
            id: self.id,
            connection_id: parse_uuid(&self.connection_id)?,
            database: self.database,
            sql_text: self.sql_text,
            redacted: self.redacted,
            kind: self.kind,
            started_at: self.started_at,
            elapsed_ms: from_sql_u64(self.elapsed_ms),
            status: HistoryStatus::parse(&self.status)?,
            row_count: from_sql_u64(self.row_count),
            affected: from_sql_u64(self.affected),
            error_kind: self.error_kind,
            error_code: self.error_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_wildcards() {
        assert_eq!(escape_like(r"50%_a\b"), r"50\%\_a\\b");
    }
}
