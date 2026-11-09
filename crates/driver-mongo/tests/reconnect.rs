//! A connection the server (or a NAT) dropped while the session sat idle, against the
//! compose services `mongo60` and `mongo80`: an idle session `ping`s before its command,
//! a read whose connection dropped under it is sent again once, and a write never is.
//! Needs `DNT_TEST_DB=1`. Everything runs as `root` in scratch databases
//! (`dnt_reconnect_*`).
//!
//! The drop is `configureFailPoint failCommand` with `closeConnection` on one command name,
//! limited to the session's own `appName` ([`APP`]): the server closes the connection
//! before it runs the command, as on a connection that died while idle.

use dnt_core::command::{CommandLanguage, CommandSession, ParsedCommand};
use dnt_core::config::ResolvedConfig;
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session, StmtOutcome, StopSignal};
use dnt_core::error::DbError;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mongo::shell::MongoLanguage;
use dnt_driver_mongo::test_helpers::{open, session};
use dnt_driver_mongo::MongoSession;
use dnt_testkit::{require_db, Target, TEST_PASSWORD};
use mongodb::bson::{doc, Bson, Document};
use mongodb::Client;
use std::time::{Duration, Instant};

/// The sessions under test connect as this application; the failpoints only hit it.
const APP: &str = "dnt-reconnect-test";

#[derive(Default)]
struct Rec {
    rows: Vec<Vec<Value>>,
    ends: Vec<ResultSummary>,
}

impl RowSink for Rec {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.ends.push(summary.clone());
    }
}

fn root(t: Target, db: &str) -> ResolvedConfig {
    t.resolved_as("root", TEST_PASSWORD, Some(db))
}

