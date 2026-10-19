use dnt_core::driver::{ExecOptions, ExecSummary, RowSink, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_driver_pg::{PgDriver, PgSession};
use dnt_testkit::{require_db, Target, FIXTURE_DB};
use std::time::{Duration, Instant};

/// One result as seen by the sink.
#[derive(Debug, Default)]
struct Res {
    cols: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    batches: Vec<usize>,
    affected: Option<u64>,
}

#[derive(Debug, Default)]
struct Collect {
    results: Vec<Res>,
}

impl RowSink for Collect {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.results.push(Res {
            cols: cols.to_vec(),
            ..Res::default()
        });
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        let r = self.results.last_mut().expect("rows before result_start");
        r.batches.push(batch.len());
        r.rows.extend(batch);
    }
    fn result_end(&mut self, affected: Option<u64>, _elapsed: Duration) {
        self.results
            .last_mut()
            .expect("result_end before start")
            .affected = affected;
    }
}

async fn connect(t: Target) -> PgSession {
    PgDriver
        .connect_session(&t.resolved(Some(FIXTURE_DB)))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

async fn run(s: &PgSession, sql: &str) -> Result<(ExecSummary, Collect), DbError> {
    run_with(s, sql, ExecOptions::default()).await
}

async fn run_with(
    s: &PgSession,
    sql: &str,
    opts: ExecOptions,
) -> Result<(ExecSummary, Collect), DbError> {
    let mut sink = Collect::default();
    let summary = s.execute(sql, opts, &mut sink).await?;
    Ok((summary, sink))
}

/// First value of the first row of the last result.
fn scalar(c: &Collect) -> Value {
    c.results.last().unwrap().rows[0][0].clone()
}

async fn backend_pid(s: &PgSession) -> i64 {
    let (_, c) = run(s, "SELECT pg_backend_pid()").await.unwrap();
    match scalar(&c) {
        Value::Int(pid) => pid,
        other => panic!("not an integer: {other:?}"),
    }
}

/// Terminates backend `pid` from another session and waits until it is gone.
async fn kill(t: Target, pid: i64) {
    let admin = connect(t).await;
    let (_, c) = run(&admin, &format!("SELECT pg_terminate_backend({pid})"))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: pg_terminate_backend failed: {e}"));
    assert_eq!(scalar(&c), Value::Bool(true), "{t:?}");
    Box::new(admin).close().await;
    // Let the client notice the closed socket.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn multi_statement_results() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let (summary, c) = run(
            &s,
            "select 1 a; select 'x' b, 2 c; \
             DO $$ BEGIN PERFORM 1; END $$; \
             update customers set name=name where false",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(c.results.len(), 4, "{t:?}");
        assert_eq!(summary.results.len(), 4, "{t:?}");
        assert_eq!(c.results[0].cols[0].name, "a", "{t:?}");
        assert_eq!(c.results[0].cols[0].logical, LogicalType::Integer, "{t:?}");
        assert_eq!(c.results[0].rows, vec![vec![Value::Int(1)]], "{t:?}");
        assert_eq!(
            c.results[1].rows,
            vec![vec![Value::Text("x".into()), Value::Int(2)]],
            "{t:?}"
        );
        assert_eq!(c.results[0].affected, None, "{t:?}");
        assert!(c.results[2].cols.is_empty(), "{t:?}");
        assert_eq!(c.results[3].affected, Some(0), "{t:?}");
        assert_eq!(summary.results[3].affected, Some(0), "{t:?}");
    }
}

#[tokio::test]
async fn statement_errors_keep_sqlstate_and_position() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let err = run(&s, "SELECT * FROM no_such_table").await.err();
        match err {
            Some(DbError::Server {
                sqlstate, position, ..
            }) => {
                assert_eq!(sqlstate.as_deref(), Some("42P01"), "{t:?}");
                assert_eq!(position, Some(15), "{t:?}");
            }
            other => panic!("{t:?}: expected Server error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn all_types_roundtrip() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let (_, c) = run(&s, "SELECT * FROM all_types ORDER BY id")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let res = &c.results[0];
        let snapshot = AllTypes {
            columns: res
                .cols
                .iter()
                .map(|c| SnapColumn {
                    name: &c.name,
                    logical: c.logical,
                    db_type: &c.db_type,
                })
                .collect(),
            rows: &res.rows,
        };
        insta::assert_yaml_snapshot!(format!("{t:?}_all_types"), snapshot);
    }
}

