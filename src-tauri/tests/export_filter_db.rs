//! Data view filter / sort and result export against the Docker test matrix
//! (`DNT_TEST_DB=1`).

use dnt_core::config::{ConnOptions, ConnectionConfig, EnvTag, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::DbKind;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_query::Target as Pick;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::data::{
    table_data_cancel_impl, table_data_impl, table_row_count_impl, DataFilter, FilterCond,
    FilterMode, FilterOp, SortDir, SortSpec, TableDataPage,
};
use dntdatabase::commands::export::{export_query_impl, export_table_impl, Progress};
use dntdatabase::export_format::{ExportFormat, ExportOptions};
use dntdatabase::run_events::{EventSink, RunEvent, RunOptions};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

fn app_state(dir: &Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open_env(state: &AppState, t: Target, env_tag: Option<EnvTag>) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: Some(FIXTURE_DB.into()),
        env_tag,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    };
    let secrets: SecretsInput =
        serde_json::from_value(json!({ "db_password": TEST_PASSWORD })).unwrap();
    let id = create_connection_impl(state, cfg, secrets)
        .await
        .unwrap()
        .id;
    open_connection_impl(state, id, None).await.unwrap();
    id
}

fn obj(t: Target, name: &str) -> ObjectRef {
    ObjectRef {
        database: FIXTURE_DB.into(),
        schema: (t.kind() == DbKind::Postgres).then(|| "public".to_string()),
        name: name.into(),
        kind: ObjectKind::Table,
    }
}

