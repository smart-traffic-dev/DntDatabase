//! Query runs against the Docker test matrix (`DNT_TEST_DB=1`): streaming, Stop, the
//! read-only guard and server, redacted history and logs, Explain and the completion
//! cache after DDL. Own binary: `redacted_history_and_logs` installs the global log
//! subscriber.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::{DbKind, SqlKind};
use dnt_core::sql::classify;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_query::{Target as Pick, Verdict};
use dnt_store::history::HistoryFilter;
use dnt_store::repo::NewSecrets;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::settings::set_log_level_impl;
use dntdatabase::logging::{self, LogLevel};
use dntdatabase::run_events::{EventSink, RunOptions};
use dntdatabase::state::{AppPaths, AppState};
use secrecy::SecretString;
use serde_json::{json, Value as Json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const LONG: Duration = Duration::from_secs(20);

#[derive(Default)]
struct Events(Mutex<Vec<Json>>);

impl Events {
    fn list(&self) -> Vec<Json> {
        self.0.lock().unwrap().clone()
    }

    fn of(&self, tag: &str) -> Vec<Json> {
        self.list().into_iter().filter(|e| e["t"] == tag).collect()
    }

    async fn done(&self, within: Duration) -> Json {
        tokio::time::timeout(within, async {
            loop {
                if let Some(done) = self.of("done").pop() {
                    return done;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no done within {within:?}: {:?}", self.list()))
    }
}

fn collector() -> (EventSink, Arc<Events>) {
    let events = Arc::new(Events::default());
    let out = Arc::clone(&events);
    (
        Arc::new(move |e| {
            out.0.lock().unwrap().push(serde_json::to_value(e).unwrap());
            true
        }),
        events,
    )
}

fn opts() -> RunOptions {
    RunOptions {
        max_rows: 10_000,
        continue_on_error: false,
    }
}

fn state_in(dir: &Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap()
}

fn config(t: Target, read_only: bool) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: Some(FIXTURE_DB.into()),
        env_tag: None,
        read_only,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    }
}

async fn open(state: &AppState, t: Target, read_only: bool) -> Uuid {
    let id = state
        .repo
        .create(
            config(t, read_only),
            NewSecrets {
                db_password: Some(SecretString::from(TEST_PASSWORD)),
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    state.sessions.open(id, None).await.unwrap();
    id
}

/// The rows of each stored result of `events`, read back from the result store as
/// `{ "rows": [...] }` (the shape `rows` events had).
fn stored_rows(state: &AppState, events: &Events) -> Vec<Json> {
    events
        .of("result_stored")
        .iter()
        .map(|e| {
            let token: Uuid = e["token"].as_str().unwrap().parse().unwrap();
            let page = state.sessions.results().page(token, 0, 500).unwrap();
            json!({ "rows": page.rows })
        })
        .collect()
}

/// Plans `text` (all of it) on tab `tab` and runs it; returns the events once done.
async fn run_all(state: &AppState, tab: &str, id: Uuid, text: &str) -> Arc<Events> {
    let view = state
        .sessions
        .query_plan(tab, id, None, text, Pick::All)
        .await
        .unwrap();
    let (emit, events) = collector();
    state
        .sessions
        .query_run(view.plan_id, true, opts(), emit)
        .await
        .unwrap();
    events.done(LONG).await;
    events
}

#[tokio::test]
async fn run_select_streams_rows() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    for t in Target::all() {
        let id = open(&state, t, false).await;
        let events = run_all(
            &state,
            "t1",
            id,
            "SELECT 1 AS a UNION ALL SELECT 2; SELECT 'Tiếng Việt 😀' AS b",
        )
        .await;
        let tags: Vec<String> = events
            .list()
            .iter()
            .map(|e| e["t"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            tags,
            [
                "statement_start",
                "result_start",
                "result_stored",
                "rows_stored",
                "result_end",
                "statement_start",
                "result_start",
                "result_stored",
                "rows_stored",
                "result_end",
                "done"
            ],
            "{t:?}"
        );
        let ends = events.of("result_end");
        assert_eq!(ends[0]["row_count"], 2, "{t:?}");
        assert_eq!(ends[1]["result_id"], 1, "{t:?}");
        let rows = stored_rows(&state, &events);
        assert_eq!(rows[1]["rows"][0][0]["v"], "Tiếng Việt 😀", "{t:?}");
        assert_eq!(events.of("done")[0]["stop"], Json::Null, "{t:?}");
        state.sessions.close(id).await;
    }
}

#[tokio::test]
async fn stop_sleep_under_one_second() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    for t in Target::all() {
        let id = open(&state, t, false).await;
        let sql = match t.sql_kind() {
            SqlKind::MySql => "SELECT SLEEP(10)",
            SqlKind::Postgres => "SELECT pg_sleep(10)",
        };
        let view = state
            .sessions
            .query_plan("t1", id, None, sql, Pick::All)
            .await
            .unwrap();
        let (emit, events) = collector();
        let run_id = state
            .sessions
            .query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let t0 = Instant::now();
        state.sessions.query_cancel(run_id).await.unwrap();
        let done = events.done(LONG).await;
        let took = t0.elapsed();
        assert!(took < Duration::from_secs(1), "{t:?}: took {took:?}");
        assert_eq!(done["stop"], json!({ "kind": "cancelled" }), "{t:?}");
        assert_eq!(
            events.of("stopping"),
            [json!({ "t": "stopping", "reason": "user" })],
            "{t:?}"
        );
        // The tab's session is still usable.
        let events = run_all(&state, "t1", id, "SELECT 1").await;
        assert_eq!(events.of("done")[0]["stop"], Json::Null, "{t:?}");
        state.sessions.close(id).await;
    }
}

struct Discard;

impl RowSink for Discard {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

#[tokio::test]
async fn read_only_connection_blocks_and_server_blocks() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    let insert = "INSERT INTO customers (name, email) VALUES ('ro', 'ro@example.invalid')";
    for t in Target::all() {
        let id = open(&state, t, true).await;
        let view = state
            .sessions
            .query_plan("t1", id, None, insert, Pick::All)
            .await
            .unwrap();
        assert_eq!(view.stmts[0].verdict, Verdict::Block, "{t:?}");
        let (emit, events) = collector();
        let err = state
            .sessions
            .query_run(view.plan_id, true, opts(), emit)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DbError::Blocked { count: 1 }),
            "{t:?}: {err:?}"
        );
        assert!(events.list().is_empty());
        state.sessions.close(id).await;

        // The server refuses it too: the session itself is read-only.
        let driver: Box<dyn Driver> = match t.sql_kind() {
            SqlKind::MySql => Box::new(MySqlDriver),
            SqlKind::Postgres => Box::new(PgDriver),
        };
        let mut cfg = t.resolved(Some(FIXTURE_DB));
        cfg.read_only = true;
        let session = driver.connect(&cfg).await.unwrap();
        let s = session.sql().unwrap();
        let kind = classify(insert, s.dialect());
        let err = s
            .execute_one(insert, kind, &ExecOptions::default(), &mut Discard)
            .await
            .unwrap_err();
        match (t.sql_kind(), &err) {
            (SqlKind::Postgres, DbError::Server { sqlstate, .. }) => {
                assert_eq!(sqlstate.as_deref(), Some("25006"), "{t:?}")
            }
            (SqlKind::MySql, DbError::Server { code, .. }) => {
                assert_eq!(code.as_deref(), Some("1792"), "{t:?}")
            }
            (SqlKind::MySql | SqlKind::Postgres, _) => panic!("{t:?}: {err:?}"),
        }
        session.close().await;
    }
}

/// Every file below `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if std::fs::read(&path)
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
        {
            found.push(path.display().to_string());
        }
    }
    found
}

