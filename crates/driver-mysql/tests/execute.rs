use dnt_core::driver::{
    ExecOptions, ExecProgress, ExecSummary, ResultSummary, RowSink, Session, StmtOutcome,
};
use dnt_core::error::{AppError, ConnectStage, DbError};
use dnt_core::sql::{classify, Dialect, MySqlMode, StatementKind};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_driver_mysql::{MySqlDriver, MySqlSession};
use dnt_testkit::{require_db, Target, FIXTURE_DB};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
    progress: Vec<ExecProgress>,
}

impl RowSink for Collect {
    fn progress(&mut self, p: ExecProgress) {
        self.progress.push(p);
    }
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

/// Runs `sql` as one statement through `execute_one` (classified by the session's dialect).
async fn run_one(
    s: &MySqlSession,
    sql: &str,
    opts: ExecOptions,
) -> (Result<StmtOutcome, DbError>, Collect) {
    let mut sink = Collect::default();
    let kind = classify(sql, s.dialect());
    let res = s.execute_one(sql, kind, &opts, &mut sink).await;
    (res, sink)
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

/// Waits (up to 10 s) until a statement whose text contains `marker` is running on the
/// server, so a Cancel click lands while it executes rather than after a fixed delay.
async fn wait_running(admin: &MySqlSession, marker: &str) {
    let sql = format!(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST \
         WHERE INFO LIKE '%{marker}%' AND ID <> CONNECTION_ID()"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, c) = run(admin, &sql).await.unwrap();
        if as_u64(&scalar(&c)) > 0 {
            return;
        }
        assert!(Instant::now() < deadline, "{marker:?} never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cancel_long_query() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let admin = connect(t).await;
        let handle = s.cancel_handle();
        let cancelled_at = std::sync::Mutex::new(None);
        let (res, cancel) = tokio::join!(run(&s, "SELECT SLEEP(10) AS t3_long"), async {
            wait_running(&admin, "t3_long").await;
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
/// arrives while it runs. `DO SLEEP(n)` is a `Write` that a `KILL QUERY` cuts short
/// without an error (the sleep returns 1), which forces the "completed while a cancel
/// was in flight" race deterministically.
#[tokio::test]
async fn completed_write_during_cancel_keeps_its_result() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let admin = connect(t).await;
        let handle = s.cancel_handle();
        let (res, cancel) = tokio::join!(run(&s, "DO SLEEP(5), 't3_do1'"), async {
            wait_running(&admin, "t3_do1").await;
            handle.cancel().await
        });
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        let (summary, c) = res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(summary.results.len(), 1, "{t:?}");
        assert_eq!(c.results[0].affected, Some(0), "{t:?}");

        // In a script, the cancel then stops the statements that follow.
        let handle = s.cancel_handle();
        let (res, _) = tokio::join!(
            run(&s, "DO SLEEP(5), 't3_do2'; SELECT 'not reached'"),
            async {
                wait_running(&admin, "t3_do2").await;
                handle.cancel().await
            }
        );
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
        let admin = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let (h1, h2) = (s.cancel_handle(), s.cancel_handle());
        let (res, (c1, c2)) = tokio::join!(run(&s, "SELECT SLEEP(10) AS t3_twice"), async {
            wait_running(&admin, "t3_twice").await;
            // Both clicks at once.
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
        let admin = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let handle = s.cancel_handle();
        let (res, cancel) = tokio::join!(
            run_with(&s, "SELECT SLEEP(10) AS t3_race", timeout_opts(300)),
            async {
                wait_running(&admin, "t3_race").await;
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
        let (_, c) = run(&s, "SELECT SLEEP(0.5)")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(as_u64(&scalar(&c)), 0, "{t:?}");
        run(&s, "ROLLBACK").await.unwrap();
    }
}

/// `INSERT … VALUES (SLEEP(n))` cut short by `KILL QUERY`: MySQL ends the sleep early
/// and still inserts the row (the statement succeeds), MariaDB aborts it with error 1317
/// and inserts nothing. A write that was applied during the timeout / cancel grace
/// period is never reported as a timeout or a cancel; a killed autocommit write is
/// reported with an unknown outcome (it may be partly applied on a non-transactional
/// table), never as "not applied".
#[tokio::test]
async fn write_finishing_in_grace_reports_success() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let admin = connect(t).await;
        run(
            &s,
            "CREATE TABLE IF NOT EXISTS t3_grace (v int); DELETE FROM t3_grace",
        )
        .await
        .unwrap();
        let mysql = matches!(t, Target::MySql57 | Target::MySql8);

        // Timeout.
        let (res, c) = run_one(
            &s,
            "INSERT INTO t3_grace VALUES (SLEEP(5))",
            timeout_opts(500),
        )
        .await;
        let rows = count_rows(&s, "t3_grace").await;
        assert_eq!(c.progress, [ExecProgress::CancelSent], "{t:?}");
        match &res {
            Ok(outcome) => {
                assert_eq!(rows, 1, "{t:?}");
                assert_eq!(c.results[0].affected, Some(1), "{t:?}");
                assert!(outcome.late_completion, "{t:?}");
            }
            Err(DbError::Timeout {
                stage: ConnectStage::Execute,
                outcome_unknown: true,
                was_in_tx: false,
            }) => assert_eq!(rows, 0, "{t:?}"),
            Err(e) => panic!("{t:?}: {e:?}"),
        }
        assert_eq!(res.is_ok(), mysql, "{t:?}: {res:?}");

        // Cancel.
        run(&s, "DELETE FROM t3_grace").await.unwrap();
        let handle = s.cancel_handle();
        let ((res, c), cancel) = tokio::join!(
            run_one(
                &s,
                "INSERT INTO t3_grace VALUES (SLEEP(5))",
                ExecOptions::default()
            ),
            async {
                wait_running(&admin, "t3_grace VALUES").await;
                handle.cancel().await
            }
        );
        cancel.unwrap_or_else(|e| panic!("{t:?}: cancel failed: {e}"));
        let rows = count_rows(&s, "t3_grace").await;
        assert_eq!(c.progress, [ExecProgress::CancelSent], "{t:?}");
        match &res {
            // Completed despite the cancel, but no timeout expired: not late.
            Ok(outcome) => {
                assert_eq!(rows, 1, "{t:?}");
                assert!(!outcome.late_completion, "{t:?}");
            }
            Err(DbError::Cancelled {
                outcome_unknown: true,
                was_in_tx: false,
            }) => assert_eq!(rows, 0, "{t:?}"),
            Err(e) => panic!("{t:?}: {e:?}"),
        }
        assert_eq!(res.is_ok(), mysql, "{t:?}: {res:?}");
        assert!(!s.in_transaction(), "{t:?}");

        // Inside a transaction InnoDB rolls a killed write back: not applied.
        run(&s, "DELETE FROM t3_grace; BEGIN").await.unwrap();
        let (res, _) = run_one(
            &s,
            "INSERT INTO t3_grace VALUES (SLEEP(5))",
            timeout_opts(500),
        )
        .await;
        match &res {
            Ok(outcome) => assert!(mysql && outcome.late_completion, "{t:?}"),
            Err(DbError::Timeout {
                outcome_unknown: false,
                was_in_tx: true,
                ..
            }) => assert!(!mysql, "{t:?}"),
            Err(e) => panic!("{t:?}: {e:?}"),
        }
        run(&s, "ROLLBACK").await.unwrap();
        assert_eq!(count_rows(&s, "t3_grace").await, 0, "{t:?}");
    }
}

/// A reconnect restores what it can (`autocommit = 0`, the database) and says what it
/// could not; a read is not silently re-run on a session whose state was reset.
#[tokio::test]
async fn reconnect_restores_autocommit_and_reports_reset_state() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "SET autocommit = 0").await.unwrap();
        kill(t, connection_id(&s).await).await;
        match run(&s, "SELECT 1").await.err() {
            Some(DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message,
            }) => assert!(
                message.contains("transaction was rolled back"),
                "{t:?}: {message}"
            ),
            other => panic!("{t:?}: {other:?}"),
        }
        let (_, c) = run(&s, "SELECT @@autocommit").await.unwrap();
        assert_eq!(as_u64(&scalar(&c)), 0, "{t:?}: autocommit not re-applied");
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "SET autocommit = 1").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");

        let s = connect(t).await;
        run(&s, "SET @x = 1").await.unwrap();
        kill(t, connection_id(&s).await).await;
        let err = run(&s, "SELECT 1").await.err();
        match &err {
            Some(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message,
            }) => assert!(message.contains("user variables"), "{t:?}: {message}"),
            other => panic!("{t:?}: the read was re-run silently: {other:?}"),
        }
        assert!(
            dnt_core::error::AppError::from(err.unwrap()).retryable,
            "{t:?}"
        );
        // The new session is clean, and the flag went with the old one.
        let (_, c) = run(&s, "SELECT @x IS NULL").await.unwrap();
        assert_eq!(as_u64(&scalar(&c)), 1, "{t:?}");
        kill(t, connection_id(&s).await).await;
        let (_, c) = run(&s, "SELECT 2")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: clean session not retried: {e}"));
        assert_eq!(scalar(&c), Value::Int(2), "{t:?}");
    }
}

/// A session `SET` inside an executable comment (mysqldump's `/*!40101 SET NAMES … */`)
/// is session state a reconnect cannot restore: the read after a lost connection is not
/// re-run silently, and the error says the settings were reset (M3).
#[tokio::test]
async fn exec_comment_set_reports_reset_state() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "/*!40101 SET NAMES latin1 */")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        kill(t, connection_id(&s).await).await;
        match run(&s, "SELECT 1").await.err() {
            Some(DbError::ConnectionLost { message, .. }) => {
                assert!(message.contains("session settings"), "{t:?}: {message}")
            }
            other => panic!("{t:?}: the read was re-run silently: {other:?}"),
        }
    }
}

