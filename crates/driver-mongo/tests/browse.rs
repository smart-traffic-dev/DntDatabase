//! The object tree and the data view's reads (spec §6.1, §6.2) against the compose
//! services `mongo60` and `mongo80`. Needs `DNT_TEST_DB=1`. `dnt_fixture` is only read;
//! scratch databases (`dnt_browse_*`) are written as `root`.

use dnt_core::command::{CommandLanguage, CommandSession};
use dnt_core::config::ResolvedConfig;
use dnt_core::data_access::{check_read, PageSpec};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::sql::filter::{DataFilter, FilterCond, FilterMode, FilterOp, SortDir, SortSpec};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mongo::shell::MongoLanguage;
use dnt_driver_mongo::MongoDriver;
use dnt_query::edit::command_fingerprint;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};

#[derive(Debug, Default)]
struct Rec {
    cols: Vec<String>,
    rows: Vec<Vec<Value>>,
    ends: usize,
}

impl RowSink for Rec {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.cols = cols.iter().map(|c| c.name.clone()).collect();
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {
        self.ends += 1;
    }
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

    /// The `customer` of every row, as text.
    fn customers(&self) -> Vec<String> {
        self.col("customer")
            .into_iter()
            .map(|v| match v {
                Value::Text(s) => s,
                other => panic!("{other:?}"),
            })
            .collect()
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

fn command(s: &dyn Session) -> &dyn CommandSession {
    s.command().expect("a Command session")
}

/// Runs a console command (scratch set-up).
async fn console(s: &dyn Session, text: &str) {
    let cmd = MongoLanguage
        .parse(text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    command(s)
        .execute_command(&cmd, &ExecOptions::default(), &mut Rec::default())
        .await
        .unwrap_or_else(|e| panic!("{text}: {e:?}"));
}

/// A root session in a fresh scratch database `dnt_browse_<name>`.
async fn scratch(t: Target, name: &str) -> (Box<dyn Session>, String) {
    let db = format!("dnt_browse_{name}");
    let s = connect(&cfg(t, "root", &db)).await;
    console(&*s, "db.dropDatabase()").await;
    (s, db)
}

fn object(db: &str, name: &str, kind: ObjectKind) -> ObjectRef {
    ObjectRef {
        database: db.into(),
        schema: None,
        name: name.into(),
        kind,
    }
}

fn orders() -> ObjectRef {
    object(FIXTURE_DB, "orders", ObjectKind::Collection)
}

fn cond(column: &str, op: FilterOp, values: &[&str]) -> FilterCond {
    FilterCond {
        column: column.into(),
        op,
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

fn conditions(mode: FilterMode, conditions: Vec<FilterCond>) -> Option<DataFilter> {
    Some(DataFilter {
        mode,
        conditions,
        raw_where: None,
        raw_json: None,
    })
}

fn where_(c: FilterCond) -> Option<DataFilter> {
    conditions(FilterMode::And, vec![c])
}

fn raw(text: &str) -> Option<DataFilter> {
    Some(DataFilter {
        mode: FilterMode::And,
        conditions: Vec::new(),
        raw_where: None,
        raw_json: Some(text.into()),
    })
}

/// One page as the backend reads it: built by `DataAccess::page`, checked as a read, run
/// with `max_rows` = the page's limit.
async fn page(
    s: &dyn Session,
    obj: &ObjectRef,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
    skip: u64,
    limit: u32,
) -> Result<Rec, DbError> {
    let c = command(s);
    let spec = PageSpec {
        sort,
        filter,
        limit,
        skip,
    };
    let cmd = c.data_access().page(obj, &spec)?;
    check_read(&cmd)?;
    let opts = ExecOptions {
        max_rows: limit as usize,
        ..ExecOptions::default()
    };
    let mut rec = Rec::default();
    c.execute_command(&cmd, &opts, &mut rec).await?;
    assert_eq!(rec.ends, 1, "{}", cmd.text);
    Ok(rec)
}

/// All matching documents, in `_id` order.
async fn all(s: &dyn Session, filter: Option<DataFilter>) -> Rec {
    page(s, &orders(), Vec::new(), filter, 0, 1000)
        .await
        .unwrap_or_else(|e| panic!("{e:?}"))
}

async fn count(
    s: &dyn Session,
    obj: &ObjectRef,
    filter: Option<&DataFilter>,
) -> Result<i64, DbError> {
    let c = command(s);
    let cmd = c.data_access().count(obj, filter)?;
    check_read(&cmd)?;
    let opts = ExecOptions {
        max_rows: 1,
        ..ExecOptions::default()
    };
    let mut rec = Rec::default();
    c.execute_command(&cmd, &opts, &mut rec).await?;
    match rec.col("value").as_slice() {
        [Value::Int(n)] => Ok(*n),
        other => panic!("{}: {other:?}", cmd.text),
    }
}

fn filter_error(r: Result<impl std::fmt::Debug, DbError>) -> String {
    match r {
        Err(DbError::Config { field, message }) => {
            assert_eq!(field.as_deref(), Some("filter"), "{message}");
            message
        }
        other => panic!("expected a filter error, got {other:?}"),
    }
}

fn names(objects: &[dnt_core::meta::ObjectSummary]) -> Vec<&str> {
    objects.iter().map(|o| o.name.as_str()).collect()
}

#[tokio::test]
async fn lists_collections_and_views() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let i = s.introspector();
        let dbs = i.list_databases().await.unwrap();
        assert!(dbs.iter().any(|d| d.name == FIXTURE_DB), "{dbs:?}");
        assert!(dbs.iter().all(|d| d.key_count.is_none()));
        let mut sorted = dbs.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(dbs, sorted);

        let scope = Scope {
            database: FIXTURE_DB.into(),
            schema: None,
        };
        let colls = i
            .list_objects(&scope, ObjectKind::Collection)
            .await
            .unwrap();
        assert_eq!(names(&colls), ["orders"]);
        assert!(colls
            .iter()
            .all(|o| o.kind == ObjectKind::Collection && o.row_estimate.is_none()));
        let views = i.list_objects(&scope, ObjectKind::View).await.unwrap();
        assert_eq!(names(&views), ["paid_orders"]);
        assert_eq!(views[0].kind, ObjectKind::View);
        assert!(i
            .list_objects(&scope, ObjectKind::Table)
            .await
            .unwrap()
            .is_empty());
        assert!(i.list_schemas(FIXTURE_DB).await.unwrap().is_empty());
        assert!(i.list_columns(&scope).await.unwrap().is_empty());

        // Definitions: a view's source and pipeline; a collection's options.
        let view: serde_json::Value = serde_json::from_str(
            &i.object_ddl(&object(FIXTURE_DB, "paid_orders", ObjectKind::View))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            view,
            serde_json::json!({"viewOn": "orders", "pipeline": [{"$match": {"status": "paid"}}]})
        );
        let ddl = i.object_ddl(&orders()).await.unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&ddl)
            .unwrap()
            .is_object());
        match i
            .object_ddl(&object(FIXTURE_DB, "missing", ObjectKind::Collection))
            .await
        {
            Err(DbError::Server { code, .. }) => {
                assert_eq!(code.as_deref(), Some("NamespaceNotFound"))
            }
            other => panic!("{other:?}"),
        }
        s.close().await;

        // A validator, and a timeseries collection listed as a collection (with its
        // `system.*` buckets, which the app hides).
        let (s, db) = scratch(t, "lists").await;
        console(
            &*s,
            "db.createCollection('checked', {validator: {$jsonSchema: {required: ['sku']}}})",
        )
        .await;
        console(
            &*s,
            "db.createCollection('ticks', {timeseries: {timeField: 't'}})",
        )
        .await;
        let i = s.introspector();
        let scope = Scope {
            database: db.clone(),
            schema: None,
        };
        let colls = i
            .list_objects(&scope, ObjectKind::Collection)
            .await
            .unwrap();
        let listed = names(&colls);
        assert!(
            listed.contains(&"checked") && listed.contains(&"ticks"),
            "{listed:?}"
        );
        assert!(
            listed.iter().any(|n| n.starts_with("system.")),
            "{listed:?}"
        );
        let ddl = i
            .object_ddl(&object(&db, "checked", ObjectKind::Collection))
            .await
            .unwrap();
        assert!(ddl.contains('\n'), "indented: {ddl}");
        let options: serde_json::Value = serde_json::from_str(&ddl).unwrap();
        assert_eq!(
            options["validator"],
            serde_json::json!({"$jsonSchema": {"required": ["sku"]}})
        );
        console(&*s, "db.dropDatabase()").await;
        s.close().await;
    }
}

#[tokio::test]
async fn table_detail_mixed_and_stable() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let i = s.introspector();
        let first = i.table_detail(&orders()).await.unwrap();
        let second = i.table_detail(&orders()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(command_fingerprint(&first), command_fingerprint(&second));

        // `_id` first, then by first appearance in the sample (largest `_id` first).
        let order: Vec<&str> = first.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            order[..7],
            [
                "_id",
                "status",
                "customer",
                "qty",
                "price",
                "created_at",
                "mixed"
            ]
        );
        let ordinals: Vec<u32> = first.columns.iter().map(|c| c.ordinal).collect();
        assert_eq!(
            ordinals,
            (1..=first.columns.len() as u32).collect::<Vec<_>>()
        );
        let ty = |name: &str| {
            let c = first
                .columns
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no field {name}"));
            assert_eq!(c.nullable, name != "_id", "{name}");
            c.db_type.as_str()
        };
        for (field, want) in [
            ("_id", "mixed(binData|objectId)"),
            ("mixed", "mixed(array|int|null|object|string)"),
            ("status", "string"),
            ("qty", "int"),
            ("total_cents", "long"),
            ("price", "double"),
            ("amount", "decimal"),
            ("gift", "bool"),
            ("note", "null"),
            ("created_at", "date"),
            ("ts", "timestamp"),
            ("pattern", "regex"),
            ("token", "binData"),
            ("uid", "uuid"),
            ("shipping", "object"),
            ("items", "array"),
        ] {
            assert_eq!(ty(field), want, "{field}");
        }
        let pk = first.primary_key.as_ref().unwrap();
        assert_eq!(
            (pk.name.as_str(), pk.primary, pk.unique),
            ("_id_", true, true)
        );
        assert_eq!(pk.columns, ["_id"]);
        assert!(first.indexes.is_empty(), "{:?}", first.indexes);