#[tokio::test]
async fn redacted_history_and_logs() {
    require_db!();
    const LITERAL: &str = "P2-hunter2-literal";
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::in_dir(dir.path().join("data"));
    std::fs::create_dir_all(&paths.logs_dir).unwrap();
    let log = logging::init(&paths.logs_dir);
    let state = AppState::open(paths.clone(), Arc::new(MemorySecretStore::default())).unwrap();
    set_log_level_impl(&state, &log, LogLevel::Debug)
        .await
        .unwrap();

    for t in Target::all() {
        let id = open(&state, t, false).await;
        let script = match t.sql_kind() {
            SqlKind::MySql => format!(
                "DROP USER IF EXISTS 'p2_redact'@'%';\n\
                 CREATE USER 'p2_redact'@'%' IDENTIFIED BY '{LITERAL}';\n\
                 DROP USER 'p2_redact'@'%'"
            ),
            SqlKind::Postgres => format!(
                "DROP ROLE IF EXISTS p2_redact;\n\
                 CREATE ROLE p2_redact LOGIN PASSWORD '{LITERAL}';\n\
                 DROP ROLE p2_redact"
            ),
        };
        let events = run_all(&state, "t1", id, &script).await;
        assert_eq!(
            events.of("done")[0]["stop"],
            Json::Null,
            "{t:?}: {:?}",
            events.list()
        );
        let rows = state
            .repo
            .history_list(&HistoryFilter {
                connection_id: Some(id),
                search: None,
                origin: None,
                limit: 10,
            })
            .unwrap();
        assert_eq!(rows.len(), 3, "{t:?}");
        let created = rows
            .iter()
            .find(|r| r.sql_text.starts_with("CREATE"))
            .unwrap();
        assert!(created.redacted, "{t:?}: {created:?}");
        assert!(created.sql_text.contains("[REDACTED]"), "{t:?}");
        state.sessions.close(id).await;
    }

    drop(state);
    // Flushes the log writer.
    drop(log);
    let logs: String = std::fs::read_dir(&paths.logs_dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap_or_default())
        .collect();
    assert!(
        logs.contains("executing") && logs.contains("[REDACTED]"),
        "debug logs carry the redacted SQL"
    );
    let leaks = files_containing(&paths.data_dir, LITERAL);
    assert!(leaks.is_empty(), "literal found in {leaks:?}");
}