/// An interrupt we did not ask for (the server's statement time limit) is the server's
/// own error with its code and text, not a Cancel. MySQL enforces `max_execution_time`
/// on SELECTs (3024; a killed `SLEEP` would just return 1, hence the slow cross join),
/// MariaDB `max_statement_time` (1969).
#[tokio::test]
async fn server_statement_time_limit_is_a_server_error() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let (setup, sql, code) = match t {
            Target::MySql57 | Target::MySql8 => (
                "SET SESSION max_execution_time = 200",
                "SELECT COUNT(*) FROM information_schema.columns a, \
                 information_schema.columns b, information_schema.columns c",
                "3024",
            ),
            _ => (
                "SET SESSION max_statement_time = 0.2",
                "SELECT SLEEP(5)",
                "1969",
            ),
        };
        run(&s, setup).await.unwrap();
        match run(&s, sql).await {
            Err(DbError::Server {
                code: Some(c),
                message,
                ..
            }) => {
                assert_eq!(c, code, "{t:?}: {message}");
                assert!(message.contains("interrupted"), "{t:?}: {message}");
            }
            other => panic!("{t:?}: {:?}", other.map(|(s, _)| s)),
        }
        let (_, c) = run(&s, "SELECT 1").await.unwrap();
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
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
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
        run(&s, "START TRANSACTION").await.unwrap();
        assert_eq!(s.tx_state(), TxState::Active, "{t:?}");
        assert!(s.in_transaction(), "{t:?}");
        run(&s, "COMMIT").await.unwrap();
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
        run(&s, "SET autocommit = 0").await.unwrap();
        assert_eq!(s.tx_state(), TxState::Active, "{t:?}");
        // With autocommit off a COMMIT just starts the next transaction.
        run(&s, "COMMIT").await.unwrap();
        assert_eq!(s.tx_state(), TxState::Active, "{t:?}");
        run(&s, "SET autocommit = 1").await.unwrap();
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
        assert!(!s.in_transaction(), "{t:?}");
        // A rejected `SET autocommit` changes nothing.
        let err = run(&s, "SET autocommit = 7").await.err();
        assert!(
            matches!(err, Some(DbError::Server { .. })),
            "{t:?}: {err:?}"
        );
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
        run(
            &s,
            "DROP TABLE IF EXISTS t10_ddl; BEGIN; CREATE TABLE t10_ddl (a int); DROP TABLE t10_ddl",
        )
        .await
        .unwrap();
        // DDL implicitly commits.
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
        // ... before it runs, so even failing DDL ends the transaction.
        run(&s, "BEGIN").await.unwrap();
        let err = run(&s, "DROP TABLE t10_no_such_table").await.err();
        assert!(
            matches!(err, Some(DbError::Server { .. })),
            "{t:?}: {err:?}"
        );
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
        // ... unless it failed to parse: then it never ran.
        run(&s, "BEGIN").await.unwrap();
        let err = run(&s, "CREATE TABLE (").await.err();
        assert!(
            matches!(err, Some(DbError::Server { .. })),
            "{t:?}: {err:?}"
        );
        assert_eq!(s.tx_state(), TxState::Active, "{t:?}");
        run(&s, "ROLLBACK").await.unwrap();
        assert_eq!(s.tx_state(), TxState::Idle, "{t:?}");
    }
}