fn cond(column: &str, op: FilterOp, values: &[&str]) -> FilterCond {
    FilterCond {
        column: column.into(),
        op,
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

fn and(conditions: Vec<FilterCond>) -> Option<DataFilter> {
    Some(DataFilter {
        mode: FilterMode::And,
        conditions,
        raw_where: None,
    })
}

fn key(column: &str, dir: SortDir) -> SortSpec {
    SortSpec {
        column: column.into(),
        dir,
    }
}

async fn page(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
) -> Result<TableDataPage, DbError> {
    table_data_impl(state, id, obj, 100, 0, sort, filter, "tab:page".into()).await
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::UInt(n) => *n as i64,
        other => panic!("not an integer: {other:?}"),
    }
}

fn ids(p: &TableDataPage) -> Vec<i64> {
    p.rows.iter().map(|r| int(&r[0])).collect()
}

/// The ids of `obj` matching `filter`, by primary key.
async fn matching(state: &AppState, id: Uuid, obj: ObjectRef, f: Option<DataFilter>) -> Vec<i64> {
    let p = page(state, id, obj.clone(), vec![], f.clone())
        .await
        .unwrap_or_else(|e| panic!("{f:?}: {e:?}"));
    let count = table_row_count_impl(state, id, obj, f, "tab:count".into())
        .await
        .unwrap();
    assert_eq!(
        count.rows,
        p.rows.len() as u64,
        "the count respects the filter"
    );
    ids(&p)
}

#[tokio::test]
async fn filters_sort_and_count_on_every_target() {
    require_db!();
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open_env(&state, t, None).await;
        let customers = obj(t, "customers");
        let orders = obj(t, "orders");
        let m = |o: &ObjectRef, f| matching(&state, id, o.clone(), f);

        assert_eq!(
            m(
                &customers,
                and(vec![cond("name", FilterOp::Contains, &["văn"])])
            )
            .await,
            [1],
            "{t:?}"
        );
        assert_eq!(
            m(
                &customers,
                and(vec![cond("name", FilterOp::StartsWith, &["Alice"])])
            )
            .await,
            [3],
            "{t:?}"
        );
        assert_eq!(
            m(
                &customers,
                and(vec![cond("email", FilterOp::EndsWith, &["@example.com"])])
            )
            .await,
            [1, 2, 3],
            "{t:?}"
        );
        // `_` is literal in contains, a wildcard in LIKE.
        assert!(
            m(
                &customers,
                and(vec![cond("email", FilterOp::Contains, &["_"])])
            )
            .await
            .is_empty(),
            "{t:?}"
        );
        assert_eq!(
            m(
                &customers,
                and(vec![cond("email", FilterOp::Like, &["_@%"])])
            )
            .await,
            [1, 2],
            "{t:?}"
        );
        assert_eq!(
            m(
                &customers,
                and(vec![cond("email", FilterOp::NotLike, &["a%"])])
            )
            .await,
            [2],
            "{t:?}"
        );
        assert_eq!(
            m(
                &customers,
                and(vec![cond("created_at", FilterOp::IsNull, &[])])
            )
            .await,
            [3],
            "{t:?}"
        );
        assert_eq!(
            m(&orders, and(vec![cond("total", FilterOp::Gt, &["100"])])).await,
            [1, 2],
            "{t:?}"
        );
        assert_eq!(
            m(&orders, and(vec![cond("total", FilterOp::Le, &["100.50"])])).await,
            [1, 3],
            "{t:?}"
        );
        // An enum on PG (the parameter takes the column's type).
        assert_eq!(
            m(&orders, and(vec![cond("status", FilterOp::Eq, &["paid"])])).await,
            [2],
            "{t:?}"
        );
        assert_eq!(
            m(
                &orders,
                and(vec![cond("status", FilterOp::In, &["new", "shipped"])])
            )
            .await,
            [1, 3],
            "{t:?}"
        );
        assert_eq!(
            m(
                &orders,
                and(vec![cond(
                    "created_at",
                    FilterOp::Between,
                    &["2024-02-01 00:00:00", "2024-02-02 23:59:59"]
                )])
            )
            .await,
            [1, 2],
            "{t:?}"
        );
        assert_eq!(
            m(
                &orders,
                Some(DataFilter {
                    mode: FilterMode::Or,
                    conditions: vec![
                        cond("id", FilterOp::Eq, &["1"]),
                        cond("id", FilterOp::Eq, &["3"]),
                    ],
                    raw_where: None,
                })
            )
            .await,
            [1, 3],
            "{t:?}"
        );
        assert_eq!(
            m(
                &orders,
                and(vec![
                    cond("customer_id", FilterOp::Eq, &["1"]),
                    cond("total", FilterOp::Ne, &["250"]),
                ])
            )
            .await,
            [1],
            "{t:?}"
        );

        // Values never reach the SQL text: an injection attempt matches nothing.
        let evil = "x'); DROP TABLE customers; -- \" `";
        assert!(
            m(&customers, and(vec![cond("name", FilterOp::Eq, &[evil])]))
                .await
                .is_empty(),
            "{t:?}"
        );
        assert!(
            m(
                &customers,
                and(vec![cond("name", FilterOp::Contains, &[evil])])
            )
            .await
            .is_empty(),
            "{t:?}"
        );
        assert_eq!(m(&customers, None).await, [1, 2, 3], "{t:?}");

        // Quoted column names work in a filter.
        assert_eq!(
            m(
                &obj(t, "weird_names"),
                and(vec![cond(
                    "we`ird\"col",
                    FilterOp::Contains,
                    &["\"value\""]
                )])
            )
            .await,
            [1],
            "{t:?}"
        );

        // Unknown columns and bad arity are refused before anything runs.
        for f in [
            and(vec![cond("nope", FilterOp::Eq, &["1"])]),
            and(vec![cond(
                "id; DROP TABLE customers",
                FilterOp::IsNull,
                &[],
            )]),
            and(vec![cond("id", FilterOp::Between, &["1"])]),
        ] {
            let err = page(&state, id, customers.clone(), vec![], f)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, DbError::Config { field: Some(f), .. } if f == "filter"),
                "{t:?}: {err:?}"
            );
        }

        // Several sort keys, then the primary key.
        let p = page(
            &state,
            id,
            orders.clone(),
            vec![
                key("customer_id", SortDir::Asc),
                key("total", SortDir::Desc),
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(ids(&p), [2, 1, 3], "{t:?}");
        // Filter and sort together.
        let p = page(
            &state,
            id,
            orders.clone(),
            vec![key("total", SortDir::Desc)],
            and(vec![cond("total", FilterOp::Ge, &["75.25"])]),
        )
        .await
        .unwrap();
        assert_eq!(ids(&p), [2, 1, 3], "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
    }
}

#[tokio::test]
async fn postgres_typed_parameters() {
    require_db!();
    let t = Target::Pg17;
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open_env(&state, t, None).await;
    let types = obj(t, "all_types");
    let m = |f| matching(&state, id, types.clone(), f);
    for (column, op, values) in [
        ("c_boolean", FilterOp::Eq, vec!["true"]),
        ("c_boolean", FilterOp::Eq, vec!["t"]),
        ("c_int", FilterOp::Ge, vec!["2147483647"]),
        ("c_bigint", FilterOp::Eq, vec!["9223372036854775807"]),
        ("c_numeric", FilterOp::Gt, vec!["1e29"]),
        (
            "c_date",
            FilterOp::Between,
            vec!["9999-01-01", "9999-12-31"],
        ),
        (
            "c_timestamptz",
            FilterOp::Eq,
            vec!["2038-01-19 03:14:07+00"],
        ),
        (
            "c_uuid",
            FilterOp::Eq,
            vec!["123e4567-e89b-12d3-a456-426614174000"],
        ),
        (
            "c_uuid",
            FilterOp::In,
            vec!["123E4567-E89B-12D3-A456-426614174000"],
        ),
        // jsonb equality ignores key order; json has none, so it compares as text.
        (
            "c_jsonb",
            FilterOp::Eq,
            vec![r#"{"tags": [1, 2, 3], "name": "Nguyễn Văn A"}"#],
        ),
        ("c_json", FilterOp::Contains, vec!["\"tags\": [1, 2, 3]"]),
        ("c_int_array", FilterOp::Eq, vec!["{1,2,3}"]),
        ("c_enum", FilterOp::Eq, vec!["shipped"]),
        ("c_bytea", FilterOp::Eq, vec!["\\x00ff10deadbeef"]),
    ] {
        assert_eq!(
            m(and(vec![cond(column, op, &values)])).await,
            [1],
            "{column} {op:?} {values:?}"
        );
    }
    assert_eq!(
        m(and(vec![cond("c_json", FilterOp::IsNull, &[])])).await,
        [2]
    );
    // A malformed value is the server's own error, never a different query.
    let err = page(
        &state,
        id,
        types.clone(),
        vec![],
        and(vec![cond("c_int", FilterOp::Eq, &["abc"])]),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, DbError::Server { .. }), "{err:?}");
    close_connection_impl(&state, id).await.unwrap();
}

struct NullSink;

impl RowSink for NullSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn admin(t: Target, sql: &str) {
    let driver: Box<dyn Driver> = match t.kind() {
        DbKind::MySql => Box::new(MySqlDriver),
        DbKind::Postgres => Box::new(PgDriver),
    };
    let session = driver.connect(&t.resolved(Some(FIXTURE_DB))).await.unwrap();
    session
        .execute(sql, ExecOptions::default(), &mut NullSink)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    session.close().await;
}

fn opts(format: ExportFormat) -> ExportOptions {
    ExportOptions {
        format,
        delimiter: ",".into(),
        header: true,
        quote_all: false,
        bom: false,
        table_name: "t".into(),
        formula_guard: false,
    }
}

fn collect() -> (Arc<Mutex<Vec<u64>>>, Progress) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    (seen, Box::new(move |n| s.lock().unwrap().push(n)))
}

