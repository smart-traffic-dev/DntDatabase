use dnt_core::driver::{ExecOptions, Introspector, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope, TableDetail};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_pg::ddl::quote_ident;
use dnt_driver_pg::{PgDriver, PgSession};
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use std::time::{Duration, Instant};

const BIG_DB: &str = "dnt_big";
const BIG_TABLES: usize = 5000;

async fn connect_to(t: Target, db: &str) -> PgSession {
    PgDriver
        .connect_session(&t.resolved(Some(db)))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

/// Connects to the fixture database after removing the table Task 12's deadlock test
/// used to leave behind, so listings and snapshots do not depend on test order.
async fn connect(t: Target) -> PgSession {
    let s = connect_to(t, FIXTURE_DB).await;
    exec(&s, "DROP TABLE IF EXISTS public.t12_deadlock")
        .await
        .unwrap();
    s
}

struct Discard;

impl RowSink for Discard {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn exec(s: &PgSession, sql: &str) -> Result<(), DbError> {
    s.execute(sql, ExecOptions::default(), &mut Discard)
        .await
        .map(|_| ())
}

fn scope(schema: &str) -> Scope {
    Scope {
        database: FIXTURE_DB.into(),
        schema: Some(schema.into()),
    }
}

fn obj(schema: &str, name: &str, kind: ObjectKind) -> ObjectRef {
    ObjectRef {
        database: FIXTURE_DB.into(),
        schema: Some(schema.into()),
        name: name.into(),
        kind,
    }
}

async fn names(s: &PgSession, scope: &Scope, kind: ObjectKind) -> Vec<String> {
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

async fn detail(s: &PgSession, t: Target, schema: &str, table: &str) -> TableDetail {
    s.table_detail(&obj(schema, table, ObjectKind::Table))
        .await
        .unwrap_or_else(|e| panic!("{t:?} {table}: {e}"))
}

fn assert_sorted(t: Target, kind: ObjectKind, v: &[String]) {
    let mut sorted = v.to_vec();
    sorted.sort();
    assert_eq!(v, sorted, "{t:?} {kind:?} not in stable order");
}

fn has(v: &[String], name: &str) -> bool {
    v.iter().any(|n| n == name)
}

#[tokio::test]
async fn lists_fixture_objects() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let dbs: Vec<String> = s
            .list_databases()
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(has(&dbs, FIXTURE_DB), "{t:?}: {dbs:?}");
        assert!(!has(&dbs, "template0") && !has(&dbs, "template1"), "{t:?}");
        assert_sorted(t, ObjectKind::Table, &dbs);

        let schemas: Vec<String> = s
            .list_schemas(FIXTURE_DB)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        for want in ["public", "sales", "pg_catalog", "information_schema"] {
            assert!(has(&schemas, want), "{t:?}: {want} in {schemas:?}");
        }
        assert!(
            !schemas
                .iter()
                .any(|n| n.starts_with("pg_toast") || n.starts_with("pg_temp")),
            "{t:?}: {schemas:?}"
        );
        assert_sorted(t, ObjectKind::Table, &schemas);

        let public = scope("public");
        let sales = scope("sales");
        let tables = names(&s, &public, ObjectKind::Table).await;
        for want in [
            "all_types",
            "bin_blob",
            "customers",
            "orders",
            "weird_names",
            "đơn_hàng",
        ] {
            assert!(has(&tables, want), "{t:?}: {want} in {tables:?}");
        }
        assert!(!has(&tables, "v_customer_orders"), "{t:?}");
        assert_sorted(t, ObjectKind::Table, &tables);
        // The partitioned table is listed once; its partitions are not.
        assert_eq!(
            names(&s, &sales, ObjectKind::Table).await,
            ["invoices", "measurements"],
            "{t:?}"
        );

        assert_eq!(
            names(&s, &public, ObjectKind::View).await,
            ["v_customer_orders"],
            "{t:?}"
        );
        let views = s.list_objects(&public, ObjectKind::View).await.unwrap();
        assert_eq!(views[0].row_estimate, None, "{t:?}");
        assert_eq!(
            names(&s, &sales, ObjectKind::MaterializedView).await,
            ["mv_order_totals"],
            "{t:?}"
        );
        assert!(names(&s, &public, ObjectKind::MaterializedView)
            .await
            .is_empty());
        assert_eq!(
            names(&s, &public, ObjectKind::Type).await,
            ["order_status"],
            "{t:?}"
        );
        let sequences = names(&s, &sales, ObjectKind::Sequence).await;
        assert!(has(&sequences, "seq_inv"), "{t:?}: {sequences:?}");
        assert_eq!(
            names(&s, &public, ObjectKind::Function).await,
            ["f_add", "fn_orders_bi"],
            "{t:?}"
        );
        assert_eq!(
            names(&s, &public, ObjectKind::Procedure).await,
            ["p_noop"],
            "{t:?}"
        );
        assert_eq!(
            names(&s, &public, ObjectKind::Trigger).await,
            ["trg_orders_bi"],
            "{t:?}"
        );
        assert!(names(&s, &public, ObjectKind::Event).await.is_empty());

        // Comments come from `pg_description`. Uncommitted DDL is visible to the
        // session's own introspection queries, so nothing permanent is created.
        exec(
            &s,
            "BEGIN; CREATE TABLE public.t13_commented (id int, note text); \
             COMMENT ON TABLE public.t13_commented IS 'hello''s'; \
             COMMENT ON COLUMN public.t13_commented.note IS 'a note'",
        )
        .await
        .unwrap();
        let listed = s.list_objects(&public, ObjectKind::Table).await.unwrap();
        let commented = listed.iter().find(|o| o.name == "t13_commented").unwrap();
        assert_eq!(commented.comment.as_deref(), Some("hello's"), "{t:?}");
        let orders = listed.iter().find(|o| o.name == "orders").unwrap();
        assert_eq!(orders.comment, None, "{t:?}");
        let d = detail(&s, t, "public", "t13_commented").await;
        assert_eq!(d.comment.as_deref(), Some("hello's"), "{t:?}");
        assert_eq!(d.columns[1].comment.as_deref(), Some("a note"), "{t:?}");
        let ddl = s
            .object_ddl(&obj("public", "t13_commented", ObjectKind::Table))
            .await
            .unwrap();
        assert!(
            ddl.contains("COMMENT ON TABLE public.t13_commented IS 'hello''s';"),
            "{t:?}: {ddl}"
        );
        assert!(
            ddl.contains("COMMENT ON COLUMN public.t13_commented.note IS 'a note';"),
            "{t:?}: {ddl}"
        );
        exec(&s, "ROLLBACK").await.unwrap();
        assert!(!s.in_transaction(), "{t:?}");

        // PostgreSQL cannot read another database's catalogs.
        let other = Scope {
            database: "postgres".into(),
            schema: Some("public".into()),
        };
        let r = s.list_objects(&other, ObjectKind::Table).await;
        assert!(
            matches!(&r, Err(DbError::Unsupported { feature, .. })
                if feature == "cross-database introspection"),
            "{t:?}: {r:?}"
        );
        let r = s.list_schemas("postgres").await;
        assert!(
            matches!(r, Err(DbError::Unsupported { .. })),
            "{t:?}: {r:?}"
        );
        let r = s
            .table_detail(&ObjectRef {
                database: "postgres".into(),
                ..obj("public", "orders", ObjectKind::Table)
            })
            .await;
        assert!(
            matches!(r, Err(DbError::Unsupported { .. })),
            "{t:?}: {r:?}"
        );
    }
}

#[tokio::test]
async fn table_detail_snapshot() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        for (schema, table) in [
            ("public", "orders"),
            ("public", "all_types"),
            ("public", "weird_names"),
            ("sales", "invoices"),
            ("sales", "measurements"),
        ] {
            let d = detail(&s, t, schema, table).await;
            assert!(d.size_bytes.is_some(), "{t:?} {table}");
            insta::assert_yaml_snapshot!(format!("{t:?}_{table}"), d, {
                ".row_estimate" => "[redacted]",
                ".size_bytes" => "[redacted]",
            });
        }
        let weird = detail(&s, t, "public", "weird_names").await;
        assert_eq!(weird.columns[1].name, "we`ird\"col", "{t:?}");
        let vn = detail(&s, t, "public", "đơn_hàng").await;
        assert_eq!(vn.object.name, "đơn_hàng");
        assert_eq!(vn.columns.len(), 3, "{t:?}");
        let invoices = detail(&s, t, "sales", "invoices").await;
        assert_eq!(invoices.columns[0].identity.as_deref(), Some("ALWAYS"));
        assert!(invoices.columns[3].generated.is_some(), "{t:?}");
        let parted = detail(&s, t, "sales", "measurements").await;
        assert_eq!(
            parted.options.partitioned_by.as_deref(),
            Some("RANGE (taken_at)"),
            "{t:?}"
        );
        let mv = s
            .table_detail(&obj(
                "sales",
                "mv_order_totals",
                ObjectKind::MaterializedView,
            ))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(mv.columns.len(), 3, "{t:?}");

        // Missing tables (or tables the user cannot see) are a server error.
        let missing = s
            .table_detail(&obj("public", "no_such_table", ObjectKind::Table))
            .await;
        assert!(
            matches!(missing, Err(DbError::Server { .. })),
            "{t:?}: {missing:?}"
        );
        // A view is not found as a table.
        let wrong_kind = s
            .table_detail(&obj("public", "v_customer_orders", ObjectKind::Table))
            .await;
        assert!(
            matches!(wrong_kind, Err(DbError::Server { .. })),
            "{t:?}: {wrong_kind:?}"
        );
        let unsupported = s
            .table_detail(&obj("public", "f_add", ObjectKind::Function))
            .await;
        assert!(
            matches!(unsupported, Err(DbError::Unsupported { .. })),
            "{t:?}: {unsupported:?}"
        );
    }
}

