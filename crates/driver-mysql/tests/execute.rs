use dnt_core::driver::{ExecOptions, ExecSummary, RowSink, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_driver_mysql::{MySqlDriver, MySqlSession};
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

async fn connect(t: Target) -> MySqlSession {
    MySqlDriver
        .connect_session(&t.resolved(Some(FIXTURE_DB)))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

async fn run(s: &MySqlSession, sql: &str) -> Result<(ExecSummary, Collect), DbError> {
    run_with(s, sql, ExecOptions::default()).await
}

async fn run_with(
    s: &MySqlSession,
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

fn as_u64(v: &Value) -> u64 {
    match v {
        Value::Int(i) => *i as u64,
        Value::UInt(u) => *u,
        other => panic!("not an integer: {other:?}"),
    }
}

async fn connection_id(s: &MySqlSession) -> u64 {
    let (_, c) = run(s, "SELECT CONNECTION_ID()").await.unwrap();
    as_u64(&scalar(&c))
}

async fn kill(t: Target, id: u64) {
    let admin = connect(t).await;
    run(&admin, &format!("KILL {id}"))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: KILL failed: {e}"));
    Box::new(admin).close().await;
    // Let the server tear the killed session down.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn multi_statement_results() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let (summary, c) = run(
            &s,
            "select 1 a; select 'x' b, 2 c; update customers set name=name where 1=0",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(c.results.len(), 3, "{t:?}");
        assert_eq!(summary.results.len(), 3, "{t:?}");
        assert_eq!(c.results[0].cols[0].name, "a", "{t:?}");
        assert_eq!(c.results[0].rows, vec![vec![Value::Int(1)]], "{t:?}");
        assert_eq!(
            c.results[1].rows,
            vec![vec![Value::Text("x".into()), Value::Int(2)]],
            "{t:?}"
        );
        assert_eq!(c.results[0].affected, None, "{t:?}");
        assert_eq!(c.results[2].affected, Some(0), "{t:?}");
        assert_eq!(summary.results[2].affected, Some(0), "{t:?}");
        assert!(c.results[2].cols.is_empty(), "{t:?}");
    }
}

#[tokio::test]
async fn all_types_roundtrip() {
    require_db!();
    for t in Target::mysql_family() {
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
async fn unicode_text() {
    require_db!();
    for t in Target::mysql_family() {
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
async fn zero_dates_do_not_fail() {
    require_db!();
    for t in [Target::MySql57, Target::MariaDb106] {
        let s = connect(t).await;
        let (_, c) = run(&s, "SELECT * FROM zero_dates ORDER BY id")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let rows = &c.results[0].rows;
        assert!(
            rows.iter()
                .flatten()
                .any(|v| *v == Value::DateTime("0000-00-00 00:00:00".into())),
            "{t:?}: {rows:?}"
        );
        assert_eq!(rows[0][1], Value::Date("0000-00-00".into()), "{t:?}");
        assert_eq!(
            rows[1][2],
            Value::DateTime("2020-00-00 00:00:00".into()),
            "{t:?}"
        );
    }
}

#[tokio::test]
async fn max_rows_truncates() {
    require_db!();
    for t in Target::mysql_family() {
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
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let opts = ExecOptions {
            batch_size: 2,
            ..ExecOptions::default()
        };
        let (summary, c) = run_with(
            &s,
            "select 1 union all select 2 union all select 3 union all select 4 union all select 5",
            opts,
        )
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
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let handle = s.cancel_handle();
        let cancelled_at = std::sync::Mutex::new(None);
        let (res, cancel) = tokio::join!(run(&s, "SELECT SLEEP(10)"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            *cancelled_at.lock().unwrap() = Some(Instant::now());
            handle.cancel().await
        });
        let since_cancel = cancelled_at.lock().unwrap().unwrap().elapsed();
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        assert!(
            matches!(res, Err(DbError::Cancelled)),
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
    for t in Target::mysql_family() {
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

/// A statement that completes with an OK status keeps its result even when a cancel
/// arrives while it runs. `DO SLEEP(1)` is a `Write` that a `KILL QUERY` cuts short
/// without an error (the sleep returns 1), which forces the "completed while a cancel
/// was in flight" race deterministically.
#[tokio::test]
async fn completed_write_during_cancel_keeps_its_result() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let handle = s.cancel_handle();
        let (res, cancel) = tokio::join!(run(&s, "DO SLEEP(1)"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel().await
        });
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        let (summary, c) = res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(summary.results.len(), 1, "{t:?}");
        assert_eq!(c.results[0].affected, Some(0), "{t:?}");

        // In a script, the cancel then stops the statements that follow.
        let handle = s.cancel_handle();
        let (res, _) = tokio::join!(run(&s, "DO SLEEP(1); SELECT 'not reached'"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel().await
        });
        assert!(
            matches!(res, Err(DbError::Cancelled)),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
    }
}

fn timeout_opts(ms: u64) -> ExecOptions {
    ExecOptions {
        timeout: Some(Duration::from_millis(ms)),
        ..ExecOptions::default()
    }
}

async fn count_rows(s: &MySqlSession, table: &str) -> u64 {
    let (_, c) = run(s, &format!("SELECT COUNT(*) FROM {table}"))
        .await
        .unwrap();
    as_u64(&scalar(&c))
}

/// Two Cancel clicks on the same statement (each sends its own `KILL QUERY`): the call
/// is cancelled once, the open transaction is untouched, and no stray kill reaches the
/// statements that follow.
#[tokio::test]
async fn cancel_twice_keeps_session_usable() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let (h1, h2) = (s.cancel_handle(), s.cancel_handle());
        let click = |h: dnt_core::driver::CancelHandle| async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            h.cancel().await
        };
        let (res, c1, c2) = tokio::join!(run(&s, "SELECT SLEEP(10)"), click(h1), click(h2));
        c1.unwrap_or_else(|e| panic!("{t:?}: first cancel failed: {e}"));
        c2.unwrap_or_else(|e| panic!("{t:?}: second cancel failed: {e}"));
        assert!(
            matches!(res, Err(DbError::Cancelled)),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(
            s.in_transaction(),
            "{t:?}: a cancel must not end the transaction"
        );
        // A sleep that is not interrupted returns 0.
        let (_, c) = run(&s, "SELECT SLEEP(0.5)")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(
            as_u64(&scalar(&c)),
            0,
            "{t:?}: a stray kill hit the next call"
        );
        run(&s, "ROLLBACK").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
    }
}

/// A timeout and a Cancel click land on the same statement: either error is fine, but
/// the session stays usable and keeps its transaction state.
#[tokio::test]
async fn timeout_racing_cancel_keeps_session_usable() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let handle = s.cancel_handle();
        let (res, cancel) =
            tokio::join!(run_with(&s, "SELECT SLEEP(10)", timeout_opts(300)), async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                handle.cancel().await
            });
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        assert!(
            matches!(
                res,
                Err(DbError::Cancelled
                    | DbError::Timeout {
                        outcome_unknown: false,
                        ..
                    })
            ),
            "{t:?}: {:?}",
            res.map(|(s, _)| s)
        );
        assert!(s.in_transaction(), "{t:?}");
        let (_, c) = run(&s, "SELECT SLEEP(0.5)")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(as_u64(&scalar(&c)), 0, "{t:?}");
        run(&s, "ROLLBACK").await.unwrap();
    }
}

/// `INSERT … VALUES (SLEEP(n))` cut short by `KILL QUERY`: MySQL ends the sleep early
/// and still inserts the row (the statement succeeds), MariaDB aborts it with error 1317
/// and inserts nothing. Whichever happens, the reported result must match the table:
/// a write that was applied during the timeout / cancel grace period is never reported
/// as a timeout or a cancel.
#[tokio::test]
async fn write_finishing_in_grace_reports_success() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(
            &s,
            "CREATE TABLE IF NOT EXISTS t3_grace (v int); DELETE FROM t3_grace",
        )
        .await
        .unwrap();
        let mysql = matches!(t, Target::MySql57 | Target::MySql8);

        // Timeout.
        let res = run_with(
            &s,
            "INSERT INTO t3_grace VALUES (SLEEP(5))",
            timeout_opts(500),
        )
        .await;
        let rows = count_rows(&s, "t3_grace").await;
        match &res {
            Ok((_, c)) => {
                assert_eq!(rows, 1, "{t:?}");
                assert_eq!(c.results[0].affected, Some(1), "{t:?}");
            }
            Err(DbError::Timeout {
                stage: ConnectStage::Execute,
                outcome_unknown: false,
            }) => assert_eq!(rows, 0, "{t:?}"),
            Err(e) => panic!("{t:?}: {e:?}"),
        }
        assert_eq!(res.is_ok(), mysql, "{t:?}: {:?}", res.map(|(s, _)| s));

        // Cancel.
        run(&s, "DELETE FROM t3_grace").await.unwrap();
        let handle = s.cancel_handle();
        let (res, cancel) =
            tokio::join!(run(&s, "INSERT INTO t3_grace VALUES (SLEEP(5))"), async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                handle.cancel().await
            });
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        let rows = count_rows(&s, "t3_grace").await;
        match &res {
            Ok(_) => assert_eq!(rows, 1, "{t:?}"),
            Err(DbError::Cancelled) => assert_eq!(rows, 0, "{t:?}"),
            Err(e) => panic!("{t:?}: {e:?}"),
        }
        assert_eq!(res.is_ok(), mysql, "{t:?}: {:?}", res.map(|(s, _)| s));
        assert!(!s.in_transaction(), "{t:?}");
    }
}

/// A `USE` behind a comment is still the database re-applied after a reconnect.
#[tokio::test]
async fn use_after_comment_tracks_db() {
    require_db!();
    for t in Target::mysql_family() {
        for (sql, db) in [
            ("/* pick */ USE information_schema", "information_schema"),
            ("-- pick\nUSE mysql -- trailing", "mysql"),
            (
                "# pick\n USE `information_schema` /* end */",
                "information_schema",
            ),
        ] {
            let s = connect(t).await;
            run(&s, sql)
                .await
                .unwrap_or_else(|e| panic!("{t:?} {sql:?}: {e}"));
            kill(t, connection_id(&s).await).await;
            let (_, c) = run(&s, "SELECT DATABASE()")
                .await
                .unwrap_or_else(|e| panic!("{t:?} {sql:?}: {e}"));
            assert_eq!(scalar(&c), Value::Text(db.into()), "{t:?} {sql:?}");
        }
    }
}

#[tokio::test]
async fn timeout_option() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(500)),
            ..ExecOptions::default()
        };
        let started = Instant::now();
        let res = run_with(&s, "SELECT SLEEP(5)", opts).await;
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
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "USE information_schema").await.unwrap();
        let id = connection_id(&s).await;
        kill(t, id).await;
        let (_, c) = run(&s, "select 1")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
        assert_ne!(connection_id(&s).await, id, "{t:?}: same connection");
        // The last `USE` survives the reconnect.
        let (_, c) = run(&s, "SELECT DATABASE()").await.unwrap();
        assert_eq!(
            scalar(&c),
            Value::Text("information_schema".into()),
            "{t:?}"
        );
        assert!(!s.in_transaction(), "{t:?}");
    }
}