#[tokio::test]
async fn export_all_streams_filtered_rows_to_files() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open_env(&state, t, None).await;

        // CSV of a filtered, sorted table.
        let path = dir.path().join("customers.csv");
        let (seen, progress) = collect();
        let summary = export_table_impl(
            &state,
            id,
            obj(t, "customers"),
            vec![key("id", SortDir::Desc)],
            and(vec![cond("id", FilterOp::In, &["1", "2"])]),
            opts(ExportFormat::Csv),
            path.to_string_lossy().into(),
            "export:1".into(),
            progress,
        )
        .await
        .unwrap();
        assert_eq!(summary.rows, 2, "{t:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines[0], "id,name,email,created_at", "{t:?}");
        assert!(
            lines[1].starts_with("2,Trần Thị B,b@example.com,2024-01-02T11:30:00"),
            "{t:?}: {text}"
        );
        assert!(lines[2].starts_with("1,Nguyễn Văn A,"), "{t:?}: {text}");
        assert_eq!(seen.lock().unwrap().last(), Some(&2), "{t:?}");
        assert!(!dir.path().join("customers.csv.dntpart").exists());

        // Binary values are exported whole (1000 bytes, not the 64-byte preview).
        let path = dir.path().join("blob.csv");
        export_table_impl(
            &state,
            id,
            obj(t, "bin_blob"),
            vec![],
            None,
            opts(ExportFormat::Csv),
            path.to_string_lossy().into(),
            "export:2".into(),
            collect().1,
        )
        .await
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let blob = text
            .split("\r\n")
            .nth(1)
            .unwrap()
            .split(',')
            .nth(1)
            .unwrap();
        assert_eq!(blob, format!("0x{}", "00ff".repeat(500)), "{t:?}");

        // JSON, Excel and SQL of every order.
        for (format, name) in [
            (ExportFormat::Json, "orders.json"),
            (ExportFormat::Xlsx, "orders.xlsx"),
            (ExportFormat::Sql, "orders.sql"),
            (ExportFormat::Markdown, "orders.md"),
        ] {
            let path = dir.path().join(name);
            let s = export_table_impl(
                &state,
                id,
                obj(t, "orders"),
                vec![],
                None,
                opts(format),
                path.to_string_lossy().into(),
                "export:3".into(),
                collect().1,
            )
            .await
            .unwrap();
            assert_eq!(s.rows, 3, "{t:?} {format:?}");
            let bytes = std::fs::read(&path).unwrap();
            match format {
                ExportFormat::Json => {
                    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(v.as_array().unwrap().len(), 3);
                    assert_eq!(v[1]["status"], "paid", "{t:?}");
                    assert_eq!(v[1]["total"], json!(250.00), "{t:?}");
                }
                ExportFormat::Xlsx => assert_eq!(&bytes[..2], b"PK"),
                ExportFormat::Sql => {
                    let text = String::from_utf8(bytes).unwrap();
                    let inserts = text.lines().filter(|l| l.starts_with("INSERT INTO "));
                    assert_eq!(inserts.count(), 3, "{t:?}");
                    assert_eq!(
                        text.starts_with("SET NAMES utf8mb4;\n"),
                        t.kind() == DbKind::MySql,
                        "{t:?}"
                    );
                    assert!(text.contains("'paid'"), "{t:?}: {text}");
                }
                _ => {
                    let text = String::from_utf8(bytes).unwrap();
                    assert_eq!(text.lines().count(), 5, "{t:?}");
                }
            }
        }
        close_connection_impl(&state, id).await.unwrap();
    }
}

