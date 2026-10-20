//! Query editor tabs (`query_tabs`, migration 2), autosaved by the UI.
//!
//! Spec §7.2: the editor content is the user's draft, so `sql_text` is stored verbatim
//! (the one exception to "no secrets in SQLite"; history and logs are always redacted).

use crate::db::internal;
use crate::repo::{now_rfc3339, parse_uuid, ConnectionRepo};
use dnt_core::error::DbError;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

/// The row limits a tab can choose.
pub const ROW_LIMITS: [u32; 4] = [1_000, 10_000, 100_000, 1_000_000];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct QueryTabRecord {
    pub id: String,
    pub title: String,
    /// Exported as a JS number (positions are small).
    #[specta(type = f64)]
    pub sort_order: i64,
    pub connection_id: Option<Uuid>,
    pub database: Option<String>,
    pub sql_text: String,
    /// UTF-16 offset of the cursor in `sql_text`.
    pub cursor: u32,
    pub file_path: Option<String>,
    pub dirty: bool,
    pub continue_on_error: bool,
    pub row_limit: u32,
    /// RFC 3339 UTC; set by `tabs_save` (the value passed in is ignored).
    pub updated_at: String,
}

/// The row limit of a tab saved before limits were validated, or with a corrupt one.
const DEFAULT_ROW_LIMIT: u32 = 10_000;

/// One `query_tabs` row; see `tabs_load` for which bad values are repaired.
fn read_tab(r: &rusqlite::Row<'_>) -> rusqlite::Result<QueryTabRecord> {
    let connection_id = r
        .get::<_, Option<String>>(3)
        .ok()
        .flatten()
        .and_then(|s| parse_uuid(&s).ok());
    let cursor = r
        .get::<_, i64>(6)
        .ok()
        .and_then(|c| u32::try_from(c).ok())
        .unwrap_or(0);
    let row_limit = r
        .get::<_, i64>(10)
        .ok()
        .and_then(|l| u32::try_from(l).ok())
        .filter(|l| ROW_LIMITS.contains(l))
        .unwrap_or(DEFAULT_ROW_LIMIT);
    Ok(QueryTabRecord {
        id: r.get(0)?,
        title: r.get(1)?,
        sort_order: r.get(2)?,
        connection_id,
        database: r.get(4)?,
        sql_text: r.get(5)?,
        cursor,
        file_path: r.get(7)?,
        dirty: r.get(8)?,
        continue_on_error: r.get(9)?,
        row_limit,
        updated_at: r.get(11)?,
    })
}

const TAB_COLUMNS: &str = "id, title, sort_order, connection_id, database, sql_text, cursor, \
    file_path, dirty, continue_on_error, row_limit, updated_at";

impl ConnectionRepo {
    /// Every saved tab, in tab-bar order.
    ///
    /// One bad row never fails the restore: an unparsable `connection_id` becomes None, an
    /// out-of-range `cursor` becomes 0 and an unknown `row_limit` becomes 10 000. Any other
    /// unreadable row is skipped with a warning naming only the tab id.
    pub fn tabs_load(&self) -> Result<Vec<QueryTabRecord>, DbError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {TAB_COLUMNS} FROM query_tabs ORDER BY sort_order, id"
            ))
            .map_err(|e| internal("list query tabs", e))?;
        let mut rows = stmt.query([]).map_err(|e| internal("list query tabs", e))?;
        let mut tabs = Vec::new();
        while let Some(r) = rows.next().map_err(|e| internal("list query tabs", e))? {
            match read_tab(r) {
                Ok(tab) => tabs.push(tab),
                Err(_) => {
                    // The error may quote the row's content; log the id only.
                    let id = r.get::<_, String>(0).ok();
                    tracing::warn!(
                        tab = id.as_deref().unwrap_or("<unreadable id>"),
                        "skipping a saved query tab that cannot be read"
                    );
                }
            }
        }
        Ok(tabs)
    }

    /// Inserts or replaces `tab` and stamps `updated_at`. A `connection_id` that no longer
    /// exists (deleted while the tab was open) is stored as NULL rather than failing.
    pub fn tabs_save(&self, tab: &QueryTabRecord) -> Result<(), DbError> {
        if tab.id.trim().is_empty() {
            return Err(DbError::Config {
                field: Some("id".into()),
                message: "tab id must not be empty".into(),
            });
        }
        if !ROW_LIMITS.contains(&tab.row_limit) {
            return Err(DbError::Config {
                field: Some("row_limit".into()),
                message: format!("unsupported row limit {}", tab.row_limit),
            });
        }
        let now = now_rfc3339()?;
        let conn = self.lock()?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO query_tabs ({TAB_COLUMNS}) VALUES \
                 (?1, ?2, ?3, (SELECT id FROM connections WHERE id = ?4), ?5, ?6, ?7, ?8, ?9, \
                 ?10, ?11, ?12)"
            ),
            params![
                tab.id,
                tab.title,
                tab.sort_order,
                tab.connection_id.map(|c| c.to_string()),
                tab.database,
                tab.sql_text,
                tab.cursor,
                tab.file_path,
                tab.dirty,
                tab.continue_on_error,
                tab.row_limit,
                now,
            ],
        )
        .map_err(|e| internal("save query tab", e))?;
        Ok(())
    }

    /// Removes a tab; deleting one that is not stored is not an error.
    pub fn tabs_delete(&self, id: &str) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM query_tabs WHERE id = ?1", params![id])
            .map_err(|e| internal("delete query tab", e))?;
        Ok(())
    }
}