#[tokio::test]
async fn ddl_for_each_kind() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        for (schema, name, kind) in [
            ("public", "orders", ObjectKind::Table),
            ("public", "đơn_hàng", ObjectKind::Table),
            ("public", "v_customer_orders", ObjectKind::View),
            ("sales", "mv_order_totals", ObjectKind::MaterializedView),
            ("public", "f_add", ObjectKind::Function),
            ("public", "p_noop", ObjectKind::Procedure),
            ("public", "trg_orders_bi", ObjectKind::Trigger),
            ("sales", "seq_inv", ObjectKind::Sequence),
            ("public", "order_status", ObjectKind::Type),
        ] {
            let ddl = s
                .object_ddl(&obj(schema, name, kind))
                .await
                .unwrap_or_else(|e| panic!("{t:?} {kind:?} {name}: {e}"));
            assert!(ddl.starts_with("CREATE"), "{t:?} {kind:?}: {ddl}");
            assert!(ddl.contains(name), "{t:?} {kind:?}: {ddl}");
            assert!(ddl.trim_end().ends_with(';'), "{t:?} {kind:?}: {ddl}");
        }
        let view = s
            .object_ddl(&obj("public", "v_customer_orders", ObjectKind::View))
            .await
            .unwrap();
        assert!(
            view.starts_with("CREATE OR REPLACE VIEW public.v_customer_orders AS"),
            "{view}"
        );
        let seq = s
            .object_ddl(&obj("sales", "seq_inv", ObjectKind::Sequence))
            .await
            .unwrap();
        assert!(seq.contains("START WITH 1000"), "{t:?}: {seq}");
        let ty = s
            .object_ddl(&obj("public", "order_status", ObjectKind::Type))
            .await
            .unwrap();
        assert_eq!(
            ty, "CREATE TYPE public.order_status AS ENUM ('new', 'paid', 'shipped', 'cancelled');",
            "{t:?}"
        );
        let r = s.object_ddl(&obj("public", "x", ObjectKind::Event)).await;
        assert!(
            matches!(r, Err(DbError::Unsupported { .. })),
            "{t:?}: {r:?}"
        );
        let r = s
            .object_ddl(&obj("public", "no_such_fn", ObjectKind::Function))
            .await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
    }
}

