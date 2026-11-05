//! Document edits (spec §6.3) and the atomic apply (§6.4) against the compose services
//! `mongo60` (standalone: no transactions) and `mongo80` (replica set `rs0`). Needs
//! `DNT_TEST_DB=1`. Everything runs as `root` in scratch databases (`dnt_edits_*`); the
//! edits are applied as the backend applies them: one `execute_atomic` unit when the
//! server has transactions, else each command in order.

use dnt_core::command::{AtomicFailure, CommandLanguage, CommandSession, ParsedCommand};
use dnt_core::config::ResolvedConfig;
use dnt_core::data_access::PageSpec;
use dnt_core::driver::{
    CellMode, Driver, ExecOptions, ResultSummary, RowSink, Session, StmtOutcome,
};
use dnt_core::edit::{CellInput, EditSet, RowEdit, RowRef};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mongo::rows::bson_to_value;
use dnt_driver_mongo::shell::MongoLanguage;
use dnt_driver_mongo::test_helpers::open;
use dnt_driver_mongo::MongoDriver;
use dnt_testkit::{require_db, Target, TEST_PASSWORD};
use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{doc, Binary, Bson, DateTime, Document};
use mongodb::Client;

const A: &str = "65a1b2c3d4e5f60718293a01";
const B: &str = "65a1b2c3d4e5f60718293a02";

