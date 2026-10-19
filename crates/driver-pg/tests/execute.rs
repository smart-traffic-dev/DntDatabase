use dnt_core::driver::{ExecOptions, ExecSummary, ResultSummary, RowSink, Session};
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
    fn result_end(&mut self, summary: &ResultSummary) {
        self.results
            .last_mut()
            .expect("result_end before start")
            .affected = summary.affected;
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

/// Types without a dedicated `Value`, decoded to PostgreSQL's text form (plus BC
/// timestamps). Each is checked against the server's output function (`format('%s', x)`;
/// a `::text` cast differs for `inet`) in
/// [`unmapped_types_match_server_text`] and snapshotted by `all_types_roundtrip`.
const UNMAPPED: [(&str, &str); 21] = [
    ("c_inet", "'192.168.0.1/24'::inet"),
    ("c_inet_host", "'127.0.0.1'::inet"),
    ("c_inet6", "'fe80::1:0:0:1/64'::inet"),
    ("c_inet6_mapped", "'::ffff:1.2.3.4'::inet"),
    ("c_inet6_compat", "'::1.2.3.4'::inet"),
    ("c_inet6_single_zero", "'1:0:2:3:4:5:6:7'::inet"),
    ("c_cidr", "'10/8'::cidr"),
    ("c_cidr6", "'2001:db8::/32'::cidr"),
    ("c_macaddr", "'08:00:2b:01:02:03'::macaddr"),
    ("c_macaddr8", "'08:00:2b:01:02:03:04:05'::macaddr8"),
    ("c_money", "'-1234567.89'::money"),
    ("c_money_small", "'0.05'::money"),
    ("c_timetz", "'12:34:56.5+05:30'::timetz"),
    ("c_timetz_secs", "'01:02:03-08:00:15'::timetz"),
    ("c_bit", "B'1010'::bit(4)"),
    ("c_varbit", "B'1111111101'::varbit"),
    (
        "c_tsvector",
        "'a:1A fat:2B,4C cat:5 ''it''''s'' back\\\\slash ưu:3'::tsvector",
    ),
    ("c_xml", "'<a>Nguyễn &amp; x</a>'::xml"),
    ("c_timestamp_bc", "'0044-03-15 12:00:00 BC'::timestamp"),
    (
        "c_timestamptz_bc",
        "'0044-03-15 12:00:00.5 BC'::timestamptz",
    ),
    ("c_date_bc", "'0044-03-15 BC'::date"),
];

/// The decoded text of a value that has no dedicated `Value` variant.
fn value_text(v: &Value) -> &str {
    match v {
        Value::Other { text, .. }
        | Value::DateTime(text)
        | Value::TimestampTz(text)
        | Value::Date(text) => text,
        other => panic!("not a text-form value: {other:?}"),
    }
}

#[tokio::test]
async fn unmapped_types_match_server_text() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        // timestamptz is always UTC on the wire; interval as ISO 8601 (the P1 mapping).
        run(&s, "SET TimeZone = 'UTC'; SET intervalstyle = iso_8601")
            .await
            .unwrap();
        for (name, expr) in UNMAPPED {
            let (_, c) = run(&s, &format!("SELECT {expr}, format('%s', {expr})"))
                .await
                .unwrap_or_else(|e| panic!("{t:?} {name}: {e}"));
            let row = &c.results[0].rows[0];
            let server = match &row[1] {
                Value::Text(s) => s.as_str(),
                other => panic!("{t:?} {name}: {other:?}"),
            };
            assert_eq!(value_text(&row[0]), server, "{t:?} {name}");
        }
    }
}

