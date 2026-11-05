//! Console execution (spec §5.2–§5.4, §5.6) against the compose services `mongo60` and
//! `mongo80`. Needs `DNT_TEST_DB=1`. Writes go to scratch databases (`dnt_console_*`) as
//! `root`; `dnt_fixture` is only read.

use dnt_core::command::{CommandLanguage, ParsedCommand};
use dnt_core::config::ResolvedConfig;
use dnt_core::driver::{
    CellMode, Driver, ExecOptions, ResultSummary, RowSink, Session, StmtOutcome,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::value::{ColumnMeta, FullValue, Value};
use dnt_driver_mongo::shell::MongoLanguage;
use dnt_driver_mongo::MongoDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use mongodb::bson::{Bson, Document};
use std::time::Duration;

#[derive(Default)]
struct Rec {
    /// The column names of every `result_start`.
    starts: Vec<Vec<String>>,
    metas: Vec<Vec<ColumnMeta>>,
    rows: Vec<Vec<Value>>,
    full: Vec<Vec<FullValue>>,
    /// The size of every `rows` / `full_rows` batch.
    batches: Vec<usize>,
    ends: Vec<ResultSummary>,
}

impl RowSink for Rec {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.starts
            .push(cols.iter().map(|c| c.name.clone()).collect());
        self.metas.push(cols.to_vec());
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.batches.push(batch.len());
        self.rows.extend(batch);
    }
    fn full_rows(&mut self, batch: Vec<Vec<FullValue>>) {
        self.batches.push(batch.len());
        self.full.extend(batch);
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.ends.push(summary.clone());
    }
}

