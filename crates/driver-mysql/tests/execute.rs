use dnt_core::driver::{ExecOptions, ExecSummary, RowSink, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::value::{ColumnMeta, Value};
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
        let rows = &c.results[0].rows;
        insta::assert_yaml_snapshot!(format!("{t:?}_all_types"), rows);
    }
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
                    stage: ConnectStage::Execute
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
                    in_transaction: true
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
