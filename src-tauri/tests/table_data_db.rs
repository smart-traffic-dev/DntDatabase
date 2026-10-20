//! Table data view commands against the Docker test matrix (`DNT_TEST_DB=1`).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::server::DbKind;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_query::Target as Pick;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::browse::{list_objects_impl, table_detail_impl};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::data::{
    table_data_cancel_impl, table_data_impl, table_row_count_impl, SortDir, SortSpec,
    TableDataPage, TableRowCount,
};
use dntdatabase::data_sessions::MAX_CELL_BYTES;
use dntdatabase::run_events::{EventSink, RunOptions};
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{default_drivers, AppPaths, AppState};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open_with(state: &AppState, t: Target, read_only: bool, options: ConnOptions) -> Uuid {
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
        options,
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

async fn open(state: &AppState, t: Target, read_only: bool) -> Uuid {
    open_with(state, t, read_only, ConnOptions::default()).await
}

fn obj(t: Target, name: &str) -> ObjectRef {
    ObjectRef {
        database: FIXTURE_DB.into(),
        schema: (t.kind() == DbKind::Postgres).then(|| "public".to_string()),
        name: name.into(),
        kind: ObjectKind::Table,
    }
}

fn view(t: Target, name: &str) -> ObjectRef {
    ObjectRef {
        kind: ObjectKind::View,
        ..obj(t, name)
    }
}

async fn page(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    limit: u32,
    offset: u64,
    order: Option<SortSpec>,
) -> Result<TableDataPage, DbError> {
    table_data_impl(state, id, obj, limit, offset, order, "tab:page".into()).await
}

async fn count(state: &AppState, id: Uuid, obj: ObjectRef) -> Result<TableRowCount, DbError> {
    table_row_count_impl(state, id, obj, "tab:count".into()).await
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

struct NullSink;

impl RowSink for NullSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Runs `sql` on a fresh admin session to the fixture database of `t`.
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

/// A view of the fixture database, dropped again by [`TempView::drop_it`].
struct TempView {
    t: Target,
    name: String,
}

impl TempView {
    /// `body` is the view's SELECT.
    async fn create(t: Target, tag: &str, body: &str) -> TempView {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let name = format!("dnt_k4_{tag}_{}_{nanos}", std::process::id());
        let qualified = match t.kind() {
            DbKind::MySql => format!("`{FIXTURE_DB}`.`{name}`"),
            DbKind::Postgres => format!("public.{name}"),
        };
        admin(t, &format!("CREATE VIEW {qualified} AS {body}")).await;
        TempView { t, name }
    }

    async fn drop_it(self) {
        let qualified = match self.t.kind() {
            DbKind::MySql => format!("`{FIXTURE_DB}`.`{}`", self.name),
            DbKind::Postgres => format!("public.{}", self.name),
        };
        admin(self.t, &format!("DROP VIEW {qualified}")).await;
    }
}

fn sleep_body(t: Target, secs: u32) -> String {
    match t.kind() {
        DbKind::MySql => format!("SELECT SLEEP({secs}) AS s"),
        DbKind::Postgres => format!("SELECT 1 AS s FROM pg_sleep({secs})"),
    }
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
        let p1 = page(&state, id, vn.clone(), 1, 0, by("id", SortDir::Asc))
            .await
            .unwrap();
        let names: Vec<&str> = p1.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "ten", "ghi_chu"], "{t:?}");
        assert_eq!(ids(&p1), [1], "{t:?}");
        assert!(p1.has_more, "{t:?}");
        assert!(p1.key_order.is_empty(), "{t:?}");
        assert_eq!(text(&p1.rows[0][1]), "Nguyễn Văn A", "{t:?}");

        // Offset page 2 is the last.
        let p2 = page(&state, id, vn.clone(), 1, 1, by("id", SortDir::Asc))
            .await
            .unwrap();
        assert_eq!(ids(&p2), [2], "{t:?}");
        assert!(!p2.has_more, "{t:?}");

        // A page larger than the table; descending order.
        let desc = page(&state, id, vn.clone(), 100, 0, by("id", SortDir::Desc))
            .await
            .unwrap();
        assert_eq!(ids(&desc), [2, 1], "{t:?}");
        assert!(!desc.has_more, "{t:?}");

        // Without a sort the page is ordered by the primary key (stable paging).
        for offset in [0, 1] {
            let p = page(&state, id, vn.clone(), 1, offset, None).await.unwrap();
            assert_eq!(p.key_order, ["id"], "{t:?}");
            assert_eq!(ids(&p), [offset as i64 + 1], "{t:?}");
        }

        // Quoted names everywhere: table and ORDER BY column with ` and ".
        let weird = page(
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
            let err = page(&state, id, vn.clone(), 10, 0, by(bad, SortDir::Asc))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, DbError::Config { field: Some(f), .. } if f == "order"),
                "{t:?} {bad}: {err:?}"
            );
        }

        // Views have data too (and no primary key to order by).
        let v = page(&state, id, view(t, "v_customer_orders"), 100, 0, None)
            .await
            .unwrap();
        assert!(!v.rows.is_empty(), "{t:?}");
        assert!(v.key_order.is_empty(), "{t:?}");

        assert_eq!(
            count(&state, id, vn.clone()).await.unwrap().rows,
            2,
            "{t:?}"
        );

        // A missing table is an error from the server, not a panic.
        assert!(
            page(&state, id, obj(t, "no_such_table"), 10, 0, None)
                .await
                .is_err(),
            "{t:?}"
        );
        close_connection_impl(&state, id).await.unwrap();
    }
}

