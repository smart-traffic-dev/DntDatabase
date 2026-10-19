use dnt_core::driver::Introspector;
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope, TableDetail};
use dnt_driver_mysql::{MySqlDriver, MySqlSession};
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use std::time::{Duration, Instant};

const BIG_DB: &str = "dnt_big";
const BIG_TABLES: usize = 5000;

/// Connects without a default database: introspection must qualify every name itself.
async fn connect(t: Target) -> MySqlSession {
    MySqlDriver
        .connect_session(&t.resolved(None))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

fn is_maria(t: Target) -> bool {
    matches!(t, Target::MariaDb106 | Target::MariaDb114)
}

fn fixture() -> Scope {
    Scope {
        database: FIXTURE_DB.into(),
        schema: None,
    }
}

fn obj(name: &str, kind: ObjectKind) -> ObjectRef {
    ObjectRef {
        database: FIXTURE_DB.into(),
        schema: None,
        name: name.into(),
        kind,
    }
}

async fn names(s: &MySqlSession, scope: &Scope, kind: ObjectKind) -> Vec<String> {
    s.list_objects(scope, kind)
        .await
        .unwrap_or_else(|e| panic!("{kind:?}: {e}"))
        .into_iter()
        .map(|o| {
            assert_eq!(o.kind, kind, "{}", o.name);
            o.name
        })
        .collect()
}

async fn detail(s: &MySqlSession, t: Target, table: &str) -> TableDetail {
    s.table_detail(&obj(table, ObjectKind::Table))
        .await
        .unwrap_or_else(|e| panic!("{t:?} {table}: {e}"))
}

fn assert_sorted(t: Target, kind: ObjectKind, v: &[String]) {
    let mut sorted = v.to_vec();
    sorted.sort();
    assert_eq!(v, sorted, "{t:?} {kind:?} not in stable order");
}

#[tokio::test]
async fn lists_fixture_objects() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let dbs = s.list_databases().await.unwrap();
        assert!(dbs.iter().any(|d| d.name == FIXTURE_DB), "{t:?}: {dbs:?}");
        let schemas = s.list_schemas(FIXTURE_DB).await.unwrap();
        assert_eq!(schemas.len(), 1, "{t:?}");
        assert_eq!(schemas[0].name, FIXTURE_DB);

        let scope = fixture();
        let tables = names(&s, &scope, ObjectKind::Table).await;
        for want in [
            "customers",
            "orders",
            "đơn_hàng",
            "weird_names",
            "all_types",
        ] {
            assert!(
                tables.iter().any(|n| n == want),
                "{t:?}: {want} in {tables:?}"
            );
        }
        assert!(!tables.iter().any(|n| n == "v_customer_orders"), "{t:?}");
        assert_sorted(t, ObjectKind::Table, &tables);

        let summaries = s.list_objects(&scope, ObjectKind::Table).await.unwrap();
        let orders = summaries.iter().find(|o| o.name == "orders").unwrap();
        assert!(orders.row_estimate.is_some(), "{t:?}: {orders:?}");
        assert_eq!(orders.comment, None, "{t:?}");

        let views = s.list_objects(&scope, ObjectKind::View).await.unwrap();
        assert_eq!(
            views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["v_customer_orders"],
            "{t:?}"
        );
        assert_eq!((views[0].row_estimate, &views[0].comment), (None, &None));

        assert_eq!(names(&s, &scope, ObjectKind::Function).await, ["f_add"]);
        assert_eq!(names(&s, &scope, ObjectKind::Procedure).await, ["p_noop"]);
        assert_eq!(
            names(&s, &scope, ObjectKind::Trigger).await,
            ["trg_orders_bi"]
        );
        assert_eq!(names(&s, &scope, ObjectKind::Event).await, ["ev_cleanup"]);

        let sequences = names(&s, &scope, ObjectKind::Sequence).await;
        if is_maria(t) {
            assert_eq!(sequences, ["seq_inv"], "{t:?}");
            assert!(!tables.iter().any(|n| n == "seq_inv"), "{t:?}");
            assert!(tables.iter().any(|n| n == "versioned_items"), "{t:?}");
        } else {
            assert!(sequences.is_empty(), "{t:?}: {sequences:?}");
        }
        for kind in [ObjectKind::MaterializedView, ObjectKind::Type] {
            assert!(names(&s, &scope, kind).await.is_empty(), "{t:?} {kind:?}");
        }

        // Introspection never switches the session's current database.
        let current = s.query_row_text("SELECT DATABASE()").await.unwrap();
        assert_eq!(current, Some(vec![None]), "{t:?}");
    }
}

