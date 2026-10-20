//! Favorites (Navicat "saved queries") and user-defined editor snippets (migration 3).
//!
//! A favorite is the user's own saved SQL, optionally bound to a connection and database.
//! Unlike a query tab's draft, it is kept on purpose, so it must never hold a secret: SQL
//! in which `dnt_query::redact` finds a possible secret literal (`IDENTIFIED BY '…'`,
//! `PASSWORD '…'`, a connection string with a password, …) under either dialect is
//! refused with a [`DbError::Config`] naming the `sql` field.
//!
//! Folders are `/`-separated paths (`Reports/Daily`); `None` is the root. Paths are
//! normalised: each segment is trimmed and empty segments are dropped.

use crate::db::internal;
use crate::repo::{now_rfc3339, parse_uuid, ConnectionRepo};
use dnt_core::error::DbError;
use dnt_core::sql::Dialect;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use specta::Type;
use uuid::Uuid;

/// Longest favorite name / snippet prefix or label, in characters.
pub const MAX_NAME_CHARS: usize = 200;
/// Largest favorite SQL / snippet body, in bytes.
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Favorite {
    pub id: Uuid,
    pub name: String,
    /// `/`-separated folder path; `None` is the root.
    pub folder: Option<String>,
    pub sql: String,
    pub connection_id: Option<Uuid>,
    pub database: Option<String>,
    /// RFC 3339 UTC.
    pub created_at: String,
    pub updated_at: String,
    /// Position inside its folder (new and moved favorites go last).
    #[specta(type = f64)]
    pub sort: i64,
}

/// What `favorite_create` / `favorite_update` take.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FavoriteInput {
    pub name: String,
    pub folder: Option<String>,
    pub sql: String,
    pub connection_id: Option<Uuid>,
    pub database: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Snippet {
    pub id: Uuid,
    /// What the user types to trigger it; unique, case-insensitively.
    pub prefix: String,
    pub label: String,
    /// Monaco snippet syntax (`${1:placeholder}`, `$0`).
    pub body: String,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SnippetInput {
    pub prefix: String,
    pub label: String,
    pub body: String,
    pub description: String,
}

fn config(field: &str, message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: message.into(),
    }
}

fn not_found(what: &str, id: Uuid) -> DbError {
    config("id", format!("The {what} {id} no longer exists."))
}

/// The trimmed `value`, or an error when it is empty or too long.
fn required(field: &str, what: &str, value: &str) -> Result<String, DbError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(config(field, format!("{what} must not be empty.")));
    }
    if v.chars().count() > MAX_NAME_CHARS {
        return Err(config(
            field,
            format!("{what} is longer than {MAX_NAME_CHARS} characters."),
        ));
    }
    Ok(v.to_string())
}

fn text_size(field: &str, text: &str) -> Result<(), DbError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(config(
            field,
            format!(
                "The text is larger than {} MB.",
                MAX_TEXT_BYTES / (1024 * 1024)
            ),
        ));
    }
    Ok(())
}

/// `Reports / Daily/` -> `Reports/Daily`; blank -> `None`.
pub fn normalize_folder(folder: Option<&str>) -> Result<Option<String>, DbError> {
    let Some(folder) = folder else {
        return Ok(None);
    };
    let segments: Vec<&str> = folder
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Ok(None);
    }
    let path = segments.join("/");
    if path.chars().count() > MAX_NAME_CHARS {
        return Err(config(
            "folder",
            format!("The folder path is longer than {MAX_NAME_CHARS} characters."),
        ));
    }
    Ok(Some(path))
}

/// Refuses SQL in which the history redaction would hide something.
pub fn refuse_secrets(sql: &str) -> Result<(), DbError> {
    let secret = [Dialect::MYSQL, Dialect::Postgres]
        .into_iter()
        .any(|d| dnt_query::redact(sql, d).changed);
    if secret {
        return Err(config(
            "sql",
            "This SQL contains a password or another secret literal (for example \
             IDENTIFIED BY '…' or PASSWORD '…'). Favorites never store secrets: remove it \
             and save again.",
        ));
    }
    Ok(())
}

