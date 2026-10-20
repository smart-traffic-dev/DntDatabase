//! Migration 2: query tabs and query history.

use dnt_core::config::ConnectionConfig;
use dnt_core::secret::MemorySecretStore;
use dnt_core::test_util::sample_config;
use dnt_store::db::{open, open_in_memory};
use dnt_store::history::{HistoryFilter, HistoryStatus, NewHistoryEntry};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_store::tabs::QueryTabRecord;
use std::sync::Arc;
use uuid::Uuid;

fn repo() -> ConnectionRepo {
    ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::new(MemorySecretStore::default()),
    )
}

fn connection(repo: &ConnectionRepo, name: &str) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        name: name.into(),
        ..sample_config()
    };
    repo.create(
        cfg,
        NewSecrets {
            db_password: None,
            ssh_password: None,
            ssh_key_passphrase: None,
        },
    )
    .unwrap()
    .id
}

fn tab(id: &str, sort_order: i64, connection_id: Option<Uuid>) -> QueryTabRecord {
    QueryTabRecord {
        id: id.into(),
        title: format!("Query {id}"),
        sort_order,
        connection_id,
        database: Some("shop".into()),
        sql_text: "SELECT 1;".into(),
        cursor: 3,
        file_path: None,
        dirty: false,
        continue_on_error: false,
        row_limit: 10_000,
        updated_at: String::new(),
    }
}

fn entry(connection_id: Uuid, sql: &str) -> NewHistoryEntry {
    NewHistoryEntry {
        connection_id,
        database: None,
        sql_text: sql.into(),
        redacted: false,
        kind: "select".into(),
        started_at: "2026-10-20T10:00:00Z".into(),
        elapsed_ms: Some(12),
        status: HistoryStatus::Ok,
        row_count: Some(3),
        affected: None,
        error_kind: None,
        error_code: None,
    }
}

fn all() -> HistoryFilter {
    HistoryFilter {
        connection_id: None,
        search: None,
        limit: 1000,
    }
}

#[test]
fn migration_2_adds_tables() {
    let repo = repo();
    assert!(repo.tabs_load().unwrap().is_empty());
    assert!(repo.history_list(&all()).unwrap().is_empty());
}

#[test]
fn tabs_roundtrip_keeps_text_verbatim() {
    let repo = repo();
    let conn = connection(&repo, "a");
    let sql = "CREATE USER u IDENTIFIED BY 'x';\r\n-- Tiếng Việt 😀\n";
    let mut first = tab("t2", 1, Some(conn));
    first.sql_text = sql.into();
    first.file_path = Some(r"C:\work\q.sql".into());
    first.dirty = true;
    first.continue_on_error = true;
    first.row_limit = 1_000;
    repo.tabs_save(&first).unwrap();
    repo.tabs_save(&tab("t1", 0, None)).unwrap();

    let loaded = repo.tabs_load().unwrap();
    assert_eq!(
        loaded.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        ["t1", "t2"]
    );
    let got = &loaded[1];
    assert_eq!(got.sql_text, sql);
    assert!(!got.updated_at.is_empty());
    assert_eq!(
        QueryTabRecord {
            updated_at: String::new(),
            ..got.clone()
        },
        first
    );

    // Upsert replaces the row.
    first.title = "Renamed".into();
    first.sort_order = 5;
    repo.tabs_save(&first).unwrap();
    let loaded = repo.tabs_load().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[1].title, "Renamed");

    repo.tabs_delete("t2").unwrap();
    assert_eq!(repo.tabs_load().unwrap().len(), 1);
}

#[test]
fn deleting_connection_nulls_tab_connection() {
    let repo = repo();
    let conn = connection(&repo, "a");
    repo.tabs_save(&tab("t1", 0, Some(conn))).unwrap();
    repo.delete(conn).unwrap();
    let loaded = repo.tabs_load().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].connection_id, None);
    assert_eq!(loaded[0].database.as_deref(), Some("shop"));

    // Saving a tab that still names the deleted connection detaches it instead of failing.
    repo.tabs_save(&tab("t1", 0, Some(conn))).unwrap();
    assert_eq!(repo.tabs_load().unwrap()[0].connection_id, None);
}

