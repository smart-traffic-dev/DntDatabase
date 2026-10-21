//! Opening the app database and applying schema migrations.

use dnt_core::error::DbError;
use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};
use std::path::Path;

const MIGRATION_1: &str = "
CREATE TABLE groups (
  id TEXT PRIMARY KEY, parent_id TEXT NULL REFERENCES groups(id) ON DELETE CASCADE,
  name TEXT NOT NULL, sort_order INTEGER NOT NULL);
CREATE TABLE connections (
  id TEXT PRIMARY KEY, group_id TEXT NULL REFERENCES groups(id) ON DELETE SET NULL,
  name TEXT NOT NULL, sort_order INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('mysql','postgres')),
  host TEXT NOT NULL, port INTEGER NOT NULL, username TEXT NOT NULL,
  default_db TEXT NULL, env_tag TEXT NULL, read_only INTEGER NOT NULL DEFAULT 0,
  secret_mode TEXT NOT NULL CHECK (secret_mode IN ('saved','ask')),
  ssh_json TEXT NULL, ssl_json TEXT NULL, options_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_connected_at TEXT NULL);
CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// P2 spec §7.2 (`query_tabs`) and §8.1 (`query_history`), verbatim.
const MIGRATION_2: &str = "
CREATE TABLE query_tabs (
  id TEXT PRIMARY KEY, title TEXT NOT NULL, sort_order INTEGER NOT NULL,
  connection_id TEXT NULL REFERENCES connections(id) ON DELETE SET NULL,
  database TEXT NULL, sql_text TEXT NOT NULL, cursor INTEGER NOT NULL DEFAULT 0,
  file_path TEXT NULL, dirty INTEGER NOT NULL DEFAULT 0,
  continue_on_error INTEGER NOT NULL DEFAULT 0, row_limit INTEGER NOT NULL DEFAULT 10000,
  updated_at TEXT NOT NULL);
CREATE TABLE query_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  database TEXT NULL, sql_text TEXT NOT NULL, redacted INTEGER NOT NULL,
  kind TEXT NOT NULL, started_at TEXT NOT NULL, elapsed_ms INTEGER NULL,
  status TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
  row_count INTEGER NULL, affected INTEGER NULL,
  error_kind TEXT NULL, error_code TEXT NULL);
CREATE INDEX query_history_conn_time ON query_history(connection_id, started_at DESC);
";

/// Favorites (saved queries, `folder` is a `/`-separated path or NULL for the root) and
/// user-defined editor snippets. A favorite outlives its connection (SET NULL).
const MIGRATION_3: &str = "
CREATE TABLE favorites (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, folder TEXT NULL, sql TEXT NOT NULL,
  connection_id TEXT NULL REFERENCES connections(id) ON DELETE SET NULL,
  database TEXT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  sort INTEGER NOT NULL);
CREATE INDEX favorites_folder_sort ON favorites(folder, sort);
CREATE TABLE snippets (
  id TEXT PRIMARY KEY, prefix TEXT NOT NULL UNIQUE COLLATE NOCASE, label TEXT NOT NULL,
  body TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
";

/// Where a history row came from (a query tab, or a statement the app built for a data
/// edit) and whether its outcome is unknown (no answer: it may or may not have applied).
const MIGRATION_4: &str = "
ALTER TABLE query_history ADD COLUMN origin TEXT NOT NULL DEFAULT 'query'
  CHECK (origin IN ('query','data_edit'));
ALTER TABLE query_history ADD COLUMN outcome_unknown INTEGER NOT NULL DEFAULT 0;
";

// Rebuild only history to extend its origin constraint, preserving ids and rows.
const MIGRATION_5: &str = "
CREATE TABLE query_history_next (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  database TEXT NULL, sql_text TEXT NOT NULL, redacted INTEGER NOT NULL,
  kind TEXT NOT NULL, started_at TEXT NOT NULL, elapsed_ms INTEGER NULL,
  status TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
  row_count INTEGER NULL, affected INTEGER NULL, error_kind TEXT NULL, error_code TEXT NULL,
  origin TEXT NOT NULL DEFAULT 'query' CHECK (origin IN ('query','data_edit','object_action')),
  outcome_unknown INTEGER NOT NULL DEFAULT 0);
INSERT INTO query_history_next SELECT * FROM query_history;
DROP TABLE query_history;
ALTER TABLE query_history_next RENAME TO query_history;
CREATE INDEX query_history_conn_time ON query_history(connection_id, started_at DESC);
";

// Rebuild history again to allow the table designer's origin, preserving ids and rows.
const MIGRATION_6: &str = "
CREATE TABLE query_history_next (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  database TEXT NULL, sql_text TEXT NOT NULL, redacted INTEGER NOT NULL,
  kind TEXT NOT NULL, started_at TEXT NOT NULL, elapsed_ms INTEGER NULL,
  status TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
  row_count INTEGER NULL, affected INTEGER NULL, error_kind TEXT NULL, error_code TEXT NULL,
  origin TEXT NOT NULL DEFAULT 'query'
    CHECK (origin IN ('query','data_edit','object_action','table_design')),
  outcome_unknown INTEGER NOT NULL DEFAULT 0);
INSERT INTO query_history_next SELECT * FROM query_history;
DROP TABLE query_history;
ALTER TABLE query_history_next RENAME TO query_history;
CREATE INDEX query_history_conn_time ON query_history(connection_id, started_at DESC);
";

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(MIGRATION_1),
        M::up(MIGRATION_2),
        M::up(MIGRATION_3),
        M::up(MIGRATION_4),
        M::up(MIGRATION_5),
        M::up(MIGRATION_6),
    ])
}

