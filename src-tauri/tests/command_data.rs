//! Data-view reads of a Command connection (the fake Command driver of `dnt_testkit`,
//! registered as Redis) through its `DataAccess`, the per-family filter types, and the
//! SQL data view (unchanged) (R4a Task 5).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, DriverOptions, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, Engine, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, SqlSession, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    ColumnDetail, DatabaseInfo, IndexDetail, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo,
    Scope, TableColumns, TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::fake_command::{collection_key, FakeCommandDriver, FakeStore, FakeValue};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::data::{
    table_data_impl, table_row_count_impl, DataFilter, FilterCond, FilterMode, FilterOp, SortDir,
    SortSpec,
};
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

struct Fixture {
    state: AppState,
    id: Uuid,
    _dir: tempfile::TempDir,
}

/// An `AppState` whose only driver is `driver` (registered as `cfg.kind`), with `cfg`
/// saved and open.
async fn fixture(driver: Arc<dyn Driver>, cfg: ConnectionConfig) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let dir = tempfile::tempdir().unwrap();
    let sessions = SessionManager::new(
        Arc::clone(&repo),
        Arc::clone(&secrets),
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(cfg.kind, driver)]),
    );
    let new_secrets = NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    let id = repo.create(cfg, new_secrets).unwrap().id;
    sessions.open(id, None).await.unwrap();
    let state = AppState {
        repo,
        secrets,
        sessions,
        paths: AppPaths::in_dir(dir.path().join("data")),
        export_paths: ExportPaths::default(),
        file_picks: FilePicks::default(),
    };
    Fixture {
        state,
        id,
        _dir: dir,
    }
}

/// The fake Command driver as Redis, open on db "0", with collection `users` in db "2":
/// `u1 = alice`, `u2 = bob`, `u3 = carol`.
async fn redis_fixture() -> (Fixture, FakeStore) {
    let driver = FakeCommandDriver::with_kind(DbKind::Redis);
    let store = driver.store();
    let mut cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: "Fake Redis".into(),
        port: 6379,
        default_db: Some("0".into()),
        secret_mode: SecretMode::Saved,
        ..sample_config()
    };
    cfg.options.driver = DriverOptions::Redis { db: 0 };
    store.set(
        "2",
        &collection_key("users"),
        FakeValue::Hash(BTreeMap::from([
            ("u1".into(), "alice".into()),
            ("u2".into(), "bob".into()),
            ("u3".into(), "carol".into()),
        ])),
    );
    (fixture(Arc::new(driver), cfg).await, store)
}