#[derive(serde::Serialize)]
struct SnapColumn<'a> {
    name: &'a str,
    logical: LogicalType,
    db_type: &'a str,
}

#[derive(serde::Serialize)]
struct AllTypes<'a> {
    columns: Vec<SnapColumn<'a>>,
    rows: &'a [Vec<Value>],
}

#[tokio::test]
async fn special_values() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let (_, c) = run(
            &s,
            "SELECT 'NaN'::numeric, 'infinity'::date, '-infinity'::timestamptz, \
             '0001-01-01 BC'::date, INTERVAL '0', 'NaN'::float8, \
             ARRAY[[1,2],[3,4]], '{}'::int[], ARRAY[NULL, 'a']::text[]",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let row = &c.results[0].rows[0];
        assert_eq!(row[0], Value::Decimal("NaN".into()), "{t:?}");
        assert_eq!(row[1], Value::Date("infinity".into()), "{t:?}");
        assert_eq!(row[2], Value::TimestampTz("-infinity".into()), "{t:?}");
        assert_eq!(row[3], Value::Date("0001-01-01 BC".into()), "{t:?}");
        assert_eq!(
            row[4],
            Value::Other {
                type_name: "interval".into(),
                text: "PT0S".into()
            },
            "{t:?}"
        );
        assert_eq!(
            row[5],
            Value::Other {
                type_name: "float8".into(),
                text: "NaN".into()
            },
            "{t:?}"
        );
        assert_eq!(
            row[6],
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)]),
            ]),
            "{t:?}"
        );
        assert_eq!(row[7], Value::Array(vec![]), "{t:?}");
        assert_eq!(
            row[8],
            Value::Array(vec![Value::Null, Value::Text("a".into())]),
            "{t:?}"
        );
    }
}

#[tokio::test]
async fn interval_matches_server_iso_8601() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let (_, c) = run(
            &s,
            "SET intervalstyle = iso_8601; \
             SELECT c_interval, c_interval::text FROM all_types WHERE id = 1; \
             SELECT i, i::text FROM (VALUES (INTERVAL '-1 year -2 months +3 days -04:05:06.5')) v(i)",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        for res in &c.results[1..] {
            let (decoded, server) = (&res.rows[0][0], &res.rows[0][1]);
            match (decoded, server) {
                (Value::Other { text, .. }, Value::Text(expected)) => {
                    assert_eq!(text, expected, "{t:?}")
                }
                other => panic!("{t:?}: {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn unicode_text() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let (_, c) = run(&s, "SELECT ten FROM đơn_hàng")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(
            c.results[0]
                .rows
                .iter()
                .any(|r| r[0] == Value::Text("Nguyễn Văn A".into())),
            "{t:?}: {:?}",
            c.results[0].rows
        );
    }
}

#[tokio::test]
async fn max_rows_truncates() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let opts = ExecOptions {
            max_rows: 5,
            ..ExecOptions::default()
        };
        let (summary, c) = run_with(&s, "SELECT * FROM information_schema.columns", opts)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(summary.results[0].row_count, 5, "{t:?}");
        assert!(summary.results[0].truncated, "{t:?}");
        assert_eq!(c.results[0].rows.len(), 5, "{t:?}");
        // The rest of the result set was drained: the connection is still usable.
        let (_, c) = run(&s, "select 7").await.unwrap();
        assert_eq!(scalar(&c), Value::Int(7), "{t:?}");
    }
}

#[tokio::test]
async fn batches_respect_batch_size() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let opts = ExecOptions {
            batch_size: 2,
            ..ExecOptions::default()
        };
        let (summary, c) = run_with(&s, "SELECT generate_series(1, 5)", opts)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(c.results[0].batches, vec![2, 2, 1], "{t:?}");
        assert_eq!(summary.results[0].row_count, 5, "{t:?}");
        assert!(!summary.results[0].truncated, "{t:?}");
    }
}