/// The session's `sql_mode` lexing flags as the server reports them.
async fn server_mode(s: &MySqlSession) -> MySqlMode {
    let (_, c) = run(s, "SELECT @@SESSION.sql_mode").await.unwrap();
    let mode = match scalar(&c) {
        Value::Text(m) => m.to_ascii_uppercase(),
        other => panic!("sql_mode: {other:?}"),
    };
    let flags: Vec<&str> = mode.split(',').map(str::trim).collect();
    MySqlMode {
        ansi_quotes: flags.contains(&"ANSI_QUOTES") || flags.contains(&"ANSI"),
        no_backslash_escapes: flags.contains(&"NO_BACKSLASH_ESCAPES"),
    }
}

fn mode_of(d: Dialect) -> MySqlMode {
    match d {
        Dialect::MySql(m) => m,
        other => panic!("not MySQL: {other:?}"),
    }
}

#[tokio::test]
async fn ansi_quotes_session_splits_double_quotes_as_identifiers() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        assert_eq!(mode_of(s.dialect()), server_mode(&s).await, "{t:?}");
        let (res, _) = run_one(
            &s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',ANSI_QUOTES')",
            ExecOptions::default(),
        )
        .await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(mode_of(s.dialect()).ansi_quotes, "{t:?}");
        let (summary, c) = run(
            &s,
            "CREATE TEMPORARY TABLE \"t;x\" (id INT); SELECT COUNT(*) FROM \"t;x\"",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(summary.results.len(), 2, "{t:?}");
        assert_eq!(as_u64(&scalar(&c)), 0, "{t:?}");
    }
}