#[tokio::test]
async fn no_retry_inside_transaction() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        assert!(s.in_transaction(), "{t:?}");
        let id = connection_id(&s).await;
        kill(t, id).await;
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
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "START TRANSACTION").await.unwrap();
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "COMMIT").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        run(&s, "SET autocommit = 0").await.unwrap();
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "SET autocommit = 1").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");
        run(
            &s,
            "DROP TABLE IF EXISTS t10_ddl; BEGIN; CREATE TABLE t10_ddl (a int); DROP TABLE t10_ddl",
        )
        .await
        .unwrap();
        // DDL implicitly commits.
        assert!(!s.in_transaction(), "{t:?}");
        // ... before it runs, so even failing DDL ends the transaction.
        run(&s, "BEGIN").await.unwrap();
        let err = run(&s, "DROP TABLE t10_no_such_table").await.err();
        assert!(
            matches!(err, Some(DbError::Server { .. })),
            "{t:?}: {err:?}"
        );
        assert!(!s.in_transaction(), "{t:?}");
    }
}

#[tokio::test]
async fn deadlock_ends_the_transaction() {
    require_db!();
    for t in Target::mysql_family() {
        let a = connect(t).await;
        let b = connect(t).await;
        run(
            &a,
            "CREATE TABLE IF NOT EXISTS t10_deadlock (id int PRIMARY KEY, v int); \
             INSERT IGNORE INTO t10_deadlock VALUES (1, 0), (2, 0)",
        )
        .await
        .unwrap();
        run(&a, "BEGIN; UPDATE t10_deadlock SET v = v + 1 WHERE id = 1")
            .await
            .unwrap();
        run(&b, "BEGIN; UPDATE t10_deadlock SET v = v + 1 WHERE id = 2")
            .await
            .unwrap();
        let (ra, rb) = tokio::join!(
            run(&a, "UPDATE t10_deadlock SET v = v + 1 WHERE id = 2"),
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                run(&b, "UPDATE t10_deadlock SET v = v + 1 WHERE id = 1").await
            }
        );
        let code = |r: &Result<_, DbError>| match r {
            Err(DbError::Server { code, .. }) => code.clone(),
            _ => None,
        };
        let (victim, survivor) = match (code(&ra).as_deref(), code(&rb).as_deref()) {
            (Some("1213"), None) => (&a, &b),
            (None, Some("1213")) => (&b, &a),
            other => panic!("{t:?}: expected exactly one deadlock, got {other:?}"),
        };
        assert!(!victim.in_transaction(), "{t:?}: victim still in tx");
        assert!(survivor.in_transaction(), "{t:?}");
        run(survivor, "ROLLBACK").await.unwrap();
    }
}

#[tokio::test]
async fn delimiter_procedure_script() {
    require_db!();
    let script = "DROP PROCEDURE IF EXISTS p_task10;\n\
        DELIMITER $$\n\
        CREATE PROCEDURE p_task10()\n\
        BEGIN\n\
          SELECT 1 AS one;\n\
          SELECT 'two' AS two;\n\
        END$$\n\
        DELIMITER ;\n\
        CALL p_task10();\n\
        DROP PROCEDURE p_task10;";
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let (_, c) = run(&s, script)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let rows: Vec<&Vec<Vec<Value>>> = c
            .results
            .iter()
            .filter(|r| !r.cols.is_empty())
            .map(|r| &r.rows)
            .collect();
        assert_eq!(
            rows,
            vec![
                &vec![vec![Value::Int(1)]],
                &vec![vec![Value::Text("two".into())]]
            ],
            "{t:?}"
        );
        let (_, c) = run(
            &s,
            "SELECT COUNT(*) FROM information_schema.routines \
             WHERE routine_schema = DATABASE() AND routine_name = 'p_task10'",
        )
        .await
        .unwrap();
        assert_eq!(as_u64(&scalar(&c)), 0, "{t:?}: procedure not dropped");
    }
}