/// Plans `text` on query tab `tab` (not run): its plan id.
async fn plan(state: &AppState, tab: &str, id: Uuid, text: &str) -> Uuid {
    state
        .sessions
        .query_plan(tab, id, None, text, Pick::All)
        .await
        .unwrap()
        .plan_id
}

#[tokio::test]
async fn export_query_reads_only_read_statements_again() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        // On Production a plain read needs no confirmation.
        let id = open_env(&state, t, Some(EnvTag::Production)).await;
        let read = plan(
            &state,
            "tab-1",
            id,
            "SELECT id, name FROM customers ORDER BY id DESC",
        )
        .await;
        let path = dir.path().join("q.json");
        let s = export_query_impl(
            &state,
            read,
            0,
            opts(ExportFormat::Json),
            path.to_string_lossy().into(),
            Uuid::new_v4(),
            collect().1,
        )
        .await
        .unwrap();
        assert_eq!(s.rows, 3, "{t:?}");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v[0], json!({"id": 3, "name": "Alice Smith"}), "{t:?}");

        // Writes, and reads the guard wants confirmed (Explain Analyze on Production),
        // are never run again for an export.
        for text in [
            "DELETE FROM customers WHERE id = -1",
            "EXPLAIN ANALYZE SELECT 1",
        ] {
            let p = plan(&state, "tab-2", id, text).await;
            let err = export_query_impl(
                &state,
                p,
                0,
                opts(ExportFormat::Csv),
                dir.path().join("no.csv").to_string_lossy().into(),
                Uuid::new_v4(),
                collect().1,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&err, DbError::Config { field: Some(f), .. } if f == "scope"),
                "{t:?} {text}: {err:?}"
            );
            assert!(!dir.path().join("no.csv").exists());
        }
        close_connection_impl(&state, id).await.unwrap();
    }
}