/// The session under test, as [`APP`].
async fn connect(t: Target, db: &str) -> MongoSession {
    let mut c = root(t, db);
    c.application_name = APP.into();
    session(&c).await.unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

/// A plain `Client` as root (not [`APP`]): it sets the failpoints and checks the results.
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

fn command(s: &MongoSession) -> &dyn CommandSession {
    s
}

async fn run_with(
    s: &MongoSession,
    text: &str,
    opts: &ExecOptions,
) -> (Result<StmtOutcome, DbError>, Rec) {
    let mut rec = Rec::default();
    let r = command(s)
        .execute_command(&parse(text), opts, &mut rec)
        .await;
    (r, rec)
}

async fn run(s: &MongoSession, text: &str) -> (Result<StmtOutcome, DbError>, Rec) {
    run_with(s, text, &ExecOptions::default()).await
}

/// Drops `db`, then fills `db.items` with `n` documents.
async fn seed(admin: &Client, db: &str, n: i32) {
    let d = admin.database(db);
    d.drop().await.unwrap();
    d.create_collection("items").await.unwrap();
    if n > 0 {
        let docs: Vec<Document> = (0..n).map(|i| doc! { "_id": i, "n": i }).collect();
        d.collection::<Document>("items")
            .insert_many(docs)
            .await
            .unwrap();
    }
}

/// The failpoint's `count` in a `configureFailPoint` reply: how often it fired since the
/// server started (it is never reset).
fn fired(reply: &Document) -> i64 {
    match reply.get("count") {
        Some(Bson::Int32(n)) => i64::from(*n),
        Some(Bson::Int64(n)) => *n,
        other => panic!("configureFailPoint reply without a count: {other:?} in {reply}"),
    }
}

/// An armed [`drop_next`]: its count when it was armed.
struct Armed(i64);

/// Arms `failCommand` with `data` for [`APP`]'s next `times` commands.
async fn arm(admin: &Client, times: i32, mut data: Document) -> Armed {
    data.insert("appName", APP);
    let reply = admin
        .database("admin")
        .run_command(doc! {
            "configureFailPoint": "failCommand",
            "mode": { "times": times },
            "data": data,
        })
        .await
        .unwrap_or_else(|e| panic!("failpoint on: {e}"));
    Armed(fired(&reply))
}

/// Closes the connection of [`APP`]'s next `times` commands named `name`.
async fn drop_next(admin: &Client, name: &str, times: i32) -> Armed {
    arm(
        admin,
        times,
        doc! { "failCommands": [name], "closeConnection": true },
    )
    .await
}

/// Holds [`APP`]'s next command named `name` for `ms` before it runs (a connection that
/// does not answer).
async fn stall_next(admin: &Client, name: &str, ms: i32) -> Armed {
    arm(
        admin,
        1,
        doc! { "failCommands": [name], "blockConnection": true, "blockTimeMS": ms },
    )
    .await
}

impl Armed {
    /// Turns the failpoint off: how many commands it closed the connection of.
    async fn off(self, admin: &Client) -> i64 {
        let reply = admin
            .database("admin")
            .run_command(doc! { "configureFailPoint": "failCommand", "mode": "off" })
            .await
            .unwrap_or_else(|e| panic!("failpoint off: {e}"));
        fired(&reply) - self.0
    }
}

/// The documents of `db.items`, counted by the admin client (a session that pings before
/// every command must not count them itself).
async fn documents(admin: &Client, db: &str) -> u64 {
    admin
        .database(db)
        .collection::<Document>("items")
        .count_documents(doc! {})
        .await
        .unwrap()
}

/// The inserts on `db.items` the profiler recorded (an insert whose connection the
/// failpoint closed never ran).
async fn profiled_inserts(admin: &Client, db: &str) -> u64 {
    admin
        .database(db)
        .collection::<Document>("system.profile")
        .count_documents(doc! { "op": "insert", "ns": format!("{db}.items") })
        .await
        .unwrap()
}

/// The connection drops under a `find`: it is sent again once on a fresh connection and
/// returns its rows, with one reconnect counted. Dropped twice, it is `ConnectionLost`
/// with the outcome known (a read).
#[tokio::test]
async fn dropped_read_is_sent_again() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_read";
        let admin = admin_client(t).await;
        seed(&admin, db, 3).await;
        let s = connect(t, db).await;

        let armed = drop_next(&admin, "find", 1).await;
        let (res, rec) = run(&s, "db.items.find({})").await;
        let closed = armed.off(&admin).await;
        res.unwrap_or_else(|e| panic!("{t:?}: the read was not sent again: {e:?}"));
        assert_eq!(closed, 1, "{t:?}: the failpoint closed the first find");
        assert_eq!(rec.rows.len(), 3, "{t:?}");
        assert_eq!(rec.ends.len(), 1, "{t:?}: one result");
        assert_eq!(s.reconnects(), 1, "{t:?}");

        let armed = drop_next(&admin, "find", 2).await;
        let (res, rec) = run(&s, "db.items.find({})").await;
        let closed = armed.off(&admin).await;
        match res {
            Err(DbError::ConnectionLost {
                outcome_unknown: false,
                was_in_tx: false,
                ..
            }) => {}
            other => panic!("{t:?}: expected ConnectionLost, outcome known, got {other:?}"),
        }
        assert_eq!(closed, 2, "{t:?}: sent twice, never a third time");
        assert!(rec.rows.is_empty() && rec.ends.is_empty(), "{t:?}");

        // The session goes on.
        let (res, rec) = run(&s, "db.items.find({})").await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert_eq!(rec.rows.len(), 3, "{t:?}");

        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// After an idle spell the session `ping`s before the command. A ping whose connection
/// dropped is sent once more on a fresh connection, then the `insertOne` itself, once:
/// one document, one profiled insert, one reconnect. A ping that fails twice sends
/// nothing: `ConnectionLost` with the outcome known.
#[tokio::test]
async fn idle_session_pings_before_a_write() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_idle";
        let admin = admin_client(t).await;
        seed(&admin, db, 0).await;
        admin
            .database(db)
            .run_command(doc! { "profile": 2 })
            .await
            .unwrap();
        let s = connect(t, db).await;
        s.set_idle_after(Duration::ZERO);

        // A live connection answers the ping: nothing to replace.
        let (res, _) = run(&s, "db.items.insertOne({_id: 1})").await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert_eq!(s.reconnects(), 0, "{t:?}");

        let armed = drop_next(&admin, "ping", 1).await;
        let (res, _) = run(&s, "db.items.insertOne({_id: 2})").await;
        let closed = armed.off(&admin).await;
        res.unwrap_or_else(|e| panic!("{t:?}: the insert after the ping failed: {e:?}"));
        assert_eq!(closed, 1, "{t:?}: the failpoint closed the first ping");
        assert_eq!(s.reconnects(), 1, "{t:?}");
        assert_eq!(documents(&admin, db).await, 2, "{t:?}");
        assert_eq!(
            profiled_inserts(&admin, db).await,
            2,
            "{t:?}: one insert each"
        );

        let armed = drop_next(&admin, "ping", 2).await;
        let (res, _) = run(&s, "db.items.insertOne({_id: 3})").await;
        let closed = armed.off(&admin).await;
        match res {
            Err(DbError::ConnectionLost {
                outcome_unknown: false,
                was_in_tx: false,
                ..
            }) => {}
            other => panic!("{t:?}: expected ConnectionLost, nothing sent, got {other:?}"),
        }
        assert_eq!(closed, 2, "{t:?}: two pings, no third");
        assert_eq!(
            documents(&admin, db).await,
            2,
            "{t:?}: the insert was not sent"
        );
        assert_eq!(profiled_inserts(&admin, db).await, 2, "{t:?}");

        admin
            .database(db)
            .run_command(doc! { "profile": 0 })
            .await
            .unwrap();
        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// An idle ping that gets no answer within the command's timeout (a NAT that drops the
/// packets silently): the `Client` is rebuilt, the second ping answers, and the insert is
/// sent once, with one reconnect counted.
#[tokio::test]
async fn idle_ping_without_an_answer_rebuilds_the_client() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_stall";
        let admin = admin_client(t).await;
        seed(&admin, db, 0).await;
        let s = connect(t, db).await;
        s.set_idle_after(Duration::ZERO);
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(500)),
            ..ExecOptions::default()
        };

        let armed = stall_next(&admin, "ping", 3_000).await;
        let started = Instant::now();
        let (res, _) = run_with(&s, "db.items.insertOne({_id: 1})", &opts).await;
        let took = started.elapsed();
        let stalled = armed.off(&admin).await;
        res.unwrap_or_else(|e| panic!("{t:?}: the insert after the stalled ping failed: {e:?}"));
        assert_eq!(stalled, 1, "{t:?}: the failpoint held the first ping");
        assert!(
            took < Duration::from_millis(2_500),
            "{t:?}: waited {took:?}"
        );
        assert_eq!(s.reconnects(), 1, "{t:?}");
        assert_eq!(documents(&admin, db).await, 1, "{t:?}");

        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// The connection drops under the `insertOne` itself: `ConnectionLost` with the outcome