        // A view: sampled the same way, without indexes.
        let view = i
            .table_detail(&object(FIXTURE_DB, "paid_orders", ObjectKind::View))
            .await
            .unwrap();
        assert_eq!(view.columns[0].name, "_id");
        assert!(view.primary_key.is_none() && view.indexes.is_empty());
        s.close().await;

        // Indexes, an empty collection, and a fingerprint that ignores sampled fields.
        let (s, db) = scratch(t, "detail").await;
        console(&*s, "db.items.createIndex({sku: 1}, {unique: true})").await;
        console(&*s, "db.items.createIndex({at: -1, sku: 1})").await;
        console(&*s, "db.items.insertOne({sku: 'a', at: new Date()})").await;
        console(&*s, "db.createCollection('empty')").await;
        let i = s.introspector();
        let items = object(&db, "items", ObjectKind::Collection);
        let detail = i.table_detail(&items).await.unwrap();
        let idx: Vec<(&str, bool, Vec<String>)> = detail
            .indexes
            .iter()
            .map(|x| (x.name.as_str(), x.unique, x.columns.clone()))
            .collect();
        assert_eq!(
            idx,
            [
                ("at_-1_sku_1", false, vec!["at".to_string(), "sku".into()]),
                ("sku_1", true, vec!["sku".to_string()]),
            ]
        );
        assert!(detail.indexes[0].parts[0].descending);
        console(&*s, "db.items.insertOne({sku: 'b', extra: 1})").await;
        let grown = i.table_detail(&items).await.unwrap();
        assert_ne!(grown.columns, detail.columns);
        assert_eq!(command_fingerprint(&grown), command_fingerprint(&detail));
        let empty = i
            .table_detail(&object(&db, "empty", ObjectKind::Collection))
            .await
            .unwrap();
        let cols: Vec<(&str, &str, bool)> = empty
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.db_type.as_str(), c.nullable))
            .collect();
        assert_eq!(cols, [("_id", "objectId", false)]);
        console(&*s, "db.dropDatabase()").await;
        s.close().await;
    }
}