#[tokio::test]
async fn a_cancelled_export_leaves_no_file() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let name = format!("dnt_exp_slow_{}_{nanos}", std::process::id());
        let (qualified, body) = match t.kind() {
            DbKind::MySql => (format!("`{FIXTURE_DB}`.`{name}`"), "SELECT SLEEP(8) AS s"),
            DbKind::Postgres => (format!("public.{name}"), "SELECT 1 AS s FROM pg_sleep(8)"),
        };
        admin(t, &format!("CREATE VIEW {qualified} AS {body}")).await;
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open_env(&state, t, None).await;
        let path = dir.path().join("slow.csv");
        let view = ObjectRef {
            kind: ObjectKind::View,
            ..obj(t, &name)
        };
        let (result, ()) = tokio::join!(
            export_table_impl(
                &state,
                id,
                view,
                vec![],
                None,
                opts(ExportFormat::Csv),
                path.to_string_lossy().into(),
                "export:slow".into(),
                collect().1,
            ),
            async {
                tokio::time::sleep(Duration::from_millis(700)).await;
                assert!(table_data_cancel_impl(&state, id, "export:slow".into())
                    .await
                    .unwrap());
            }
        );
        assert!(
            matches!(result, Err(DbError::Cancelled { .. })),
            "{t:?}: {result:?}"
        );
        assert!(!path.exists(), "{t:?}");
        assert!(!dir.path().join("slow.csv.dntpart").exists(), "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
        admin(t, &format!("DROP VIEW {qualified}")).await;
    }
}

