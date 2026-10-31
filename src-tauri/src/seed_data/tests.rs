//! The data seeder against the import tests' fake server: the target read, the request
//! checks, the foreign-key key reads, the plans and the job.
use super::keys::{fk_key_sql, read_fk_keys, KeyId, SeedKeyCache};
use super::*;
use crate::import_data::tests::{column, fixture_detail, index, FakeDriver, Fx};
use dnt_core::config::EnvTag;
use dnt_core::driver::Driver;
use dnt_core::meta::{ForeignKeyDetail, ObjectKind, ObjectRef, TableDetail, TableOptions};
use dnt_core::server::{DbKind, SqlKind};
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_transfer::seed::{FakeKind, Generator};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// `orders (id int AI PK, customer_id int NOT NULL FK→customers(id), email varchar(120) NOT
/// NULL UNIQUE, status enum('new','paid','shipped') NOT NULL, total decimal(12,2) NULL,
/// note text NULL)`; PostgreSQL: `id` from a sequence, `status` varchar(10).
pub(crate) fn orders_detail(kind: DbKind) -> TableDetail {
    let pg = kind == DbKind::Postgres;
    let mut id = column("id", 1, if pg { "integer" } else { "int" }, false);
    if pg {
        id.default = Some("nextval('orders_id_seq'::regclass)".into());
    } else {
        id.auto_increment = true;
    }
    let mut status = column(
        "status",
        4,
        if pg {
            "character varying(10)"
        } else {
            "enum('new','paid','shipped')"
        },
        false,
    );
    if !pg {
        status.enum_values = vec!["new".into(), "paid".into(), "shipped".into()];
    }
    let pk = index(if pg { "orders_pkey" } else { "PRIMARY" }, "id", true);
    TableDetail {
        object: ObjectRef {
            database: "shop".into(),
            schema: pg.then(|| "public".into()),
            name: "orders".into(),
            kind: ObjectKind::Table,
        },
        columns: vec![
            id,
            column("customer_id", 2, if pg { "integer" } else { "int" }, false),
            column(
                "email",
                3,
                if pg {
                    "character varying(120)"
                } else {
                    "varchar(120)"
                },
                false,
            ),
            status,
            column("total", 5, "decimal(12,2)", true),
            column("note", 6, "text", true),
        ],
        primary_key: Some(pk.clone()),
        indexes: vec![pk, index("orders_email_key", "email", false)],
        foreign_keys: vec![ForeignKeyDetail {
            name: "fk_orders_customer".into(),
            columns: vec!["customer_id".into()],
            ref_schema: pg.then(|| "public".into()),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            ..ForeignKeyDetail::default()
        }],
        checks: vec![],
        triggers: vec![],
        options: TableOptions {
            engine: (!pg).then(|| "InnoDB".into()),
            ..TableOptions::default()
        },
        row_estimate: None,
        size_bytes: None,
        comment: None,
    }
}

pub(crate) fn int_meta(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        db_type: "int".into(),
        logical: LogicalType::Integer,
        nullable: Some(false),
        origin_table: None,
        origin_column: None,
    }
}

/// Customers 1, 2 and 3 for the key read.
pub(crate) fn with_customers(fx: &Fx) {
    fx.fake.rows_for(
        "FROM `shop`.`customers`",
        vec![int_meta("id")],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
        ],
    );
    fx.fake.rows_for(
        "FROM \"public\".\"customers\"",
        vec![int_meta("id")],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
        ],
    );
}

pub(crate) async fn orders(kind: DbKind, env: Option<EnvTag>, read_only: bool) -> Fx {
    let fx = fixture_detail(kind, orders_detail(kind), env, read_only, None).await;
    with_customers(&fx);
    fx
}

fn table(fx: &Fx) -> ObjectRef {
    fx.fake.detail.lock().unwrap().object.clone()
}

