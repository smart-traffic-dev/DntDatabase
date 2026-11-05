//! Cancel (spec §5.5) against the compose services `mongo60` and `mongo80`: `killOp` of the
//! running command found by its comment, the grace period and the `Client` rebuild after
//! it, and no automatic retry (§5.2). Needs `DNT_TEST_DB=1`. Everything runs as `root` in
//! scratch databases (`dnt_cancel_*`).

use dnt_core::command::{CommandLanguage, ParsedCommand};
use dnt_core::config::ResolvedConfig;
use dnt_core::driver::{
    CancelHandle, Driver, ExecOptions, ExecProgress, ResultSummary, RowSink, Session, StmtOutcome,
    StopSignal,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mongo::shell::MongoLanguage;
use dnt_driver_mongo::test_helpers::open;
use dnt_driver_mongo::MongoDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use mongodb::bson::{doc, Bson, Document};
use mongodb::options::{ClientOptions, Credential, ServerAddress};
use mongodb::Client;
use std::time::{Duration, Instant};

/// Documents in the slow collection; each one sleeps 200 ms in `$where` ([`SLOW_FILTER`]),
/// so a full scan takes 6 s, far longer than any cancel may.
const DOCS: usize = 30;
const SLOW_FILTER: &str = r#"{$where: "sleep(200) || true"}"#;
/// A cancel must end the command well before the scan would.
const QUICK: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Rec {
    starts: usize,
    rows: Vec<Vec<Value>>,
    ends: Vec<ResultSummary>,
    progress: Vec<ExecProgress>,
}

impl RowSink for Rec {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {
        self.starts += 1;
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.ends.push(summary.clone());
    }
    fn progress(&mut self, p: ExecProgress) {
        self.progress.push(p);
    }
}

fn root(t: Target, db: &str) -> ResolvedConfig {
    t.resolved_as("root", TEST_PASSWORD, Some(db))
}

async fn connect(c: &ResolvedConfig) -> Box<dyn Session> {
    MongoDriver
        .connect(c)
        .await
        .unwrap_or_else(|e| panic!("{c:?}: {e}"))
}

/// A plain `Client` as root, for what the session under test must not do itself.
async fn admin_client(t: Target) -> Client {
    open(&root(t, "admin"))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
        .client
}

fn parse(text: &str) -> ParsedCommand {
    MongoLanguage
        .parse(text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

async fn run_with(
    s: &dyn Session,
    text: &str,
    opts: &ExecOptions,
) -> (Result<StmtOutcome, DbError>, Rec) {
    let mut rec = Rec::default();
    let cmd = parse(text);
    let r = s
        .command()
        .unwrap()
        .execute_command(&cmd, opts, &mut rec)
        .await;
    (r, rec)
}

async fn run(s: &dyn Session, text: &str) -> Rec {
    let (r, rec) = run_with(s, text, &ExecOptions::default()).await;
    let outcome = r.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    assert!(!outcome.late_completion, "{text}");
    rec
}

/// The `value` of `countDocuments` on `coll`.
async fn count(s: &dyn Session, coll: &str) -> Value {
    let r = run(s, &format!("db.{coll}.countDocuments({{}})")).await;
    match r.rows.as_slice() {
        [row] => row[0].clone(),
        other => panic!("{other:?}"),
    }
}

/// Drops `db`, then fills `db.slow` with [`DOCS`] documents.
async fn seed(admin: &Client, db: &str) {
    let d = admin.database(db);
    d.drop().await.unwrap();
    let docs: Vec<Document> = (0..DOCS as i32)
        .map(|i| doc! { "_id": i, "n": i })
        .collect();
    d.collection::<Document>("slow")
        .insert_many(docs)
        .await
        .unwrap();
}

/// The opids of the ops on `db.coll` sent by this crate (comment `dnt:…`).
async fn dnt_ops(admin: &Client, ns: &str) -> Vec<Bson> {
    let reply = admin
        .database("admin")
        .run_command(doc! {
            "currentOp": 1,
            "ns": ns,
            "command.comment": { "$regex": "^dnt:" },
        })
        .await
        .unwrap();
    reply
        .get_array("inprog")
        .unwrap()
        .iter()
        .filter_map(|op| op.as_document()?.get("opid").cloned())
        .collect()
}

/// Waits until a command of this crate runs on `ns`.
async fn wait_running(admin: &Client, ns: &str) {
    let until = Instant::now() + Duration::from_secs(10);
    while dnt_ops(admin, ns).await.is_empty() {
        assert!(Instant::now() < until, "no command started on {ns}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Kills what a test left running on `ns` (a command given up after its grace).
async fn kill_leftovers(admin: &Client, ns: &str) {
    for op in dnt_ops(admin, ns).await {
        let _ = admin
            .database("admin")
            .run_command(doc! { "killOp": 1, "op": op })
            .await;
    }
}

fn grace(d: Duration) -> ExecOptions {
    ExecOptions {
        stop: StopSignal::new(),
        cancel_grace: d,
        ..ExecOptions::default()
    }
}

/// Runs `text` on `s` and, once it runs on the server, does what the backend does on a
/// cancel: raises `opts.stop` (when `stop`), then calls `handle` (when given).
async fn run_and_cancel(
    s: &dyn Session,
    admin: &Client,
    ns: &str,
    text: &str,
    opts: &ExecOptions,
    stop: bool,
    handle: Option<&CancelHandle>,
) -> (Result<StmtOutcome, DbError>, Rec, Duration) {
    let started = Instant::now();
    let ((res, rec), cancelled_at) = tokio::join!(run_with(s, text, opts), async {
        wait_running(admin, ns).await;
        let at = Instant::now();
        if stop {
            opts.stop.stop();
        }
        if let Some(handle) = handle {
            handle
                .cancel()
                .await
                .unwrap_or_else(|e| panic!("{ns}: cancel failed: {e}"));
        }
        at
    });
    assert!(cancelled_at >= started);
    (res, rec, cancelled_at.elapsed())
}

#[tokio::test]
async fn cancel_slow_find_kills_op() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_cancel_find";
        let ns = format!("{db}.slow");
        let admin = admin_client(t).await;
        seed(&admin, db).await;
        let s = connect(&root(t, db)).await;
        let opts = grace(Duration::from_secs(10));
        let text = format!("db.slow.find({SLOW_FILTER})");
        let handle = s.cancel_handle();
        let (res, rec, since) =
            run_and_cancel(&*s, &admin, &ns, &text, &opts, true, Some(&handle)).await;
        match res {
            Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected Cancelled, got {other:?}"),
        }
        assert!(since < QUICK, "{t:?}: the op ran on for {since:?}");
        assert_eq!(rec.progress, [ExecProgress::CancelSent], "{t:?}");
        assert_eq!(rec.starts, 0, "{t:?}: nothing reached the sink");
        assert!(dnt_ops(&admin, &ns).await.is_empty(), "{t:?}: op killed");
        // The same connection runs the next command.
        assert_eq!(count(&*s, "slow").await, Value::Int(DOCS as i64), "{t:?}");
        assert_eq!(s.reconnects(), 0, "{t:?}");
        s.close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

#[tokio::test]
async fn cancel_write_is_outcome_unknown() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_cancel_write";
        let ns = format!("{db}.slow");
        let admin = admin_client(t).await;
        seed(&admin, db).await;
        let s = connect(&root(t, db)).await;
        let opts = grace(Duration::from_secs(10));
        let text = format!("db.slow.updateMany({SLOW_FILTER}, {{$set: {{touched: 1}}}})");
        let handle = s.cancel_handle();
        let (res, rec, since) =
            run_and_cancel(&*s, &admin, &ns, &text, &opts, true, Some(&handle)).await;
        match res {
            Err(DbError::Cancelled {
                outcome_unknown: true,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected Cancelled with outcome unknown, got {other:?}"),
        }
        assert!(since < QUICK, "{t:?}: the op ran on for {since:?}");
        assert_eq!(rec.progress, [ExecProgress::CancelSent], "{t:?}");
        assert!(dnt_ops(&admin, &ns).await.is_empty(), "{t:?}: op killed");
        assert_eq!(count(&*s, "slow").await, Value::Int(DOCS as i64), "{t:?}");
        assert_eq!(s.reconnects(), 0, "{t:?}");
        s.close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// A write past `opts.timeout` is cancelled the same way (spec §5.4): `Timeout`, with the
/// outcome unknown.
#[tokio::test]
async fn write_timeout_kills_op() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_cancel_timeout";
        let ns = format!("{db}.slow");
        let admin = admin_client(t).await;
        seed(&admin, db).await;
        let s = connect(&root(t, db)).await;
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(500)),
            ..grace(Duration::from_secs(10))
        };
        let text = format!("db.slow.updateMany({SLOW_FILTER}, {{$set: {{touched: 1}}}})");
        let started = Instant::now();
        let (res, rec) = run_with(&*s, &text, &opts).await;
        let took = started.elapsed();
        match res {
            Err(DbError::Timeout {
                stage: ConnectStage::Execute,
                outcome_unknown: true,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected Timeout with outcome unknown, got {other:?}"),
        }
        assert!(took < QUICK, "{t:?}: the write ran on for {took:?}");
        assert_eq!(rec.progress, [ExecProgress::CancelSent], "{t:?}");
        assert!(dnt_ops(&admin, &ns).await.is_empty(), "{t:?}: op killed");
        assert!(!opts.stop.is_stopped(), "{t:?}: a timeout is not a stop");
        assert_eq!(count(&*s, "slow").await, Value::Int(DOCS as i64), "{t:?}");
        assert_eq!(s.reconnects(), 0, "{t:?}");
        s.close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

#[tokio::test]
async fn cancel_between_commands_is_noop() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&root(t, FIXTURE_DB)).await;
        let handle = s.cancel_handle();
        handle
            .cancel()
            .await
            .unwrap_or_else(|e| panic!("{t:?}: idle cancel failed: {e}"));
        let before = count(&*s, "orders").await;
        // After a command too: nothing is left for the cancel to reach.
        handle
            .cancel()
            .await
            .unwrap_or_else(|e| panic!("{t:?}: cancel after a command failed: {e}"));
        let (res, rec) =
            run_with(&*s, "db.orders.countDocuments({})", &ExecOptions::default()).await;
        let outcome = res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert!(!outcome.late_completion, "{t:?}");
        assert!(rec.progress.is_empty(), "{t:?}");
        assert_eq!(rec.rows, [vec![before]], "{t:?}");
        assert_eq!(s.reconnects(), 0, "{t:?}");
        s.close().await;
    }
}

#[tokio::test]
async fn tiny_grace_rebuilds_client() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_cancel_tiny";
        let ns = format!("{db}.slow");
        let admin = admin_client(t).await;
        seed(&admin, db).await;
        // The session moves away from the config's database: a rebuild must keep it.
        let s = connect(&root(t, FIXTURE_DB)).await;
        run(&*s, &format!("use {db}")).await;
        let cmd = s.command().unwrap();
        assert_eq!(cmd.current_database().as_deref(), Some(db), "{t:?}");
        let handle = s.cancel_handle();

        // Only the stop: nothing kills the op within the grace.
        let opts = grace(Duration::from_millis(1));
        let text = format!("db.slow.find({SLOW_FILTER})");
        let (res, rec, since) = run_and_cancel(&*s, &admin, &ns, &text, &opts, true, None).await;
        match res {
            Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected Cancelled, got {other:?}"),
        }
        assert!(since < QUICK, "{t:?}: waited {since:?}");
        assert_eq!(
            rec.progress,
            [ExecProgress::CancelSent, ExecProgress::GraceExpired],
            "{t:?}"
        );
        assert_eq!(s.reconnects(), 1, "{t:?}");
        assert_eq!(cmd.current_database().as_deref(), Some(db), "{t:?}");
        kill_leftovers(&admin, &ns).await;

        // The fresh `Client` runs the next command, and the handle taken before the
        // rebuild still kills on it (alone: no stop raised, so only its `killOp` acts).
        assert_eq!(count(&*s, "slow").await, Value::Int(DOCS as i64), "{t:?}");
        let opts = grace(Duration::from_secs(10));
        let (res, rec, since) =
            run_and_cancel(&*s, &admin, &ns, &text, &opts, false, Some(&handle)).await;
        match res {
            Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected Cancelled, got {other:?}"),
        }
        assert!(since < QUICK, "{t:?}: the op ran on for {since:?}");
        assert!(rec.progress.is_empty(), "{t:?}: {:?}", rec.progress);
        assert!(dnt_ops(&admin, &ns).await.is_empty(), "{t:?}: op killed");
        assert_eq!(s.reconnects(), 1, "{t:?}");
        s.close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

#[tokio::test]
async fn close_with_open_cursor_returns() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_cancel_close";
        let admin = admin_client(t).await;
        seed(&admin, db).await;
        let s = connect(&root(t, db)).await;
        // One row of many: the cursor stays open on the server.
        let opts = ExecOptions {
            max_rows: 1,
            batch_size: 1,
            ..ExecOptions::default()
        };
        let (res, rec) = run_with(&*s, "db.slow.find({})", &opts).await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert!(rec.ends[0].truncated, "{t:?}");
        tokio::time::timeout(Duration::from_secs(5), s.close())
            .await
            .unwrap_or_else(|_| panic!("{t:?}: close hung"));
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// `configureFailPoint failCommand` on the next `insert` (one hit): the server closes the
/// connection before it runs the insert.
fn drop_next_insert(on: bool) -> Document {
    let mode: Bson = if on {
        doc! { "times": 1 }.into()
    } else {
        "off".into()
    };
    doc! {
        "configureFailPoint": "failCommand",
        "mode": mode,
        "data": { "failCommands": ["insert"], "closeConnection": true },
    }
}

/// Runs `f` with [`drop_next_insert`] on; turns it off again whatever `f` did.
async fn with_dropped_insert<T>(admin: &Client, f: impl std::future::IntoFuture<Output = T>) -> T {
    let on = admin
        .database("admin")
        .run_command(drop_next_insert(true))
        .await;
    let out = f.await;
    let off = admin
        .database("admin")
        .run_command(drop_next_insert(false))
        .await;
    on.unwrap_or_else(|e| panic!("failpoint on: {e}"));
    off.unwrap_or_else(|e| panic!("failpoint off: {e}"));
    out
}

/// The inserts on `ns` the profiler recorded (an insert the failpoint dropped never ran,
/// so only a sent-again one is recorded).
async fn profiled_inserts(admin: &Client, db: &str, ns: &str) -> u64 {
    admin
        .database(db)
        .collection::<Document>("system.profile")
        .count_documents(doc! { "op": "insert", "ns": ns })
        .await
        .unwrap()
}

/// A plain `Client` as root with the crate's default retryable writes, straight to the
/// server.
fn retrying_client(t: Target) -> Client {
    let cfg = root(t, "admin");
    let mut o = ClientOptions::default();
    o.hosts = vec![ServerAddress::Tcp {
        host: cfg.host.clone(),
        port: Some(cfg.port),
    }];
    let mut c = Credential::default();
    c.username = Some("root".into());
    c.password = Some(TEST_PASSWORD.into());
    c.source = Some("admin".into());
    o.credential = Some(c);
    o.direct_connection = Some(true);
    Client::with_options(o).unwrap()
}

/// The failpoint drops the connection on the session's `insertOne`: it is
/// `ConnectionLost` with the outcome unknown, and the crate does not send it again
/// (`retry_writes` is off). The dropped insert itself never runs, so the profiler shows
/// none, and no document. On the replica set a `Client` with retryable writes, under the
/// same failpoint, shows what a retry looks like: exactly one profiled insert.
#[tokio::test]
async fn retries_are_off() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_cancel_retry";
        let ns = format!("{db}.items");
        let admin = admin_client(t).await;
        let d = admin.database(db);
        d.drop().await.unwrap();
        d.create_collection("items").await.unwrap();
        d.run_command(doc! { "profile": 2 }).await.unwrap();
        let s = connect(&root(t, db)).await;

        let (res, _) = with_dropped_insert(
            &admin,
            run_with(
                &*s,
                "db.items.insertOne({_id: 1, x: 1})",
                &ExecOptions::default(),
            ),
        )
        .await;
        match res {
            Err(DbError::ConnectionLost {
                outcome_unknown: true,
                was_in_tx: false,
                ..
            }) => {}
            other => panic!("{t:?}: expected ConnectionLost with outcome unknown, got {other:?}"),
        }
        assert_eq!(
            profiled_inserts(&admin, db, &ns).await,
            0,
            "{t:?}: no retry"
        );
        // The session goes on with a fresh connection; nothing was inserted.
        assert_eq!(count(&*s, "items").await, Value::Int(0), "{t:?}");

        // Retryable writes exist on a replica set only.
        if s.server_info().capabilities.transactions {
            let control = retrying_client(t);
            let items = control.database(db).collection::<Document>("items");
            with_dropped_insert(&admin, items.insert_one(doc! { "_id": 2 }))
                .await
                .unwrap_or_else(|e| panic!("{t:?}: the retried insert failed: {e}"));
            assert_eq!(
                profiled_inserts(&admin, db, &ns).await,
                1,
                "{t:?}: the retry is the one insert that ran"
            );
            control.shutdown().immediate(true).await;
        }

        d.run_command(doc! { "profile": 0 }).await.unwrap();
        s.close().await;
        d.drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}