impl Rec {
    /// The values of column `name` in every row.
    fn col(&self, name: &str) -> Vec<Value> {
        let i = self.starts[0]
            .iter()
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.starts));
        self.rows.iter().map(|r| r[i].clone()).collect()
    }

    fn summary(&self) -> &ResultSummary {
        assert_eq!(self.ends.len(), 1, "one result");
        &self.ends[0]
    }

    /// The `result` cell of a write, as a document.
    fn result_doc(&self) -> Document {
        match self.col("result").as_slice() {
            [Value::Json(text)] => {
                let json: serde_json::Value = serde_json::from_str(text).unwrap();
                match Bson::try_from(json).unwrap() {
                    Bson::Document(d) => d,
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("expected one result cell, got {other:?}"),
        }
    }
}

fn cfg(t: Target, user: &str, db: &str) -> ResolvedConfig {
    t.resolved_as(user, TEST_PASSWORD, Some(db))
}

async fn connect(c: &ResolvedConfig) -> Box<dyn Session> {
    MongoDriver
        .connect(c)
        .await
        .unwrap_or_else(|e| panic!("{c:?}: {e}"))
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
    assert_eq!(outcome.results.len(), 1, "{text}");
    assert_eq!(outcome.results[0], *rec.summary(), "{text}");
    rec
}

async fn run_err(s: &dyn Session, text: &str) -> DbError {
    match run_with(s, text, &ExecOptions::default()).await.0 {
        Ok(o) => panic!("{text}: expected an error, got {o:?}"),
        Err(e) => e,
    }
}

/// The `value` of a one-row, one-column result.
async fn value(s: &dyn Session, text: &str) -> Value {
    let r = run(s, text).await;
    assert_eq!(r.starts, [vec!["value".to_string()]], "{text}");
    match r.col("value").as_slice() {
        [v] => v.clone(),
        other => panic!("{text}: {other:?}"),
    }
}

/// A root session in a fresh scratch database `dnt_console_<name>`.
async fn scratch(t: Target, name: &str) -> Box<dyn Session> {
    let db = format!("dnt_console_{name}");
    let s = connect(&cfg(t, "root", &db)).await;
    run(&*s, "db.dropDatabase()").await;
    s
}

async fn drop_scratch(s: Box<dyn Session>) {
    run(&*s, "db.dropDatabase()").await;
    s.close().await;
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn names(r: &Rec) -> Vec<String> {
    r.col("name")
        .into_iter()
        .map(|v| match v {
            Value::Text(s) => s,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn find_sort_limit_project() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let r = run(
            &*s,
            "db.orders.find({}, {customer: 1}).sort({qty: -1}).limit(2)",
        )
        .await;
        assert_eq!(r.starts, [vec!["_id".to_string(), "customer".into()]]);
        assert_eq!(r.col("customer"), [text("Dave"), text("Eve")]);
        assert_eq!((r.summary().row_count, r.summary().truncated), (2, false));
        assert_eq!(r.summary().affected, None);

        let r = run(
            &*s,
            "db.orders.find({status: 'paid'})\n  .sort({customer: 1}).skip(1)\n  .project({customer: 1, _id: 0})",
        )
        .await;
        assert_eq!(r.starts, [vec!["customer".to_string()]]);
        assert_eq!(r.col("customer"), [text("Carol"), text("Eve")]);

        // `findOne`: one row.
        let r = run(
            &*s,
            "db.orders.findOne({customer: 'Bob'}, {qty: 1, _id: 0})",
        )
        .await;
        assert_eq!(r.col("qty"), [Value::Int(1)]);
        let r = run(&*s, "db.orders.findOne({customer: 'Nobody'})").await;
        assert_eq!(r.summary().row_count, 0);

        // `find().count()` keeps the modifiers before it.
        assert_eq!(
            value(&*s, "db.orders.find({status: 'paid'}).count()").await,
            Value::Int(3)
        );
        assert_eq!(
            value(&*s, "db.orders.find({status: 'paid'}).limit(2).count()").await,
            Value::Int(2)
        );
        assert_eq!(
            value(&*s, "db.orders.countDocuments({})").await,
            Value::Int(5)
        );
        assert_eq!(
            value(&*s, "db.orders.estimatedDocumentCount()").await,
            Value::Int(5)
        );
        let r = run(&*s, "db.orders.distinct('status')").await;
        assert_eq!(r.starts, [vec!["value".to_string()]]);
        assert_eq!(
            r.col("value"),
            [text("paid"), text("pending"), text("shipped")]
        );
        // Columns: the BSON type of each field.
        let r = run(
            &*s,
            "db.orders.find({customer: 'Alice'}, {qty: 1, mixed: 1})",
        )
        .await;
        let types: Vec<(&str, &str)> = r.metas[0]
            .iter()
            .map(|m| (m.name.as_str(), m.db_type.as_str()))
            .collect();
        assert_eq!(
            types,
            [("_id", "objectId"), ("qty", "int"), ("mixed", "int")]
        );
        let r = run(&*s, "db.orders.find({}, {mixed: 1, _id: 0})").await;
        assert_eq!(r.metas[0][0].db_type, "mixed(array|int|null|object|string)");
        s.close().await;
    }
}

#[tokio::test]
async fn new_fields_in_later_batches_one_header() {
    require_db!();
    for t in Target::mongo() {
        let s = scratch(t, "batches").await;
        run(
            &*s,
            "db.docs.insertMany([{_id: 1, a: 1}, {_id: 2, a: 2}, {_id: 3, b: 3}, {_id: 4, c: 4}, {_id: 5, a: 5}])",
        )
        .await;
        let opts = ExecOptions {
            batch_size: 2,
            ..ExecOptions::default()
        };
        let (r, rec) = run_with(
            &*s,
            "db.docs.find({}, {}, {batchSize: 2}).sort({_id: 1})",
            &opts,
        )
        .await;
        r.unwrap();
        assert_eq!(
            rec.starts,
            [vec!["_id".to_string(), "a".into(), "b".into(), "c".into()]]
        );
        assert_eq!(rec.batches, [2, 2, 1]);
        assert_eq!(
            rec.rows[2],
            [Value::Int(3), Value::Null, Value::Int(3), Value::Null]
        );
        assert_eq!(
            (rec.summary().row_count, rec.summary().truncated),
            (5, false)
        );

        // `max_rows` bounds the buffer: one more document is read to know it is cut.
        let opts = ExecOptions {
            batch_size: 2,
            max_rows: 3,
            ..ExecOptions::default()
        };
        let (r, rec) = run_with(&*s, "db.docs.find().sort({_id: 1})", &opts).await;
        r.unwrap();
        assert_eq!(rec.starts.len(), 1);
        assert_eq!(rec.batches, [2, 1]);
        assert_eq!(
            (rec.summary().row_count, rec.summary().truncated),
            (3, true)
        );
        drop_scratch(s).await;
    }
}

#[tokio::test]
async fn aggregate_out_writes() {
    require_db!();
    for t in Target::mongo() {
        let s = scratch(t, "out").await;
        run(
            &*s,
            "db.src.insertMany([{_id: 1, k: 'a'}, {_id: 2, k: 'b'}, {_id: 3, k: 'a'}])",
        )
        .await;
        let r = run(
            &*s,
            "db.src.aggregate([{$match: {k: 'a'}}, {$out: 'dest'}])",
        )
        .await;
        assert_eq!(r.summary().row_count, 0);
        assert_eq!(
            value(&*s, "db.dest.countDocuments({})").await,
            Value::Int(2)
        );
        // Modifiers after `aggregate` are stages after the pipeline.
        let r = run(
            &*s,
            "db.src.aggregate([{$match: {}}]).sort({_id: -1}).skip(1).limit(1).project({k: 1, _id: 0})",
        )
        .await;
        assert_eq!(r.col("k"), [text("b")]);
        drop_scratch(s).await;
    }
}

#[tokio::test]
async fn writes_report_affected() {
    require_db!();
    for t in Target::mongo() {
        let s = scratch(t, "writes").await;
        let r = run(&*s, "db.c.insertOne({_id: 1, a: 1})").await;
        assert_eq!(r.starts, [vec!["result".to_string()]]);
        assert_eq!(r.summary().affected, Some(1));
        let d = r.result_doc();
        assert_eq!(d.get("acknowledged"), Some(&Bson::Boolean(true)));
        assert_eq!(
            d.get("insertedIds"),
            Some(&Bson::Array(vec![Bson::Int32(1)]))
        );

        // A generated `_id` is reported.
        let r = run(&*s, "db.c.insertOne({a: 7})").await;
        assert!(matches!(
            r.result_doc().get_array("insertedIds").unwrap().as_slice(),
            [Bson::ObjectId(_)]
        ));
        run(&*s, "db.c.deleteOne({a: 7})").await;

        let r = run(&*s, "db.c.insertMany([{_id: 2, a: 1}, {_id: 3, a: 2}])").await;
        assert_eq!(r.summary().affected, Some(2));

        // Update: `affected` is the matched count, not the modified one.
        let r = run(&*s, "db.c.updateMany({a: 1}, {$set: {a: 1}})").await;
        assert_eq!(r.summary().affected, Some(2));
        let d = r.result_doc();
        assert_eq!(
            (d.get_i64("matchedCount"), d.get_i64("modifiedCount")),
            (Ok(2), Ok(0))
        );
        let r = run(&*s, "db.c.updateOne({_id: 3}, {$set: {a: 5}})").await;
        assert_eq!(r.summary().affected, Some(1));
        let r = run(&*s, "db.c.replaceOne({_id: 3}, {b: 1})").await;
        assert_eq!(r.summary().affected, Some(1));
        let r = run(
            &*s,
            "db.c.updateOne({_id: 9}, {$set: {a: 1}}, {upsert: true})",
        )
        .await;
        assert_eq!(r.summary().affected, Some(0));
        assert_eq!(r.result_doc().get("upsertedId"), Some(&Bson::Int32(9)));

        let r = run(&*s, "db.c.deleteOne({_id: 9})").await;
        assert_eq!(r.summary().affected, Some(1));
        assert_eq!(r.result_doc().get_i64("deletedCount"), Ok(1));
        let r = run(&*s, "db.c.deleteMany({a: 1})").await;
        assert_eq!(r.summary().affected, Some(2));

        // findOneAnd*: the document, and 1 or 0.
        let r = run(
            &*s,
            "db.c.findOneAndUpdate({_id: 3}, {$set: {c: 1}}, {returnDocument: 'after'})",
        )
        .await;
        assert_eq!(r.summary().affected, Some(1));
        assert_eq!(r.col("c"), [Value::Int(1)]);
        let r = run(&*s, "db.c.findOneAndDelete({_id: 42})").await;
        assert_eq!((r.summary().affected, r.summary().row_count), (Some(0), 0));

        // An update document without operators would replace the document: refused.
        assert!(matches!(
            run_err(&*s, "db.c.updateOne({_id: 3}, {b: 2})").await,
            DbError::Config { field: None, .. }
        ));
        assert!(matches!(
            run_err(&*s, "db.c.replaceOne({_id: 3}, {$set: {b: 2}})").await,
            DbError::Config { field: None, .. }
        ));
        // A duplicate key: the server's code name.
        match run_err(&*s, "db.c.insertOne({_id: 3})").await {
            DbError::Server { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("DuplicateKey"));
                assert!(message.contains("_id_"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        drop_scratch(s).await;
    }
}

#[tokio::test]
async fn insert_many_partial_failure_message() {
    require_db!();
    for t in Target::mongo() {
        let s = scratch(t, "partial").await;
        match run_err(
            &*s,
            "db.c.insertMany([{_id: 1}, {_id: 2}, {_id: 1}, {_id: 3}])",
        )
        .await
        {
            DbError::Server { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("DuplicateKey"));
                assert!(
                    message.starts_with("Inserted 2 of 4 before the error: "),
                    "{message}"
                );
                assert!(message.contains("duplicate key"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(value(&*s, "db.c.countDocuments({})").await, Value::Int(2));
        drop_scratch(s).await;
    }
}

#[tokio::test]
async fn use_switches_db_and_checks_permission() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "root", FIXTURE_DB)).await;
        let cs = s.command().unwrap();
        assert_eq!(cs.current_database().as_deref(), Some(FIXTURE_DB));
        let r = run(&*s, "use dnt_console_use").await;
        assert_eq!((r.starts.len(), r.summary().row_count), (1, 0));
        assert!(r.starts[0].is_empty());
        assert_eq!(cs.current_database().as_deref(), Some("dnt_console_use"));
        run(&*s, "db.c.insertOne({_id: 1})").await;
        assert_eq!(value(&*s, "db.c.countDocuments({})").await, Value::Int(1));
        run(&*s, "db.dropDatabase()").await;
        s.close().await;

        // `dnt` may only use dnt_fixture: the server's refusal comes at `use`.
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let cs = s.command().unwrap();
        match run_err(&*s, "use dnt_console_use").await {
            DbError::Server { code, .. } => assert_eq!(code.as_deref(), Some("Unauthorized")),
            other => panic!("{other:?}"),
        }
        assert_eq!(cs.current_database().as_deref(), Some(FIXTURE_DB));
        run(&*s, "use dnt_fixture").await;
        assert_eq!(
            value(&*s, "db.orders.countDocuments({})").await,
            Value::Int(5)
        );
        s.close().await;
    }
}

#[tokio::test]
async fn show_dbs_and_collections() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "root", FIXTURE_DB)).await;
        let r = run(&*s, "show dbs").await;
        assert_eq!(r.starts, [vec!["name".to_string(), "sizeOnDisk".into()]]);
        let dbs = names(&r);
        assert!(dbs.iter().any(|n| n == FIXTURE_DB), "{dbs:?}");
        assert!(dbs.iter().any(|n| n == "admin"), "{dbs:?}");

        for text in ["show collections", "show tables", "db.getCollectionNames()"] {
            let r = run(&*s, text).await;
            assert_eq!(r.starts, [vec!["name".to_string()]], "{text}");
            let colls = names(&r);
            assert!(colls.iter().any(|n| n == "orders"), "{text}: {colls:?}");
            assert!(
                colls.iter().any(|n| n == "paid_orders"),
                "{text}: {colls:?}"
            );
            let mut sorted = colls.clone();
            sorted.sort();
            assert_eq!(colls, sorted, "{text}");
        }
        // Other db methods.
        let r = run(&*s, "db.stats()").await;
        assert_eq!(r.col("db"), [text(FIXTURE_DB)]);
        match value(&*s, "db.version()").await {
            Value::Text(v) => {
                assert!(v.starts_with(if t == Target::Mongo60 { "6." } else { "8." }))
            }
            other => panic!("{other:?}"),
        }
        let r = run(&*s, "db.serverStatus()").await;
        assert_eq!(r.summary().row_count, 1);
        let r = run(&*s, "db.orders.getIndexes()").await;
        assert_eq!(r.col("name"), [text("_id_")]);
        s.close().await;
    }
}

#[tokio::test]
async fn explain_query_planner() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        for text in [
            "db.orders.find({status: 'paid'}).explain()",
            "db.orders.explain().find({status: 'paid'})",
            "db.orders.explain().aggregate([{$match: {status: 'paid'}}])",
            "db.orders.explain().countDocuments({status: 'paid'})",
        ] {
            let r = run(&*s, text).await;
            assert_eq!(r.summary().row_count, 1, "{text}");
            // An aggregation the server cannot push down whole explains as `stages`.
            assert!(
                r.starts[0]
                    .iter()
                    .any(|c| c == "queryPlanner" || c == "stages"),
                "{text}: {:?}",
                r.starts
            );
        }
        let r = run(&*s, "db.orders.find().explain('executionStats')").await;
        assert!(
            r.starts[0].iter().any(|c| c == "executionStats"),
            "{:?}",
            r.starts
        );

        // Explaining a write runs nothing.
        for text in [
            "db.orders.explain().updateMany({}, {$set: {explained: 1}})",
            "db.orders.explain().deleteMany({})",
            "db.orders.explain().findOneAndUpdate({}, {$set: {explained: 1}})",
        ] {
            let r = run(&*s, text).await;
            assert_eq!(r.summary().row_count, 1, "{text}");
        }
        assert_eq!(
            value(&*s, "db.orders.countDocuments({})").await,
            Value::Int(5)
        );
        assert_eq!(
            value(&*s, "db.orders.countDocuments({explained: 1})").await,
            Value::Int(0)
        );
        // Not every method has an explain.
        assert!(matches!(
            run_err(&*s, "db.orders.explain().getIndexes()").await,
            DbError::Unsupported { .. }
        ));
        s.close().await;
    }
}