#[tokio::test]
async fn target_infers_generators() {
    let fx = orders(DbKind::MySql, Some(EnvTag::Staging), false).await;
    let t = fx.sm.seed_target(fx.conn, table(&fx)).await.unwrap();
    assert_eq!(t.env, Some(EnvTag::Staging));
    assert!(!t.read_only);
    assert!(t.transactional);
    let by = |n: &str| t.columns.iter().find(|c| c.column.name == n).unwrap();
    assert_eq!(by("id").plan.generator, Generator::Skip);
    assert!(by("id").primary);
    assert!(!by("id").locked);
    let fk = by("customer_id");
    assert_eq!(
        fk.plan.generator,
        Generator::ForeignKey {
            constraint: "fk_orders_customer".into()
        }
    );
    assert_eq!(fk.foreign_key.as_ref().unwrap().ref_table, "customers");
    assert_eq!(
        by("email").plan.generator,
        Generator::Fake {
            fake: FakeKind::Email
        }
    );
    assert!(by("email").plan.unique && by("email").unique_index);
    assert_eq!(
        by("status").plan.generator,
        Generator::Pick {
            values: vec!["new".into(), "paid".into(), "shipped".into()]
        }
    );
    assert_eq!(
        by("total").plan.generator,
        Generator::DecimalRange {
            min: 1.0,
            max: 1000.0,
            scale: 2
        }
    );
    assert_eq!(
        by("note").plan.generator,
        Generator::Fake {
            fake: FakeKind::Paragraph
        }
    );
}

fn request(fx: &Fx, t: &SeedTarget) -> SeedRequest {
    SeedRequest {
        connection_id: fx.conn,
        table: table(fx),
        rows: 3,
        batch_rows: 1000,
        existing: ExistingRows::Keep,
        seed: 7,
        columns: t.columns.iter().map(|c| c.plan.clone()).collect(),
    }
}

#[tokio::test]
async fn normalise_rules() {
    let fx = orders(DbKind::MySql, None, false).await;
    let t = fx.sm.seed_target(fx.conn, table(&fx)).await.unwrap();
    let base = request(&fx, &t);
    for rows in [0, 1_000_001] {
        let err = normalise(
            SeedRequest {
                rows,
                ..base.clone()
            },
            SqlKind::MySql,
        )
        .unwrap_err();
        assert!(
            matches!(&err, DbError::Config { field: Some(f), message } if f == "rows" && message == "Choose between 1 and 1,000,000 rows."),
            "{err:?}"
        );
    }
    let mut with_schema = base.clone();
    with_schema.table.schema = Some("x".into());
    let err = normalise(with_schema, SqlKind::MySql).unwrap_err();
    assert!(err
        .to_string()
        .contains("MySQL has no schemas inside a database."));
    let n = normalise(
        SeedRequest {
            batch_rows: 0,
            ..base.clone()
        },
        SqlKind::MySql,
    )
    .unwrap();
    assert_eq!(n.batch_rows, 1000);
    let n = normalise(
        SeedRequest {
            batch_rows: 500_000,
            ..base
        },
        SqlKind::MySql,
    )
    .unwrap();
    assert_eq!(n.batch_rows, 100_000);
}

fn fk(name: &str, cols: &[&str], ref_schema: Option<&str>, ref_table: &str) -> ForeignKeyDetail {
    ForeignKeyDetail {
        name: name.into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        ref_schema: ref_schema.map(str::to_string),
        ref_table: ref_table.into(),
        ref_columns: cols.iter().map(|c| c.to_string()).collect(),
        ..ForeignKeyDetail::default()
    }
}

fn obj(database: &str, schema: Option<&str>) -> ObjectRef {
    ObjectRef {
        database: database.into(),
        schema: schema.map(str::to_string),
        name: "child".into(),
        kind: ObjectKind::Table,
    }
}