/// `name` + `_copy`, quoted as a whole (`"Foo_copy"`, never `"Foo"_copy`).
fn copy_name(name: &str) -> String {
    quote_ident(&format!("{name}_copy"))
}

/// Renames the table and every index / constraint (they share a namespace with
/// relations) of `ddl` to `<name>_copy`. Only the table's own `schema.table ` occurrences
/// are renamed: a reference (`schema.table(`) keeps pointing at the original.
fn copy_ddl(ddl: &str, d: &TableDetail, schema: &str, table: &str) -> String {
    let mut ddl = ddl.to_string();
    let mut renamed: Vec<&str> = d
        .primary_key
        .iter()
        .chain(&d.indexes)
        .map(|i| i.name.as_str())
        .chain(d.foreign_keys.iter().map(|f| f.name.as_str()))
        .chain(d.checks.iter().map(|c| c.name.as_str()))
        .collect();
    assert!(!renamed.is_empty(), "{table}");
    renamed.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for name in renamed {
        ddl = ddl.replace(
            &format!("{} ", quote_ident(name)),
            &format!("{} ", copy_name(name)),
        );
    }
    let qualified = format!("{}.{} ", quote_ident(schema), quote_ident(table));
    assert!(ddl.contains(&qualified), "{ddl}");
    ddl.replace(
        &qualified,
        &format!("{}.{} ", quote_ident(schema), copy_name(table)),
    )
}