/// SQL files import back to the same values whatever the importing session's string
/// rules: MySQL under `NO_BACKSLASH_ESCAPES`, PostgreSQL with
/// `standard_conforming_strings` off (backslashes, quotes, NUL, binary, `bytea[]`).
#[tokio::test]
async fn sql_exports_import_back_under_any_string_mode() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let src = unique("dnt_rs");
        let dst = unique("dnt_rd");
        let (create, rows, mode, same) = match t.kind() {
            DbKind::MySql => (
                "(id INT PRIMARY KEY, s TEXT, b BLOB)",
                "(1, 'a\\\\b', X'00ff5c27'), (2, 'it''s', NULL), (3, CONCAT('nul', CHAR(0 USING utf8mb4), '\\\\'), X'')",
                "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'",
                "a.s <=> b.s AND a.b <=> b.b",
            ),
            DbKind::Postgres => (
                "(id INT PRIMARY KEY, s TEXT, b BYTEA, ba BYTEA[])",
                "(1, E'a\\\\b', '\\x00ff5c27', ARRAY['\\x01'::bytea, NULL]), (2, 'it''s', NULL, NULL), (3, E'\\\\x', '\\x', '{}')",
                "SET standard_conforming_strings = off",
                "a.s IS NOT DISTINCT FROM b.s AND a.b IS NOT DISTINCT FROM b.b \
                 AND a.ba IS NOT DISTINCT FROM b.ba",
            ),
        };
        let q = |name: &str| match t.kind() {
            DbKind::MySql => format!("`{FIXTURE_DB}`.`{name}`"),
            DbKind::Postgres => format!("public.{name}"),
        };
        admin(t, &format!("CREATE TABLE {} {create}", q(&src))).await;
        admin(t, &format!("CREATE TABLE {} {create}", q(&dst))).await;
        admin(t, &format!("INSERT INTO {} VALUES {rows}", q(&src))).await;

        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open_env(&state, t, None).await;
        let path = dir.path().join("rt.sql");
        let s = export_table_impl(
            &state,
            id,
            obj(t, &src),
            vec![],
            None,
            ExportOptions {
                table_name: dst.clone(),
                ..opts(ExportFormat::Sql)
            },
            path.to_string_lossy().into(),
            "export:rt".into(),
            collect().1,
        )
        .await
        .unwrap();
        assert_eq!(s.rows, 3, "{t:?}");
        close_connection_impl(&state, id).await.unwrap();

        let file = std::fs::read_to_string(&path).unwrap();
        admin(t, &format!("{mode};\n{file}")).await;
        let matching = scalar(
            t,
            &format!(
                "SELECT COUNT(*) FROM {} a JOIN {} b ON a.id = b.id WHERE {same}",
                q(&src),
                q(&dst)
            ),
        )
        .await;
        assert_eq!(int(&matching), 3, "{t:?}: {file}");
        admin(t, &format!("DROP TABLE {}", q(&src))).await;
        admin(t, &format!("DROP TABLE {}", q(&dst))).await;
    }
}

// ---- "All rows" of a query result runs on the tab's own session ----

/// A name no other test run uses (short: MySQL identifiers hold 64 characters).
fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos % 1_000_000)
}

/// The first cell of the first row of `sql`, as the admin sees it.
async fn scalar(t: Target, sql: &str) -> Value {
    #[derive(Default)]
    struct First(Option<Value>);
    impl RowSink for First {
        fn result_start(&mut self, _cols: &[ColumnMeta]) {}
        fn rows(&mut self, batch: Vec<Vec<Value>>) {
            if self.0.is_none() {
                self.0 = batch.into_iter().next().and_then(|r| r.into_iter().next());
            }
        }
        fn result_end(&mut self, _summary: &ResultSummary) {}
    }
    let driver: Box<dyn Driver> = match t.kind() {
        DbKind::MySql => Box::new(MySqlDriver),
        DbKind::Postgres => Box::new(PgDriver),
    };
    let session = driver.connect(&t.resolved(Some(FIXTURE_DB))).await.unwrap();
    let mut first = First::default();
    session
        .execute(sql, ExecOptions::default(), &mut first)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    session.close().await;
    first.0.unwrap_or(Value::Null)
}

/// Plans `text` (all of it) on tab `tab` and runs it to its end, as the user does; its
/// plan id. Every statement must succeed.
async fn run_on_tab(state: &AppState, tab: &str, id: Uuid, text: &str) -> Uuid {
    let view = state
        .sessions
        .query_plan(tab, id, None, text, Pick::All)
        .await
        .unwrap();
    let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let out = Arc::clone(&events);
    let emit: EventSink = Arc::new(move |e: RunEvent| {
        out.lock().unwrap().push(serde_json::to_value(e).unwrap());
        true
    });
    let opts = RunOptions {
        max_rows: 10_000,
        continue_on_error: false,
    };
    state
        .sessions
        .query_run(view.plan_id, true, opts, emit)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !events.lock().unwrap().iter().any(|e| e["t"] == "done") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{text}: no done"));
    let events = events.lock().unwrap();
    assert!(
        !events.iter().any(|e| e["t"] == "statement_error"),
        "{text}: {events:?}"
    );
    view.plan_id
}

