//! Per-connection consent to AI features (AI editor completion spec §6.3): which
//! connections may send editor text and schema names to the AI endpoint of `origin`.
//!
//! A row counts only while its `origin` equals the configured endpoint's origin; the app
//! compares them, the store just keeps the rows. Deleting a connection deletes its rows
//! (foreign key cascade); connection export files never carry them.

use crate::db::internal;
use crate::repo::{not_found, now_rfc3339, ConnectionRepo};
use dnt_core::error::DbError;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

/// What the consent covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AiScope {
    /// Editor text (passwords hidden) and table / column names.
    Editor,
}

impl AiScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AiScope::Editor => "editor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiOptIn {
    pub connection_id: Uuid,
    pub scope: AiScope,
    /// `scheme://host[:port]` of the endpoint the user agreed to.
    pub origin: String,
    /// RFC 3339.
    pub granted_at: String,
}

impl ConnectionRepo {
    /// Records consent for connection `id` to the endpoint of `origin`, replacing an
    /// earlier one of the same scope.
    pub fn ai_opt_in_grant(&self, id: Uuid, scope: AiScope, origin: &str) -> Result<(), DbError> {
        let now = now_rfc3339()?;
        let conn = self.lock()?;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM connections WHERE id = ?1",
                params![id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| internal("read connection", e))?;
        if exists.is_none() {
            return Err(not_found("connection", id));
        }
        conn.execute(
            "INSERT INTO ai_opt_in (connection_id, scope, origin, granted_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (connection_id, scope) DO UPDATE SET origin = excluded.origin, \
             granted_at = excluded.granted_at",
            params![id.to_string(), scope.as_str(), origin, now],
        )
        .map_err(|e| internal("record AI consent", e))?;
        Ok(())
    }

    /// Withdraws consent; no row is not an error.
    pub fn ai_opt_in_revoke(&self, id: Uuid, scope: AiScope) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM ai_opt_in WHERE connection_id = ?1 AND scope = ?2",
            params![id.to_string(), scope.as_str()],
        )
        .map_err(|e| internal("withdraw AI consent", e))?;
        Ok(())
    }

    /// Every consent of `scope`, whatever its origin, oldest first.
    pub fn ai_opt_ins(&self, scope: AiScope) -> Result<Vec<AiOptIn>, DbError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT connection_id, origin, granted_at FROM ai_opt_in WHERE scope = ?1 \
                 ORDER BY granted_at, connection_id",
            )
            .map_err(|e| internal("read AI consent", e))?;
        let rows = stmt
            .query_map(params![scope.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| internal("read AI consent", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, origin, granted_at) = row.map_err(|e| internal("read AI consent", e))?;
            let connection_id = Uuid::parse_str(&id).map_err(|e| internal("read AI consent", e))?;
            out.push(AiOptIn {
                connection_id,
                scope,
                origin,
                granted_at,
            });
        }
        Ok(out)
    }
}
