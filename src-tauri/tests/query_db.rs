//! Query runs against the Docker test matrix (`DNT_TEST_DB=1`): streaming, Stop, the
//! read-only guard and server, redacted history and logs, Explain and the completion
//! cache after DDL. Own binary: `redacted_history_and_logs` installs the global log
//! subscriber.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
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
                "rows",
                "result_end",
                "statement_start",
                "result_start",
                "rows",
                "result_end",
                "done"
            ],
            "{t:?}"
        );
        let ends = events.of("result_end");
        assert_eq!(ends[0]["row_count"], 2, "{t:?}");
        assert_eq!(ends[1]["result_id"], 1, "{t:?}");
        let rows = events.of("rows");
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
        let sql = match t.kind() {
            DbKind::MySql => "SELECT SLEEP(10)",
            DbKind::Postgres => "SELECT pg_sleep(10)",
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
        let driver: Box<dyn Driver> = match t.kind() {
            DbKind::MySql => Box::new(MySqlDriver),
            DbKind::Postgres => Box::new(PgDriver),
        };
        let mut cfg = t.resolved(Some(FIXTURE_DB));
        cfg.read_only = true;
        let session = driver.connect(&cfg).await.unwrap();
        let kind = classify(insert, session.dialect());
        let err = session
            .execute_one(insert, kind, &ExecOptions::default(), &mut Discard)
            .await
            .unwrap_err();
        match (t.kind(), &err) {
            (DbKind::Postgres, DbError::Server { sqlstate, .. }) => {
                assert_eq!(sqlstate.as_deref(), Some("25006"), "{t:?}")
            }
            (DbKind::MySql, DbError::Server { code, .. }) => {
                assert_eq!(code.as_deref(), Some("1792"), "{t:?}")
            }
            _ => panic!("{t:?}: {err:?}"),
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
        let script = match t.kind() {
            DbKind::MySql => format!(
                "DROP USER IF EXISTS 'p2_redact'@'%';\n\
                 CREATE USER 'p2_redact'@'%' IDENTIFIED BY '{LITERAL}';\n\
                 DROP USER 'p2_redact'@'%'"
            ),
            DbKind::Postgres => format!(
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
        let analyze = match t.kind() {
            DbKind::MySql => format!("ANALYZE TABLE {table}"),
            DbKind::Postgres => format!("ANALYZE {table}"),
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
        let rows = events.of("rows");
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