/// Runs `sql` in a transaction that is rolled back.
async fn run_rolled_back(s: &PgSession, sql: &str) -> Result<(), DbError> {
    exec(s, "BEGIN").await.unwrap();
    let res = exec(s, sql).await;
    exec(s, "ROLLBACK").await.unwrap();
    res
}

/// The generated DDL, renamed to `<table>_copy`, runs without error.
#[tokio::test]
async fn ddl_roundtrip() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        for (schema, table) in [
            ("public", "orders"),
            ("public", "weird_names"),
            ("public", "đơn_hàng"),
            ("sales", "invoices"),
            ("sales", "measurements"),
        ] {
            let d = detail(&s, t, schema, table).await;
            let ddl = s
                .object_ddl(&obj(schema, table, ObjectKind::Table))
                .await
                .unwrap_or_else(|e| panic!("{t:?} {table}: {e}"));
            let ddl = copy_ddl(&ddl, &d, schema, table);
            run_rolled_back(&s, &ddl)
                .await
                .unwrap_or_else(|e| panic!("{t:?} {table}: {e}\n{ddl}"));
        }
    }
}

/// FK and CHECK options that cannot be rebuilt from their parts come through verbatim.
/// Everything happens in one transaction that is rolled back.
#[tokio::test]
async fn verbatim_constraint_definitions() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let mut setup = vec![
            "CREATE TABLE public.t13_constraints (\
               id int PRIMARY KEY, customer_id int, parent_id int, total numeric, \
               CONSTRAINT t13_uq UNIQUE (id, customer_id), \
               CONSTRAINT t13_fk FOREIGN KEY (customer_id) REFERENCES public.customers (id) \
                 MATCH FULL DEFERRABLE INITIALLY DEFERRED)",
            "ALTER TABLE public.t13_constraints \
               ADD CONSTRAINT t13_chk CHECK (total >= 0) NOT VALID",
        ];
        let mut want = vec![
            "CONSTRAINT t13_fk FOREIGN KEY (customer_id) REFERENCES public.customers(id) \
             MATCH FULL DEFERRABLE INITIALLY DEFERRED",
            "CONSTRAINT t13_chk CHECK ((total >= (0)::numeric)) NOT VALID",
        ];
        if t == Target::Pg17 {
            // PG 15+: a column list on ON DELETE SET NULL.
            setup.push(
                "ALTER TABLE public.t13_constraints ADD CONSTRAINT t13_parent \
                 FOREIGN KEY (parent_id, customer_id) \
                 REFERENCES public.t13_constraints (id, customer_id) \
                 ON DELETE SET NULL (parent_id)",
            );
            want.push(
                "CONSTRAINT t13_parent FOREIGN KEY (parent_id, customer_id) \
                 REFERENCES public.t13_constraints(id, customer_id) ON DELETE SET NULL (parent_id)",
            );
        }
        exec(&s, "BEGIN").await.unwrap();
        let checked = async {
            exec(&s, &setup.join("; ")).await?;
            let d = detail(&s, t, "public", "t13_constraints").await;
            assert!(
                d.foreign_keys.iter().all(|f| f.definition.is_some()),
                "{t:?}: {:?}",
                d.foreign_keys
            );
            assert!(d.checks[0].definition.is_some(), "{t:?}");
            let ddl = s
                .object_ddl(&obj("public", "t13_constraints", ObjectKind::Table))
                .await?;
            for w in &want {
                assert!(ddl.contains(w), "{t:?}: {w}\n{ddl}");
            }
            let copy = copy_ddl(&ddl, &d, "public", "t13_constraints");
            exec(&s, &copy).await.map_err(|e| DbError::Internal {
                message: format!("{e}\n{copy}"),
            })
        }
        .await;
        exec(&s, "ROLLBACK").await.unwrap();
        checked.unwrap_or_else(|e| panic!("{t:?}: {e}"));
    }
}

