//! Consent to AI features (AI editor completion spec §6.3).
//!
//! - `ai_consent`: the user agreed, once for the app, that editor text and schema names may go
//!   to the AI endpoint of `origin`. It counts only while `origin` equals the configured
//!   endpoint's origin; the app compares them, the store just keeps the row.
//! - `ai_connection`: a connection's own choice, on or off. Without one the app decides
//!   (on, except for connections tagged Production).
//!
//! Deleting a connection deletes its choice (foreign key cascade); connection export files never
//! carry either table.

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
pub struct AiConsent {
    /// `scheme://host[:port]` of the endpoint the user agreed to.
    pub origin: String,
    /// RFC 3339.
    pub granted_at: String,
}

impl ConnectionRepo {
    /// The app's consent of `scope`, whatever its origin.
    pub fn ai_consent(&self, scope: AiScope) -> Result<Option<AiConsent>, DbError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT origin, granted_at FROM ai_consent WHERE scope = ?1",
            params![scope.as_str()],
            |r| {
                Ok(AiConsent {
                    origin: r.get(0)?,
                    granted_at: r.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|e| internal("read AI consent", e))
    }

    /// Records consent to the endpoint of `origin`, replacing an earlier one.
    pub fn ai_consent_grant(&self, scope: AiScope, origin: &str) -> Result<(), DbError> {
        let now = now_rfc3339()?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO ai_consent (scope, origin, granted_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (scope) DO UPDATE SET origin = excluded.origin, granted_at = excluded.granted_at",
            params![scope.as_str(), origin, now],
        )
        .map_err(|e| internal("record AI consent", e))?;
        Ok(())
    }

    /// Withdraws consent; none is not an error.
    pub fn ai_consent_revoke(&self, scope: AiScope) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM ai_consent WHERE scope = ?1",
            params![scope.as_str()],
        )
        .map_err(|e| internal("withdraw AI consent", e))?;
        Ok(())
    }

    /// Records connection `id`'s own choice, replacing an earlier one.
    pub fn ai_connection_set(
        &self,
        id: Uuid,
        scope: AiScope,
        enabled: bool,
    ) -> Result<(), DbError> {
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
            "INSERT INTO ai_connection (connection_id, scope, enabled) VALUES (?1, ?2, ?3) \
             ON CONFLICT (connection_id, scope) DO UPDATE SET enabled = excluded.enabled",
            params![id.to_string(), scope.as_str(), enabled],
        )
        .map_err(|e| internal("record the AI choice", e))?;
        Ok(())
    }

    /// Every connection's own choice of `scope`.
    pub fn ai_connection_choices(&self, scope: AiScope) -> Result<Vec<(Uuid, bool)>, DbError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT connection_id, enabled FROM ai_connection WHERE scope = ?1 ORDER BY connection_id",
            )
            .map_err(|e| internal("read AI choices", e))?;
        let rows = stmt
            .query_map(params![scope.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
            })
            .map_err(|e| internal("read AI choices", e))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, enabled) = row.map_err(|e| internal("read AI choices", e))?;
            let id = Uuid::parse_str(&id).map_err(|e| internal("read AI choices", e))?;
            out.push((id, enabled));
        }
        Ok(out)
    }
}