#[tokio::test]
async fn explain_tree_per_engine() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    for t in Target::all() {
        let id = open(&state, t, false).await;
        let text = "SELECT * FROM customers WHERE id = 1";
        let view = state
            .sessions
            .query_explain("t1", id, None, text, 0, false)
            .await
            .unwrap();
        let (emit, events) = collector();
        state
            .sessions
            .query_run(view.plan_id, false, opts(), emit)
            .await
            .unwrap();
        let done = events.done(LONG).await;
        assert_eq!(done["stop"], Json::Null, "{t:?}: {:?}", events.list());
        let explain = events.of("explain");
        assert_eq!(explain.len(), 1, "{t:?}: {:?}", events.list());
        let want = if t == Target::MySql57 {
            "table"
        } else {
            "tree"
        };
        assert_eq!(explain[0]["plan"]["kind"], want, "{t:?}");
        assert!(events.of("rows").is_empty(), "{t:?}");
        assert!(events.of("result_stored").is_empty(), "{t:?}");
        assert!(events.of("result_stored").is_empty(), "{t:?}");

        // Explain Analyze of a write is refused before anything runs.
        let err = state
            .sessions
            .query_explain("t1", id, None, "DELETE FROM customers", 0, true)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DbError::Config { field: Some(f), .. } if f == "explain"),
            "{t:?}: {err:?}"
        );
        state.sessions.close(id).await;
    }
}