#[tokio::test]
async fn pg_materialized_view_has_data_and_json_is_not_sortable() {
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
    let p = page(&state, id, mv.clone(), 100, 0, None).await.unwrap();
    assert!(!p.columns.is_empty());
    count(&state, id, mv).await.unwrap();

    let types = obj(Target::Pg17, "all_types");
    let err = page(&state, id, types.clone(), 10, 0, by("c_json", SortDir::Asc))
        .await
        .unwrap_err();
    match err {
        DbError::Config { field, message } => {
            assert_eq!(field.as_deref(), Some("order"));
            assert!(message.starts_with("Can't sort by \"c_json\""), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // jsonb has an order.
    page(&state, id, types, 10, 0, by("c_jsonb", SortDir::Desc))
        .await
        .unwrap();
    close_connection_impl(&state, id).await.unwrap();
}

/// A slow page holds neither the tree nor Info: browsing on the same connection
/// answers at once, and cancelling the page stops it quickly.
#[tokio::test]
async fn slow_page_does_not_block_browsing_and_cancel_stops_it() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let slow = TempView::create(t, "slow", &sleep_body(t, 8)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t, false).await;
        let scope = Scope {
            database: FIXTURE_DB.into(),
            schema: (t.kind() == DbKind::Postgres).then(|| "public".into()),
        };
        let started = Instant::now();
        let (slow_result, (browse_ms, cancel_at)) = tokio::join!(
            table_data_impl(
                &state,
                id,
                view(t, &slow.name),
                10,
                0,
                None,
                "tab-slow:page".into()
            ),
            async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let browse = Instant::now();
                list_objects_impl(&state, id, scope.clone(), ObjectKind::Table)
                    .await
                    .unwrap();
                table_detail_impl(&state, id, obj(t, "customers"))
                    .await
                    .unwrap();
                let browse_ms = browse.elapsed().as_millis();
                assert!(
                    table_data_cancel_impl(&state, id, "tab-slow:page".into())
                        .await
                        .unwrap(),
                    "{t:?}: the slow page was not running"
                );
                (browse_ms, started.elapsed())
            }
        );
        assert!(browse_ms < 2_000, "{t:?}: browsing waited {browse_ms} ms");
        let stopped_after_cancel = started.elapsed() - cancel_at;
        assert!(
            stopped_after_cancel < Duration::from_secs(3),
            "{t:?}: the cancelled page ran {stopped_after_cancel:?} more"
        );
        let err = slow_result.expect_err("a cancelled page fails");
        assert!(matches!(err, DbError::Cancelled { .. }), "{t:?}: {err:?}");
        // The connection still serves pages afterwards.
        page(&state, id, obj(t, "customers"), 2, 0, None)
            .await
            .unwrap();
        close_connection_impl(&state, id).await.unwrap();
        slow.drop_it().await;
    }
}

