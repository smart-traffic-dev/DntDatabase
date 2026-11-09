//! Opening the app database and applying schema migrations.

use dnt_core::error::DbError;
use rusqlite::Connection;
use rusqlite_migration::{HookError, HookResult, Migrations, M};
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

/// P5 spec §4.4: persisted background jobs, and history rebuilt once more to allow the
/// import and restore origins, preserving ids and rows.
const MIGRATION_7: &str = "
CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('import','import_dry_run','dump','tool_dump','restore','tool_restore','export')),
  connection_id TEXT NULL REFERENCES connections(id) ON DELETE SET NULL,
  title TEXT NOT NULL, params_json TEXT NULL, rerunnable INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL CHECK (state IN ('queued','running','done','failed','cancelled')),
  created_at TEXT NOT NULL, started_at TEXT NULL, finished_at TEXT NULL,
  result_path TEXT NULL, summary TEXT NULL, error_json TEXT NULL,
  outcome_unknown INTEGER NOT NULL DEFAULT 0, log_json TEXT NOT NULL DEFAULT '[]', dropped_lines INTEGER NOT NULL DEFAULT 0);
CREATE INDEX jobs_created ON jobs(created_at DESC);
CREATE TABLE query_history_next (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  database TEXT NULL, sql_text TEXT NOT NULL, redacted INTEGER NOT NULL,
  kind TEXT NOT NULL, started_at TEXT NOT NULL, elapsed_ms INTEGER NULL,
  status TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
  row_count INTEGER NULL, affected INTEGER NULL, error_kind TEXT NULL, error_code TEXT NULL,
  origin TEXT NOT NULL DEFAULT 'query'
    CHECK (origin IN ('query','data_edit','object_action','table_design','import','restore')),
  outcome_unknown INTEGER NOT NULL DEFAULT 0);
INSERT INTO query_history_next SELECT * FROM query_history;
DROP TABLE query_history;
ALTER TABLE query_history_next RENAME TO query_history;
CREATE INDEX query_history_conn_time ON query_history(connection_id, started_at DESC);
";

/// The connection dialog used to save its default SSL mode, Prefer, as no SSL config at
/// all, which every driver reads as plaintext. Connections saved that way get the Prefer
/// their dialog showed (it falls back to plaintext only when the server has no TLS).
const MIGRATION_8: &str = r#"
UPDATE connections
SET ssl_json = '{"version":1,"mode":"prefer","ca_path":null,"cert_path":null,"key_path":null}'
WHERE ssl_json IS NULL;
"#;

/// Data seeder spec §5.6: `jobs` and `query_history` rebuilt to allow the `seed` job kind
/// and history origin, preserving ids and rows.
const MIGRATION_9: &str = "
CREATE TABLE jobs_next (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('import','import_dry_run','dump','tool_dump','restore','tool_restore','export','seed')),
  connection_id TEXT NULL REFERENCES connections(id) ON DELETE SET NULL,
  title TEXT NOT NULL, params_json TEXT NULL, rerunnable INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL CHECK (state IN ('queued','running','done','failed','cancelled')),
  created_at TEXT NOT NULL, started_at TEXT NULL, finished_at TEXT NULL,
  result_path TEXT NULL, summary TEXT NULL, error_json TEXT NULL,
  outcome_unknown INTEGER NOT NULL DEFAULT 0, log_json TEXT NOT NULL DEFAULT '[]', dropped_lines INTEGER NOT NULL DEFAULT 0);
INSERT INTO jobs_next SELECT * FROM jobs;
DROP TABLE jobs;
ALTER TABLE jobs_next RENAME TO jobs;
CREATE INDEX jobs_created ON jobs(created_at DESC);
CREATE TABLE query_history_next (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  database TEXT NULL, sql_text TEXT NOT NULL, redacted INTEGER NOT NULL,
  kind TEXT NOT NULL, started_at TEXT NOT NULL, elapsed_ms INTEGER NULL,
  status TEXT NOT NULL CHECK (status IN ('ok','error','cancelled')),
  row_count INTEGER NULL, affected INTEGER NULL, error_kind TEXT NULL, error_code TEXT NULL,
  origin TEXT NOT NULL DEFAULT 'query'
    CHECK (origin IN ('query','data_edit','object_action','table_design','import','restore','seed')),
  outcome_unknown INTEGER NOT NULL DEFAULT 0);