/// Row estimates (spec §4.3): on a Staging connection a DELETE without WHERE gets the
/// server's `EXPLAIN` estimate, which deletes nothing and keeps the tab's transaction.
#[tokio::test]
async fn estimate_on_staging_delete() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    for t in Target::all() {
        let mut cfg = config(t, false);
        cfg.env_tag = Some(dnt_core::config::EnvTag::Staging);
        let id = state
            .repo
            .create(
                cfg,
                NewSecrets {
                    db_password: Some(SecretString::from(TEST_PASSWORD)),
                    ssh_password: None,
                    ssh_key_passphrase: None,
                },
            )
            .unwrap()
            .id;
        state.sessions.open(id, None).await.unwrap();
        let table = format!("p3_estimate_{}", &Uuid::new_v4().simple().to_string()[..8]);
        let values: Vec<String> = (1..=300).map(|i| format!("({i})")).collect();
        let analyze = match t.sql_kind() {
            SqlKind::MySql => format!("ANALYZE TABLE {table}"),
            SqlKind::Postgres => format!("ANALYZE {table}"),
        };
        let setup = format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY);\n\
             INSERT INTO {table} VALUES {};\n{analyze}",
            values.join(", ")
        );
        let events = run_all(&state, "t1", id, &setup).await;
        assert_eq!(
            events.of("done")[0]["stop"],
            Json::Null,
            "{t:?}: {:?}",
            events.list()
        );

        for (text, in_tx) in [
            (format!("DELETE FROM {table}"), false),
            (format!("UPDATE {table} SET id = id WHERE 1 = 1"), true),
        ] {
            if in_tx {
                run_all(&state, "t1", id, "BEGIN").await;
                // Nothing has read in the transaction yet: no estimate (it would take
                // the transaction's first snapshot).
                let view = state
                    .sessions
                    .query_plan("t1", id, None, &text, Pick::All)
                    .await
                    .unwrap();
                assert_eq!(
                    view.stmts[0].estimate,
                    Some(dnt_query::RowEstimate::Unavailable),
                    "{t:?}"
                );
                run_all(&state, "t1", id, "SELECT 1").await;
            }
            let view = state
                .sessions
                .query_plan("t1", id, None, &text, Pick::All)
                .await
                .unwrap();
            let s = &view.stmts[0];
            assert_eq!(s.verdict, Verdict::Confirm, "{t:?}");
            assert_eq!(s.confirm_name, None, "{t:?}: no typed name off Production");
            match s.estimate {
                Some(dnt_query::RowEstimate::Rows { n }) => assert!(n > 0.0, "{t:?}: {n}"),
                other => panic!("{t:?} {text}: {other:?}"),
            }
            let want = if in_tx {
                dnt_core::tx::TxState::Active
            } else {
                dnt_core::tx::TxState::Idle
            };
            assert_eq!(view.tx_state, want, "{t:?}");
            if in_tx {
                let after = state
                    .sessions
                    .query_tx("t1", dntdatabase::query_sessions::TxAction::Rollback)
                    .await
                    .unwrap();
                assert_eq!(after, dnt_core::tx::TxState::Idle, "{t:?}");
            }
        }
        // Nothing was deleted.
        let events = run_all(&state, "t1", id, &format!("SELECT COUNT(*) FROM {table}")).await;
        let rows = stored_rows(&state, &events);
        let count = &rows[0]["rows"][0][0]["v"];
        assert!(count == 300 || count == "300", "{t:?}: {count}");
        run_all(&state, "t1", id, &format!("DROP TABLE {table}")).await;
        state.sessions.close(id).await;
    }
}

