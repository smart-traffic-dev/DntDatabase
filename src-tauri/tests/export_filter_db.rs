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
                    assert_eq!(text.lines().count(), 3, "{t:?}");
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
            "export:q".into(),
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
                "export:no".into(),
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