fn users() -> ObjectRef {
    ObjectRef {
        database: "2".into(),
        schema: None,
        name: "users".into(),
        kind: ObjectKind::Collection,
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn row(id: &str, value: &str) -> Vec<Value> {
    vec![text(id), text(value)]
}

fn eq(column: &str, value: &str) -> FilterCond {
    FilterCond {
        column: column.into(),
        op: FilterOp::Eq,
        values: vec![value.into()],
    }
}

fn structured(mode: FilterMode, conditions: Vec<FilterCond>) -> DataFilter {
    DataFilter {
        mode,
        conditions,
        raw_where: None,
        raw_json: None,
    }
}

fn json_filter(text: &str) -> DataFilter {
    DataFilter {
        raw_json: Some(text.into()),
        ..structured(FilterMode::And, vec![])
    }
}

fn where_filter(text: &str) -> DataFilter {
    DataFilter {
        raw_where: Some(text.into()),
        ..structured(FilterMode::And, vec![])
    }
}

/// The message of a `Config` error on field `filter`.
fn filter_message(e: DbError) -> String {
    match e {
        DbError::Config { field, message } => {
            assert_eq!(field.as_deref(), Some("filter"), "{message}");
            message
        }
        other => panic!("expected a filter Config error, got {other:?}"),
    }
}

async fn page(
    f: &Fixture,
    limit: u32,
    offset: u64,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
) -> Result<dntdatabase::commands::data::TableDataPage, DbError> {
    table_data_impl(
        &f.state,
        f.id,
        users(),
        limit,
        offset,
        sort,
        filter,
        "tab-1:page".into(),
    )
    .await
}

async fn count(f: &Fixture, filter: Option<DataFilter>) -> Result<u64, DbError> {
    table_row_count_impl(&f.state, f.id, users(), filter, "tab-1:count".into())
        .await
        .map(|c| c.rows)
}

#[tokio::test]
async fn data_page_reads_through_data_access() {
    let (f, store) = redis_fixture().await;

    let p = page(&f, 2, 0, vec![], None).await.unwrap();
    let names: Vec<_> = p.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["_id", "value"]);
    assert_eq!(p.rows, [row("u1", "alice"), row("u2", "bob")]);
    assert!(p.has_more);
    assert_eq!(p.key_order, ["_id"]);
    assert_eq!(p.row_versions, None);
    assert!(p.hidden_key.is_empty() && p.hidden_key_values.is_empty());
    assert!(!p.size_capped);

    // The next page.
    let p = page(&f, 2, 2, vec![], None).await.unwrap();
    assert_eq!(p.rows, [row("u3", "carol")]);
    assert!(!p.has_more);

    // A sort and a structured filter reach the driver.
    let desc = vec![SortSpec {
        column: "value".into(),
        dir: SortDir::Desc,
    }];
    let p = page(&f, 10, 0, desc, None).await.unwrap();
    assert_eq!(
        p.rows,
        [row("u3", "carol"), row("u2", "bob"), row("u1", "alice")]
    );
    assert!(
        p.key_order.is_empty(),
        "an explicit sort has no implicit key order"
    );
    let either = structured(FilterMode::Or, vec![eq("_id", "u1"), eq("value", "carol")]);
    let p = page(&f, 10, 0, vec![], Some(either)).await.unwrap();
    assert_eq!(p.rows, [row("u1", "alice"), row("u3", "carol")]);

    // An operator the driver does not support is its error.
    let like = structured(
        FilterMode::And,
        vec![FilterCond {
            op: FilterOp::Contains,
            ..eq("value", "o")
        }],
    );
    let err = page(&f, 10, 0, vec![], Some(like)).await.unwrap_err();
    assert!(matches!(err, DbError::Unsupported { .. }), "{err:?}");

    // Only driver-built reads ran.
    assert_eq!(store.executed(), ["FIND", "FIND", "FIND", "FIND"]);
}

#[tokio::test]
async fn data_count_reads_through_data_access() {
    let (f, store) = redis_fixture().await;
    assert_eq!(count(&f, None).await.unwrap(), 3);
    let one = structured(FilterMode::And, vec![eq("value", "bob")]);
    assert_eq!(count(&f, Some(one)).await.unwrap(), 1);
    let none = structured(FilterMode::And, vec![eq("_id", "u1"), eq("value", "bob")]);
    assert_eq!(count(&f, Some(none)).await.unwrap(), 0);
    assert_eq!(store.executed(), ["COUNT", "COUNT", "COUNT"]);
}

#[tokio::test]
async fn data_page_refuses_non_read_command() {
    let (f, store) = redis_fixture().await;
    store.build_reads_as_writes();
    let before = store.get("2", &collection_key("users"));

    for err in [
        page(&f, 10, 0, vec![], None).await.unwrap_err(),
        count(&f, None).await.map(|_| ()).unwrap_err(),
    ] {
        match err {
            DbError::Internal { message } => assert_eq!(
                message,
                "the driver built `HSET` for a read, but it is not a read"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
    // Nothing was sent: no command ran and the collection is as it was.
    assert!(store.executed().is_empty(), "{:?}", store.executed());
    assert_eq!(store.get("2", &collection_key("users")), before);
}

#[tokio::test]
async fn raw_json_blocks_server_side_js() {
    let (f, store) = redis_fixture().await;
    let js = r#"{"a":{"$where":"1"}}"#;
    let err = page(&f, 10, 0, vec![], Some(json_filter(js)))
        .await
        .unwrap_err();
    assert_eq!(
        filter_message(err),
        "Server-side JavaScript is not allowed in a filter"
    );
    let err = count(&f, Some(json_filter(js))).await.unwrap_err();
    assert_eq!(
        filter_message(err),
        "Server-side JavaScript is not allowed in a filter"
    );
    assert!(store.executed().is_empty());
}

#[tokio::test]
async fn raw_json_must_be_an_object() {
    let (f, store) = redis_fixture().await;
    for bad in ["[1]", "\"a\"", "{not json"] {
        let err = page(&f, 10, 0, vec![], Some(json_filter(bad)))
            .await
            .unwrap_err();
        assert_eq!(
            filter_message(err),
            "The filter must be a JSON object",
            "{bad}"
        );
    }
    assert!(store.executed().is_empty());
}

#[tokio::test]
async fn filter_types_are_per_family() {
    // Command: no raw WHERE, never both.
    let (f, store) = redis_fixture().await;
    let err = page(&f, 10, 0, vec![], Some(where_filter("1=1")))
        .await
        .unwrap_err();
    assert_eq!(
        filter_message(err),
        "This filter type is not available for this engine"
    );
    let both = DataFilter {
        raw_json: Some("{}".into()),
        ..where_filter("1=1")
    };
    let err = count(&f, Some(both.clone())).await.unwrap_err();
    assert_eq!(
        filter_message(err),
        "Use either a WHERE filter or a JSON filter, not both"
    );
    // A safe JSON filter passes the backend's checks and reaches the driver, which
    // decides what it supports (the fake: nothing raw).
    let err = page(&f, 10, 0, vec![], Some(json_filter(r#"{"value":"bob"}"#)))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Unsupported { .. }), "{err:?}");
    assert!(store.executed().is_empty());

    // SQL: no JSON filter, never both.
    let (f, executed) = sql_fixture().await;
    let orders = orders();
    let err = table_data_impl(
        &f.state,
        f.id,
        orders.clone(),
        10,
        0,
        vec![],
        Some(json_filter("{}")),
        "k".into(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        filter_message(err),
        "This filter type is not available for this engine"
    );
    let err = table_row_count_impl(&f.state, f.id, orders, Some(both), "k".into())
        .await
        .unwrap_err();
    assert_eq!(
        filter_message(err),
        "Use either a WHERE filter or a JSON filter, not both"
    );
    assert!(lock(&executed).is_empty());
}

// ---------------------------------------------------------------------------------
// SQL: a fake MySQL that records what it runs.

type Executed = Arc<Mutex<Vec<String>>>;

fn lock(e: &Executed) -> std::sync::MutexGuard<'_, Vec<String>> {
    e.lock().unwrap()
}

struct RecordingSqlDriver {
    executed: Executed,
}

struct RecordingSqlSession {
    info: ServerInfo,
    executed: Executed,
}

struct NoopCancel;

#[async_trait]
impl Canceller for NoopCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        Ok(())
    }
}

#[async_trait]
impl Driver for RecordingSqlDriver {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }

    async fn connect(&self, _cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        Ok(Box::new(RecordingSqlSession {
            info: detect_mysql("8.0.36")?,
            executed: Arc::clone(&self.executed),
        }))
    }
}

#[async_trait]
impl Session for RecordingSqlSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    fn engine(&self) -> Engine<'_> {
        Engine::Sql(self)
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoopCancel)
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }
    async fn close(self: Box<Self>) {}
}

fn int_col(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        db_type: "int".into(),
        logical: LogicalType::Integer,
        nullable: Some(false),
        origin_table: None,
        origin_column: None,
    }
}