#[test]
fn fk_key_query_quotes_identifiers() {
    let f = fk("f", &["id"], Some("shop"), "we`ird");
    assert_eq!(
        fk_key_sql(SqlKind::MySql, &obj("shop", None), &f),
        "SELECT DISTINCT `id` FROM `shop`.`we``ird` WHERE `id` IS NOT NULL ORDER BY `id` LIMIT 10000"
    );
    // MySQL without ref_schema: the child's database.
    let f = fk("f", &["id"], None, "parent");
    assert_eq!(
        fk_key_sql(SqlKind::MySql, &obj("app", None), &f),
        "SELECT DISTINCT `id` FROM `app`.`parent` WHERE `id` IS NOT NULL ORDER BY `id` LIMIT 10000"
    );
}

#[tokio::test]
async fn fk_keys_skip_null_tuples_and_use_ref_schema() {
    let f = fk("f", &["a", "b"], Some("other"), "parent");
    let sql = fk_key_sql(SqlKind::Postgres, &obj("shop", Some("public")), &f);
    assert_eq!(
        sql,
        "SELECT DISTINCT \"a\", \"b\" FROM \"other\".\"parent\" WHERE \"a\" IS NOT NULL AND \"b\" IS NOT NULL ORDER BY \"a\", \"b\" LIMIT 10000"
    );
    let fx = orders(DbKind::Postgres, None, false).await;
    fx.fake.rows_for(
        "FROM \"other\".\"parent\"",
        vec![int_meta("a"), int_meta("b")],
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
        ],
    );
    let session = FakeDriver(Arc::clone(&fx.fake))
        .connect(&dnt_core::test_util::sample_resolved(None))
        .await
        .unwrap();
    let keys = read_fk_keys(session.sql().unwrap(), &obj("shop", Some("public")), &f)
        .await
        .unwrap();
    let shown: Vec<Vec<Value>> = keys
        .iter()
        .map(|t| t.iter().map(|v| v.to_display()).collect())
        .collect();
    assert_eq!(
        shown,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)]
        ]
    );
}

fn one_key() -> Vec<Vec<dnt_core::value::FullValue>> {
    vec![vec![dnt_core::value::FullValue::try_from_value(
        Value::Int(1),
    )
    .unwrap()]]
}

#[tokio::test]
async fn key_cache_reuses_within_ttl() {
    let cache = SeedKeyCache::default();
    let reads = AtomicU32::new(0);
    let id = KeyId {
        conn: uuid::Uuid::nil(),
        generation: 1,
        database: "shop".into(),
        sql: "SELECT 1".into(),
    };
    for _ in 0..2 {
        let keys = cache
            .get_or_read(id.clone(), async {
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(one_key())
            })
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
    }
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    // Another generation (a reconnected tunnel) reads again.
    let other = KeyId {
        generation: 2,
        ..id.clone()
    };
    cache
        .get_or_read(other, async {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(one_key())
        })
        .await
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    cache.drop_conn(uuid::Uuid::nil());
    cache
        .get_or_read(id, async {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(one_key())
        })
        .await
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 3);
}

// ---------------------------------------------------------------------------------------
// Preview, plans and the job
// ---------------------------------------------------------------------------------------

use crate::import_data::tests::{ok_confirm, shape};
use crate::import_data::{ConfirmLevel, ImportConfirm};
use crate::jobs::{JobDetail, JobState};
use dnt_store::history::{HistoryFilter, HistoryOrigin};
use std::time::Duration;

async fn base_request(fx: &Fx, rows: u32) -> SeedRequest {
    let t = fx.sm.seed_target(fx.conn, table(fx)).await.unwrap();
    SeedRequest {
        rows,
        ..request(fx, &t)
    }
}

async fn wait(fx: &Fx, id: uuid::Uuid) -> JobDetail {
    assert!(fx.sm.jobs.wait(id, Duration::from_secs(20)).await);
    fx.sm.jobs.detail(id).unwrap()
}