INSERT INTO query_history_next SELECT * FROM query_history;
DROP TABLE query_history;
ALTER TABLE query_history_next RENAME TO query_history;
CREATE INDEX query_history_conn_time ON query_history(connection_id, started_at DESC);
";

/// R3: `connections` rebuilt so its kind CHECK accepts MongoDB and Redis. The table is
/// otherwise MIGRATION_1's, column for column (no later migration changed its schema, and it
/// has no index besides its primary key's). Runs with foreign keys off (see `prepare`): with
/// them on, `DROP TABLE connections` would delete its children's rows or null their links.
const MIGRATION_10: &str = "
CREATE TABLE connections_next (
  id TEXT PRIMARY KEY, group_id TEXT NULL REFERENCES groups(id) ON DELETE SET NULL,
  name TEXT NOT NULL, sort_order INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('mysql','postgres','mongo','redis')),
  host TEXT NOT NULL, port INTEGER NOT NULL, username TEXT NOT NULL,
  default_db TEXT NULL, env_tag TEXT NULL, read_only INTEGER NOT NULL DEFAULT 0,
  secret_mode TEXT NOT NULL CHECK (secret_mode IN ('saved','ask')),
  ssh_json TEXT NULL, ssl_json TEXT NULL, options_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, last_connected_at TEXT NULL);
INSERT INTO connections_next SELECT * FROM connections;
DROP TABLE connections;
ALTER TABLE connections_next RENAME TO connections;
";

/// AI editor completion spec §6.3: per-connection consent to send editor text and schema
/// names to the AI endpoint whose origin it names.
const MIGRATION_11: &str = "
CREATE TABLE ai_opt_in (
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  scope TEXT NOT NULL CHECK (scope IN ('editor')),
  origin TEXT NOT NULL,
  granted_at TEXT NOT NULL,
  PRIMARY KEY (connection_id, scope));
";

/// AI consent is given once, to an endpoint's origin (`ai_consent`); each connection may then
/// be turned off (or a Production one on) by a choice of its own (`ai_connection`). A former
/// per-connection consent becomes that connection's choice to be on, never the app's consent:
/// the user agrees once more, for every connection.
const MIGRATION_12: &str = "
CREATE TABLE ai_consent (
  scope TEXT PRIMARY KEY CHECK (scope IN ('editor')),
  origin TEXT NOT NULL,
  granted_at TEXT NOT NULL);
CREATE TABLE ai_connection (
  connection_id TEXT NOT NULL REFERENCES connections(id) ON DELETE CASCADE,
  scope TEXT NOT NULL CHECK (scope IN ('editor')),
  enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  PRIMARY KEY (connection_id, scope));
INSERT INTO ai_connection (connection_id, scope, enabled) SELECT connection_id, scope, 1 FROM ai_opt_in;
DROP TABLE ai_opt_in;
";

/// AI consent names what it was given for: version 1 is the consent text that named MySQL and
/// PostgreSQL only; version 2 (written from now on) names MongoDB too. A version 1 consent does
/// not let a MongoDB connection send anything (AI editor completion spec §6.3).
const MIGRATION_13: &str = "
ALTER TABLE ai_consent ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
";

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(MIGRATION_1),
        M::up(MIGRATION_2),
        M::up(MIGRATION_3),
        M::up(MIGRATION_4),
        M::up(MIGRATION_5),
        M::up(MIGRATION_6),
        M::up(MIGRATION_7),
        M::up(MIGRATION_8),
        M::up(MIGRATION_9),
        M::up_with_hook(MIGRATION_10, require_foreign_keys_off).foreign_key_check(),
        M::up(MIGRATION_11),
        M::up(MIGRATION_12),
        M::up(MIGRATION_13),
    ])
}

/// Hook of a migration that rebuilds a parent table: with foreign keys on, its `DROP TABLE`
/// has already run the children's ON DELETE actions, so it fails and the migration rolls back.
fn require_foreign_keys_off(tx: &rusqlite::Transaction) -> HookResult {
    let on: i64 = tx.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    if on != 0 {
        return Err(HookError::Hook(
            "foreign keys must be off while rebuilding the connections table".into(),
        ));
    }
    Ok(())
}

