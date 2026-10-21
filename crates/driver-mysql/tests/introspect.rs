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
        // Lists never carry statistics; the estimate comes from `table_detail`.
        assert!(summaries.iter().all(|o| o.row_estimate.is_none()), "{t:?}");
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
            "CREATE TABLE IF NOT EXISTS dnt_t11.commented (id int) COMMENT = 'hello'",
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

        // Table comments: listed on MySQL 8 / MariaDB (no stats read), never on 5.7,
        // where any `information_schema.TABLES` read opens every table.
        let t11 = Scope {
            database: "dnt_t11".into(),
            schema: None,
        };
        let listed = s.list_objects(&t11, ObjectKind::Table).await.unwrap();
        let commented = listed.iter().find(|o| o.name == "commented").unwrap();
        let want = (t != Target::MySql57).then(|| "hello".to_string());
        assert_eq!(commented.comment, want, "{t:?}");
        let d = s
            .table_detail(&ObjectRef {
                database: "dnt_t11".into(),
                schema: None,
                name: "commented".into(),
                kind: ObjectKind::Table,
            })
            .await
            .unwrap();
        assert_eq!(d.comment.as_deref(), Some("hello"), "{t:?}");
        assert!(d.row_estimate.is_some(), "{t:?}");
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
    let scope = Scope {
        database: BIG_DB.into(),
        schema: None,
    };
    for t in [Target::MySql8, Target::MySql57] {
        let s = connect(t).await;
        ensure_big_schema(&s).await;
        if t == Target::MySql8 {
            // Cold path: with expiry 0, MySQL 8 recomputes table statistics on every
            // read (~10 ms per table), so this only stays fast if listing reads none.
            s.query_row_text("SET SESSION information_schema_stats_expiry = 0")
                .await
                .unwrap();
        }
        let started = Instant::now();
        let tables = s.list_objects(&scope, ObjectKind::Table).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(tables.len(), BIG_TABLES, "{t:?}");
        assert_eq!(tables[0].name, "t_00001");
        assert!(tables.iter().all(|o| o.row_estimate.is_none()), "{t:?}");
        eprintln!("{t:?}: listed {BIG_TABLES} tables in {elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{t:?} took {elapsed:?}");
    }
}

#[tokio::test]
async fn list_columns_matches_table_detail_in_one_query() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let all = s.list_columns(&fixture()).await.unwrap();
        let names: Vec<&str> = all.iter().map(|c| c.table.as_str()).collect();
        assert_sorted(
            t,
            ObjectKind::Table,
            &names.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
        );
        for (table, kind) in [
            ("customers", ObjectKind::Table),
            ("đơn_hàng", ObjectKind::Table),
            ("weird_names", ObjectKind::Table),
            ("v_customer_orders", ObjectKind::View),
        ] {
            let listed = &all
                .iter()
                .find(|c| c.table == table)
                .unwrap_or_else(|| panic!("{t:?}: {table} missing from {names:?}"))
                .columns;
            let detail = s.table_detail(&obj(table, kind)).await.unwrap();
            let expected: Vec<(String, String)> = detail
                .columns
                .into_iter()
                .map(|c| (c.name, c.db_type))
                .collect();
            let listed: Vec<(String, String)> = listed
                .iter()
                .map(|c| (c.name.clone(), c.data_type.clone()))
                .collect();
            assert_eq!(listed, expected, "{t:?} {table}");
        }
    }
}

#[tokio::test]
async fn list_columns_has_types() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let all = s.list_columns(&fixture()).await.unwrap();
        let orders = all.iter().find(|c| c.table == "orders").unwrap();
        let status = orders.columns.iter().find(|c| c.name == "status").unwrap();
        assert!(
            status.data_type.starts_with("enum("),
            "{t:?}: {}",
            status.data_type
        );
        let total = orders.columns.iter().find(|c| c.name == "total").unwrap();
        assert_eq!(total.data_type, "decimal(12,2)", "{t:?}");
    }
}