#[tokio::test]
async fn no_backslash_escapes_tracks_flag() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let (res, _) = run_one(
            &s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')",
            ExecOptions::default(),
        )
        .await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(mode_of(s.dialect()).no_backslash_escapes, "{t:?}");
        let (summary, c) = run(&s, r"SELECT 'C:\'; SELECT 2")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(summary.results.len(), 2, "{t:?}");
        assert_eq!(c.results[0].rows[0][0], Value::Text("C:\\".into()), "{t:?}");
        assert_eq!(scalar(&c), Value::Int(2), "{t:?}");
        // Turning it off again is tracked too.
        run(
            &s,
            "SET sql_mode = REPLACE(@@sql_mode, 'NO_BACKSLASH_ESCAPES', '')",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(!mode_of(s.dialect()).no_backslash_escapes, "{t:?}");
    }
}

/// A reconnect does not restore the user's `sql_mode` (it is session state, reported as
/// reset); the session re-reads it, so `dialect()` matches what the new connection uses.
#[tokio::test]
async fn reconnect_rereads_sql_mode() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(
            &s,
            "SET SESSION sql_mode = CONCAT(@@sql_mode, ',ANSI_QUOTES')",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(mode_of(s.dialect()).ansi_quotes, "{t:?}");
        kill(t, connection_id(&s).await).await;
        // The changed sql_mode is custom session state: the read is not re-run.
        match run(&s, "SELECT 1").await {
            Ok(_) => {}
            Err(DbError::ConnectionLost { message, .. }) => {
                assert!(message.contains("session settings"), "{t:?}: {message}")
            }
            Err(e) => panic!("{t:?}: {e}"),
        }
        assert_eq!(mode_of(s.dialect()), server_mode(&s).await, "{t:?}");
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

/// A `CALL` returns every result set of the procedure from one `execute_one`.
#[tokio::test]
async fn execute_one_call_returns_every_result_set() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "DROP PROCEDURE IF EXISTS p2_multi").await.unwrap();
        let (res, _) = run_one(
            &s,
            "CREATE PROCEDURE p2_multi() BEGIN SELECT 1; SELECT 2, 3; END",
            ExecOptions::default(),
        )
        .await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let mut sink = Collect::default();
        let outcome = s
            .execute_one(
                "CALL p2_multi()",
                StatementKind::Write,
                &ExecOptions::default(),
                &mut sink,
            )
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(outcome.results.len(), 2, "{t:?}: {outcome:?}");
        assert!(!outcome.late_completion, "{t:?}");
        let counts: Vec<usize> = sink.results.iter().map(|r| r.cols.len()).collect();
        assert_eq!(counts, [1, 2], "{t:?}");
        run(&s, "DROP PROCEDURE p2_multi").await.unwrap();
    }
}