#[tokio::test]
async fn all_types_roundtrip() {
    require_db!();
    let extra: Vec<String> = UNMAPPED
        .iter()
        .map(|(name, expr)| format!("{expr} AS {name}"))
        .chain(["ARRAY['10.0.0.1'::inet, NULL] AS c_inet_array".to_string()])
        .collect();
    // The fixture table plus the types above (row 1 only; NULL in row 2).
    let sql = format!(
        "SELECT a.*, x.* FROM all_types a \
         LEFT JOIN LATERAL (SELECT {} WHERE a.id = 1) x ON true ORDER BY a.id",
        extra.join(", ")
    );
    for t in Target::pg() {
        let s = connect(t).await;
        let (_, c) = run(&s, &sql).await.unwrap_or_else(|e| panic!("{t:?}: {e}"));
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
        let admin = connect(t).await;
        let handle = s.cancel_handle();
        let cancelled_at = std::sync::Mutex::new(None);
        let (res, cancel) = tokio::join!(run(&s, "SELECT pg_sleep(10), 't4_long'"), async {
            wait_running(&admin, "t4_long").await;
            *cancelled_at.lock().unwrap() = Some(Instant::now());
            handle.cancel().await
        });
        let since_cancel = cancelled_at.lock().unwrap().unwrap().elapsed();
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        assert!(
            matches!(
                res,
                Err(DbError::Cancelled {
                    outcome_unknown: false,
                    was_in_tx: false,
                })
            ),
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
    let swallow = "DO $$ BEGIN PERFORM pg_sleep(5), 't4_swallow'; \
                   EXCEPTION WHEN query_canceled THEN NULL; END $$";
    for t in Target::pg() {
        let s = connect(t).await;
        let admin = connect(t).await;
        let handle = s.cancel_handle();
        let (res, cancel) = tokio::join!(run(&s, swallow), async {
            wait_running(&admin, "t4_swallow").await;
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
            wait_running(&admin, "t4_swallow").await;
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
                    outcome_unknown: false,
                    was_in_tx: false,
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
        let admin = connect(t).await;
        let pid = backend_pid(&s).await;
        let (res, _) = tokio::join!(run(&s, "SELECT pg_sleep(5), 't4_killed'"), async {
            wait_running(&admin, "t4_killed").await;
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

fn timeout_opts(ms: u64) -> ExecOptions {
    ExecOptions {
        timeout: Some(Duration::from_millis(ms)),
        ..ExecOptions::default()
    }
}

/// Waits (up to 10 s) until a statement whose text contains `marker` is active on the
/// server, so a Cancel click lands while it executes rather than after a fixed delay.
async fn wait_running(admin: &PgSession, marker: &str) {
    let sql = format!(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE state = 'active' AND query LIKE '%{marker}%' AND pid <> pg_backend_pid()"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, c) = run(admin, &sql).await.unwrap();
        if scalar(&c) != Value::Int(0) {
            return;
        }
        assert!(Instant::now() < deadline, "{marker:?} never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn count_rows(s: &PgSession, table: &str) -> i64 {
    let (_, c) = run(s, &format!("SELECT count(*) FROM {table}"))
        .await
        .unwrap();
    match scalar(&c) {
        Value::Int(n) => n,
        other => panic!("not an integer: {other:?}"),
    }
}

fn sqlstate_of<T>(r: &Result<T, DbError>) -> Option<&str> {
    match r {
        Err(DbError::Server { sqlstate, .. }) => sqlstate.as_deref(),
        _ => None,
    }
}

/// A statement that completes during the timeout grace period reports its real result:
/// the `DO` block swallows the cancel request sent at the timeout, then inserts. A
/// statement the cancel did stop is a `Timeout`: not applied for a plain statement (PG
/// statements are atomic), unknown for a `CALL` whose procedure committed part of its
/// work. Inside a transaction the stopped statement aborts the block, which stays open.
#[tokio::test]
async fn grace_success() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        run(&s, "CREATE TEMP TABLE t4_grace (v int)").await.unwrap();

        let swallow_then_insert = "DO $$ BEGIN \
             BEGIN PERFORM pg_sleep(5); EXCEPTION WHEN query_canceled THEN NULL; END; \
             INSERT INTO t4_grace VALUES (1); END $$";
        let started = Instant::now();
        let res = run_with(&s, swallow_then_insert, timeout_opts(500)).await;
        let (summary, _) = res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert_eq!(summary.results.len(), 1, "{t:?}");
        assert_eq!(count_rows(&s, "t4_grace").await, 1, "{t:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "{t:?}");

        // Stopped by the cancel: atomic, nothing applied.
        let res = run_with(
            &s,
            "INSERT INTO t4_grace SELECT 2 FROM pg_sleep(5)",
            timeout_opts(500),
        )
        .await;
        assert!(
            matches!(
                res,
                Err(DbError::Timeout {
                    stage: ConnectStage::Execute,
                    outcome_unknown: false,
                    was_in_tx: false,
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert_eq!(count_rows(&s, "t4_grace").await, 1, "{t:?}");

        // A procedure that commits before it is stopped: may have been applied (it was).
        run(
            &s,
            "CREATE PROCEDURE pg_temp.t4_commit_then_sleep() LANGUAGE plpgsql AS $$ \
             BEGIN INSERT INTO t4_grace VALUES (3); COMMIT; PERFORM pg_sleep(5); END $$",
        )
        .await
        .unwrap();
        let res = run_with(&s, "CALL pg_temp.t4_commit_then_sleep()", timeout_opts(500)).await;
        assert!(
            matches!(
                res,
                Err(DbError::Timeout {
                    outcome_unknown: true,
                    ..
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert_eq!(count_rows(&s, "t4_grace").await, 2, "{t:?}");
        assert!(!s.in_transaction(), "{t:?}");

        // Inside a transaction: not applied, and the block is aborted but still open.
        run(&s, "BEGIN").await.unwrap();
        let res = run_with(
            &s,
            "INSERT INTO t4_grace SELECT 4 FROM pg_sleep(5)",
            timeout_opts(500),
        )
        .await;
        assert!(
            matches!(
                res,
                Err(DbError::Timeout {
                    outcome_unknown: false,
                    was_in_tx: true,
                    ..
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(s.in_transaction(), "{t:?}");
        let res = run(&s, "SELECT 1").await;
        assert_eq!(sqlstate_of(&res), Some("25P02"), "{t:?}");
        run(&s, "ROLLBACK").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        assert_eq!(count_rows(&s, "t4_grace").await, 2, "{t:?}");
    }
}

/// Two Cancel clicks on the same statement inside a transaction: the call is cancelled
/// once, the transaction is reported open (and aborted, as the server says), and no
/// stray cancel reaches the statements that follow.
#[tokio::test]
async fn cancel_twice_keeps_session_usable() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let admin = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let (h1, h2) = (s.cancel_handle(), s.cancel_handle());
        let (res, (c1, c2)) = tokio::join!(run(&s, "SELECT pg_sleep(10), 't4_twice'"), async {
            wait_running(&admin, "t4_twice").await;
            tokio::join!(h1.cancel(), h2.cancel())
        });
        c1.unwrap_or_else(|e| panic!("{t:?}: first cancel failed: {e}"));
        c2.unwrap_or_else(|e| panic!("{t:?}: second cancel failed: {e}"));
        assert!(
            matches!(
                res,
                Err(DbError::Cancelled {
                    outcome_unknown: false,
                    was_in_tx: true,
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(s.in_transaction(), "{t:?}: the aborted block is still open");
        // A PG cancel request is asynchronous: the postmaster signals the backend
        // (SIGINT) after the request returned, so a late click could in principle still
        // land on whatever the backend runs next. The generation check stops our own
        // clicks once the call is over; the remaining window is one signal delivery,
        // which the sleep below would expose as a 57014 instead of success.
        assert_eq!(
            sqlstate_of(&run(&s, "SELECT 1").await),
            Some("25P02"),
            "{t:?}"
        );
        run(&s, "ROLLBACK").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        run(&s, "SELECT pg_sleep(0.5)")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: a stray cancel hit the next call: {e}"));
    }
}

/// A timeout and a Cancel click land on the same statement: either error is fine, but
/// the session stays usable and keeps its transaction state.
#[tokio::test]
async fn timeout_racing_cancel_keeps_session_usable() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let admin = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let handle = s.cancel_handle();
        let (res, cancel) = tokio::join!(
            run_with(&s, "SELECT pg_sleep(10), 't4_race'", timeout_opts(300)),
            async {
                wait_running(&admin, "t4_race").await;
                // Land the click next to the 300 ms timeout; either may win.
                tokio::time::sleep(Duration::from_millis(280)).await;
                handle.cancel().await
            }
        );
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        assert!(
            matches!(
                res,
                Err(DbError::Cancelled {
                    outcome_unknown: false,
                    was_in_tx: true,
                } | DbError::Timeout {
                    outcome_unknown: false,
                    was_in_tx: true,
                    ..
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(s.in_transaction(), "{t:?}");
        // The cancel request is asynchronous (the backend gets a SIGINT after the request
        // returned): if the timeout's own cancel ended the statement first, the click's
        // signal could still be in flight. The generation check skips clicks after the
        // call ended; the sleep below would show a stray signal as a 57014.
        run(&s, "ROLLBACK").await.unwrap();
        run(&s, "SELECT pg_sleep(0.5)")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(!s.in_transaction(), "{t:?}");
    }
}

/// A statement that swallows every cancel request: after the timeout and the grace
/// period its connection is abandoned and replaced, and the error says so (with the
/// rolled-back transaction and the reset session settings).
#[tokio::test]
async fn stuck_statement_replaces_the_connection() {
    require_db!();
    let stuck = "DO $$ BEGIN LOOP BEGIN PERFORM pg_sleep(0.1); \
                 EXCEPTION WHEN query_canceled THEN NULL; END; END LOOP; END $$";
    for t in Target::pg() {
        let s = connect(t).await;
        run(&s, "SET work_mem = '8MB'; BEGIN").await.unwrap();
        let pid = backend_pid(&s).await;
        let started = Instant::now();
        let res = run_with(&s, stuck, timeout_opts(300)).await;
        match &res {
            Err(DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message,
            }) => {
                assert!(message.contains("did not stop"), "{t:?}: {message}");
                assert!(message.contains("rolled back"), "{t:?}: {message}");
                assert!(message.contains("session settings"), "{t:?}: {message}");
            }
            other => panic!("{t:?}: {:?}", other.as_ref().map(|(s, _)| s)),
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{t:?}");
        assert!(!s.in_transaction(), "{t:?}");
        assert_ne!(backend_pid(&s).await, pid, "{t:?}: same backend");
        let (_, c) = run(&s, "SHOW work_mem").await.unwrap();
        assert_eq!(scalar(&c), Value::Text("4MB".into()), "{t:?}");
        // The abandoned backend still loops server-side until terminated.
        kill(t, pid).await;
    }
}

/// A `SET search_path` behind a comment is still re-applied after a reconnect.
#[tokio::test]
async fn search_path_after_comment() {
    require_db!();
    for t in Target::pg() {
        for sql in [
            "/* pick */ SET search_path TO sales, public",
            "-- pick\nSET search_path = sales, public -- trailing",
            "/* a */ SET /* b */ SESSION search_path TO sales, public",
        ] {
            let s = connect(t).await;
            run(&s, sql)
                .await
                .unwrap_or_else(|e| panic!("{t:?} {sql:?}: {e}"));
            kill(t, backend_pid(&s).await).await;
            let (_, c) = run(&s, "SHOW search_path")
                .await
                .unwrap_or_else(|e| panic!("{t:?} {sql:?}: {e}"));
            assert_eq!(
                scalar(&c),
                Value::Text("sales, public".into()),
                "{t:?} {sql:?}"
            );
        }
    }
}

/// Session state a reconnect cannot restore (other settings, temporary tables) stops the
/// silent re-run of a read and is named in the error; a clean session is retried.
#[tokio::test]
async fn reconnect_reports_reset_session_state() {
    require_db!();
    for t in Target::pg() {
        for (setup, check, reset) in [
            (
                "SET statement_timeout = '5min'",
                "SHOW statement_timeout",
                Value::Text("0".into()),
            ),
            (
                "CREATE TEMP TABLE t4_tmp (a int)",
                "SELECT to_regclass('pg_temp.t4_tmp') IS NULL",
                Value::Bool(true),
            ),
        ] {
            let s = connect(t).await;
            run(&s, setup).await.unwrap();
            kill(t, backend_pid(&s).await).await;
            let err = run(&s, "SELECT 1").await.err();
            match &err {
                Some(DbError::ConnectionLost {
                    was_in_tx: false,
                    outcome_unknown: false,
                    message,
                }) => assert!(message.contains("were reset"), "{t:?}: {message}"),
                other => panic!("{t:?} {setup}: the read was re-run silently: {other:?}"),
            }
            assert!(
                dnt_core::error::AppError::from(err.unwrap()).retryable,
                "{t:?}"
            );
            let (_, c) = run(&s, check).await.unwrap();
            assert_eq!(scalar(&c), reset, "{t:?} {setup}");
            // The flag went with the old session: the next loss is retried silently.
            kill(t, backend_pid(&s).await).await;
            let (_, c) = run(&s, "SELECT 2")
                .await
                .unwrap_or_else(|e| panic!("{t:?}: clean session not retried: {e}"));
            assert_eq!(scalar(&c), Value::Int(2), "{t:?}");
        }
    }
}

/// A write whose connection dies while it runs (outside a transaction) may have
/// committed: unknown outcome, not retryable.
#[tokio::test]
async fn write_lost_mid_statement_has_unknown_outcome() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let admin = connect(t).await;
        run(&s, "CREATE TEMP TABLE t4_lost (v int)").await.unwrap();
        let pid = backend_pid(&s).await;
        let (res, _) = tokio::join!(
            run(
                &s,
                "INSERT INTO t4_lost SELECT 1 FROM pg_sleep(5) -- t4_lost"
            ),
            async {
                wait_running(&admin, "-- t4_lost").await;
                kill(t, pid).await
            }
        );
        assert!(
            matches!(
                res,
                Err(DbError::ConnectionLost {
                    was_in_tx: false,
                    outcome_unknown: true,
                    ..
                })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        run(&s, "select 1")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
    }
}

/// PostgreSQL has no `autocommit` setting (the server rejects `SET autocommit`), and
/// transaction state only changes as the server applied it.
#[tokio::test]
async fn transaction_state_follows_the_server() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        for sql in ["SET autocommit = 0", "SET autocommit = on"] {
            assert!(run(&s, sql).await.is_err(), "{t:?} {sql}");
            assert!(!s.in_transaction(), "{t:?} {sql}");
            run(&s, "BEGIN").await.unwrap();
            assert!(run(&s, sql).await.is_err(), "{t:?} {sql}");
            assert!(s.in_transaction(), "{t:?} {sql}");
            run(&s, "ROLLBACK").await.unwrap();
            assert!(!s.in_transaction(), "{t:?} {sql}");
        }

        // A rejected BEGIN opens nothing.
        assert!(
            run(&s, "BEGIN ISOLATION LEVEL bogus").await.is_err(),
            "{t:?}"
        );
        assert!(!s.in_transaction(), "{t:?}");

        // A COMMIT that fails on a deferred constraint still ends the transaction.
        run(
            &s,
            "CREATE TEMP TABLE t4_p (id int PRIMARY KEY); \
             CREATE TEMP TABLE t4_c (pid int REFERENCES t4_p DEFERRABLE INITIALLY DEFERRED)",
        )
        .await
        .unwrap();
        run(&s, "BEGIN; INSERT INTO t4_c VALUES (1)").await.unwrap();
        let res = run(&s, "COMMIT").await;
        assert_eq!(sqlstate_of(&res), Some("23503"), "{t:?}");
        assert!(!s.in_transaction(), "{t:?}");
        run(&s, "SELECT 1")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: still in a block: {e}"));

        // AND CHAIN starts the next transaction at once.
        run(&s, "BEGIN").await.unwrap();
        run(&s, "COMMIT AND CHAIN").await.unwrap();
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "ROLLBACK").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
    }
}

/// The connection dies after a Cancel click whose request the statement swallowed: the
/// loss wins over the pending cancel. Outside a transaction the write may have committed
/// (unknown outcome); inside one it was rolled back with the transaction, and the
/// session's state says so without a stale "rolled back" on the next call.
/// Deterministic: the `DO` loop ignores every cancel, so only the termination ends it.
#[tokio::test]
async fn connection_lost_after_cancel_is_reported_as_lost() {
    require_db!();
    let swallowing_write = "DO $$ BEGIN INSERT INTO t4_cancel_lost VALUES (1); \
         LOOP BEGIN PERFORM pg_sleep(0.1), 't4_cancel_lost'; \
         EXCEPTION WHEN query_canceled THEN NULL; END; END LOOP; END $$";
    for t in Target::pg() {
        for in_tx in [false, true] {
            let s = connect(t).await;
            let admin = connect(t).await;
            run(&s, "CREATE TEMP TABLE t4_cancel_lost (v int)")
                .await
                .unwrap();
            if in_tx {
                run(&s, "BEGIN").await.unwrap();
            }
            let pid = backend_pid(&s).await;
            let handle = s.cancel_handle();
            let (res, cancel) = tokio::join!(run(&s, swallowing_write), async {
                wait_running(&admin, "t4_cancel_lost").await;
                let cancel = handle.cancel().await;
                kill(t, pid).await;
                cancel
            });
            cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
            match &res {
                Err(DbError::ConnectionLost {
                    was_in_tx,
                    outcome_unknown,
                    ..
                }) => {
                    assert_eq!(*was_in_tx, in_tx, "{t:?}");
                    assert_eq!(*outcome_unknown, !in_tx, "{t:?} in_tx={in_tx}");
                }
                other => panic!("{t:?} in_tx={in_tx}: {:?}", other.as_ref().map(|(s, _)| s)),
            }
            assert!(!s.in_transaction(), "{t:?} in_tx={in_tx}");
            let (_, c) = run(&s, "SELECT 1")
                .await
                .unwrap_or_else(|e| panic!("{t:?} in_tx={in_tx}: stale state: {e}"));
            assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
        }
    }
}

/// A `query_canceled` we did not ask for (server `statement_timeout`) is the server's
/// own error, not a Cancel.
#[tokio::test]
async fn server_statement_timeout_is_a_server_error() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let res = run(&s, "SET statement_timeout = '200ms'; SELECT pg_sleep(2)").await;
        match &res {
            Err(DbError::Server {
                sqlstate, message, ..
            }) => {
                assert_eq!(sqlstate.as_deref(), Some("57014"), "{t:?}");
                assert!(message.contains("statement timeout"), "{t:?}: {message}");
            }
            other => panic!("{t:?}: {:?}", other.as_ref().map(|(s, _)| s)),
        }
        run(&s, "RESET statement_timeout; SELECT 1").await.unwrap();
    }
}