#[derive(Debug, Default)]
struct Rec {
    cols: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl RowSink for Rec {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.cols = cols.iter().map(|c| c.name.clone()).collect();
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

impl Rec {
    fn col(&self, name: &str) -> Vec<Value> {
        let i = self
            .cols
            .iter()
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no column {name} in {:?}", self.cols));
        self.rows.iter().map(|r| r[i].clone()).collect()
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

fn command(s: &dyn Session) -> &dyn CommandSession {
    s.command().expect("a Command session")
}

fn parse(text: &str) -> ParsedCommand {
    MongoLanguage
        .parse(text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

/// Runs a console command (scratch set-up) and returns its outcome.
async fn console(s: &dyn Session, text: &str) -> StmtOutcome {
    command(s)
        .execute_command(&parse(text), &ExecOptions::default(), &mut Rec::default())
        .await
        .unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

/// A plain `Client` as root, to look at what the session wrote and to set failpoints.
async fn admin_client(t: Target) -> Client {
    open(&root(t, "admin"))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
        .client
}

/// A root session in a fresh scratch database `dnt_edits_<name>`.
async fn scratch(t: Target, name: &str) -> (Box<dyn Session>, String) {
    let db = format!("dnt_edits_{name}");
    let s = connect(&root(t, &db)).await;
    console(&*s, "db.dropDatabase()").await;
    (s, db)
}

fn coll(db: &str, name: &str) -> ObjectRef {
    ObjectRef {
        database: db.into(),
        schema: None,
        name: name.into(),
        kind: ObjectKind::Collection,
    }
}

fn oid(hex: &str) -> Bson {
    Bson::ObjectId(ObjectId::parse_str(hex).unwrap())
}

/// The grid value of `_id` (or any field) as the data view shows it.
fn shown(b: &Bson) -> Value {
    bson_to_value(b, &CellMode::Preview)
}

fn text(s: &str) -> CellInput {
    CellInput::Text { text: s.into() }
}

fn cells(list: &[(&str, CellInput)]) -> Vec<(String, CellInput)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn row(id: Value) -> RowRef {
    RowRef {
        key: vec![("_id".into(), id)],
        original: Vec::new(),
        xmin: None,
        loaded: Vec::new(),
    }
}

fn update(id: &Bson, set: &[(&str, CellInput)]) -> RowEdit {
    RowEdit::Update {
        row: row(shown(id)),
        set: cells(set),
    }
}

fn insert(values: &[(&str, CellInput)]) -> RowEdit {
    RowEdit::Insert {
        values: cells(values),
    }
}

fn delete(id: &Bson) -> RowEdit {
    RowEdit::Delete {
        row: row(shown(id)),
    }
}

/// The commands of `edits`, built as the backend builds them: from a fresh table detail.
async fn build(s: &dyn Session, obj: &ObjectRef, edits: Vec<RowEdit>) -> Vec<ParsedCommand> {
    let detail = s.introspector().table_detail(obj).await.unwrap();
    command(s)
        .data_access()
        .build_edits(
            obj,
            &detail,
            &EditSet {
                object: obj.clone(),
                edits,
                overwrite: false,
            },
        )
        .unwrap_or_else(|e| panic!("{e:?}"))
}

/// Each command in order, as the backend applies a plan without transactions: the first
/// error stops it (its index and error).
async fn apply_each(
    s: &dyn Session,
    cmds: &[ParsedCommand],
) -> Result<Vec<Option<u64>>, (usize, DbError)> {
    let mut affected = Vec::new();
    for (i, cmd) in cmds.iter().enumerate() {
        match command(s)
            .execute_command(cmd, &ExecOptions::default(), &mut Rec::default())
            .await
        {
            Ok(out) => affected.push(out.results[0].affected),
            Err(e) => return Err((i, e)),
        }
    }
    Ok(affected)
}

async fn apply_atomic(
    s: &dyn Session,
    cmds: &[ParsedCommand],
) -> Result<Vec<Option<u64>>, AtomicFailure> {
    command(s)
        .execute_atomic(cmds, &ExecOptions::default())
        .await
        .map(|outs| outs.iter().map(|o| o.results[0].affected).collect())
}

/// As the backend applies a plan: one atomic unit when the server has transactions.
async fn apply(s: &dyn Session, cmds: &[ParsedCommand]) -> Vec<Option<u64>> {
    if s.server_info().capabilities.transactions {
        apply_atomic(s, cmds)
            .await
            .unwrap_or_else(|f| panic!("{f:?}"))
    } else {
        apply_each(s, cmds)
            .await
            .unwrap_or_else(|f| panic!("{f:?}"))
    }
}

/// Every document of `db.coll`, in `_id` order, read with a plain client.
async fn docs(admin: &Client, db: &str, name: &str) -> Vec<Document> {
    let mut cursor = admin
        .database(db)
        .collection::<Document>(name)
        .find(doc! {})
        .sort(doc! { "_id": 1 })
        .await
        .unwrap();
    let mut out = Vec::new();
    while cursor.advance().await.unwrap() {
        out.push(cursor.deserialize_current().unwrap());
    }
    out
}

/// `configureFailPoint failCommand` on `admin`; `None`: off. Returns how many times the
/// failpoint has fired so far (the server's running `count`).
async fn fail_point(admin: &Client, spec: Option<Document>) -> i64 {
    let body = match spec {
        Some(s) => {
            let mut d = doc! { "configureFailPoint": "failCommand" };
            d.extend(s);
            d
        }
        None => doc! { "configureFailPoint": "failCommand", "mode": "off" },
    };
    let reply = admin.database("admin").run_command(body).await.unwrap();
    match reply.get("count") {
        Some(Bson::Int32(n)) => i64::from(*n),
        Some(Bson::Int64(n)) => *n,
        other => panic!("{other:?}"),
    }
}

fn not_found(e: &DbError, id: &str) {
    match e {
        DbError::Server { code, message, .. } => {
            assert_eq!(code.as_deref(), Some("DocumentNotFound"), "{message}");
            assert_eq!(message, &format!("Document {id} no longer exists"));
        }
        other => panic!("expected DocumentNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn update_unset_insert_delete() {
    require_db!();
    for t in Target::mongo() {
        let (s, db) = scratch(t, "basic").await;
        let s = &*s;
        console(
            s,
            &format!(
                "db.items.insertMany([\
                 {{_id: ObjectId('{A}'), name: 'a', qty: 1, note: 'x', big: NumberLong(1)}}, \
                 {{_id: ObjectId('{B}'), name: 'b', qty: 2}}])"
            ),
        )
        .await;
        let items = coll(&db, "items");
        let cmds = build(
            s,
            &items,
            vec![
                update(
                    &oid(A),
                    &[
                        ("name", text("a2")),
                        ("qty", text("5")),
                        ("big", text("7")),
                        ("note", CellInput::Unset),
                        ("added", text("new")),
                        ("gone", CellInput::Null),
                    ],
                ),
                insert(&[
                    ("name", text("c")),
                    ("qty", text("7")),
                    ("skipped", CellInput::Unset),
                ]),
                delete(&oid(B)),
            ],
        )
        .await;
        assert_eq!(cmds.len(), 3);
        // Matched, inserted, deleted: one each.
        assert_eq!(apply(s, &cmds).await, [Some(1), Some(1), Some(1)], "{t:?}");

        let admin = admin_client(t).await;
        let all = docs(&admin, &db, "items").await;
        assert_eq!(all.len(), 2, "{t:?}: {all:?}");
        assert_eq!(
            all[0],
            doc! {
                "_id": oid(A), "name": "a2", "qty": 5, "big": 7_i64,
                "added": "new", "gone": Bson::Null,
            },
            "{t:?}: the update kept the types and removed `note`"
        );
        // The insert: a server-side ObjectId `_id`, sampled types, no unset field.
        let new = &all[1];
        assert!(matches!(new.get("_id"), Some(Bson::ObjectId(_))), "{new:?}");
        assert_eq!(new.get("name"), Some(&Bson::String("c".into())));
        assert_eq!(new.get("qty"), Some(&Bson::Int32(7)));
        assert!(!new.contains_key("skipped"));
        console(s, "db.dropDatabase()").await;
        admin.shutdown().immediate(true).await;
    }
}

#[tokio::test]
async fn binary_id_document_editable() {
    require_db!();
    for t in Target::mongo() {
        let (s, db) = scratch(t, "binary").await;
        let s = &*s;
        console(
            s,
            "db.blobs.insertMany([\
             {_id: BinData(0, 'AQID'), n: 1}, \
             {_id: BinData(5, 'AAECAwQFBgcICQoLDA0ODw=='), n: 2}, \
             {_id: UUID('00010203-0405-0607-0809-0a0b0c0d0e0f'), n: 3}, \
             {_id: ObjectId('65a1b2c3d4e5f60718293a01'), n: 4}])",
        )
        .await;
        let blobs = coll(&db, "blobs");
        // The `_id`s as the data view shows them.
        let spec = PageSpec {
            sort: Vec::new(),
            filter: None,
            limit: 10,
            skip: 0,
        };
        let page = command(s).data_access().page(&blobs, &spec).unwrap();
        let mut rec = Rec::default();
        command(s)
            .execute_command(&page, &ExecOptions::default(), &mut rec)
            .await
            .unwrap();
        let ids = rec.col("_id");
        assert_eq!(ids.len(), 4, "{ids:?}");
        let mut edits: Vec<RowEdit> = ids
            .iter()
            .map(|id| RowEdit::Update {
                row: row(id.clone()),
                set: cells(&[("tag", text("x"))]),
            })
            .collect();
        // The generic-binary document goes.
        edits.push(RowEdit::Delete {
            row: row(ids[0].clone()),
        });
        let cmds = build(s, &blobs, edits).await;
        assert_eq!(
            apply(s, &cmds).await,
            [Some(1), Some(1), Some(1), Some(1), Some(1)],
            "{t:?}: every update matched its document"
        );
        let admin = admin_client(t).await;
        let all = docs(&admin, &db, "blobs").await;
        let left: Vec<(Bson, Bson)> = all
            .iter()
            .map(|d| (d.get("_id").unwrap().clone(), d.get("tag").unwrap().clone()))
            .collect();
        let bin = |subtype, bytes: Vec<u8>| Bson::Binary(Binary { subtype, bytes });
        assert_eq!(
            left,
            [
                (bin(BinarySubtype::Uuid, (0u8..16).collect()), "x".into()),
                (bin(BinarySubtype::Md5, (0u8..16).collect()), "x".into()),
                (oid(A), "x".into()),
            ],
            "{t:?}"
        );
        console(s, "db.dropDatabase()").await;
        admin.shutdown().immediate(true).await;
    }
}

#[tokio::test]
async fn nested_json_edit_keeps_types() {
    require_db!();
    for t in Target::mongo() {
        let (s, db) = scratch(t, "json").await;
        let s = &*s;
        console(
            s,
            &format!(
                "db.orders.insertOne({{_id: 1, \
                 shipping: {{carrier: 'DHL', order_ref: ObjectId('{B}'), \
                 at: ISODate('2024-11-06T00:00:00Z'), weight_g: NumberLong(1500)}}, \
                 items: [{{sku: 'A-1', qty: 2}}, {{sku: 'B-2', qty: NumberLong(1)}}]}})"
            ),
        )
        .await;
        let orders = coll(&db, "orders");
        // The cells as the grid holds them: canonical Extended JSON.
        let page = command(s)
            .data_access()
            .page(
                &orders,
                &PageSpec {
                    sort: Vec::new(),
                    filter: None,
                    limit: 10,
                    skip: 0,
                },
            )
            .unwrap();
        let mut rec = Rec::default();
        command(s)
            .execute_command(&page, &ExecOptions::default(), &mut rec)
            .await
            .unwrap();
        let shipping = match &rec.col("shipping")[0] {
            Value::Json(j) => j.clone(),
            other => panic!("{other:?}"),
        };
        let items = match &rec.col("items")[0] {
            Value::Json(j) => j.clone(),
            other => panic!("{other:?}"),
        };
        assert!(shipping.contains("$numberLong"), "{shipping}");
        // The user changes the carrier in the JSON cell, and the second sku in the
        // array as typed text.
        let edited = shipping.replace("DHL", "UPS");
        assert_ne!(edited, shipping);
        let cmds = build(
            s,
            &orders,
            vec![update(
                &Bson::Int32(1),
                &[
                    (
                        "shipping",
                        CellInput::Value {
                            value: Value::Json(edited),
                        },
                    ),
                    ("items", text(&items.replace("B-2", "C-3"))),
                ],
            )],
        )
        .await;
        assert_eq!(apply(s, &cmds).await, [Some(1)], "{t:?}");
        let admin = admin_client(t).await;
        let all = docs(&admin, &db, "orders").await;
        assert_eq!(
            all,
            [doc! {
                "_id": 1,
                "shipping": {
                    "carrier": "UPS",
                    "order_ref": oid(B),
                    "at": DateTime::from_millis(1_730_851_200_000),
                    "weight_g": 1500_i64,
                },
                "items": [{ "sku": "A-1", "qty": 2 }, { "sku": "C-3", "qty": 1_i64 }],
            }],
            "{t:?}: every nested type survived"
        );
        // mongosh syntax in a JSON field works too.
        let cmds = build(
            s,
            &orders,
            vec![update(
                &Bson::Int32(1),
                &[(
                    "shipping",
                    text("{carrier: 'Post', weight_g: NumberLong(9)}"),
                )],
            )],
        )
        .await;
        apply(s, &cmds).await;
        let all = docs(&admin, &db, "orders").await;
        assert_eq!(
            all[0].get("shipping"),
            Some(&Bson::Document(
                doc! { "carrier": "Post", "weight_g": 9_i64 }
            ))
        );
        console(s, "db.dropDatabase()").await;
        admin.shutdown().immediate(true).await;
    }
}

/// On `mongo80`, the second command of an atomic apply fails: nothing is written and the
/// unit is rolled back. A commit whose result is unknown is retried once.
#[tokio::test]
async fn atomic_apply_rolls_back() {
    require_db!();
    let t = Target::Mongo80;
    let (s, db) = scratch(t, "atomic").await;
    let s = &*s;
    assert!(s.server_info().capabilities.transactions);
    console(
        s,
        &format!("db.items.insertMany([{{_id: ObjectId('{A}'), qty: 1}}, {{_id: ObjectId('{B}'), qty: 2}}])"),
    )
    .await;
    let items = coll(&db, "items");
    let admin = admin_client(t).await;
    let cmds = build(
        s,
        &items,
        vec![
            update(&oid(A), &[("qty", text("10"))]),
            insert(&[("qty", text("30"))]),
            update(&oid(B), &[("qty", text("20"))]),
        ],
    )
    .await;
    let before = docs(&admin, &db, "items").await;

    // The server refuses the second command (the insert).
    let at = fail_point(
        &admin,
        Some(doc! {
            "mode": { "times": 1 },
            "data": { "failCommands": ["insert"], "errorCode": 2 },
        }),
    )
    .await;
    let failure = apply_atomic(s, &cmds).await;
    assert_eq!(fail_point(&admin, None).await - at, 1);
    match failure {
        Err(AtomicFailure {
            index: 1,
            error: DbError::Server { code, .. },
            rolled_back: true,
        }) => assert_eq!(code.as_deref(), Some("BadValue")),
        other => panic!("expected a rolled-back failure at 1, got {other:?}"),
    }
    assert_eq!(
        docs(&admin, &db, "items").await,
        before,
        "the first update was rolled back and nothing was inserted"
    );

    // A command with a cursor cannot run in a transaction: refused before anything is
    // sent.
    let find = parse(&format!("db.getSiblingDB('{db}').items.find()"));
    match apply_atomic(s, &[cmds[0].clone(), find]).await {
        Err(AtomicFailure {
            index: 1,
            error: DbError::Internal { .. },
            rolled_back: true,
        }) => {}
        other => panic!("expected a refusal at 1, got {other:?}"),
    }
    assert_eq!(docs(&admin, &db, "items").await, before);

    // A missing document aborts the unit too.
    let gone = build(
        s,
        &items,
        vec![
            update(&oid(A), &[("qty", text("10"))]),
            delete(&oid("65a1b2c3d4e5f60718293aff")),
        ],
    )
    .await;
    match apply_atomic(s, &gone).await {
        Err(AtomicFailure {
            index: 1,
            error,
            rolled_back: true,
        }) => not_found(&error, r#"{"$oid":"65a1b2c3d4e5f60718293aff"}"#),
        other => panic!("{other:?}"),
    }
    assert_eq!(docs(&admin, &db, "items").await, before);

    // The commit's result is unknown once (WriteConcernFailed): retried, and it commits.
    let at = fail_point(
        &admin,
        Some(doc! {
            "mode": { "times": 1 },
            "data": { "failCommands": ["commitTransaction"], "errorCode": 64 },
        }),
    )
    .await;
    let committed = apply_atomic(s, &cmds).await;
    assert_eq!(fail_point(&admin, None).await - at, 1);
    assert_eq!(
        committed.unwrap_or_else(|f| panic!("{f:?}")),
        [Some(1), Some(1), Some(1)]
    );
    let after = docs(&admin, &db, "items").await;
    let qty: Vec<&Bson> = after.iter().map(|d| d.get("qty").unwrap()).collect();
    assert_eq!(qty, [&Bson::Int32(10), &Bson::Int32(20), &Bson::Int32(30)]);

    // Unknown twice: retried once only, and the outcome stays unknown.
    let again = build(s, &items, vec![update(&oid(A), &[("qty", text("11"))])]).await;
    let at = fail_point(
        &admin,
        Some(doc! {
            "mode": { "times": 2 },
            "data": { "failCommands": ["commitTransaction"], "errorCode": 64 },
        }),
    )
    .await;
    let unknown = apply_atomic(s, &again).await;
    // Both failures were used, and a third commit (which would have gone through) was
    // never sent: the commit was retried exactly once.
    assert_eq!(fail_point(&admin, None).await - at, 2);
    match unknown {
        Err(AtomicFailure {
            index: 0,
            error:
                DbError::ConnectionLost {
                    outcome_unknown: true,
                    ..
                },
            rolled_back: false,
        }) => {}
        other => panic!("expected an unknown commit, got {other:?}"),
    }
    // The transaction never committed; end it so the scratch database can be dropped.
    admin
        .database("admin")
        .run_command(doc! { "killAllSessions": [] })
        .await
        .unwrap();
    assert_eq!(
        docs(&admin, &db, "items").await[0].get("qty"),
        Some(&Bson::Int32(10))
    );
    console(s, "db.dropDatabase()").await;
    admin.shutdown().immediate(true).await;
}

/// On `mongo60` there are no transactions: `execute_atomic` is the default `Unsupported`
/// and sends nothing; applied one by one, the first failure stops the apply.
#[tokio::test]
async fn standalone_apply_not_atomic() {
    require_db!();
    let t = Target::Mongo60;
    let (s, db) = scratch(t, "standalone").await;
    let s = &*s;
    assert!(!s.server_info().capabilities.transactions);
    console(
        s,
        &format!("db.items.insertOne({{_id: ObjectId('{A}'), qty: 1}})"),
    )
    .await;
    let items = coll(&db, "items");
    let admin = admin_client(t).await;
    let cmds = build(
        s,
        &items,
        vec![
            update(&oid(A), &[("qty", text("10"))]),
            update(&oid(B), &[("qty", text("20"))]),
            insert(&[("qty", text("30"))]),
        ],
    )
    .await;
    match apply_atomic(s, &cmds).await {
        Err(AtomicFailure {
            index: 0,
            error: DbError::Unsupported { feature, .. },
            rolled_back: false,
        }) => assert_eq!(feature, "transactions"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert_eq!(
        docs(&admin, &db, "items").await,
        [doc! { "_id": oid(A), "qty": 1 }],
        "nothing was sent"
    );
    // One by one: the first update applies, the missing document stops the rest.
    match apply_each(s, &cmds).await {
        Err((1, e)) => not_found(&e, &format!(r#"{{"$oid":"{B}"}}"#)),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        docs(&admin, &db, "items").await,
        [doc! { "_id": oid(A), "qty": 10 }],
        "applied up to the failure, not after it"
    );
    console(s, "db.dropDatabase()").await;
    admin.shutdown().immediate(true).await;
}

/// A document deleted elsewhere after the page was read: its update or delete is
/// `DocumentNotFound`, and nothing else of the apply is written. A console `updateOne`
/// that matches nothing is no error.
#[tokio::test]
async fn deleted_elsewhere_is_document_not_found() {
    require_db!();
    for t in Target::mongo() {
        let (s, db) = scratch(t, "gone").await;
        let s = &*s;
        console(
            s,
            &format!(
                "db.items.insertMany([{{_id: ObjectId('{A}'), qty: 1}}, {{_id: 'k', qty: 2}}])"
            ),
        )
        .await;
        let items = coll(&db, "items");
        let admin = admin_client(t).await;
        // Built from what the page showed, then the documents are deleted elsewhere.
        let update_a = build(s, &items, vec![update(&oid(A), &[("qty", text("5"))])]).await;
        let delete_k = build(s, &items, vec![delete(&Bson::String("k".into()))]).await;
        let coll_handle = admin.database(&db).collection::<Document>("items");
        coll_handle.delete_many(doc! {}).await.unwrap();

        for (cmds, id) in [
            (&update_a, format!(r#"{{"$oid":"{A}"}}"#)),
            (&delete_k, r#""k""#.to_string()),
        ] {
            if s.server_info().capabilities.transactions {
                match apply_atomic(s, cmds).await {
                    Err(AtomicFailure {
                        index: 0,
                        error,
                        rolled_back: true,
                    }) => not_found(&error, &id),
                    other => panic!("{t:?}: {other:?}"),
                }
            } else {
                match apply_each(s, cmds).await {
                    Err((0, e)) => not_found(&e, &id),
                    other => panic!("{t:?}: {other:?}"),
                }
            }
        }
        // The console's own updateOne matches nothing without an error.
        let out = console(s, "db.items.updateOne({_id: 'missing'}, {$set: {qty: 1}})").await;
        assert_eq!(out.results[0].affected, Some(0));
        assert!(docs(&admin, &db, "items").await.is_empty(), "{t:?}");
        console(s, "db.dropDatabase()").await;
        admin.shutdown().immediate(true).await;
    }
}