/// A Stop raised before the statement is sent cancels it without sending anything.
#[tokio::test]
async fn stop_signal_before_send_cancels() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let opts = ExecOptions::default();
        opts.stop.stop();
        let (res, c) = run_one(&s, "SELECT 1", opts).await;
        assert!(
            matches!(
                res,
                Err(DbError::Cancelled {
                    outcome_unknown: false,
                    was_in_tx: false,
                })
            ),
            "{t:?}: {res:?}"
        );
        assert!(c.results.is_empty(), "{t:?}: something was sent");
        let (res, c) = run_one(&s, "SELECT 1", ExecOptions::default()).await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(scalar(&c), Value::Int(1), "{t:?}");
    }
}

#[tokio::test]
async fn timeout_reports_cancel_sent() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let (res, c) = run_one(&s, "SELECT SLEEP(5)", timeout_opts(200)).await;
        assert!(
            matches!(res, Err(DbError::Timeout { .. })),
            "{t:?}: {res:?}"
        );
        assert_eq!(c.progress, [ExecProgress::CancelSent], "{t:?}");
    }
}

/// A TCP proxy to `upstream` on 127.0.0.1 that forwards every connection, except the
/// first one accepted after `hold` is raised: that one is accepted but never answered, so
/// a client opening it waits for the server greeting. Held sockets stay open while the
/// proxy runs.
async fn holding_proxy(upstream: u16) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = Arc::new(AtomicBool::new(false));
    let hold_next = hold.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut client, _)) = listener.accept().await {
            if hold_next.swap(false, Ordering::SeqCst) {
                held.push(client);
                continue;
            }
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    (port, hold)
}

/// The timeout's `KILL QUERY` and the grace period share one deadline: a kill that
/// cannot get through (its connection hangs in the proxy) does not delay `CancelSent`
/// nor extend the wait, so a statement that ignores the timeout is given up after
/// timeout + grace.
#[tokio::test]
async fn unconfirmed_timeout_kill_stays_within_the_grace() {
    require_db!();
    for t in Target::mysql_family() {
        let (port, hold) = holding_proxy(t.host_port()).await;
        let mut cfg = t.resolved(Some(FIXTURE_DB));
        cfg.port = port;
        let s = MySqlDriver
            .connect_session(&cfg)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let id = connection_id(&s).await;
        // The next connection is the kill's.
        hold.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let mut sink = Timed::new();
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(300)),
            cancel_grace: Duration::from_secs(1),
            ..ExecOptions::default()
        };
        let kind = classify("SELECT SLEEP(30)", s.dialect());
        let res = s
            .execute_one("SELECT SLEEP(30)", kind, &opts, &mut sink)
            .await;
        let elapsed = started.elapsed();
        assert!(
            matches!(res, Err(DbError::ConnectionLost { .. })),
            "{t:?}: {res:?}"
        );
        let kinds: Vec<ExecProgress> = sink.progress.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            kinds,
            [ExecProgress::CancelSent, ExecProgress::GraceExpired],
            "{t:?}"
        );
        // Reported when the timeout fired, not once the kill gave up.
        assert!(
            sink.progress[0].1 < Duration::from_millis(1_000),
            "{t:?}: CancelSent at {:?}",
            sink.progress[0].1
        );
        // 0.3 s + 1 s, plus the reconnect and a loaded machine; never + 10 s.
        assert!(elapsed < Duration::from_secs(5), "{t:?}: took {elapsed:?}");
        assert_ne!(connection_id(&s).await, id, "{t:?}");
        kill(t, id).await;
    }
}