/// Prepare + start with every confirmation given; the finished job and the statements the
/// start and the job sent.
async fn run(fx: &Fx, req: SeedRequest) -> (SeedPreview, JobDetail, Vec<String>) {
    let preview = fx.sm.seed_prepare(req).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .seed_start(preview.plan_id, ok_confirm(Some("orders")))
        .await
        .unwrap();
    let d = wait(fx, id).await;
    (preview, d, fx.fake.since(mark))
}

fn set_read_only(fx: &Fx) {
    let mut cfg = fx.repo.get(fx.conn).unwrap();
    cfg.read_only = true;
    fx.repo
        .update(
            cfg,
            dnt_store::repo::SecretUpdate {
                db_password: dnt_store::repo::SecretChange::Keep,
                ssh_password: dnt_store::repo::SecretChange::Keep,
                ssh_key_passphrase: dnt_store::repo::SecretChange::Keep,
            },
        )
        .unwrap();
}

#[tokio::test]
async fn pg_job_statements() {
    let fx = orders(DbKind::Postgres, None, false).await;
    let req = base_request(&fx, 3).await;
    let (preview, d, sent) = run(&fx, req).await;
    assert!(preview.transactional);
    assert_eq!(preview.confirm, ConfirmLevel::None);
    assert_eq!(preview.batches, 1);
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        d.job.summary.as_deref(),
        Some("Inserted 3 rows into orders in 1 batch")
    );
    assert_eq!(d.job.title, "Generate data for orders");
    let s = shape(&sent);
    assert_eq!(s[0], "SET standard_conforming_strings = on");
    assert_eq!(s[1], "BEGIN");
    assert!(
        s[2].starts_with("SELECT DISTINCT \"id\" FROM \"public\".\"customers\""),
        "{s:?}"
    );
    assert_eq!(&s[3..], ["INSERT x3", "COMMIT"]);
    let insert = sent.iter().find(|x| x.starts_with("INSERT")).unwrap();
    assert!(
        insert.starts_with(
            "INSERT INTO \"public\".\"orders\" (\"customer_id\", \"email\", \"status\", \"total\", \"note\") VALUES "
        ),
        "{insert}"
    );
}

#[tokio::test]
async fn mysql_truncate_is_irreversible_first() {
    let fx = orders(DbKind::MySql, None, false).await;
    let mut req = base_request(&fx, 2).await;
    req.existing = ExistingRows::Truncate;
    let preview = fx.sm.seed_prepare(req).await.unwrap();
    assert_eq!(
        preview.confirm,
        ConfirmLevel::TypeName {
            name: "orders".into()
        }
    );
    let pre = preview.pre_step.clone().unwrap();
    assert!(pre.irreversible);
    assert_eq!(pre.sql, "TRUNCATE `shop`.`orders`");
    assert!(preview
        .warnings
        .iter()
        .any(|w| w == "TRUNCATE commits at once and resets AUTO_INCREMENT."));
    let no_ack = ImportConfirm {
        irreversible_ack: false,
        ..ok_confirm(Some("orders"))
    };
    let err = fx.sm.seed_start(preview.plan_id, no_ack).await.unwrap_err();
    assert!(
        matches!(err, DbError::ConfirmationRequired { .. }),
        "{err:?}"
    );

    let mut req = base_request(&fx, 2).await;
    req.existing = ExistingRows::Truncate;
    let (_, d, sent) = run(&fx, req).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    let s = shape(&sent);
    assert_eq!(s[0], "TRUNCATE `shop`.`orders`");
    assert_eq!(s[1], "START TRANSACTION");
    assert_eq!(s.last().unwrap(), "COMMIT");
}