/// A new request under the same key supersedes (cancels) the one still running.
#[tokio::test]
async fn a_new_request_of_the_same_key_cancels_the_previous() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let slow = TempView::create(t, "super", &sleep_body(t, 8)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t, false).await;
        let started = Instant::now();
        let (first, second) =
            tokio::join!(page(&state, id, view(t, &slow.name), 10, 0, None), async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                page(&state, id, obj(t, "customers"), 2, 0, None).await
            });
        assert!(
            matches!(first, Err(DbError::Cancelled { .. })),
            "{t:?}: {first:?}"
        );
        assert_eq!(second.unwrap().rows.len(), 2, "{t:?}");
        assert!(started.elapsed() < Duration::from_secs(4), "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
        slow.drop_it().await;
    }
}

/// The connection's query timeout bounds a data read.
#[tokio::test]
async fn data_reads_time_out() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let slow = TempView::create(t, "timeout", &sleep_body(t, 8)).await;
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let options = ConnOptions {
            query_timeout_s: 1,
            ..ConnOptions::default()
        };
        let id = open_with(&state, t, false, options).await;
        let started = Instant::now();
        let err = page(&state, id, view(t, &slow.name), 10, 0, None)
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Timeout { .. }), "{t:?}: {err:?}");
        assert!(started.elapsed() < Duration::from_secs(5), "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
        slow.drop_it().await;
    }
}

/// A 1 MB text cell comes back cut to 64 KB and counted.
#[tokio::test]
async fn long_cells_are_truncated() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let body = match t.kind() {
            DbKind::MySql => "SELECT REPEAT('x', 1048576) AS t, 1 AS n",
            DbKind::Postgres => "SELECT repeat('x', 1048576) AS t, 1 AS n",
        };
        let big = TempView::create(t, "big", body).await;
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t, false).await;
        let p = page(&state, id, view(t, &big.name), 10, 0, None)
            .await
            .unwrap();
        assert_eq!(text(&p.rows[0][0]).len(), MAX_CELL_BYTES, "{t:?}");
        assert_eq!(p.truncated_cells, 1, "{t:?}");
        assert!(!p.size_capped, "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
        big.drop_it().await;
    }
}

/// A page stops at its byte budget, says so, and the next page continues after it.
#[tokio::test]
async fn the_page_byte_cap_is_honoured() {
    require_db!();
    for t in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let mut state = app_state(dir.path());
        state.sessions = SessionManager::new(
            Arc::clone(&state.repo),
            Arc::clone(&state.secrets),
            KnownHosts::new(state.paths.known_hosts.clone()),
            default_drivers(),
        )
        .with_data_limits(Duration::from_secs(30), MAX_CELL_BYTES, 60);
        let id = open(&state, t, false).await;
        let customers = obj(t, "customers");
        let p1 = page(
            &state,
            id,
            customers.clone(),
            100,
            0,
            by("id", SortDir::Asc),
        )
        .await
        .unwrap();
        assert_eq!(ids(&p1), [1], "{t:?}");
        assert!(p1.size_capped && p1.has_more, "{t:?}");
        let p2 = page(&state, id, customers, 100, 1, by("id", SortDir::Asc))
            .await
            .unwrap();
        assert_eq!(ids(&p2), [2], "{t:?}");
        close_connection_impl(&state, id).await.unwrap();
    }
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
        let p = page(&state, id, obj(t, "weird_names"), 100, 0, None)
            .await
            .unwrap();
        assert!(
            !p.rows.iter().any(|r| int(&r[0]) == 987654),
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
        let p = page(
            &state,
            id,
            obj(t, "customers"),
            2,
            0,
            by("id", SortDir::Asc),
        )
        .await
        .unwrap();
        assert_eq!(ids(&p), [1, 2], "{t:?}");
        assert!(p.has_more, "{t:?}");
        assert!(
            count(&state, id, obj(t, "customers")).await.unwrap().rows >= 3,
            "{t:?}"
        );
        close_connection_impl(&state, id).await.unwrap();
    }
}