#[tokio::test]
async fn cancel_long_query() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let handle = s.cancel_handle();
        let cancelled_at = std::sync::Mutex::new(None);
        let (res, cancel) = tokio::join!(run(&s, "SELECT pg_sleep(10)"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            *cancelled_at.lock().unwrap() = Some(Instant::now());
            handle.cancel().await
        });
        let since_cancel = cancelled_at.lock().unwrap().unwrap().elapsed();
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        assert!(
            matches!(res, Err(DbError::Cancelled { .. })),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(
            since_cancel < Duration::from_secs(1),
            "{t:?}: took {since_cancel:?}"
        );
        // The session survives a cancelled statement.
        let (_, c) = run(&s, "select 1").await.unwrap();
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
    }
}

#[tokio::test]
async fn cancel_when_idle_is_a_noop() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        s.cancel_handle()
            .cancel()
            .await
            .unwrap_or_else(|e| panic!("{t:?}: idle cancel failed: {e}"));
        // Nothing leaks into the next call.
        let (_, c) = run(&s, "select 1; select 2")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(c.results.len(), 2, "{t:?}");
    }
}

/// A statement that completes with a status keeps its result even when a cancel arrives
/// while it runs. The `DO` block swallows the cancel (`query_canceled`) and finishes
/// normally, which forces the "completed while a cancel was in flight" race.
#[tokio::test]
async fn completed_statement_during_cancel_keeps_its_result() {
    require_db!();
    let swallow = "DO $$ BEGIN PERFORM pg_sleep(1); \
                   EXCEPTION WHEN query_canceled THEN NULL; END $$";
    for t in Target::pg() {
        let s = connect(t).await;
        let handle = s.cancel_handle();
        let (res, cancel) = tokio::join!(run(&s, swallow), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel().await
        });
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        let (summary, c) = res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(summary.results.len(), 1, "{t:?}");
        assert_eq!(c.results[0].affected, Some(0), "{t:?}");

        // In a script, the cancel then stops the statements that follow.
        let handle = s.cancel_handle();
        let script = format!("{swallow}; SELECT 'not reached'");
        let (res, _) = tokio::join!(run(&s, &script), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel().await
        });
        assert!(
            matches!(res, Err(DbError::Cancelled { .. })),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
    }
}

#[tokio::test]
async fn timeout_option() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(500)),
            ..ExecOptions::default()
        };
        let started = Instant::now();
        let res = run_with(&s, "SELECT pg_sleep(5)", opts).await;
        assert!(
            matches!(
                res,
                Err(DbError::Timeout {
                    stage: ConnectStage::Execute,
                    outcome_unknown: false
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{t:?}: took {:?}",
            started.elapsed()
        );
        let (_, c) = run(&s, "select 1").await.unwrap();
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
    }
}

#[tokio::test]
async fn reconnects_when_idle_connection_killed() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        run(&s, "SET search_path TO sales, public").await.unwrap();
        let pid = backend_pid(&s).await;
        kill(t, pid).await;
        let (_, c) = run(&s, "select 1")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
        assert_ne!(backend_pid(&s).await, pid, "{t:?}: same backend");
        // The last `SET search_path` survives the reconnect ...
        let (_, c) = run(&s, "SHOW search_path").await.unwrap();
        assert_eq!(scalar(&c), Value::Text("sales, public".into()), "{t:?}");
        // ... and so does the application name from the startup parameters.
        let (_, c) = run(&s, "SHOW application_name").await.unwrap();
        assert_eq!(scalar(&c), Value::Text("DntDatabase".into()), "{t:?}");
        assert!(!s.in_transaction(), "{t:?}");
    }
}

#[tokio::test]
async fn reconnect_keeps_read_only() {
    require_db!();
    for t in Target::pg() {
        let cfg = dnt_core::config::ResolvedConfig {
            read_only: true,
            ..t.resolved(Some(FIXTURE_DB))
        };
        let s = PgDriver.connect_session(&cfg).await.unwrap();
        let pid = backend_pid(&s).await;
        kill(t, pid).await;
        let (_, c) = run(&s, "SHOW default_transaction_read_only")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(scalar(&c), Value::Text("on".into()), "{t:?}");
    }
}