/// Opens (creating if needed) the database file, migrates to latest and enables foreign keys.
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

/// Migrates with foreign keys off, then turns them on for the app.
///
/// Off while migrating, because a migration that rebuilds a parent table (`DROP TABLE`) would
/// otherwise run its children's ON DELETE actions; the pragma is a no-op inside a transaction,
/// so it is set here, around `to_latest`. Migrations that rebuild a parent carry
/// `.foreign_key_check()`, so they roll back rather than leave a dangling reference.
fn prepare(mut conn: Connection) -> Result<Connection, DbError> {
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| internal("set synchronous", e))?;
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(|e| internal("disable foreign keys", e))?;
    migrations()
        .to_latest(&mut conn)
        .map_err(|e| internal("migrate database", e))?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|e| internal("enable foreign keys", e))?;
    let on: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .map_err(|e| internal("read foreign keys", e))?;
    if on != 1 {
        return Err(DbError::Internal {
            message: "enable foreign keys: still off".into(),
        });
    }
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

    /// `user_version` after every migration.
    const LATEST: i64 = 13;

    #[test]
    fn migrations_are_valid() {
        // `Migrations::validate` opens its own connection, where the bundled SQLite has foreign
        // keys on, which migration 10's hook refuses; so do what it does, the way `prepare` does.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        migrations().to_latest(&mut conn).unwrap();
    }

    /// Migration 12: a connection that had consented keeps AI on by its own choice, but the
    /// app-wide consent starts empty (it was never given for every connection).
    #[test]
    fn migration_12_turns_old_consent_into_a_choice_not_a_consent() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        migrations().to_version(&mut conn, 11).unwrap();
        conn.execute_batch(
            "INSERT INTO connections (id, name, sort_order, kind, host, port, username, secret_mode, created_at, updated_at) \
             VALUES ('a', 'A', 0, 'mysql', 'h', 3306, 'u', 'saved', 't', 't'); \
             INSERT INTO ai_opt_in (connection_id, scope, origin, granted_at) VALUES ('a', 'editor', 'https://x.example', 't');",
        )
        .unwrap();
        migrations().to_latest(&mut conn).unwrap();
        let choice: (String, String, i64) = conn
            .query_row(
                "SELECT connection_id, scope, enabled FROM ai_connection",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(choice, ("a".into(), "editor".into(), 1));
        let consents: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_consent", [], |r| r.get(0))
            .unwrap();
        assert_eq!(consents, 0);
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'ai_opt_in'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
    }

    /// Migration 13: a consent given before keeps its origin and is version 1.
    #[test]
    fn migration_13_marks_an_earlier_consent_version_1() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        migrations().to_version(&mut conn, 12).unwrap();
        conn.execute_batch(
            "INSERT INTO ai_consent (scope, origin, granted_at) VALUES ('editor', 'https://x.example', 't');",
        )
        .unwrap();
        migrations().to_latest(&mut conn).unwrap();
        let row: (String, i64) = conn
            .query_row("SELECT origin, version FROM ai_consent", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(row, ("https://x.example".into(), 1));
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
        assert_eq!(v, LATEST);
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
        let conn = prepare(conn).unwrap();
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
        let conn = prepare(conn).unwrap();
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
    fn migration_7_adds_jobs_and_origins() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        Migrations::new(vec![
            M::up(MIGRATION_1),
            M::up(MIGRATION_2),
            M::up(MIGRATION_3),
            M::up(MIGRATION_4),
            M::up(MIGRATION_5),
            M::up(MIGRATION_6),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn.execute_batch("INSERT INTO connections (id,name,sort_order,kind,host,port,username,secret_mode,created_at,updated_at) VALUES ('c','c',0,'mysql','h',3306,'u','ask','now','now'); INSERT INTO query_history (id,connection_id,sql_text,redacted,kind,started_at,status,origin,outcome_unknown) VALUES (9,'c','ALTER TABLE t ADD c INT',0,'ddl','now','ok','table_design',1);").unwrap();
        let conn = prepare(conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, LATEST);
        let row: (i64, String, i64) = conn
            .query_row(
                "SELECT id,origin,outcome_unknown FROM query_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (9, "table_design".into(), 1));
        for origin in ["import", "restore"] {
            conn.execute(
                "INSERT INTO query_history (connection_id,sql_text,redacted,kind,started_at,status,origin) \
                 VALUES ('c','INSERT INTO t VALUES (1)',0,'write','now','ok',?1)",
                [origin],
            )
            .unwrap();
        }
        assert!(conn
            .execute("UPDATE query_history SET origin='invalid' WHERE id=9", [])
            .is_err());

        let insert_job = |kind: &str, state: &str| {
            conn.execute(
                "INSERT INTO jobs (id,kind,connection_id,title,state,created_at) \
                 VALUES (?1,?2,'c','t',?3,'now')",
                [
                    format!("{kind}-{state}"),
                    kind.to_string(),
                    state.to_string(),
                ],
            )
        };
        insert_job("import", "queued").unwrap();
        assert!(insert_job("x", "queued").is_err());
        assert!(insert_job("dump", "y").is_err());
        let (log, dropped, rerun, unknown): (String, i64, i64, i64) = conn
            .query_row(
                "SELECT log_json,dropped_lines,rerunnable,outcome_unknown FROM jobs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((log.as_str(), dropped, rerun, unknown), ("[]", 0, 0, 0));
        conn.execute("DELETE FROM connections WHERE id='c'", [])
            .unwrap();
        let conn_id: Option<String> = conn
            .query_row("SELECT connection_id FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conn_id, None);
    }
    /// Connections saved without an SSL config (the dialog's old way of saving Prefer)
    /// become Prefer; explicit configs are kept.
    #[test]
    fn migration_8_turns_missing_ssl_into_prefer() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::new(vec![
            M::up(MIGRATION_1),
            M::up(MIGRATION_2),
            M::up(MIGRATION_3),
            M::up(MIGRATION_4),
            M::up(MIGRATION_5),
            M::up(MIGRATION_6),
            M::up(MIGRATION_7),
        ])
        .to_latest(&mut conn)
        .unwrap();
        let disable =
            r#"{"version":1,"mode":"disable","ca_path":null,"cert_path":null,"key_path":null}"#;
        conn.execute_batch(&format!(
            "INSERT INTO connections (id,name,sort_order,kind,host,port,username,secret_mode,ssl_json,created_at,updated_at) \
             VALUES ('a','a',0,'mysql','h',3306,'u','ask',NULL,'now','now'), \
                    ('b','b',1,'postgres','h',5432,'u','ask','{disable}','now','now');"
        ))
        .unwrap();
        let conn = prepare(conn).unwrap();
        let ssl = |id: &str| -> dnt_core::config::SslConfig {
            let json: String = conn
                .query_row("SELECT ssl_json FROM connections WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            serde_json::from_str(&json).unwrap()
        };
        assert_eq!(ssl("a").mode, dnt_core::config::SslMode::Prefer);
        assert_eq!(ssl("a").ca_path, None);
        assert_eq!(ssl("b").mode, dnt_core::config::SslMode::Disable);
    }
    /// The data seeder's job kind and history origin are accepted; rows and ids survive.
    #[test]
    fn migration_9_accepts_seed_and_keeps_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::new(vec![
            M::up(MIGRATION_1),
            M::up(MIGRATION_2),
            M::up(MIGRATION_3),
            M::up(MIGRATION_4),
            M::up(MIGRATION_5),
            M::up(MIGRATION_6),
            M::up(MIGRATION_7),
            M::up(MIGRATION_8),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn.execute_batch(
            "INSERT INTO connections (id,name,sort_order,kind,host,port,username,secret_mode,created_at,updated_at) \
             VALUES ('c','c',0,'mysql','h',3306,'u','ask','now','now'); \
             INSERT INTO jobs (id,kind,connection_id,title,state,created_at) \
             VALUES ('j1','import','c','Import a','done','now'); \
             INSERT INTO query_history (id,connection_id,sql_text,redacted,kind,started_at,status,origin) \
             VALUES (41,'c','SELECT 1',0,'read','now','ok','import');",
        )
        .unwrap();
        let conn = prepare(conn).unwrap();
        let kind: String = conn
            .query_row("SELECT kind FROM jobs WHERE id='j1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "import");
        let origin: String = conn
            .query_row("SELECT origin FROM query_history WHERE id=41", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(origin, "import");
        conn.execute(
            "INSERT INTO jobs (id,kind,connection_id,title,state,created_at) \
             VALUES ('j2','seed','c','Generate data for t','queued','now')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO query_history (connection_id,sql_text,redacted,kind,started_at,status,origin) \
             VALUES ('c','-- Generate',0,'write','now','ok','seed')",
            [],
        )
        .unwrap();
        assert!(conn
            .execute(
                "INSERT INTO jobs (id,kind,title,state,created_at) VALUES ('j3','bogus','t','queued','now')",
                [],
            )
            .is_err());
        assert!(conn
            .execute(
                "INSERT INTO query_history (connection_id,sql_text,redacted,kind,started_at,status,origin) \
                 VALUES ('c','x',0,'write','now','ok','bogus')",
                [],
            )
            .is_err());
        // The index of each rebuilt table is back.
        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND name IN ('jobs_created','query_history_conn_time')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 2);
    }
    /// Every row of `table`, every column, in rowid order.
    fn dump(conn: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |r| {
            (0..n)
                .map(|i| r.get::<_, rusqlite::types::Value>(i))
                .collect()
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    /// `(type, name, tbl_name, sql)` of every schema object, ordered by name.
    fn schema(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        let mut stmt = conn
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    const ALL_TABLES: [&str; 8] = [
        "groups",
        "connections",
        "app_settings",
        "query_tabs",
        "query_history",
        "favorites",
        "snippets",
        "jobs",
    ];

    /// Rebuilding `connections` with foreign keys on would cascade into its children (history
    /// deleted, job/tab/favorite links nulled). Migration 10 keeps every row and every link,
    /// and only the kind CHECK of the schema changes.
    #[test]
    fn migration_10_keeps_child_rows() {
        let conn = v9_with_children();
        let rows_before: Vec<_> = ALL_TABLES.iter().map(|t| dump(&conn, t)).collect();
        let schema_before = schema(&conn);

        let conn = prepare(conn).unwrap();
        check_migrated(conn, rows_before, schema_before);
    }

    /// Migration 10 run directly with foreign keys on (not through `prepare`) would cascade
    /// its `DROP TABLE` into the children; its hook refuses, so it rolls back and keeps them.
    #[test]
    fn migration_10_refuses_with_foreign_keys_on() {
        let mut conn = v9_with_children();
        let rows_before: Vec<_> = ALL_TABLES.iter().map(|t| dump(&conn, t)).collect();
        let schema_before = schema(&conn);

        let err = migrations().to_latest(&mut conn).unwrap_err();

        assert!(
            err.to_string().contains("foreign keys must be off"),
            "{err}"
        );
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 9);
        for (table, before) in ALL_TABLES.iter().zip(&rows_before) {
            assert_eq!(&dump(&conn, table), before, "{table}");
        }
        assert_eq!(schema(&conn), schema_before);
    }

    /// A schema-9 database with foreign keys on (as the app left it) and a row in every table,
    /// each child linked to connection `a`.
    fn v9_with_children() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        // As today: foreign keys are on when the app opens its database.
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        Migrations::new(vec![
            M::up(MIGRATION_1),
            M::up(MIGRATION_2),
            M::up(MIGRATION_3),
            M::up(MIGRATION_4),
            M::up(MIGRATION_5),
            M::up(MIGRATION_6),
            M::up(MIGRATION_7),
            M::up(MIGRATION_8),
            M::up(MIGRATION_9),
        ])
        .to_latest(&mut conn)
        .unwrap();
        conn.execute_batch(
            "INSERT INTO groups (id,parent_id,name,sort_order) VALUES ('g',NULL,'G',0); \
             INSERT INTO connections (id,group_id,name,sort_order,kind,host,port,username,secret_mode,ssl_json,options_json,created_at,updated_at,last_connected_at) \
             VALUES ('a',NULL,'A',0,'mysql','h',3306,'u','saved',NULL,'{\"x\":1}','c1','u1','l1'), \
                    ('b','g','B',0,'postgres','h2',5432,'u2','ask','{}','{}','c2','u2',NULL); \
             INSERT INTO query_history (id,connection_id,sql_text,redacted,kind,started_at,status,origin) \
             VALUES (1,'a','SELECT 1',0,'read','t1','ok','query'), \
                    (2,'a','SELECT 2',0,'read','t2','error','data_edit'), \
                    (3,'a','SELECT 3',1,'write','t3','cancelled','seed'); \
             INSERT INTO jobs (id,kind,connection_id,title,state,created_at) \
             VALUES ('j1','import','a','Import','done','now'); \
             INSERT INTO query_tabs (id,title,sort_order,connection_id,sql_text,updated_at) \
             VALUES ('t1','Query',0,'a','SELECT 1','now'); \
             INSERT INTO favorites (id,name,folder,sql,connection_id,created_at,updated_at,sort) \
             VALUES ('f1','Fav',NULL,'SELECT 1','a','now','now',0); \
             INSERT INTO snippets (id,prefix,label,body,created_at,updated_at) \
             VALUES ('s1','sel','Select','SELECT','now','now'); \
             INSERT INTO app_settings (key,value) VALUES ('k','v');",
        )
        .unwrap();
        conn
    }

    type Schema = Vec<(String, String, String, Option<String>)>;

    fn check_migrated(
        conn: Connection,
        rows_before: Vec<Vec<Vec<rusqlite::types::Value>>>,
        schema_before: Schema,
    ) {
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        // Migrated to the latest (10, then the later migrations).
        assert_eq!(v, LATEST);
        // Every row of every table is still there, byte for byte (links included).
        for (table, before) in ALL_TABLES.iter().zip(&rows_before) {
            assert_eq!(&dump(&conn, table), before, "{table}");
        }
        let job_conn: Option<String> = conn
            .query_row("SELECT connection_id FROM jobs WHERE id='j1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(job_conn.as_deref(), Some("a"));
        let history: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM query_history WHERE connection_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(history, 3);

        // The schema is unchanged apart from the kind CHECK (SQLite quotes a renamed table's
        // name in its stored CREATE statement).
        let expected: Vec<_> = schema_before
            .into_iter()
            .map(|(ty, name, tbl, sql)| {
                let sql = sql.map(|s| {
                    if name == "connections" {
                        s.replace(
                            "CHECK (kind IN ('mysql','postgres'))",
                            "CHECK (kind IN ('mysql','postgres','mongo','redis'))",
                        )
                    } else {
                        s
                    }
                });
                (ty, name, tbl, sql)
            })
            .collect();
        // Tables of later migrations are not migration 10's concern.
        let actual: Vec<_> = schema(&conn)
            .into_iter()
            .filter(|(_, _, tbl, _)| !tbl.starts_with("ai_"))
            .map(|(ty, name, tbl, sql)| {
                let sql = sql.map(|s| {
                    s.replace(
                        "CREATE TABLE \"connections\" (",
                        "CREATE TABLE connections (",
                    )
                });
                (ty, name, tbl, sql)
            })
            .collect();
        assert_eq!(actual, expected);

        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        let on: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);

        let insert = |id: &str, kind: &str| {
            conn.execute(
                "INSERT INTO connections (id,name,sort_order,kind,host,port,username,secret_mode,created_at,updated_at) \
                 VALUES (?1,?1,1,?2,'h',1,'u','ask','now','now')",
                [id, kind],
            )
        };
        insert("m", "mongo").unwrap();
        insert("r", "redis").unwrap();
        let err = insert("o", "oracle").unwrap_err();
        assert!(err.to_string().contains("CHECK constraint failed"), "{err}");

        // The children's foreign keys point at the rebuilt table.
        conn.execute("DELETE FROM connections WHERE id='a'", [])
            .unwrap();
        let history: i64 = conn
            .query_row("SELECT COUNT(*) FROM query_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(history, 0);
        let links: (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT (SELECT connection_id FROM jobs), (SELECT connection_id FROM query_tabs), \
                 (SELECT connection_id FROM favorites)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(links, (None, None, None));
        conn.execute("DELETE FROM groups WHERE id='g'", []).unwrap();
        let group: Option<String> = conn
            .query_row("SELECT group_id FROM connections WHERE id='b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(group, None);
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