struct ValidFavorite {
    name: String,
    folder: Option<String>,
    sql: String,
    connection_id: Option<String>,
    database: Option<String>,
}

fn validate_favorite(input: &FavoriteInput) -> Result<ValidFavorite, DbError> {
    let name = required("name", "The name", &input.name)?;
    let folder = normalize_folder(input.folder.as_deref())?;
    if input.sql.trim().is_empty() {
        return Err(config("sql", "There is no SQL to save."));
    }
    text_size("sql", &input.sql)?;
    refuse_secrets(&input.sql)?;
    let database = input
        .database
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    Ok(ValidFavorite {
        name,
        folder,
        sql: input.sql.clone(),
        connection_id: input.connection_id.map(|c| c.to_string()),
        database,
    })
}

fn validate_snippet(input: &SnippetInput) -> Result<SnippetInput, DbError> {
    let prefix = required("prefix", "The prefix", &input.prefix)?;
    if prefix.chars().any(char::is_whitespace) {
        return Err(config("prefix", "The prefix must not contain spaces."));
    }
    let label = required("label", "The label", &input.label)?;
    if input.body.trim().is_empty() {
        return Err(config("body", "The body must not be empty."));
    }
    text_size("body", &input.body)?;
    Ok(SnippetInput {
        prefix,
        label,
        body: input.body.clone(),
        description: input.description.trim().to_string(),
    })
}

fn next_sort(conn: &Connection, folder: Option<&str>) -> Result<i64, DbError> {
    conn.query_row(
        "SELECT COALESCE(MAX(sort), -1) + 1 FROM favorites WHERE folder IS ?1",
        params![folder],
        |r| r.get(0),
    )
    .map_err(|e| internal("order favorites", e))
}

const FAVORITE_COLUMNS: &str =
    "id, name, folder, sql, connection_id, database, created_at, updated_at, sort";

/// Root first, then folders by path; inside a folder by `sort`, then name.
const FAVORITE_ORDER: &str =
    "ORDER BY folder IS NOT NULL, folder COLLATE NOCASE, folder, sort, name COLLATE NOCASE, id";

struct RawFavorite {
    id: String,
    name: String,
    folder: Option<String>,
    sql: String,
    connection_id: Option<String>,
    database: Option<String>,
    created_at: String,
    updated_at: String,
    sort: i64,
}

impl RawFavorite {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RawFavorite {
            id: r.get(0)?,
            name: r.get(1)?,
            folder: r.get(2)?,
            sql: r.get(3)?,
            connection_id: r.get(4)?,
            database: r.get(5)?,
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
            sort: r.get(8)?,
        })
    }

    fn into_favorite(self) -> Result<Favorite, DbError> {
        Ok(Favorite {
            id: parse_uuid(&self.id)?,
            name: self.name,
            folder: self.folder,
            sql: self.sql,
            // A corrupt connection id detaches the favorite rather than hiding it.
            connection_id: self.connection_id.and_then(|c| parse_uuid(&c).ok()),
            database: self.database,
            created_at: self.created_at,
            updated_at: self.updated_at,
            sort: self.sort,
        })
    }
}

fn get_favorite(conn: &Connection, id: Uuid) -> Result<Favorite, DbError> {
    conn.query_row(
        &format!("SELECT {FAVORITE_COLUMNS} FROM favorites WHERE id = ?1"),
        params![id.to_string()],
        RawFavorite::from_row,
    )
    .optional()
    .map_err(|e| internal("read favorite", e))?
    .ok_or_else(|| not_found("favorite", id))?
    .into_favorite()
}

const SNIPPET_COLUMNS: &str = "id, prefix, label, body, description, created_at, updated_at";

fn read_snippet(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, Snippet)> {
    Ok((
        r.get(0)?,
        Snippet {
            id: Uuid::nil(),
            prefix: r.get(1)?,
            label: r.get(2)?,
            body: r.get(3)?,
            description: r.get(4)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
        },
    ))
}

fn with_id((id, s): (String, Snippet)) -> Result<Snippet, DbError> {
    Ok(Snippet {
        id: parse_uuid(&id)?,
        ..s
    })
}