#[tokio::test]
async fn ddl_refreshes_completion_cache() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    for t in Target::all() {
        let id = open(&state, t, false).await;
        run_all(&state, "t1", id, "DROP TABLE IF EXISTS p2_ddl_cache").await;
        let before = tokio::time::timeout(LONG, state.sessions.schema_cache_for(id, FIXTURE_DB))
            .await
            .unwrap()
            .unwrap();
        assert!(before.tables.iter().all(|x| x.name != "p2_ddl_cache"));

        let events = run_all(&state, "t1", id, "CREATE TABLE p2_ddl_cache (a INT)").await;
        assert_eq!(events.of("done")[0]["stop"], Json::Null, "{t:?}");
        let found = tokio::time::timeout(LONG, async {
            loop {
                let cache = state
                    .sessions
                    .schema_cache_for(id, FIXTURE_DB)
                    .await
                    .unwrap();
                if cache.tables.iter().any(|x| x.name == "p2_ddl_cache") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(found.is_ok(), "{t:?}: the cache was not refreshed");
        run_all(&state, "t1", id, "DROP TABLE p2_ddl_cache").await;
        state.sessions.close(id).await;
    }
}

/// Manual commit mode (spec §4.2e): a write begins a transaction on the tab's session;
/// Rollback brings the rows back. MySQL DDL in it is flagged as an implicit commit.
#[tokio::test]
async fn rollback_in_manual_mode_restores_rows() {
    use dnt_core::tx::TxState;
    use dntdatabase::query_sessions::{TxAction, TxMode};
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    for t in Target::all() {
        let id = open(&state, t, false).await;
        let table = format!("p3_manual_{}", &Uuid::new_v4().simple().to_string()[..8]);
        let count_sql = format!("SELECT COUNT(*) FROM {table}");
        let count = |events: Arc<Events>| {
            let rows = stored_rows(&state, &events);
            let v = rows[0]["rows"][0][0]["v"].clone();
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap()
        };
        let setup = format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY);\nINSERT INTO {table} VALUES (1), (2), (3)"
        );
        let events = run_all(&state, "t1", id, &setup).await;
        assert_eq!(events.of("done")[0]["stop"], Json::Null, "{t:?}");

        let mode = state.sessions.query_tx_mode("t1", TxMode::Manual).await;
        assert_eq!(mode.unwrap(), TxState::Idle, "{t:?}");
        let events = run_all(
            &state,
            "t1",
            id,
            &format!("DELETE FROM {table} WHERE id <= 2"),
        )
        .await;
        assert_eq!(
            events.of("done")[0]["stop"],
            Json::Null,
            "{t:?}: {:?}",
            events.list()
        );
        let info = state.sessions.query_tx_info("t1").await.unwrap();
        assert_eq!(info.state, TxState::Active, "{t:?}");
        assert_eq!(
            count(run_all(&state, "t1", id, &count_sql).await),
            1,
            "{t:?}"
        );
        // Leaving Manual with the transaction open is refused.
        let err = state
            .sessions
            .query_tx_mode("t1", TxMode::Auto)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::TransactionOpen), "{t:?}: {err:?}");

        // MySQL DDL would commit the open transaction; PostgreSQL DDL would not.
        let view = state
            .sessions
            .query_plan(
                "t1",
                id,
                None,
                &format!("ALTER TABLE {table} ADD x INT"),
                Pick::All,
            )
            .await
            .unwrap();
        assert_eq!(
            view.stmts[0].implicit_commit,
            t.kind() == DbKind::MySql,
            "{t:?}"
        );

        let after = state.sessions.query_tx("t1", TxAction::Rollback).await;
        assert_eq!(after.unwrap(), TxState::Idle, "{t:?}");
        assert_eq!(
            count(run_all(&state, "t1", id, &count_sql).await),
            3,
            "{t:?}"
        );
        // Reads in Manual mode open nothing.
        let info = state.sessions.query_tx_info("t1").await.unwrap();
        assert_eq!(info.state, TxState::Idle, "{t:?}");

        // A transaction that ends mid-run (the user's COMMIT; on MySQL also DDL, which
        // commits it) is begun again before the next write: the first UPDATE stays, the
        // second is rolled back.
        let side = format!("{table}_x");
        let mut mids = vec!["COMMIT".to_string()];
        if t.kind() == DbKind::MySql {
            mids.push(format!("CREATE TABLE {side} (a INT)"));
        }
        for mid in mids {
            let reset = format!("DELETE FROM {table};\nINSERT INTO {table} VALUES (1), (2), (3)");
            let events = run_all(&state, "t1", id, &reset).await;
            assert_eq!(events.of("done")[0]["stop"], Json::Null, "{t:?}");
            let after = state.sessions.query_tx("t1", TxAction::Commit).await;
            assert_eq!(after.unwrap(), TxState::Idle, "{t:?}");
            let script = format!(
                "UPDATE {table} SET id = 10 WHERE id = 1;\n{mid};\n\
                 UPDATE {table} SET id = 20 WHERE id = 2"
            );
            let events = run_all(&state, "t1", id, &script).await;
            assert_eq!(
                events.of("done")[0]["stop"],
                Json::Null,
                "{t:?} {mid}: {:?}",
                events.list()
            );
            let info = state.sessions.query_tx_info("t1").await.unwrap();
            assert_eq!(
                (info.state, info.statements),
                (TxState::Active, 1),
                "{t:?} {mid}"
            );
            let after = state.sessions.query_tx("t1", TxAction::Rollback).await;
            assert_eq!(after.unwrap(), TxState::Idle, "{t:?}");
            let events = run_all(
                &state,
                "t1",
                id,
                &format!("SELECT id FROM {table} ORDER BY id"),
            )
            .await;
            let ids: Vec<i64> = stored_rows(&state, &events)[0]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    let v = &r[0]["v"];
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        .unwrap()
                })
                .collect();
            assert_eq!(ids, [2, 3, 10], "{t:?} {mid}");
        }
        if t.kind() == DbKind::MySql {
            run_all(&state, "t1", id, &format!("DROP TABLE {side}")).await;
        }

        let mode = state.sessions.query_tx_mode("t1", TxMode::Auto).await;
        assert_eq!(mode.unwrap(), TxState::Idle, "{t:?}");
        run_all(&state, "t1", id, &format!("DROP TABLE {table}")).await;
        state.sessions.close(id).await;
    }
}