#[tokio::test]
async fn table_detail_snapshot() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        for table in ["orders", "all_types", "weird_names"] {
            let d = detail(&s, t, table).await;
            assert!(d.row_estimate.is_some() && d.size_bytes.is_some(), "{t:?}");
            insta::assert_yaml_snapshot!(format!("{t:?}_{table}"), d, {
                ".row_estimate" => "[redacted]",
                ".size_bytes" => "[redacted]",
            });
        }
        let weird = detail(&s, t, "weird_names").await;
        assert_eq!(weird.columns[1].name, "we`ird\"col", "{t:?}");
        let vn = detail(&s, t, "đơn_hàng").await;
        assert_eq!(vn.object.name, "đơn_hàng");
        assert_eq!(vn.columns.len(), 3, "{t:?}");

        // Missing tables (or tables the user cannot see) are a server error, not a panic.
        let missing = s
            .table_detail(&obj("no_such_table", ObjectKind::Table))
            .await;
        assert!(
            matches!(missing, Err(DbError::Server { .. })),
            "{t:?}: {missing:?}"
        );
        let unsupported = s.table_detail(&obj("f_add", ObjectKind::Function)).await;
        assert!(
            matches!(unsupported, Err(DbError::Unsupported { .. })),
            "{t:?}: {unsupported:?}"
        );
    }
}

#[tokio::test]
async fn capability_specific_details() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let caps = s.capabilities();
        let customers = detail(&s, t, "customers").await;
        let orders = detail(&s, t, "orders").await;
        let pk = customers.primary_key.as_ref().expect("primary key");
        assert!(
            pk.primary && pk.unique && pk.columns == ["id"],
            "{t:?}: {pk:?}"
        );
        assert!(customers.indexes.iter().all(|i| !i.primary), "{t:?}");
        assert!(customers.columns[0].auto_increment, "{t:?}");
        assert_eq!(orders.foreign_keys.len(), 1, "{t:?}");
        assert_eq!(orders.foreign_keys[0].ref_table, "customers");
        assert_eq!(orders.triggers.len(), 1, "{t:?}");
        assert_eq!(orders.options.engine.as_deref(), Some("InnoDB"));
        match t {
            Target::MySql8 => {
                assert!(caps.functional_index && caps.check_constraints, "{caps:?}");
                assert!(
                    customers
                        .indexes
                        .iter()
                        .any(|i| i.expression.as_deref().is_some_and(|e| e.contains("lower"))),
                    "{:?}",
                    customers.indexes
                );
                assert!(customers.columns.iter().any(|c| c.invisible));
                assert!(!customers.checks.is_empty());
                assert!(orders.checks.iter().any(|c| c.name == "chk_orders_total"));
            }
            Target::MySql57 => {
                assert!(customers.checks.is_empty() && orders.checks.is_empty());
                assert!(customers.indexes.iter().all(|i| i.expression.is_none()));
            }
            _ => {
                let all_types = detail(&s, t, "all_types").await;
                let c_json = all_types
                    .columns
                    .iter()
                    .find(|c| c.name == "c_json")
                    .unwrap();
                assert_eq!(c_json.db_type, "json", "{t:?}");
                let maria_json = detail(&s, t, "maria_json").await;
                assert_eq!(maria_json.columns[1].db_type, "json", "{t:?}");
                // Text without a `json_valid` check keeps its type.
                let text = all_types
                    .columns
                    .iter()
                    .find(|c| c.name == "c_text")
                    .unwrap();
                assert_eq!(text.db_type, "text", "{t:?}");
                let versioned = detail(&s, t, "versioned_items").await;
                assert!(versioned.options.system_versioned, "{t:?}");
                assert!(!orders.options.system_versioned, "{t:?}");
                assert!(!customers.checks.is_empty(), "{t:?}");
                // MariaDB reports a missing default as the text NULL: normalized away.
                assert_eq!(customers.columns[3].default, None, "{t:?}");
            }
        }

        // Partitioned tables report their partitioning.
        for sql in [
            "CREATE DATABASE IF NOT EXISTS dnt_t11",
            "CREATE TABLE IF NOT EXISTS dnt_t11.parted (id int NOT NULL PRIMARY KEY) \
             PARTITION BY HASH (id) PARTITIONS 2",
        ] {
            s.query_row_text(sql).await.unwrap();
        }
        let parted = s
            .table_detail(&ObjectRef {
                database: "dnt_t11".into(),
                schema: None,
                name: "parted".into(),
                kind: ObjectKind::Table,
            })
            .await
            .unwrap();
        let by = parted.options.partitioned_by.unwrap_or_default();
        assert!(by.starts_with("HASH"), "{t:?}: {by}");
        assert_eq!(orders.options.partitioned_by, None, "{t:?}");
    }
}