#[tokio::test]
async fn run_command_ping() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        for text in ["db.runCommand({ping: 1})", "db.adminCommand({ping: 1})"] {
            let r = run(&*s, text).await;
            assert_eq!(r.summary().row_count, 1, "{text}");
            assert_eq!(r.col("ok"), [Value::Float(1.0)], "{text}");
        }
        s.ping().await.unwrap();
        // A server error keeps the code name.
        match run_err(&*s, "db.runCommand({noSuchCommand: 1})").await {
            DbError::Server { code, .. } => assert_eq!(code.as_deref(), Some("CommandNotFound")),
            other => panic!("{other:?}"),
        }
        s.close().await;
    }
}

#[tokio::test]
async fn collection_stats_uses_collstats_stage() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let r = run(&*s, "db.orders.stats()").await;
        assert_eq!(r.summary().row_count, 1);
        // `$collStats` puts the sizes under `storageStats`; the `collStats` command does not.
        assert!(
            r.starts[0].iter().any(|c| c == "storageStats"),
            "{:?}",
            r.starts
        );
        assert_eq!(r.col("ns"), [text("dnt_fixture.orders")]);
        let r = run(&*s, "db.orders.stats(1024)").await;
        match r.col("storageStats").as_slice() {
            [Value::Json(j)] => assert!(j.contains("\"scaleFactor\""), "{j}"),
            other => panic!("{other:?}"),
        }
        s.close().await;
    }
}