/// Disk-backed results (spec: backend result spooling): text and binary values larger
/// than the display preview come back clipped in pages but exact in copy and export,
/// without running the statement again; closing the connection removes the result.
#[tokio::test]
async fn stored_results_are_exact_in_copy_and_export() {
    use dntdatabase::commands::export::export_query_result_impl;
    use dntdatabase::export_format::{ExportFormat, ExportOptions};
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = state_in(dir.path());
    let text = "ab".repeat(70_000);
    let bytes: Vec<u8> = (0..140_000)
        .map(|i| if i % 2 == 0 { 0 } else { 0xff })
        .collect();
    for t in Target::all() {
        let id = open(&state, t, false).await;
        let sql = match t.sql_kind() {
            SqlKind::MySql => "SELECT REPEAT('ab', 70000) AS t, UNHEX(REPEAT('00FF', 70000)) AS b",
            SqlKind::Postgres => {
                "SELECT repeat('ab', 70000) AS t, decode(repeat('00ff', 70000), 'hex') AS b"
            }
        };
        let events = run_all(&state, "t1", id, sql).await;
        assert_eq!(
            events.of("done")[0]["stop"],
            Json::Null,
            "{t:?}: {:?}",
            events.list()
        );
        let token: Uuid = events.of("result_stored")[0]["token"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let results = state.sessions.results();

        // The page is a bounded preview with both cells marked incomplete.
        let page = results.page(token, 0, 500).unwrap();
        assert_eq!(page.truncated_cells, [(0, 0), (0, 1)], "{t:?}");
        assert!(page.bytes <= 64 * 1024 * 2, "{t:?}: {}", page.bytes);
        match &page.rows[0][1] {
            Value::Bytes { len, preview } => {
                assert_eq!(*len, 140_000, "{t:?}");
                assert!(preview.len() < 140_000, "{t:?}");
            }
            other => panic!("{t:?}: {other:?}"),
        }

        // Copy reads the exact values.
        let exact = results.values(token, &[0], &[0, 1]).unwrap();
        assert_eq!(exact[0][0], Value::Text(text.clone()), "{t:?}");
        assert_eq!(
            exact[0][1],
            Value::Bytes {
                len: 140_000,
                preview: bytes.clone()
            },
            "{t:?}"
        );

        // Export writes them whole, from the store.
        let path = dir.path().join(format!("{t:?}.csv"));
        let summary = export_query_result_impl(
            &state,
            token,
            t.sql_kind(),
            ExportOptions {
                format: ExportFormat::Csv,
                delimiter: ",".into(),
                header: true,
                quote_all: false,
                bom: false,
                table_name: "t".into(),
                formula_guard: false,
            },
            path.to_string_lossy().into_owned(),
            Uuid::new_v4(),
            Box::new(|_| {}),
        )
        .await
        .unwrap();
        assert_eq!(summary.rows, 1, "{t:?}");
        let written = std::fs::read_to_string(&path).unwrap();
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(written, format!("t,b\r\n{text},0x{hex}\r\n"), "{t:?}");

        // Closing the connection detaches the result.
        state.sessions.close(id).await;
        assert!(results.page(token, 0, 1).is_err(), "{t:?}");
    }
}