#[tokio::test]
async fn page_with_filters() {
    require_db!();
    for t in Target::mongo() {
        let s = connect(&cfg(t, "dnt", FIXTURE_DB)).await;
        let s = &*s;
        // `_id` order: the binary `_id` sorts before every ObjectId.
        assert_eq!(
            all(s, None).await.customers(),
            ["Carol", "Alice", "Bob", "Dave", "Eve"]
        );
        // Values are coerced by the sampled type: int, double, date; text stays text.
        assert_eq!(
            all(s, where_(cond("qty", FilterOp::Ge, &["3"])))
                .await
                .customers(),
            ["Alice", "Dave", "Eve"]
        );
        assert_eq!(
            all(s, where_(cond("price", FilterOp::Lt, &["6"])))
                .await
                .customers(),
            ["Bob", "Dave"]
        );
        assert_eq!(
            all(s, where_(cond("created_at", FilterOp::Ge, &["2024-11-07"])))
                .await
                .customers(),
            ["Carol", "Dave", "Eve"]
        );
        assert_eq!(
            all(s, where_(cond("qty", FilterOp::Between, &["2", "4"])))
                .await
                .customers(),
            ["Carol", "Alice", "Eve"]
        );
        assert_eq!(
            all(
                s,
                where_(cond("status", FilterOp::In, &["pending", "shipped"]))
            )
            .await
            .customers(),
            ["Bob", "Dave"]
        );
        assert_eq!(
            all(s, where_(cond("customer", FilterOp::Like, &["_ve"])))
                .await
                .customers(),
            ["Eve"]
        );
        assert_eq!(
            all(s, where_(cond("customer", FilterOp::NotLike, &["%e%"])))
                .await
                .customers(),
            ["Carol", "Bob"]
        );
        assert_eq!(
            all(s, where_(cond("customer", FilterOp::Contains, &["o"])))
                .await
                .customers(),
            ["Carol", "Bob"]
        );
        assert_eq!(
            all(s, where_(cond("customer", FilterOp::EndsWith, &["e"])))
                .await
                .customers(),
            ["Alice", "Dave", "Eve"]
        );
        // A missing field is null; `note` is null or missing everywhere.
        assert_eq!(
            all(s, where_(cond("note", FilterOp::IsNull, &[])))
                .await
                .rows
                .len(),
            5
        );
        assert_eq!(
            all(s, where_(cond("total_cents", FilterOp::IsNotNull, &[])))
                .await
                .customers(),
            ["Alice"]
        );
        assert_eq!(
            all(
                s,
                conditions(
                    FilterMode::Or,
                    vec![
                        cond("qty", FilterOp::Eq, &["1"]),
                        cond("customer", FilterOp::Eq, &["Eve"]),
                    ]
                )
            )
            .await
            .customers(),
            ["Bob", "Eve"]
        );
        // A mixed field keeps the text: the int 42 is not "42".
        assert!(all(s, where_(cond("mixed", FilterOp::Eq, &["42"])))
            .await
            .rows
            .is_empty());
        assert_eq!(
            all(s, where_(cond("mixed", FilterOp::Eq, &["forty-two"])))
                .await
                .customers(),
            ["Bob"]
        );
        // A value the type cannot take is an error, and nothing is read.
        assert_eq!(
            filter_error(
                page(
                    s,
                    &orders(),
                    vec![],
                    where_(cond("qty", FilterOp::Eq, &["x"])),
                    0,
                    10
                )
                .await
            ),
            "Value \"x\" is not a valid int"
        );

        // Raw filters: mongosh and Extended JSON, the binary `_id` too.
        assert_eq!(
            all(s, raw("{status: 'paid', qty: {$gt: 2}}"))
                .await
                .customers(),
            ["Alice", "Eve"]
        );
        assert_eq!(
            all(s, raw(r#"{"_id": {"$oid": "65a1b2c3d4e5f60718293a02"}}"#))
                .await
                .customers(),
            ["Bob"]
        );
        assert_eq!(
            all(s, raw("{_id: ObjectId('65a1b2c3d4e5f60718293a04')}"))
                .await
                .customers(),
            ["Dave"]
        );
        assert_eq!(
            all(s, raw("{_id: BinData(0, 'AQID')}")).await.customers(),
            ["Carol"]
        );
        assert_eq!(
            filter_error(
                page(
                    s,
                    &orders(),
                    vec![],
                    raw("{$where: 'sleep(10) || true'}"),
                    0,
                    10
                )
                .await
            ),
            "Server-side JavaScript is not allowed in a filter"
        );

        // Sort, with `_id` as the last key; skip and limit.
        let by_status = page(
            s,
            &orders(),
            vec![SortSpec {
                column: "status".into(),
                dir: SortDir::Asc,
            }],
            None,
            0,
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            by_status.customers(),
            ["Carol", "Alice", "Eve", "Bob", "Dave"]
        );
        let desc = page(
            s,
            &orders(),
            vec![SortSpec {
                column: "qty".into(),
                dir: SortDir::Desc,
            }],
            None,
            1,
            2,
        )
        .await
        .unwrap();
        assert_eq!(desc.customers(), ["Eve", "Alice"]);
        let second = page(s, &orders(), vec![], None, 2, 2).await.unwrap();
        assert_eq!(second.customers(), ["Bob", "Dave"]);
        assert_eq!(second.cols[0], "_id");
    }
}

#[tokio::test]
async fn count_matches_page() {
    require_db!();
    for t in Target::mongo() {
        let mut c = cfg(t, "dnt", FIXTURE_DB);
        // Both reads pass the read-only allowlist.
        c.read_only = true;
        let s = connect(&c).await;
        let s = &*s;
        let filters = [
            None,
            where_(cond("qty", FilterOp::Gt, &["1"])),
            where_(cond("created_at", FilterOp::Lt, &["2024-11-08T00:00:00Z"])),
            where_(cond("customer", FilterOp::StartsWith, &["D"])),
            where_(cond("note", FilterOp::IsNull, &[])),
            conditions(
                FilterMode::And,
                vec![
                    cond("status", FilterOp::Eq, &["paid"]),
                    cond("qty", FilterOp::Ne, &["2"]),
                ],
            ),
            raw("{status: {$in: ['paid', 'pending']}}"),
            raw("{}"),
        ];
        for f in filters {
            let rows = all(s, f.clone()).await.rows.len() as i64;
            let n = count(s, &orders(), f.as_ref()).await.unwrap();
            assert_eq!(n, rows, "{f:?}");
        }
        assert_eq!(count(s, &orders(), None).await.unwrap(), 5);
        assert_eq!(
            count(
                s,
                &object(FIXTURE_DB, "paid_orders", ObjectKind::View),
                None
            )
            .await
            .unwrap(),
            3
        );
        assert_eq!(
            filter_error(
                count(
                    s,
                    &orders(),
                    where_(cond("price", FilterOp::Eq, &["cheap"])).as_ref()
                )
                .await
            ),
            "Value \"cheap\" is not a valid double"
        );
        assert_eq!(
            filter_error(count(s, &orders(), raw("[1]").as_ref()).await),
            "The filter must be a JSON object"
        );
    }
}