#[tokio::test]
async fn schema_cache_queries_on_5000_tables_are_fast_cold() {
    require_db!();
    let scope = Scope {
        database: BIG_DB.into(),
        schema: None,
    };
    let t = Target::MySql8;
    let s = connect(t).await;
    ensure_big_schema(&s).await;
    s.query_row_text("SET SESSION information_schema_stats_expiry = 0")
        .await
        .unwrap();
    // What the SchemaCache loader runs for one database.
    let started = Instant::now();
    let tables = s.list_objects(&scope, ObjectKind::Table).await.unwrap();
    let views = s.list_objects(&scope, ObjectKind::View).await.unwrap();
    let columns = s.list_columns(&scope).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!((tables.len(), views.len()), (BIG_TABLES, 0));
    assert_eq!(columns.len(), BIG_TABLES);
    assert!(columns.iter().all(|c| !c.columns.is_empty()));
    eprintln!("{t:?}: SchemaCache queries for {BIG_TABLES} tables in {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{t:?} took {elapsed:?}");
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
        let columns = s.list_columns(&scope).await.unwrap();
        assert_eq!(
            columns.iter().map(|c| c.table.as_str()).collect::<Vec<_>>(),
            ["customers"],
            "{t:?}"
        );
        let r = s.object_ddl(&obj("orders", ObjectKind::Table)).await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
        let r = s.table_detail(&obj("orders", ObjectKind::Table)).await;
        assert!(matches!(r, Err(DbError::Server { .. })), "{t:?}: {r:?}");
    }
}

fn scratch(test: &str) -> String {
    format!(
        "p3_{test}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

#[tokio::test]
async fn column_collation_is_reported() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let customers = detail(&s, t, "customers").await;
        let name = customers.columns.iter().find(|c| c.name == "name").unwrap();
        assert_eq!(
            name.collation.as_deref(),
            Some("utf8mb4_unicode_ci"),
            "{t:?}"
        );
        // Non-text columns have no collation.
        assert_eq!(customers.columns[0].collation, None, "{t:?}");

        let table = scratch("collation");
        s.query_row_text(&format!(
            "CREATE TABLE {FIXTURE_DB}.{table} (a varchar(10) COLLATE utf8mb4_bin, b int)"
        ))
        .await
        .unwrap();
        let d = detail(&s, t, &table).await;
        s.query_row_text(&format!("DROP TABLE {FIXTURE_DB}.{table}"))
            .await
            .unwrap();
        assert_eq!(
            d.columns[0].collation.as_deref(),
            Some("utf8mb4_bin"),
            "{t:?}"
        );
        assert_eq!(d.columns[1].collation, None, "{t:?}");
    }
}

#[tokio::test]
async fn myisam_engine_is_reported() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        let table = scratch("myisam");
        s.query_row_text(&format!(
            "CREATE TABLE {FIXTURE_DB}.{table} (id int) ENGINE=MyISAM"
        ))
        .await
        .unwrap();
        let d = detail(&s, t, &table).await;
        s.query_row_text(&format!("DROP TABLE {FIXTURE_DB}.{table}"))
            .await
            .unwrap();
        assert_eq!(d.options.engine.as_deref(), Some("MyISAM"), "{t:?}");
        let orders = detail(&s, t, "orders").await;
        assert_eq!(orders.options.engine.as_deref(), Some("InnoDB"), "{t:?}");
    }
}