/// Opens (creating if needed) the database file, enables foreign keys and migrates to latest.
///
/// The file is in WAL mode with `synchronous = NORMAL`: a commit is one append to the
/// `-wal` file next to it (no fsync per commit), so the query history of a long run
/// costs little; a power loss can lose the last commits, never corrupt the database.
pub fn open(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open(path)
        .map_err(|e| internal(format!("open database {}", path.display()), e))?;
    // WAL needs a file system with shared memory; elsewhere SQLite keeps its
    // rollback journal, which still works (only slower).
    let _mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
        .map_err(|e| internal("enable WAL", e))?;
    prepare(conn)
}

/// In-memory database with the full schema; used by tests.
pub fn open_in_memory() -> Result<Connection, DbError> {
    let conn = Connection::open_in_memory().map_err(|e| internal("open in-memory database", e))?;
    prepare(conn)
}

fn prepare(mut conn: Connection) -> Result<Connection, DbError> {
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|e| internal("enable foreign keys", e))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| internal("set synchronous", e))?;
    migrations()
        .to_latest(&mut conn)
        .map_err(|e| internal("migrate database", e))?;
    Ok(conn)
}

pub(crate) fn internal(context: impl std::fmt::Display, err: impl std::fmt::Display) -> DbError {
    DbError::Internal {
        message: format!("{context}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        migrations().validate().unwrap();
    }

    /// A database at schema version 2 (before favorites) migrates to the latest version and keeps its rows.
    #[test]
    fn migrates_version_2_database_to_favorites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            Migrations::new(vec![M::up(MIGRATION_1), M::up(MIGRATION_2)])
                .to_latest(&mut conn)
                .unwrap();
            conn.execute(
                "INSERT INTO query_tabs (id, title, sort_order, sql_text, updated_at) \
                 VALUES ('t1', 'Query', 0, 'SELECT 1', 'now')",
                [],
            )
            .unwrap();
            let v: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 2);
        }
        let conn = open(&path).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 6);
        let tabs: i64 = conn
            .query_row("SELECT COUNT(*) FROM query_tabs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tabs, 1);
        for table in ["favorites", "snippets"] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
    }

    /// A version 3 database with history rows migrates to 4: old rows are `query`
    /// rows with a known outcome.
    #[test]
    fn migration_adds_origin_with_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            Migrations::new(vec![
                M::up(MIGRATION_1),
                M::up(MIGRATION_2),
                M::up(MIGRATION_3),
            ])
            .to_latest(&mut conn)
            .unwrap();
            conn.execute(
                "INSERT INTO connections (id, name, sort_order, kind, host, port, username, \
                 secret_mode, created_at, updated_at) \
                 VALUES ('c1', 'c', 0, 'mysql', 'h', 3306, 'u', 'ask', 'now', 'now')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO query_history (connection_id, sql_text, redacted, kind, \
                 started_at, status) VALUES ('c1', 'SELECT 1', 0, 'read', 'now', 'ok')",
                [],
            )
            .unwrap();
        }
        let conn = open(&path).unwrap();
        let (origin, unknown): (String, i64) = conn
            .query_row(
                "SELECT origin, outcome_unknown FROM query_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((origin.as_str(), unknown), ("query", 0));
    }
    #[test]
    fn migration_five_preserves_history_ids_origins_and_unknown_outcomes() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::new(vec![
            M::up(MIGRATION_1),
            M::up(MIGRATION_2),
            M::up(MIGRATION_3),
            M::up(MIGRATION_4),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn.execute_batch("INSERT INTO connections (id,name,sort_order,kind,host,port,username,secret_mode,created_at,updated_at) VALUES ('c','c',0,'mysql','h',3306,'u','ask','now','now'); INSERT INTO query_history (id,connection_id,sql_text,redacted,kind,started_at,status,origin,outcome_unknown) VALUES (42,'c','UPDATE t SET a=1',0,'write','now','error','data_edit',1);").unwrap();
        migrations().to_latest(&mut conn).unwrap();
        let row: (i64, String, i64) = conn
            .query_row(
                "SELECT id,origin,outcome_unknown FROM query_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (42, "data_edit".into(), 1));
        conn.execute(
            "UPDATE query_history SET origin='object_action' WHERE id=42",
            [],
        )
        .unwrap();
        assert!(conn
            .execute("UPDATE query_history SET origin='invalid' WHERE id=42", [])
            .is_err());
    }
    #[test]
    fn migration_6_accepts_table_design_origin() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::new(vec![
            M::up(MIGRATION_1),
            M::up(MIGRATION_2),
            M::up(MIGRATION_3),
            M::up(MIGRATION_4),
            M::up(MIGRATION_5),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn.execute_batch("INSERT INTO connections (id,name,sort_order,kind,host,port,username,secret_mode,created_at,updated_at) VALUES ('c','c',0,'mysql','h',3306,'u','ask','now','now'); INSERT INTO query_history (id,connection_id,sql_text,redacted,kind,started_at,status,origin,outcome_unknown) VALUES (7,'c','ALTER TABLE t ADD c INT',0,'ddl','now','ok','object_action',1);").unwrap();
        migrations().to_latest(&mut conn).unwrap();
        let row: (i64, String, i64) = conn
            .query_row(
                "SELECT id,origin,outcome_unknown FROM query_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (7, "object_action".into(), 1));
        conn.execute(
            "INSERT INTO query_history (connection_id,sql_text,redacted,kind,started_at,status,origin) \
             VALUES ('c','ALTER TABLE t DROP c',0,'ddl','now','ok','table_design')",
            [],
        )
        .unwrap();
        assert!(conn
            .execute("UPDATE query_history SET origin='invalid' WHERE id=7", [])
            .is_err());
    }
    #[test]
    fn foreign_keys_enabled() {
        let conn = open_in_memory().unwrap();
        let on: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }
}