#[tokio::test]
async fn ddl_for_each_kind() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let mut objects = vec![
            ("orders", ObjectKind::Table),
            ("đơn_hàng", ObjectKind::Table),
            ("v_customer_orders", ObjectKind::View),
            ("f_add", ObjectKind::Function),
            ("p_noop", ObjectKind::Procedure),
            ("trg_orders_bi", ObjectKind::Trigger),
            ("ev_cleanup", ObjectKind::Event),
        ];
        if is_maria(t) {
            objects.push(("seq_inv", ObjectKind::Sequence));
        }
        for (name, kind) in objects {
            let ddl = s
                .object_ddl(&obj(name, kind))
                .await
                .unwrap_or_else(|e| panic!("{t:?} {kind:?} {name}: {e}"));
            assert!(ddl.starts_with("CREATE"), "{t:?} {kind:?}: {ddl}");
            assert!(ddl.contains(name), "{t:?} {kind:?}: {ddl}");
        }
        for kind in [ObjectKind::MaterializedView, ObjectKind::Type] {
            let r = s.object_ddl(&obj("x", kind)).await;
            assert!(
                matches!(r, Err(DbError::Unsupported { .. })),
                "{t:?}: {r:?}"
            );
        }
        if !is_maria(t) {
            let r = s.object_ddl(&obj("seq_inv", ObjectKind::Sequence)).await;
            assert!(
                matches!(r, Err(DbError::Unsupported { .. })),
                "{t:?}: {r:?}"
            );
        }
    }
}

/// Creates `dnt_big` with 5 000 tables from `big_schema.sql.tmpl` unless already there.
async fn ensure_big_schema(s: &MySqlSession) {
    let count = s
        .query_row_text(
            "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'dnt_big'",
        )
        .await
        .unwrap()
        .and_then(|r| r[0].clone())
        .unwrap_or_default();
    if count == BIG_TABLES.to_string() {
        return;
    }
    let tmpl = std::fs::read_to_string(dnt_testkit::testdata("sql/big_schema.sql.tmpl")).unwrap();
    let stmt = tmpl
        .lines()
        .skip(1)
        .find(|l| !l.trim().is_empty())
        .expect("template statement")
        .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS dnt_big.", 1);
    s.query_row_text("CREATE DATABASE IF NOT EXISTS dnt_big")
        .await
        .unwrap();
    for n in 1..=BIG_TABLES {
        let sql = stmt.replace("{n}", &format!("{n:05}"));
        s.query_row_text(&sql).await.unwrap();
    }
}

#[tokio::test]
async fn lists_5000_tables_fast() {
    require_db!();
    let s = connect(Target::MySql8).await;
    ensure_big_schema(&s).await;
    // Setup, not measured: MySQL 8 computes `TABLE_ROWS` on first read after creation
    // (or after `information_schema_stats_expiry`), ~10 ms per table, then caches it.
    // The timed call below measures the listing with the server's stats cache warm.
    s.query_row_text(
        "SELECT COUNT(TABLE_ROWS) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'dnt_big'",
    )
    .await
    .unwrap();
    let scope = Scope {
        database: BIG_DB.into(),
        schema: None,
    };
    let started = Instant::now();
    let tables = s.list_objects(&scope, ObjectKind::Table).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(tables.len(), BIG_TABLES);
    assert_eq!(tables[0].name, "t_00001");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[tokio::test]
async fn limited_user_sees_partial_tree() {
    require_db!();
    for t in Target::mysql_family() {
        let s = MySqlDriver
            .connect_session(&t.resolved_as("dnt_limited", TEST_PASSWORD, None))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let scope = fixture();
        assert_eq!(
            names(&s, &scope, ObjectKind::Table).await,
            ["customers"],
            "{t:?}"
        );
        assert!(names(&s, &scope, ObjectKind::View).await.is_empty());
        let customers = detail(&s, t, "customers").await;
        assert!(!customers.columns.is_empty(), "{t:?}");
        let r = s.object_ddl(&obj("orders", ObjectKind::Table)).await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
        let r = s.table_detail(&obj("orders", ObjectKind::Table)).await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
    }
}