#[tokio::test]
async fn delete_first_needs_the_name_everywhere() {
    let fx = orders(DbKind::Postgres, None, false).await;
    let mut req = base_request(&fx, 2).await;
    req.existing = ExistingRows::Delete;
    let preview = fx.sm.seed_prepare(req).await.unwrap();
    assert_eq!(
        preview.confirm,
        ConfirmLevel::TypeName {
            name: "orders".into()
        }
    );
    assert!(!preview.pre_step.as_ref().unwrap().irreversible);
    let err = fx
        .sm
        .seed_start(preview.plan_id, ok_confirm(None))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::ConfirmationRequired { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn mysql_myisam_autocommit() {
    let mut detail = orders_detail(DbKind::MySql);
    detail.options.engine = Some("MyISAM".into());
    let fx = fixture_detail(DbKind::MySql, detail, None, false, None).await;
    with_customers(&fx);
    let req = base_request(&fx, 2).await;
    let (preview, d, sent) = run(&fx, req).await;
    assert!(!preview.transactional);
    assert!(preview
        .warnings
        .iter()
        .any(|w| w == "Not transactional: batches already written stay if the job stops."));
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    let s = shape(&sent);
    assert!(
        !s.iter().any(|x| x == "START TRANSACTION" || x == "COMMIT"),
        "{s:?}"
    );
}

#[tokio::test]
async fn read_only_blocked() {
    let fx = orders(DbKind::MySql, None, true).await;
    let req = base_request(&fx, 2).await;
    let err = fx.sm.seed_prepare(req).await.unwrap_err();
    assert!(matches!(err, DbError::Blocked { .. }), "{err:?}");

    // Switched to read-only between prepare and start: refused, nothing written.
    let fx = orders(DbKind::MySql, None, false).await;
    let req = base_request(&fx, 2).await;
    let preview = fx.sm.seed_prepare(req).await.unwrap();
    set_read_only(&fx);
    let mark = fx.fake.mark();
    let err = fx
        .sm
        .seed_start(preview.plan_id, ok_confirm(None))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Blocked { .. }), "{err:?}");
    assert!(!fx.fake.since(mark).iter().any(|s| s.starts_with("INSERT")));
}

#[tokio::test]
async fn production_needs_typed_name() {
    let fx = orders(DbKind::MySql, Some(EnvTag::Production), false).await;
    let req = base_request(&fx, 2).await;
    let preview = fx.sm.seed_prepare(req.clone()).await.unwrap();
    assert_eq!(
        preview.confirm,
        ConfirmLevel::TypeName {
            name: "orders".into()
        }
    );
    let err = fx
        .sm
        .seed_start(preview.plan_id, ok_confirm(Some("order")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DbError::ConfirmationRequired { .. }),
        "{err:?}"
    );
    let staging = orders(DbKind::MySql, Some(EnvTag::Staging), false).await;
    let req = base_request(&staging, 2).await;
    let preview = staging.sm.seed_prepare(req).await.unwrap();
    assert_eq!(preview.confirm, ConfirmLevel::Click);
}

#[tokio::test]
async fn stop_rolls_back_and_ends_generator() {
    let fx = orders(DbKind::Postgres, None, false).await;
    *fx.fake.delay.lock().unwrap() = Some(("INSERT".into(), Duration::from_millis(300)));
    let mut req = base_request(&fx, 200_000).await;
    req.batch_rows = 100;
    let preview = fx.sm.seed_prepare(req).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .seed_start(preview.plan_id, ok_confirm(None))
        .await
        .unwrap();
    for _ in 0..200 {
        if fx.fake.since(mark).iter().any(|s| s.starts_with("INSERT")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(fx.sm.jobs.cancel(id).await);
    let d = wait(&fx, id).await;
    assert_eq!(d.job.state, JobState::Cancelled, "{:?}", d.job);
    let s = shape(&fx.fake.since(mark));
    assert_eq!(s.last().unwrap(), "ROLLBACK", "{s:?}");
    assert!(!s.iter().any(|x| x == "COMMIT"));
}

#[tokio::test]
async fn table_changed_between_prepare_and_start() {
    let fx = orders(DbKind::MySql, None, false).await;
    let req = base_request(&fx, 2).await;
    let preview = fx.sm.seed_prepare(req).await.unwrap();
    fx.fake.detail.lock().unwrap().columns[5].nullable = false;
    let err = fx
        .sm
        .seed_start(preview.plan_id, ok_confirm(None))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::PlanStale { .. }), "{err:?}");
}