#[tokio::test]
async fn mysql_detail_has_ddl_fields() {
    require_db!();
    for t in Target::mysql_family().into_iter().chain([Target::MySql84]) {
        let s = connect(t).await;
        let modern = matches!(t, Target::MySql8 | Target::MySql84);
        let extra_keys = if modern {
            ", KEY k_desc (name DESC), KEY k_inv (s) INVISIBLE"
        } else {
            ""
        };
        let table = scratch("ddlfields");
        s.query_row_text(&format!(
            "CREATE TABLE {FIXTURE_DB}.{table} (id int unsigned NOT NULL AUTO_INCREMENT, \
             ts timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, \
             s enum('a','b''c') NOT NULL, name varchar(100) CHARACTER SET latin1, \
             PRIMARY KEY (id), KEY k_name (name(10)) COMMENT 'pfx'{extra_keys}) \
             ROW_FORMAT=DYNAMIC"
        ))
        .await
        .unwrap();
        // MySQL 8 reports no AUTO_INCREMENT for a table that never held a row.
        let inserted = s
            .query_row_text(&format!(
                "INSERT INTO {FIXTURE_DB}.{table} (s) VALUES ('a')"
            ))
            .await;
        let d = s
            .table_detail(&obj(&table, ObjectKind::Table))
            .await
            .map_err(|e| e.to_string());
        s.query_row_text(&format!("DROP TABLE {FIXTURE_DB}.{table}"))
            .await
            .unwrap();
        inserted.unwrap();
        let d = d.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let c = |n: &str| d.columns.iter().find(|c| c.name == n).unwrap();
        assert!(c("id").unsigned && !c("ts").unsigned, "{t:?}");
        assert!(
            c("ts")
                .on_update
                .as_deref()
                .is_some_and(|u| u.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP")),
            "{t:?}: {:?}",
            c("ts").on_update
        );
        assert_eq!(c("s").enum_values, ["a", "b'c"], "{t:?}");
        assert_eq!(c("name").charset.as_deref(), Some("latin1"), "{t:?}");
        let idx = |n: &str| d.indexes.iter().find(|i| i.name == n).unwrap();
        let k = idx("k_name");
        assert_eq!(k.parts.len(), 1, "{t:?}");
        assert_eq!(k.parts[0].prefix, Some(10), "{t:?}");
        assert_eq!(k.parts[0].column.as_deref(), Some("name"), "{t:?}");
        assert_eq!(k.comment.as_deref(), Some("pfx"), "{t:?}");
        assert!(k.visible, "{t:?}");
        let pk = d.primary_key.as_ref().unwrap();
        assert_eq!(pk.parts.len(), 1, "{t:?}");
        assert_eq!(pk.comment, None, "{t:?}");
        assert!(
            d.options
                .row_format
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case("dynamic")),
            "{t:?}: {:?}",
            d.options.row_format
        );
        assert!(
            d.options
                .extra
                .iter()
                .any(|(k, v)| k == "row_format" && v.eq_ignore_ascii_case("dynamic")),
            "{t:?}: {:?}",
            d.options.extra
        );
        assert!(d.options.auto_increment.is_some(), "{t:?}");
        assert!(d.options.charset.is_some(), "{t:?}");
        if modern {
            assert!(idx("k_desc").parts[0].descending, "{t:?}");
            assert!(!idx("k_inv").visible, "{t:?}");
        }
    }
}

#[tokio::test]
async fn auto_increment_is_live() {
    require_db!();
    for t in Target::mysql_family().into_iter().chain([Target::MySql84]) {
        let s = connect(t).await;
        let table = scratch("autoinc");
        s.query_row_text(&format!(
            "CREATE TABLE {FIXTURE_DB}.{table} (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, v int)"
        ))
        .await
        .unwrap();
        let inserted = s
            .query_row_text(&format!(
                "INSERT INTO {FIXTURE_DB}.{table} (v) VALUES (1), (2)"
            ))
            .await;
        let first = s
            .table_detail(&obj(&table, ObjectKind::Table))
            .await
            .map(|d| d.options.auto_increment);
        let more = s
            .query_row_text(&format!(
                "INSERT INTO {FIXTURE_DB}.{table} (v) VALUES (3), (4), (5)"
            ))
            .await;
        let second = s
            .table_detail(&obj(&table, ObjectKind::Table))
            .await
            .map(|d| d.options.auto_increment);
        let expiry = s
            .query_row_text("SELECT @@session.information_schema_stats_expiry")
            .await;
        s.query_row_text(&format!("DROP TABLE {FIXTURE_DB}.{table}"))
            .await
            .unwrap();
        inserted.unwrap();
        more.unwrap();
        assert_eq!(first.unwrap(), Some(3), "{t:?}");
        assert_eq!(second.unwrap(), Some(6), "{t:?}");
        if matches!(t, Target::MySql8 | Target::MySql84) {
            // The session's own cache setting is put back.
            assert_eq!(
                expiry.unwrap(),
                Some(vec![Some("86400".to_string())]),
                "{t:?}"
            );
        }
    }
}

#[tokio::test]
async fn invisible_column_flag() {
    require_db!();
    for t in Target::mysql_family() {
        let s = connect(t).await;
        if !s.capabilities().invisible_columns {
            continue;
        }
        let table = scratch("invisible");
        s.query_row_text(&format!(
            "CREATE TABLE {FIXTURE_DB}.{table} (id int, hidden int INVISIBLE)"
        ))
        .await
        .unwrap();
        let d = detail(&s, t, &table).await;
        s.query_row_text(&format!("DROP TABLE {FIXTURE_DB}.{table}"))
            .await
            .unwrap();
        let flags: Vec<(&str, bool)> = d
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.invisible))
            .collect();
        assert_eq!(flags, [("id", false), ("hidden", true)], "{t:?}");
    }
}