/// Creates database `dnt_big` with 5 000 tables from `big_schema.sql.tmpl` unless they
/// are already there.
async fn ensure_big_schema(t: Target) {
    let admin = connect_to(t, FIXTURE_DB).await;
    let exists = admin
        .query_row_text("SELECT 1 FROM pg_database WHERE datname = 'dnt_big'")
        .await
        .unwrap();
    if exists.is_none() {
        admin
            .query_row_text("CREATE DATABASE dnt_big")
            .await
            .unwrap();
    }
    let s = connect_to(t, BIG_DB).await;
    let count = s
        .query_row_text(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relkind = 'r'",
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
        .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS public.", 1);
    // One simple-protocol round trip (an implicit transaction) per 500 tables.
    let all: Vec<String> = (1..=BIG_TABLES)
        .map(|n| stmt.replace("{n}", &format!("{n:05}")))
        .collect();
    for chunk in all.chunks(500) {
        s.query_row_text(&chunk.join("\n")).await.unwrap();
    }
}

#[tokio::test]
async fn lists_5000_tables_fast() {
    require_db!();
    let t = Target::Pg17;
    ensure_big_schema(t).await;
    // A fresh backend: catalog caches are cold.
    let s = connect_to(t, BIG_DB).await;
    let scope = Scope {
        database: BIG_DB.into(),
        schema: Some("public".into()),
    };
    let started = Instant::now();
    let tables = s.list_objects(&scope, ObjectKind::Table).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(tables.len(), BIG_TABLES);
    assert_eq!(tables[0].name, "t_00001");
    assert_eq!(tables[BIG_TABLES - 1].name, "t_05000");
    eprintln!("{t:?}: listed {BIG_TABLES} tables in {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{t:?} took {elapsed:?}");
}

#[tokio::test]
async fn list_columns_matches_table_detail_in_one_query() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        for (schema, table, kind) in [
            ("public", "customers", ObjectKind::Table),
            ("public", "weird_names", ObjectKind::Table),
            ("public", "v_customer_orders", ObjectKind::View),
            ("sales", "mv_order_totals", ObjectKind::MaterializedView),
        ] {
            let all = s.list_columns(&scope(schema)).await.unwrap();
            let tables: Vec<String> = all.iter().map(|c| c.table.clone()).collect();
            assert_sorted(t, ObjectKind::Table, &tables);
            let listed = &all
                .iter()
                .find(|c| c.table == table)
                .unwrap_or_else(|| panic!("{t:?}: {table} missing from {tables:?}"))
                .columns;
            let detail = s.table_detail(&obj(schema, table, kind)).await.unwrap();
            let expected: Vec<(String, String)> = detail
                .columns
                .into_iter()
                .map(|c| (c.name, c.db_type))
                .collect();
            let listed: Vec<(String, String)> = listed
                .iter()
                .map(|c| (c.name.clone(), c.data_type.clone()))
                .collect();
            assert_eq!(listed, expected, "{t:?} {schema}.{table}");
        }
    }
}

#[tokio::test]
async fn list_columns_has_types() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let all = s.list_columns(&scope("public")).await.unwrap();
        let orders = all.iter().find(|c| c.table == "orders").unwrap();
        let total = orders.columns.iter().find(|c| c.name == "total").unwrap();
        assert_eq!(total.data_type, "numeric(12,2)", "{t:?}");
        let status = orders.columns.iter().find(|c| c.name == "status").unwrap();
        assert_eq!(status.data_type, "order_status", "{t:?}");
    }
}

#[tokio::test]
async fn schema_cache_queries_on_5000_tables_are_fast_cold() {
    require_db!();
    let t = Target::Pg17;
    ensure_big_schema(t).await;
    // A fresh backend: catalog caches are cold.
    let s = connect_to(t, BIG_DB).await;
    let scope = Scope {
        database: BIG_DB.into(),
        schema: Some("public".into()),
    };
    // What the SchemaCache loader runs for one schema.
    let started = Instant::now();
    let mut listed = 0;
    for kind in [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
    ] {
        listed += s.list_objects(&scope, kind).await.unwrap().len();
    }
    let columns = s.list_columns(&scope).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(listed, BIG_TABLES);
    assert_eq!(columns.len(), BIG_TABLES);
    assert!(columns.iter().all(|c| !c.columns.is_empty()));
    eprintln!("{t:?}: SchemaCache queries for {BIG_TABLES} tables in {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{t:?} took {elapsed:?}");
}