#[test]
fn tabs_save_validates_row_limit() {
    let repo = repo();
    let mut t = tab("t1", 0, None);
    t.row_limit = 7;
    assert!(repo.tabs_save(&t).is_err());
    t.id = String::new();
    t.row_limit = 10_000;
    assert!(repo.tabs_save(&t).is_err());
}

#[test]
fn tabs_load_survives_corrupt_rows() {
    let conn = open_in_memory().unwrap();
    // Corrupt rows cannot be written through the repository; bypass the FK as well.
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO query_tabs (id, title, sort_order, connection_id, database, sql_text,
           cursor, file_path, dirty, continue_on_error, row_limit, updated_at) VALUES
         ('a-good', 'Good', 0, NULL, NULL, 'SELECT 1', 3, NULL, 0, 0, 1000, 'now'),
         ('b-bad-conn', 'Bad conn', 1, 'not-a-uuid', 'shop', 'SELECT 2', 1, NULL, 0, 0, 10000, 'now'),
         ('c-bad-cursor', 'Bad cursor', 2, NULL, NULL, 'SELECT 3', -5, NULL, 0, 0, 10000, 'now'),
         ('d-bad-limit', 'Bad limit', 3, NULL, NULL, 'SELECT 4', 99999999999, NULL, 0, 0, 7, 'now'),
         ('e-broken', 'Broken', 4, NULL, NULL, X'FF00FF', 0, NULL, 0, 0, 10000, 'now'),
         ('f-good', 'Good too', 5, NULL, NULL, 'SELECT 6', 0, NULL, 1, 1, 100000, 'now');
         PRAGMA foreign_keys = ON;",
    )
    .unwrap();
    let repo = ConnectionRepo::new(conn, Arc::new(MemorySecretStore::default()));

    let tabs = repo.tabs_load().unwrap();
    assert_eq!(
        tabs.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        [
            "a-good",
            "b-bad-conn",
            "c-bad-cursor",
            "d-bad-limit",
            "f-good"
        ]
    );
    assert_eq!(tabs[0].cursor, 3);
    assert_eq!(tabs[0].row_limit, 1000);
    assert_eq!(tabs[1].connection_id, None);
    assert_eq!(tabs[1].database.as_deref(), Some("shop"));
    assert_eq!(tabs[2].cursor, 0);
    assert_eq!(tabs[3].cursor, 0);
    assert_eq!(tabs[3].row_limit, 10_000);
    assert_eq!(tabs[4].row_limit, 100_000);
    assert!(tabs[4].dirty && tabs[4].continue_on_error);
}

#[test]
fn history_prunes_oldest_beyond_keep() {
    let repo = repo();
    let conn = connection(&repo, "a");
    let mut ids = Vec::new();
    for i in 0..8 {
        ids.push(
            repo.history_insert_keep(&entry(conn, &format!("SELECT {i}")), 5)
                .unwrap(),
        );
    }
    let mut kept: Vec<i64> = repo
        .history_list(&all())
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    kept.sort();
    assert_eq!(kept, ids[3..]);
}

#[test]
fn history_cascade_on_connection_delete() {
    let repo = repo();
    let a = connection(&repo, "a");
    let b = connection(&repo, "b");
    repo.history_insert(&entry(a, "SELECT 1")).unwrap();
    repo.history_insert(&entry(b, "SELECT 2")).unwrap();
    repo.delete(a).unwrap();
    let left = repo.history_list(&all()).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].connection_id, b);
}