/// The tuples of a multi-row `INSERT` (text between the outer parentheses).
fn tuples(insert: &str) -> Vec<String> {
    let values = &insert[insert.find(" VALUES ").unwrap() + 8..];
    let inner = values.trim().trim_start_matches('(').trim_end_matches(')');
    inner.split("), (").map(str::to_string).collect()
}

#[tokio::test]
async fn preview_equals_first_rows() {
    let fx = orders(DbKind::MySql, None, false).await;
    let req = base_request(&fx, 25).await;
    let sample = fx.sm.seed_preview(req.clone()).await.unwrap();
    assert!(sample.issues.is_empty(), "{:?}", sample.issues);
    assert_eq!(sample.rows.len(), 10);
    assert_eq!(
        sample.columns,
        vec!["customer_id", "email", "status", "total", "note"]
    );
    assert_eq!(
        sample.fk_keys,
        vec![SeedKeyCount {
            constraint: "fk_orders_customer".into(),
            count: 3
        }]
    );
    let (_, d, sent) = run(&fx, req).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    let insert = sent.iter().find(|s| s.starts_with("INSERT")).unwrap();
    let preview_sql = sample.sql.unwrap();
    assert_eq!(tuples(&preview_sql), tuples(insert)[..10].to_vec());
}

#[tokio::test]
async fn empty_parent_not_null_is_an_issue() {
    let fx = fixture_detail(
        DbKind::MySql,
        orders_detail(DbKind::MySql),
        None,
        false,
        None,
    )
    .await;
    fx.fake
        .rows_for("FROM `shop`.`customers`", vec![int_meta("id")], vec![]);
    let req = base_request(&fx, 2).await;
    let sample = fx.sm.seed_preview(req.clone()).await.unwrap();
    assert_eq!(
        sample.issues,
        vec![dnt_transfer::seed::ColumnIssue {
            column: "customer_id".into(),
            message: "customers has no rows to reference.".into()
        }]
    );
    let err = fx.sm.seed_prepare(req).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("customer_id: customers has no rows to reference."),
        "{err}"
    );
}

#[tokio::test]
async fn job_quotes_reserved_and_quoted_names() {
    let mut detail = TableDetail {
        foreign_keys: vec![],
        indexes: vec![],
        primary_key: None,
        ..orders_detail(DbKind::Postgres)
    };
    detail.object.name = "order".into();
    detail.columns = vec![column("we\"ird", 1, "integer", false)];
    let fx = fixture_detail(DbKind::Postgres, detail, None, false, None).await;
    let req = base_request(&fx, 2).await;
    let (preview, d, sent) = run(&fx, req).await;
    assert_eq!(
        preview.insert_head,
        "INSERT INTO \"public\".\"order\" (\"we\"\"ird\") VALUES "
    );
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert!(sent
        .iter()
        .any(|s| s.starts_with("INSERT INTO \"public\".\"order\" (\"we\"\"ird\") VALUES ")));
}

#[tokio::test]
async fn history_entry_has_no_values() {
    let fx = orders(DbKind::MySql, None, false).await;
    let req = base_request(&fx, 3).await;
    let sample = fx.sm.seed_preview(req.clone()).await.unwrap();
    let email = match &sample.rows[0][1] {
        Value::Text(s) => s.clone(),
        other => panic!("{other:?}"),
    };
    let (_, d, _) = run(&fx, req).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    let h = fx
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(fx.conn),
            search: None,
            origin: Some(HistoryOrigin::Seed),
            limit: 10,
        })
        .unwrap();
    assert_eq!(h.len(), 1);
    assert!(
        h[0].sql_text
            .starts_with("-- Generate 3 rows into orders: seed 7, keep"),
        "{}",
        h[0].sql_text
    );
    assert!(!h[0].sql_text.contains(&email));
}