fn get_snippet(conn: &Connection, id: Uuid) -> Result<Snippet, DbError> {
    let row = conn
        .query_row(
            &format!("SELECT {SNIPPET_COLUMNS} FROM snippets WHERE id = ?1"),
            params![id.to_string()],
            read_snippet,
        )
        .optional()
        .map_err(|e| internal("read snippet", e))?
        .ok_or_else(|| not_found("snippet", id))?;
    with_id(row)
}

/// A UNIQUE violation on `snippets.prefix` becomes a readable error.
fn snippet_write_error(prefix: &str, e: rusqlite::Error) -> DbError {
    if let rusqlite::Error::SqliteFailure(f, _) = &e {
        if f.code == rusqlite::ErrorCode::ConstraintViolation {
            return config(
                "prefix",
                format!("A snippet with the prefix \"{prefix}\" already exists."),
            );
        }
    }
    internal("save snippet", e)
}

impl ConnectionRepo {
    /// Every favorite: root first, then by folder path, inside a folder by `sort`.
    pub fn favorites_list(&self) -> Result<Vec<Favorite>, DbError> {
        let raw = {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {FAVORITE_COLUMNS} FROM favorites {FAVORITE_ORDER}"
                ))
                .map_err(|e| internal("list favorites", e))?;
            let rows = stmt
                .query_map([], RawFavorite::from_row)
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|e| internal("list favorites", e))?;
            rows
        };
        raw.into_iter().map(RawFavorite::into_favorite).collect()
    }

    /// Saves a new favorite at the end of its folder. A `connection_id` that does not
    /// exist is stored as NULL.
    pub fn favorite_create(&self, input: &FavoriteInput) -> Result<Favorite, DbError> {
        let v = validate_favorite(input)?;
        let now = now_rfc3339()?;
        let id = Uuid::new_v4();
        self.transaction(|tx| {
            let sort = next_sort(tx, v.folder.as_deref())?;
            tx.execute(
                &format!(
                    "INSERT INTO favorites ({FAVORITE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, \
                     (SELECT id FROM connections WHERE id = ?5), ?6, ?7, ?7, ?8)"
                ),
                params![
                    id.to_string(),
                    v.name,
                    v.folder,
                    v.sql,
                    v.connection_id,
                    v.database,
                    now,
                    sort
                ],
            )
            .map_err(|e| internal("save favorite", e))?;
            get_favorite(tx, id)
        })
    }

    /// Replaces every field of favorite `id`; a new folder puts it last there.
    pub fn favorite_update(&self, id: Uuid, input: &FavoriteInput) -> Result<Favorite, DbError> {
        let v = validate_favorite(input)?;
        let now = now_rfc3339()?;
        self.transaction(|tx| {
            let old = get_favorite(tx, id)?;
            let sort = if old.folder == v.folder {
                old.sort
            } else {
                next_sort(tx, v.folder.as_deref())?
            };
            tx.execute(
                "UPDATE favorites SET name = ?2, folder = ?3, sql = ?4, \
                 connection_id = (SELECT id FROM connections WHERE id = ?5), database = ?6, \
                 updated_at = ?7, sort = ?8 WHERE id = ?1",
                params![
                    id.to_string(),
                    v.name,
                    v.folder,
                    v.sql,
                    v.connection_id,
                    v.database,
                    now,
                    sort
                ],
            )
            .map_err(|e| internal("update favorite", e))?;
            get_favorite(tx, id)
        })
    }

    pub fn favorite_rename(&self, id: Uuid, name: &str) -> Result<Favorite, DbError> {
        let name = required("name", "The name", name)?;
        let now = now_rfc3339()?;
        self.transaction(|tx| {
            let n = tx
                .execute(
                    "UPDATE favorites SET name = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id.to_string(), name, now],
                )
                .map_err(|e| internal("rename favorite", e))?;
            if n == 0 {
                return Err(not_found("favorite", id));
            }
            get_favorite(tx, id)
        })
    }

    /// Moves favorite `id` to the end of `folder` (`None`: the root).
    pub fn favorite_move(&self, id: Uuid, folder: Option<&str>) -> Result<Favorite, DbError> {
        let folder = normalize_folder(folder)?;
        let now = now_rfc3339()?;
        self.transaction(|tx| {
            let old = get_favorite(tx, id)?;
            if old.folder == folder {
                return Ok(old);
            }
            let sort = next_sort(tx, folder.as_deref())?;
            tx.execute(
                "UPDATE favorites SET folder = ?2, sort = ?3, updated_at = ?4 WHERE id = ?1",
                params![id.to_string(), folder, sort, now],
            )
            .map_err(|e| internal("move favorite", e))?;
            get_favorite(tx, id)
        })
    }

    /// Deleting a favorite that is not stored is not an error.
    pub fn favorite_delete(&self, id: Uuid) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM favorites WHERE id = ?1",
            params![id.to_string()],
        )
        .map_err(|e| internal("delete favorite", e))?;
        Ok(())
    }

    /// Every snippet, by prefix (case-insensitive).
    pub fn snippets_list(&self) -> Result<Vec<Snippet>, DbError> {
        let rows = {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {SNIPPET_COLUMNS} FROM snippets ORDER BY prefix COLLATE NOCASE, id"
                ))
                .map_err(|e| internal("list snippets", e))?;
            let rows = stmt
                .query_map([], read_snippet)
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(|e| internal("list snippets", e))?;
            rows
        };
        rows.into_iter().map(with_id).collect()
    }

    pub fn snippet_create(&self, input: &SnippetInput) -> Result<Snippet, DbError> {
        let v = validate_snippet(input)?;
        let now = now_rfc3339()?;
        let id = Uuid::new_v4();
        self.transaction(|tx| {
            tx.execute(
                &format!(
                    "INSERT INTO snippets ({SNIPPET_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)"
                ),
                params![
                    id.to_string(),
                    v.prefix,
                    v.label,
                    v.body,
                    v.description,
                    now
                ],
            )
            .map_err(|e| snippet_write_error(&v.prefix, e))?;
            get_snippet(tx, id)
        })
    }

    pub fn snippet_update(&self, id: Uuid, input: &SnippetInput) -> Result<Snippet, DbError> {
        let v = validate_snippet(input)?;
        let now = now_rfc3339()?;
        self.transaction(|tx| {
            let n = tx
                .execute(
                    "UPDATE snippets SET prefix = ?2, label = ?3, body = ?4, description = ?5, \
                     updated_at = ?6 WHERE id = ?1",
                    params![
                        id.to_string(),
                        v.prefix,
                        v.label,
                        v.body,
                        v.description,
                        now
                    ],
                )
                .map_err(|e| snippet_write_error(&v.prefix, e))?;
            if n == 0 {
                return Err(not_found("snippet", id));
            }
            get_snippet(tx, id)
        })
    }

    /// Deleting a snippet that is not stored is not an error.
    pub fn snippet_delete(&self, id: Uuid) -> Result<(), DbError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM snippets WHERE id = ?1",
            params![id.to_string()],
        )
        .map_err(|e| internal("delete snippet", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_normalized() {
        assert_eq!(normalize_folder(None).unwrap(), None);
        assert_eq!(normalize_folder(Some("  / ")).unwrap(), None);
        assert_eq!(
            normalize_folder(Some(" Reports / Daily/ "))
                .unwrap()
                .as_deref(),
            Some("Reports/Daily")
        );
        assert!(normalize_folder(Some(&"x".repeat(MAX_NAME_CHARS + 1))).is_err());
    }

    #[test]
    fn secrets_are_refused_in_either_dialect() {
        for sql in [
            "CREATE USER u IDENTIFIED BY 'hunter2'",
            "ALTER ROLE r PASSWORD 'x'",
            "SELECT dblink_connect('postgresql://u:pw@h/db')",
        ] {
            let err = refuse_secrets(sql).unwrap_err();
            assert!(
                matches!(&err, DbError::Config { field: Some(f), .. } if f == "sql"),
                "{sql}: {err:?}"
            );
        }
        refuse_secrets("SELECT * FROM users WHERE id = 1").unwrap();
    }
}