#[tokio::test]
async fn limited_user_sees_partial_tree() {
    require_db!();
    for t in Target::pg() {
        let s = PgDriver
            .connect_session(&t.resolved_as("dnt_limited", TEST_PASSWORD, Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let schemas: Vec<String> = s
            .list_schemas(FIXTURE_DB)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(has(&schemas, "public"), "{t:?}: {schemas:?}");
        assert!(!has(&schemas, "sales"), "{t:?}: {schemas:?}");
        let public = scope("public");
        assert_eq!(
            names(&s, &public, ObjectKind::Table).await,
            ["customers"],
            "{t:?}"
        );
        assert!(names(&s, &public, ObjectKind::View).await.is_empty());
        assert!(names(&s, &public, ObjectKind::Sequence).await.is_empty());
        assert!(names(&s, &public, ObjectKind::Trigger).await.is_empty());
        let sales = scope("sales");
        assert!(names(&s, &sales, ObjectKind::Table).await.is_empty());
        assert!(names(&s, &sales, ObjectKind::MaterializedView)
            .await
            .is_empty());
        let customers = detail(&s, t, "public", "customers").await;
        assert!(!customers.columns.is_empty(), "{t:?}");
        assert!(customers.primary_key.is_some(), "{t:?}");
        // Same privilege filter as the listings.
        let columns = s.list_columns(&public).await.unwrap();
        assert_eq!(
            columns.iter().map(|c| c.table.as_str()).collect::<Vec<_>>(),
            ["customers"],
            "{t:?}"
        );
        assert!(s.list_columns(&sales).await.unwrap().is_empty(), "{t:?}");
        let r = s
            .object_ddl(&obj("public", "orders", ObjectKind::Table))
            .await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
        let r = s
            .table_detail(&obj("public", "orders", ObjectKind::Table))
            .await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
    }
}

#[tokio::test]
async fn explicit_column_collation_is_reported() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let table = format!(
            "p3_collation_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        exec(
            &s,
            &format!("CREATE TABLE public.{table} (a text COLLATE \"C\", b text, c int)"),
        )
        .await
        .unwrap();
        let d = detail(&s, t, "public", &table).await;
        exec(&s, &format!("DROP TABLE public.{table}"))
            .await
            .unwrap();
        let got: Vec<Option<&str>> = d.columns.iter().map(|c| c.collation.as_deref()).collect();
        assert_eq!(got, [Some("C"), None, None], "{t:?}");
    }
}

/// A partial unique index (soft-delete pattern) is reported as `partial`; a unique
/// index left invalid by a failed `CREATE UNIQUE INDEX CONCURRENTLY` is not unique.
#[tokio::test]
async fn partial_and_invalid_unique_indexes_are_reported() {
    require_db!();
    for t in Target::pg() {
        let s = connect(t).await;
        let table = format!(
            "p3_partial_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        exec(
            &s,
            &format!(
                "CREATE TABLE public.{table} (code text NOT NULL, sku text NOT NULL, \
                 deleted_at timestamptz); \
                 CREATE UNIQUE INDEX {table}_live ON public.{table} (code) \
                 WHERE deleted_at IS NULL; \
                 CREATE UNIQUE INDEX {table}_full ON public.{table} (code, sku); \
                 INSERT INTO public.{table} VALUES ('a', 'x', now()), ('b', 'x', now())"
            ),
        )
        .await
        .unwrap();
        // Fails on the duplicates and leaves an invalid index behind.
        let r = exec(
            &s,
            &format!("CREATE UNIQUE INDEX CONCURRENTLY {table}_bad ON public.{table} (sku)"),
        )
        .await;
        let d = detail(&s, t, "public", &table).await;
        exec(&s, &format!("DROP TABLE public.{table}"))
            .await
            .unwrap();
        assert!(r.is_err(), "{t:?}: {r:?}");
        let got: Vec<(String, bool, bool)> = d
            .indexes
            .iter()
            .map(|i| {
                (
                    i.name.trim_start_matches(&table).to_string(),
                    i.unique,
                    i.partial,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("_bad".to_string(), false, false),
                ("_full".to_string(), true, false),
                ("_live".to_string(), true, true),
            ],
            "{t:?}"
        );
    }
}