// ---------------------------------------------------------------------------------------
// Final review fixes
// ---------------------------------------------------------------------------------------

/// PostgreSQL relations leave the database out: the cache key must keep it (review I1).
#[test]
fn key_ids_differ_by_database() {
    let f = fk("f", &["id"], Some("public"), "customers");
    let a = super::keys::key_id(
        uuid::Uuid::nil(),
        1,
        SqlKind::Postgres,
        &obj("db_a", Some("public")),
        &f,
    );
    let b = super::keys::key_id(
        uuid::Uuid::nil(),
        1,
        SqlKind::Postgres,
        &obj("db_b", Some("public")),
        &f,
    );
    assert_eq!(a.sql, b.sql);
    assert_ne!(a, b);
}

/// An empty parent is read again: it may be filled by now (review I5).
#[tokio::test]
async fn empty_key_sets_are_not_cached() {
    let cache = SeedKeyCache::default();
    let reads = AtomicU32::new(0);
    let id = KeyId {
        conn: uuid::Uuid::nil(),
        generation: 1,
        database: "shop".into(),
        sql: "SELECT 1".into(),
    };
    for _ in 0..2 {
        cache
            .get_or_read(id.clone(), async {
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            })
            .await
            .unwrap();
    }
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}

/// The engine-specific review warnings: MySQL's trigger note and each engine's TRUNCATE note.
#[tokio::test]
async fn prepare_warnings_by_kind() {
    for kind in [DbKind::MySql, DbKind::Postgres] {
        let mut detail = orders_detail(kind);
        detail.triggers = vec![dnt_core::meta::TriggerDetail {
            name: "orders_audit".into(),
            timing: "AFTER".into(),
            event: "INSERT".into(),
        }];
        let fx = fixture_detail(kind, detail, None, false, None).await;
        with_customers(&fx);
        let mut req = base_request(&fx, 2).await;
        req.existing = ExistingRows::Truncate;
        let warnings = fx.sm.seed_prepare(req).await.unwrap().warnings;
        let triggers = "The table has triggers: the job runs without one transaction (each batch \
                        commits on its own).";
        let has = |w: &str| warnings.iter().any(|x| x == w);
        match kind.require_sql().unwrap() {
            SqlKind::MySql => {
                assert!(has(triggers), "{warnings:?}");
                assert!(has("TRUNCATE commits at once and resets AUTO_INCREMENT."));
            }
            SqlKind::Postgres => {
                assert!(!has(triggers), "{warnings:?}");
                assert!(has(
                    "TRUNCATE also empties the tables that inherit from this one and fires ON \
                     TRUNCATE triggers."
                ));
            }
        }
    }
}

/// A job that generates no column: PostgreSQL refuses it, MySQL inserts default rows.
#[tokio::test]
async fn no_generated_column_by_kind() {
    for kind in [DbKind::MySql, DbKind::Postgres] {
        let mut detail = orders_detail(kind);
        detail
            .columns
            .retain(|c| c.name == "id" || c.name == "note");
        detail.indexes.retain(|ix| ix.primary);
        detail.foreign_keys.clear();
        let fx = fixture_detail(kind, detail, None, false, None).await;
        let mut req = base_request(&fx, 2).await;
        for c in &mut req.columns {
            c.generator = Generator::Skip;
        }
        let result = fx.sm.seed_prepare(req).await;
        match kind.require_sql().unwrap() {
            SqlKind::MySql => assert!(result.is_ok(), "{result:?}"),
            SqlKind::Postgres => assert!(
                matches!(&result, Err(DbError::Config { field: Some(f), message }) if f == "columns" && message == "Choose a generator for at least one column."),
                "{result:?}"
            ),
        }
    }
}