#[test]
fn history_filter_by_connection_and_search() {
    let repo = repo();
    let a = connection(&repo, "a");
    let b = connection(&repo, "b");
    let mut first = entry(a, "SELECT * FROM t WHERE x LIKE '50%'");
    first.started_at = "2026-10-20T09:00:00Z".into();
    first.status = HistoryStatus::Error;
    first.error_kind = Some("query".into());
    first.error_code = Some("1064".into());
    first.elapsed_ms = None;
    first.row_count = None;
    first.affected = Some(4);
    repo.history_insert(&first).unwrap();
    repo.history_insert(&entry(a, "SELECT 500 FROM t")).unwrap();
    repo.history_insert(&entry(b, "SELECT '50%' FROM u"))
        .unwrap();

    // Newest first.
    let listed = repo.history_list(&all()).unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[2].sql_text, first.sql_text);
    assert_eq!(listed[2].status, HistoryStatus::Error);
    assert_eq!(listed[2].error_code.as_deref(), Some("1064"));
    assert_eq!(listed[2].affected, Some(4));
    assert_eq!(listed[2].elapsed_ms, None);

    let only_a = repo
        .history_list(&HistoryFilter {
            connection_id: Some(a),
            search: None,
            limit: 1000,
        })
        .unwrap();
    assert_eq!(only_a.len(), 2);

    // `%` is literal: "SELECT 500" does not match "50%".
    let found = repo
        .history_list(&HistoryFilter {
            connection_id: Some(a),
            search: Some("50%".into()),
            limit: 1000,
        })
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].sql_text, first.sql_text);

    // `_` and `\` are literal too.
    let none = repo
        .history_list(&HistoryFilter {
            connection_id: None,
            search: Some("SELECT_".into()),
            limit: 1000,
        })
        .unwrap();
    assert!(none.is_empty());

    let limited = repo
        .history_list(&HistoryFilter {
            connection_id: None,
            search: None,
            limit: 1,
        })
        .unwrap();
    assert_eq!(limited.len(), 1);
}

/// A file database in WAL mode: 10 000 rows in batches (as a long run's history
/// writer sends them) go in one transaction per batch, fast, in order.
#[test]
fn history_batches_are_fast_and_ordered_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let repo = ConnectionRepo::new(
        open(&dir.path().join("app.db")).unwrap(),
        Arc::new(MemorySecretStore::default()),
    );
    let conn = connection(&repo, "a");
    let entries: Vec<NewHistoryEntry> = (0..10_000)
        .map(|i| entry(conn, &format!("SELECT {i}")))
        .collect();
    let started = std::time::Instant::now();
    let mut ids = Vec::new();
    for batch in entries.chunks(500) {
        ids.extend(repo.history_insert_batch(batch).unwrap());
    }
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");
    assert_eq!(ids.len(), 10_000);
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids follow the order sent"
    );
    // Same `started_at`: newest first is by id.
    let newest = repo.history_list(&all()).unwrap();
    assert_eq!(newest.len(), 1000);
    assert_eq!(newest[0].sql_text, "SELECT 9999");
    assert_eq!(newest[999].sql_text, "SELECT 9000");
    assert!(repo.history_insert_batch(&[]).unwrap().is_empty());
}

#[test]
fn history_batch_prunes_once_beyond_keep() {
    let repo = repo();
    let conn = connection(&repo, "a");
    let batch: Vec<NewHistoryEntry> = (0..8)
        .map(|i| entry(conn, &format!("SELECT {i}")))
        .collect();
    let ids = repo.history_insert_batch_keep(&batch, 5).unwrap();
    let mut kept: Vec<i64> = repo
        .history_list(&all())
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    kept.sort();
    assert_eq!(kept, ids[3..]);
}

#[test]
fn file_database_uses_wal() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir.path().join("app.db")).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let sync: i64 = conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1, "NORMAL");
}

#[test]
fn history_delete_and_clear() {
    let repo = repo();
    let a = connection(&repo, "a");
    let one = repo.history_insert(&entry(a, "SELECT 1")).unwrap();
    let two = repo.history_insert(&entry(a, "SELECT 2")).unwrap();
    repo.history_insert(&entry(a, "SELECT 3")).unwrap();
    repo.history_delete(&[one, two]).unwrap();
    repo.history_delete(&[]).unwrap();
    let left = repo.history_list(&all()).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].sql_text, "SELECT 3");
    repo.history_clear().unwrap();
    assert!(repo.history_list(&all()).unwrap().is_empty());
}