async fn export_csv(
    state: &AppState,
    plan_id: Uuid,
    index: u32,
    path: &Path,
) -> Result<(u64, String), DbError> {
    let s = export_query_impl(
        state,
        plan_id,
        index,
        ExportOptions {
            delimiter: "|".into(),
            ..opts(ExportFormat::Csv)
        },
        path.to_string_lossy().into(),
        Uuid::new_v4(),
        collect().1,
    )
    .await?;
    Ok((s.rows, std::fs::read_to_string(path).unwrap()))
}

fn is_scope_error(e: &DbError) -> bool {
    matches!(e, DbError::Config { field: Some(f), .. } if f == "scope")
}

/// The security review's repro: a statement that is ONE read under the tab's `sql_mode`
/// but several statements under the default one. Exporting all rows must run it under
/// the tab's mode (never `DELETE`), also on Production.
#[tokio::test]
async fn mysql_export_runs_under_the_tabs_sql_mode() {
    require_db!();
    let t = Target::MySql8;
    let table = unique("dnt_v");
    let qualified = format!("`{FIXTURE_DB}`.`{table}`");
    admin(t, &format!("CREATE TABLE {qualified} (id INT PRIMARY KEY)")).await;
    admin(t, &format!("INSERT INTO {qualified} VALUES (1), (2), (3)")).await;
    let count_sql = format!("SELECT COUNT(*) FROM {qualified}");
    let count = || scalar(t, &count_sql);
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open_env(&state, t, Some(EnvTag::Production)).await;

    // NO_BACKSLASH_ESCAPES: `'\'` is a whole string.
    run_on_tab(
        &state,
        "nbe",
        id,
        "SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_BACKSLASH_ESCAPES')",
    )
    .await;
    let text = format!("SELECT '\\' AS a, '; DELETE FROM {table}; SELECT 1 -- ' AS b");
    let shown = run_on_tab(&state, "nbe", id, &text).await;
    let path = dir.path().join("nbe.csv");
    let (rows, csv) = export_csv(&state, shown, 0, &path).await.unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        csv,
        format!("a|b\r\n\\|; DELETE FROM {table}; SELECT 1 -- \r\n")
    );
    assert_eq!(int(&count().await), 3, "nothing was deleted");

    // ANSI_QUOTES: `"a\"` is an identifier (a string with an escaped quote otherwise).
    run_on_tab(
        &state,
        "ansi",
        id,
        "SET SESSION sql_mode = CONCAT(@@sql_mode, ',ANSI_QUOTES')",
    )
    .await;
    let text = format!("SELECT 1 AS \"a\\\", 2 AS \"; DELETE FROM {table}; SELECT 1 -- \"");
    let shown = run_on_tab(&state, "ansi", id, &text).await;
    let path = dir.path().join("ansi.csv");
    let (rows, csv) = export_csv(&state, shown, 0, &path).await.unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        csv,
        format!("a\\|; DELETE FROM {table}; SELECT 1 -- \r\n1|2\r\n")
    );
    assert_eq!(int(&count().await), 3, "nothing was deleted");

    // Defense in depth: the tab's mode changed after the result was read (the script's
    // last statement): the statement now splits differently, so it is not sent.
    let script = format!(
        "SELECT '\\' AS a, '; DELETE FROM {table}; SELECT 1 -- ' AS b; \
         SET SESSION sql_mode = DEFAULT"
    );
    let shown = run_on_tab(&state, "nbe", id, &script).await;
    let path = dir.path().join("changed.csv");
    let err = export_csv(&state, shown, 0, &path).await.unwrap_err();
    assert!(is_scope_error(&err), "{err:?}");
    assert!(!path.exists());
    assert_eq!(int(&count().await), 3, "nothing was deleted");

    close_connection_impl(&state, id).await.unwrap();
    admin(t, &format!("DROP TABLE {qualified}")).await;
}

