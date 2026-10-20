//! Table data view commands against the Docker test matrix (`DNT_TEST_DB=1`).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::DbKind;
use dnt_core::value::Value;
use dnt_query::Target as Pick;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::data::{
    table_data_impl, table_row_count_impl, SortDir, SortSpec, TableDataPage,
};
use dntdatabase::run_events::{EventSink, RunOptions};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open(state: &AppState, t: Target, read_only: bool) -> Uuid {
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
        env_tag: None,
        read_only,
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

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::UInt(n) => *n as i64,
        other => panic!("not an integer: {other:?}"),
    }
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => panic!("not text: {other:?}"),
    }
}

fn ids(page: &TableDataPage) -> Vec<i64> {
    page.rows.iter().map(|r| int(&r[0])).collect()
}

fn by(column: &str, dir: SortDir) -> Option<SortSpec> {
    Some(SortSpec {
        column: column.into(),
        dir,
    })
}

#[tokio::test]
async fn pages_sorts_and_counts_on_every_target() {
    require_db!();
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t, false).await;
        let vn = obj(t, "đơn_hàng");

        // First page of one row: there is more.
        let p1 = table_data_impl(&state, id, vn.clone(), 1, 0, by("id", SortDir::Asc))
            .await
            .unwrap();
        let names: Vec<&str> = p1.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "ten", "ghi_chu"], "{t:?}");
        assert_eq!(ids(&p1), [1], "{t:?}");
        assert!(p1.has_more, "{t:?}");
        assert_eq!(text(&p1.rows[0][1]), "Nguyễn Văn A", "{t:?}");

        // Offset page 2 is the last.
        let p2 = table_data_impl(&state, id, vn.clone(), 1, 1, by("id", SortDir::Asc))
            .await
            .unwrap();
        assert_eq!(ids(&p2), [2], "{t:?}");
        assert!(!p2.has_more, "{t:?}");

        // A page larger than the table; descending order.
        let desc = table_data_impl(&state, id, vn.clone(), 100, 0, by("id", SortDir::Desc))
            .await
            .unwrap();
        assert_eq!(ids(&desc), [2, 1], "{t:?}");
        assert!(!desc.has_more, "{t:?}");

        // No order at all still pages.
        let any = table_data_impl(&state, id, vn.clone(), 100, 0, None)
            .await
            .unwrap();
        assert_eq!(any.rows.len(), 2, "{t:?}");

        // Quoted names everywhere: table and ORDER BY column with ` and ".
        let weird = table_data_impl(
            &state,
            id,
            obj(t, "weird_names"),
            10,
            0,
            by("we`ird\"col", SortDir::Desc),
        )
        .await
        .unwrap();
        assert_eq!(weird.columns[1].name, "we`ird\"col", "{t:?}");
        assert_eq!(text(&weird.rows[0][1]), "quote'd \"value\"", "{t:?}");

        // An ORDER BY column that is not a column is rejected before anything runs.
        for bad in ["ID", "id; DROP TABLE weird_names", "nope"] {
            let err = table_data_impl(&state, id, vn.clone(), 10, 0, by(bad, SortDir::Asc))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, DbError::Config { field: Some(f), .. } if f == "order"),
                "{t:?} {bad}: {err:?}"
            );
        }

        // Views have data too.
        let view = ObjectRef {
            kind: ObjectKind::View,
            ..obj(t, "v_customer_orders")
        };
        let v = table_data_impl(&state, id, view, 100, 0, None)
            .await
            .unwrap();
        assert!(!v.rows.is_empty(), "{t:?}");

        let count = table_row_count_impl(&state, id, vn.clone()).await.unwrap();
        assert_eq!(count.rows, 2, "{t:?}");

        // A missing table is an error from the server, not a panic.
        assert!(
            table_data_impl(&state, id, obj(t, "no_such_table"), 10, 0, None)
                .await
                .is_err(),
            "{t:?}"
        );
        close_connection_impl(&state, id).await.unwrap();
    }
}

#[tokio::test]
async fn pg_materialized_view_has_data() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state, Target::Pg17, false).await;
    let mv = ObjectRef {
        database: FIXTURE_DB.into(),
        schema: Some("sales".into()),
        name: "mv_order_totals".into(),
        kind: ObjectKind::MaterializedView,
    };
    let page = table_data_impl(&state, id, mv.clone(), 100, 0, None)
        .await
        .unwrap();
    assert!(!page.columns.is_empty());
    table_row_count_impl(&state, id, mv).await.unwrap();
    close_connection_impl(&state, id).await.unwrap();
}

/// Plans `text` on query tab `tab` and runs it to the end.
async fn run_on_tab(state: &AppState, tab: &str, id: Uuid, text: &str) {
    let view = state
        .sessions
        .query_plan(tab, id, None, text, Pick::All)
        .await
        .unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&done);
    let emit: EventSink = Arc::new(move |e| {
        if serde_json::to_value(e).unwrap()["t"] == "done" {
            seen.store(true, Ordering::SeqCst);
        }
        true
    });
    let opts = RunOptions {
        max_rows: 100,
        continue_on_error: false,
    };
    state
        .sessions
        .query_run(view.plan_id, true, opts, emit)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !done.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the tab run did not finish");
}

/// The data view does not read inside a query tab's open transaction: a row the tab
/// inserted (uncommitted) is not visible to it, and it does not wait for the tab.
#[tokio::test]
async fn reads_outside_a_tab_transaction() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t, false).await;
        let table = match t.kind() {
            DbKind::MySql => "`dnt_fixture`.`weird_names`",
            DbKind::Postgres => "public.weird_names",
        };
        run_on_tab(
            &state,
            "tab-1",
            id,
            &format!("BEGIN; INSERT INTO {table} (id) VALUES (987654)"),
        )
        .await;
        let page = table_data_impl(&state, id, obj(t, "weird_names"), 100, 0, None)
            .await
            .unwrap();
        assert!(
            !page.rows.iter().any(|r| int(&r[0]) == 987654),
            "{t:?}: the tab's uncommitted row leaked"
        );
        run_on_tab(&state, "tab-1", id, "ROLLBACK").await;
        close_connection_impl(&state, id).await.unwrap();
    }
}

/// A plain SELECT works on a read-only connection (no confirmation, no block).
#[tokio::test]
async fn reads_on_a_read_only_connection() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t, true).await;
        let page = table_data_impl(
            &state,
            id,
            obj(t, "customers"),
            2,
            0,
            by("id", SortDir::Asc),
        )
        .await
        .unwrap();
        assert_eq!(ids(&page), [1, 2], "{t:?}");
        assert!(page.has_more, "{t:?}");
        let count = table_row_count_impl(&state, id, obj(t, "customers"))
            .await
            .unwrap();
        assert!(count.rows >= 3, "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
    }
}