/// unknown, and it is not sent again. The failpoint closed exactly one insert (the one
/// attempt), which never ran: no profiled insert, no document.
#[tokio::test]
async fn dropped_write_is_not_sent_again() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_write";
        let admin = admin_client(t).await;
        seed(&admin, db, 0).await;
        admin
            .database(db)
            .run_command(doc! { "profile": 2 })
            .await
            .unwrap();
        let s = connect(t, db).await;

        let armed = drop_next(&admin, "insert", 1).await;
        let (res, _) = run(&s, "db.items.insertOne({_id: 1})").await;
        let closed = armed.off(&admin).await;
        match res {
            Err(DbError::ConnectionLost {
                outcome_unknown: true,
                was_in_tx: false,
                ..
            }) => {}
            other => panic!("{t:?}: expected ConnectionLost, outcome unknown, got {other:?}"),
        }
        assert_eq!(closed, 1, "{t:?}: one attempt");
        assert_eq!(
            profiled_inserts(&admin, db).await,
            0,
            "{t:?}: never sent again"
        );
        assert_eq!(documents(&admin, db).await, 0, "{t:?}");

        admin
            .database(db)
            .run_command(doc! { "profile": 0 })
            .await
            .unwrap();
        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// A stop raised while the idle ping waits for its answer: a cancel with nothing sent.