/// A TCP proxy to `upstream` on 127.0.0.1 that forwards every connection, except that
/// once `hold` is raised the next `KILL QUERY` a client sends (a plaintext `COM_QUERY`
/// packet) is swallowed: the client has written it, the server never sees it. The test
/// connections use no TLS, so the client's packets can be read.
async fn kill_holding_proxy(upstream: u16) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hold = Arc::new(AtomicBool::new(false));
    let hold_kill = hold.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let hold_kill = hold_kill.clone();
            tokio::spawn(async move {
                let Ok(server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
                    return;
                };
                let (mut client_rx, mut client_tx) = client.into_split();
                let (mut server_rx, mut server_tx) = server.into_split();
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut server_rx, &mut client_tx).await;
                });
                // Client -> server, packet by packet: 3-byte length, sequence, payload.
                loop {
                    let mut head = [0u8; 4];
                    if client_rx.read_exact(&mut head).await.is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes([head[0], head[1], head[2], 0]) as usize;
                    let mut payload = vec![0u8; len];
                    if client_rx.read_exact(&mut payload).await.is_err() {
                        break;
                    }
                    let is_kill =
                        payload.first() == Some(&0x03) && payload[1..].starts_with(b"KILL QUERY");
                    if is_kill && hold_kill.swap(false, Ordering::SeqCst) {
                        // Never forwarded; the sockets stay open until the proxy ends.
                        std::future::pending::<()>().await;
                    }
                    if server_tx.write_all(&head).await.is_err()
                        || server_tx.write_all(&payload).await.is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    (port, hold)
}

/// A statement that completes during the grace period while the timeout's `KILL QUERY`
/// is still unanswered (the proxy swallows it) reports its real result, a
/// success with `late_completion` or its own error. The kill may still land, so the
/// connection is replaced before the next statement (silently: nothing of the session is
/// lost).
#[tokio::test]
async fn late_result_with_unconfirmed_kill_is_kept() {
    require_db!();
    for t in Target::mysql_family() {
        let (port, hold) = kill_holding_proxy(t.host_port()).await;
        let mut cfg = t.resolved(Some(FIXTURE_DB));
        cfg.port = port;
        let s = MySqlDriver
            .connect_session(&cfg)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        run(
            &s,
            "CREATE TABLE IF NOT EXISTS t5_late (id int PRIMARY KEY); \
             DELETE FROM t5_late; INSERT INTO t5_late VALUES (1)",
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(300)),
            cancel_grace: Duration::from_secs(2),
            ..ExecOptions::default()
        };

        // Succeeds after its timeout: kept, late.
        let id = connection_id(&s).await;
        hold.store(true, Ordering::SeqCst);
        let (res, c) = run_one(
            &s,
            "INSERT INTO t5_late (id) SELECT 2 FROM (SELECT SLEEP(1)) AS s",
            opts.clone(),
        )
        .await;
        let outcome = res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert!(outcome.late_completion, "{t:?}");
        assert_eq!(c.results[0].affected, Some(1), "{t:?}");
        assert_ne!(connection_id(&s).await, id, "{t:?}: same connection");

        // Fails on its own after its timeout: its own error, not a lost connection.
        let id = connection_id(&s).await;
        hold.store(true, Ordering::SeqCst);
        let (res, _) = run_one(
            &s,
            "INSERT INTO t5_late (id) SELECT 1 FROM (SELECT SLEEP(1)) AS s",
            opts.clone(),
        )
        .await;
        match &res {
            Err(DbError::Server { code, .. }) => {
                assert_eq!(code.as_deref(), Some("1062"), "{t:?}")
            }
            other => panic!("{t:?}: {other:?}"),
        }
        assert_ne!(connection_id(&s).await, id, "{t:?}: same connection");
        let (_, c) = run(&s, "SELECT COUNT(*) FROM t5_late").await.unwrap();
        assert_eq!(as_u64(&scalar(&c)), 2, "{t:?}");
    }
}

/// Records when each progress event arrived.
struct Timed {
    started: Instant,
    progress: Vec<(ExecProgress, Duration)>,
}

impl Timed {
    fn new() -> Self {
        Timed {
            started: Instant::now(),
            progress: Vec::new(),
        }
    }
}

impl RowSink for Timed {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
    fn progress(&mut self, p: ExecProgress) {
        self.progress.push((p, self.started.elapsed()));
    }
}

#[tokio::test]
async fn timeout_in_tx_carries_was_in_tx() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        run(&s, "BEGIN").await.unwrap();
        let (res, _) = run_one(&s, "SELECT SLEEP(5)", timeout_opts(200)).await;
        match res {
            Err(
                e @ DbError::Timeout {
                    was_in_tx: true, ..
                },
            ) => {
                assert!(!AppError::from(e).retryable, "{t:?}")
            }
            other => panic!("{t:?}: {other:?}"),
        }
        run(&s, "ROLLBACK").await.unwrap();
    }
}