#[tokio::test]
async fn max_time_ms_is_timeout() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let slow = "db.orders.find({$where: 'sleep(200) || true'})";
        let timeout = |e: &DbError| {
            matches!(
                e,
                DbError::Timeout {
                    stage: ConnectStage::Execute,
                    outcome_unknown: false,
                    was_in_tx: false
                }
            )
        };
        // The user's maxTimeMS.
        let e = run_err(&*s, &format!("{slow}.maxTimeMS(50)")).await;
        assert!(timeout(&e), "{e:?}");
        // The statement timeout, and the smaller of the two.
        let opts = ExecOptions {
            timeout: Some(Duration::from_millis(50)),
            ..ExecOptions::default()
        };
        for text in [slow.to_string(), format!("{slow}.maxTimeMS(600000)")] {
            let (r, rec) = run_with(&*s, &text, &opts).await;
            let e = r.unwrap_err();
            assert!(timeout(&e), "{text}: {e:?}");
            assert!(rec.starts.is_empty(), "{text}");
        }
        // The session goes on.
        assert_eq!(
            value(&*s, "db.orders.countDocuments({})").await,
            Value::Int(5)
        );
        assert_eq!(s.reconnects(), 0);
        s.close().await;
    }
}

#[tokio::test]
async fn read_only_refuses_insert() {
    require_db!();
    for t in Target::mongo() {
        let mut c = cfg(t, "dnt", FIXTURE_DB);
        c.read_only = true;
        let s = connect(&c).await;
        let before = value(&*s, "db.orders.countDocuments({})").await;
        assert_eq!(before, Value::Int(5));
        for text in [
            "db.orders.insertOne({x: 1})",
            "db.orders.updateOne({}, {$set: {x: 1}})",
            "db.orders.aggregate([{$facet: {a: [{$out: 'copy'}]}}])",
            "db.runCommand({insert: 'orders', documents: [{x: 1}]})",
            "db.orders.explain('executionStats').find({})",
            "db.copy.drop()",
        ] {
            match run_err(&*s, text).await {
                DbError::Config {
                    field: None,
                    message,
                } => assert_eq!(message, "This connection is read-only", "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        assert_eq!(value(&*s, "db.orders.countDocuments({})").await, before);
        assert_eq!(
            value(&*s, "db.copy.countDocuments({})").await,
            Value::Int(0)
        );
        // Reads, explain queryPlanner and `use` still run.
        run(&*s, "db.orders.find({status: 'paid'})").await;
        run(&*s, "db.orders.find().explain()").await;
        run(&*s, "db.orders.aggregate([{$match: {}}])").await;
        run(&*s, "db.runCommand({count: 'orders'})").await;
        run(&*s, "use dnt_fixture").await;
        s.close().await;
    }
}

#[tokio::test]
async fn cells_preview_and_full() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let query = "db.orders.find({customer: 'Alice'}, {customer: 1, items: 1, _id: 0})";
        let r = run(&*s, query).await;
        assert!(r.full.is_empty());
        assert_eq!(r.col("customer"), [text("Alice")]);
        assert!(matches!(r.col("items").as_slice(), [Value::Json(_)]));

        let full = ExecOptions {
            cells: CellMode::Full {
                max_cell_bytes: 1 << 20,
            },
            ..ExecOptions::default()
        };
        let (out, f) = run_with(&*s, query, &full).await;
        out.unwrap();
        assert!(f.rows.is_empty());
        let shown: Vec<Vec<Value>> = f
            .full
            .iter()
            .map(|row| row.iter().map(FullValue::to_display).collect())
            .collect();
        assert_eq!(shown, r.rows);

        // A cell over the limit is an error, never a cut value.
        let small = ExecOptions {
            cells: CellMode::Full { max_cell_bytes: 4 },
            ..ExecOptions::default()
        };
        let (out, f) = run_with(&*s, query, &small).await;
        match out {
            Err(DbError::Unsupported { feature, .. }) => {
                assert_eq!(feature, "cell larger than 4 bytes")
            }
            other => panic!("{other:?}"),
        }
        assert!(f.full.is_empty() && f.rows.is_empty());
        s.close().await;
    }
}