#[tokio::test]
async fn killed_during_query_is_connection_lost() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let pid = backend_pid(&s).await;
        let (res, _) = tokio::join!(run(&s, "SELECT pg_sleep(5)"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            kill(t, pid).await
        });
        // The read had already started its result set: it is not replayed.
        assert!(
            matches!(
                res,
                Err(DbError::ConnectionLost {
                    was_in_tx: false,
                    ..
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        let (_, c) = run(&s, "select 1")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
    }
}

#[tokio::test]
async fn no_retry_inside_transaction() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        assert!(s.in_transaction(), "{t:?}");
        let pid = backend_pid(&s).await;
        kill(t, pid).await;
        let err = run(&s, "select 1").await.err();
        assert!(
            matches!(
                err,
                Some(DbError::ConnectionLost {
                    was_in_tx: true,
                    ..
                })
            ),
            "{t:?}: {err:?}"
        );
        assert!(!s.in_transaction(), "{t:?}");
        let (_, c) = run(&s, "select 1")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
    }
}

#[tokio::test]
async fn transaction_flags_follow_statements() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        run(&s, "START TRANSACTION").await.unwrap();
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "COMMIT").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        // DDL is transactional: it does not end the transaction.
        run(&s, "BEGIN; CREATE TEMP TABLE t12_ddl (a int)")
            .await
            .unwrap();
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "ROLLBACK").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        // An error leaves the transaction open (aborted) until it is ended.
        run(&s, "BEGIN").await.unwrap();
        let err = run(&s, "SELECT * FROM t12_no_such_table").await.err();
        assert!(
            matches!(err, Some(DbError::Server { .. })),
            "{t:?}: {err:?}"
        );
        assert!(s.in_transaction(), "{t:?}");
        let err = run(&s, "SELECT 1").await.err();
        assert!(
            matches!(&err, Some(DbError::Server { sqlstate: Some(st), .. }) if st == "25P02"),
            "{t:?}: {err:?}"
        );
        run(&s, "END").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        run(&s, "BEGIN").await.unwrap();
        run(&s, "ABORT").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
    }
}

#[tokio::test]
async fn deadlock_keeps_the_transaction() {
    require_db!();
    for t in Target::pg() {
        let a = connect(t).await;
        let b = connect(t).await;
        run(
            &a,
            "DROP TABLE IF EXISTS t12_deadlock; \
             CREATE TABLE t12_deadlock (id int PRIMARY KEY, v int); \
             INSERT INTO t12_deadlock VALUES (1, 0), (2, 0)",
        )
        .await
        .unwrap();
        run(&a, "BEGIN; UPDATE t12_deadlock SET v = v + 1 WHERE id = 1")
            .await
            .unwrap();
        run(&b, "BEGIN; UPDATE t12_deadlock SET v = v + 1 WHERE id = 2")
            .await
            .unwrap();
        let (ra, rb) = tokio::join!(
            run(&a, "UPDATE t12_deadlock SET v = v + 1 WHERE id = 2"),
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                run(&b, "UPDATE t12_deadlock SET v = v + 1 WHERE id = 1").await
            }
        );
        let state = |r: &Result<_, DbError>| match r {
            Err(DbError::Server { sqlstate, .. }) => sqlstate.clone(),
            _ => None,
        };
        let (victim, survivor) = match (state(&ra).as_deref(), state(&rb).as_deref()) {
            (Some("40P01"), None) => (&a, &b),
            (None, Some("40P01")) => (&b, &a),
            other => panic!("{t:?}: expected exactly one deadlock, got {other:?}"),
        };
        // Unlike InnoDB, PostgreSQL keeps the (now aborted) transaction open.
        assert!(victim.in_transaction(), "{t:?}: victim left tx");
        assert!(survivor.in_transaction(), "{t:?}");
        run(victim, "ROLLBACK").await.unwrap();
        run(survivor, "ROLLBACK").await.unwrap();
        assert!(
            !victim.in_transaction() && !survivor.in_transaction(),
            "{t:?}"
        );
        // Leave no permanent table behind (introspection snapshots list the schema).
        run(&a, "DROP TABLE t12_deadlock").await.unwrap();
    }
}