/// And one raised just as the ping answers (by [`MongoSession::on_ping_answered`], so the
/// race is not left to timing): the command is not sent either.
#[tokio::test]
async fn a_stop_during_the_idle_ping_sends_nothing() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_ping_stop";
        let admin = admin_client(t).await;
        seed(&admin, db, 0).await;
        let s = connect(t, db).await;
        s.set_idle_after(Duration::ZERO);

        // The ping is held; the stop comes meanwhile.
        let stop = StopSignal::new();
        let opts = ExecOptions {
            stop: stop.clone(),
            ..ExecOptions::default()
        };
        let armed = stall_next(&admin, "ping", 1_500).await;
        let raise = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.stop();
        });
        let (res, rec) = run_with(&s, "db.items.insertOne({_id: 1})", &opts).await;
        raise.await.unwrap();
        let stalled = armed.off(&admin).await;
        match res {
            Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected a cancel with nothing sent, got {other:?}"),
        }
        assert_eq!(stalled, 1, "{t:?}: the failpoint held the ping");
        assert!(rec.ends.is_empty(), "{t:?}");

        // The stop lands as the ping answers.
        let stop = StopSignal::new();
        let opts = ExecOptions {
            stop: stop.clone(),
            ..ExecOptions::default()
        };
        s.on_ping_answered(move || stop.stop());
        let (res, rec) = run_with(&s, "db.items.insertOne({_id: 2})", &opts).await;
        match res {
            Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            }) => {}
            other => panic!("{t:?}: expected a cancel with nothing sent, got {other:?}"),
        }
        assert!(rec.ends.is_empty(), "{t:?}");
        assert_eq!(
            documents(&admin, db).await,
            0,
            "{t:?}: nothing was inserted"
        );

        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// An `aggregate` with `$out` may write: its dropped connection is `ConnectionLost` with
/// the outcome unknown, and it is not sent again (the failpoint fired once, and no `$out`
/// collection was written: a second send would have run with the failpoint used up).
#[tokio::test]
async fn dropped_aggregate_with_out_is_not_sent_again() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_out";
        let admin = admin_client(t).await;
        seed(&admin, db, 3).await;
        let s = connect(t, db).await;

        let armed = drop_next(&admin, "aggregate", 1).await;
        let (res, _) = run(&s, "db.items.aggregate([{$match: {}}, {$out: 'copy'}])").await;
        let closed = armed.off(&admin).await;
        match res {
            Err(DbError::ConnectionLost {
                outcome_unknown: true,
                was_in_tx: false,
                ..
            }) => {}
            other => panic!("{t:?}: expected ConnectionLost, outcome unknown, got {other:?}"),
        }
        assert_eq!(closed, 1, "{t:?}: one attempt");
        let names = admin.database(db).list_collection_names().await.unwrap();
        assert!(!names.iter().any(|n| n == "copy"), "{t:?}: {names:?}");

        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}

/// The connection drops on a `getMore`, after the first batch came: the read runs again
/// from the start, and the sink gets every document once, in one result.
#[tokio::test]
async fn a_drop_during_get_more_reruns_the_read() {
    require_db!();
    for t in Target::mongo() {
        let db = "dnt_reconnect_getmore";
        let admin = admin_client(t).await;
        seed(&admin, db, 5).await;
        let s = connect(t, db).await;
        let opts = ExecOptions {
            batch_size: 2,
            ..ExecOptions::default()
        };

        let armed = drop_next(&admin, "getMore", 1).await;
        let (res, rec) = run_with(&s, "db.items.find({}).sort({_id: 1})", &opts).await;
        let closed = armed.off(&admin).await;
        res.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
        assert_eq!(closed, 1, "{t:?}: the first getMore was dropped");
        assert_eq!(rec.ends.len(), 1, "{t:?}: one result");
        assert_eq!(rec.ends[0].row_count, 5, "{t:?}");
        assert_eq!(rec.rows.len(), 5, "{t:?}: no row twice");
        let ids: Vec<Value> = rec.rows.iter().map(|r| r[0].clone()).collect();
        assert_eq!(ids, (0..5).map(Value::Int).collect::<Vec<_>>(), "{t:?}");
        assert_eq!(s.reconnects(), 1, "{t:?}");

        Box::new(s).close().await;
        admin.database(db).drop().await.unwrap();
        admin.shutdown().immediate(true).await;
    }
}