/// A `COUNT(*)` gives 7; any other read gives rows `id = 1, 2`.
#[async_trait]
impl SqlSession for RecordingSqlSession {
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        _opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        lock(&self.executed).push(stmt.to_string());
        let (cols, rows) = if stmt.contains("COUNT(*)") {
            (vec![int_col("COUNT(*)")], vec![vec![Value::Int(7)]])
        } else {
            (
                vec![int_col("id")],
                vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            )
        };
        let summary = ResultSummary {
            affected: None,
            row_count: rows.len() as u64,
            truncated: false,
            elapsed_ms: 0,
        };
        sink.result_start(&cols);
        sink.rows(rows);
        sink.result_end(&summary);
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: false,
        })
    }
    fn tx_state(&self) -> TxState {
        TxState::Idle
    }
    fn dialect(&self) -> Dialect {
        Dialect::MySql(Default::default())
    }
}

#[async_trait]
impl Introspector for RecordingSqlSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Ok(vec![DatabaseInfo {
            name: "shop".into(),
        }])
    }
    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_objects(
        &self,
        _scope: &Scope,
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Ok(vec![])
    }
    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }
    /// `orders (id int primary key)`.
    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Ok(TableDetail {
            object: obj.clone(),
            columns: vec![ColumnDetail {
                name: "id".into(),
                ordinal: 1,
                db_type: "int".into(),
                nullable: false,
                ..ColumnDetail::default()
            }],
            primary_key: Some(IndexDetail {
                name: "PRIMARY".into(),
                unique: true,
                primary: true,
                columns: vec!["id".into()],
                ..IndexDetail::default()
            }),
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            options: Default::default(),
            row_estimate: None,
            size_bytes: None,
            comment: None,
        })
    }
    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Ok(String::new())
    }
}

async fn sql_fixture() -> (Fixture, Executed) {
    let executed = Executed::default();
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::MySql,
        default_db: Some("shop".into()),
        ..sample_config()
    };
    let driver = RecordingSqlDriver {
        executed: Arc::clone(&executed),
    };
    (fixture(Arc::new(driver), cfg).await, executed)
}

fn orders() -> ObjectRef {
    ObjectRef {
        database: "shop".into(),
        schema: None,
        name: "orders".into(),
        kind: ObjectKind::Table,
    }
}

#[tokio::test]
async fn sql_data_view_unchanged() {
    let (f, executed) = sql_fixture().await;
    lock(&executed).clear();

    let p = table_data_impl(&f.state, f.id, orders(), 1, 0, vec![], None, "k".into())
        .await
        .unwrap();
    assert_eq!(p.rows, [vec![Value::Int(1)]]);
    assert!(p.has_more);
    assert_eq!(p.key_order, ["id"]);
    let n = table_row_count_impl(
        &f.state,
        f.id,
        orders(),
        Some(structured(FilterMode::And, vec![eq("id", "1")])),
        "k".into(),
    )
    .await
    .unwrap();
    assert_eq!(n.rows, 7);
    assert_eq!(
        *lock(&executed),
        [
            "SELECT * FROM `shop`.`orders` ORDER BY `id` ASC LIMIT 2 OFFSET 0",
            "SELECT COUNT(*) FROM `shop`.`orders` WHERE (`id` = ?)",
        ]
    );
}