/// User variables (MySQL) and `search_path` (PostgreSQL) of the tab's session apply:
/// the file holds the rows the user saw.
#[tokio::test]
async fn export_keeps_the_tabs_session_state() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();

    let t = Target::MySql8;
    let state = app_state(dir.path());
    let id = open_env(&state, t, None).await;
    run_on_tab(&state, "vars", id, "SET @c = 1").await;
    let shown = run_on_tab(
        &state,
        "vars",
        id,
        "SELECT id FROM customers WHERE id > @c ORDER BY id",
    )
    .await;
    let (rows, csv) = export_csv(&state, shown, 0, &dir.path().join("vars.csv"))
        .await
        .unwrap();
    assert_eq!((rows, csv.as_str()), (2, "id\r\n2\r\n3\r\n"));
    close_connection_impl(&state, id).await.unwrap();

    let t = Target::Pg17;
    let schema = unique("dnt_s");
    admin(t, &format!("CREATE SCHEMA {schema}")).await;
    admin(
        t,
        &format!("CREATE TABLE {schema}.only_here (v TEXT); INSERT INTO {schema}.only_here VALUES ('x'), ('y')"),
    )
    .await;
    let state = app_state(&dir.path().join("pg"));
    let id = open_env(&state, t, None).await;
    run_on_tab(&state, "path", id, &format!("SET search_path TO {schema}")).await;
    let shown = run_on_tab(&state, "path", id, "SELECT v FROM only_here ORDER BY v").await;
    let (rows, csv) = export_csv(&state, shown, 0, &dir.path().join("path.csv"))
        .await
        .unwrap();
    assert_eq!((rows, csv.as_str()), (2, "v\r\nx\r\ny\r\n"));
    close_connection_impl(&state, id).await.unwrap();
    admin(t, &format!("DROP SCHEMA {schema} CASCADE")).await;
}

/// The export is a run of its tab: the tab is busy meanwhile (another export or a plan
/// is refused), `query_cancel(run_id)` stops it with no file left behind, and the tab is
/// usable afterwards.
#[tokio::test]
async fn query_export_is_a_cancellable_run_of_its_tab() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open_env(&state, t, None).await;
        let slow = match t.kind() {
            DbKind::MySql => "SELECT SLEEP(8) AS s",
            DbKind::Postgres => "SELECT 1 AS s FROM pg_sleep(8)",
        };
        let p = plan(&state, "slow", id, slow).await;
        let path = dir.path().join("slow.csv");
        let busy_path = dir.path().join("busy.csv");
        let run_id = Uuid::new_v4();
        let started = std::time::Instant::now();
        let (result, ()) = tokio::join!(
            export_query_impl(
                &state,
                p,
                0,
                opts(ExportFormat::Csv),
                path.to_string_lossy().into(),
                run_id,
                collect().1,
            ),
            async {
                tokio::time::sleep(Duration::from_millis(700)).await;
                let part = dir.path().join("slow.csv.dntpart");
                assert!(part.exists(), "{t:?}: the running export's temp file");
                // Refused for the busy tab before any file is opened: neither a temp
                // file of its own nor the running export's (same target) is touched.
                for target in [&busy_path, &path] {
                    let err = export_csv(&state, p, 0, target).await.unwrap_err();
                    assert!(
                        matches!(&err, DbError::Config { field: Some(f), .. } if f == "tab"),
                        "{t:?}: {err:?}"
                    );
                }
                assert!(!dir.path().join("busy.csv.dntpart").exists(), "{t:?}");
                assert!(part.exists(), "{t:?}: still the running export's");
                let err = state
                    .sessions
                    .query_plan("slow", id, None, "SELECT 1", Pick::All)
                    .await
                    .unwrap_err();
                assert!(matches!(err, DbError::Busy), "{t:?}: {err:?}");
                state.sessions.query_cancel(run_id).await.unwrap();
            }
        );
        assert!(
            matches!(result, Err(DbError::Cancelled { .. })),
            "{t:?}: {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(7), "{t:?}");
        for p in [&path, &busy_path] {
            assert!(!p.exists(), "{t:?} {p:?}");
        }
        assert!(!dir.path().join("slow.csv.dntpart").exists(), "{t:?}");
        // The tab runs again.
        run_on_tab(&state, "slow", id, "SELECT 1").await;
        close_connection_impl(&state, id).await.unwrap();
    }
}
