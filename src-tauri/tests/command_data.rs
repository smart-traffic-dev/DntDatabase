//! Data-view reads of a Command connection (the fake Command driver of `dnt_testkit`,
//! registered as Redis) through its `DataAccess`, the per-family filter types, and the
//! SQL data view (unchanged) (R4a Task 5).

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, DriverOptions, EnvTag, ResolvedConfig, SecretMode};
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
use dnt_query::edit::{
    command_fingerprint, display_command, CellInput, Protection, RowEdit, RowRef,
};
use dnt_query::Verdict;
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryEntry, HistoryFilter, HistoryOrigin, HistoryStatus};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_store::repo::{SecretChange, SecretUpdate};
use dnt_testkit::fake_command::{collection_key, FakeCommandDriver, FakeStore, FakeValue};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::data::{
    data_apply_impl, data_editable_impl, data_preview_impl, table_data_impl, table_row_count_impl,
    Atomicity, DataFilter, EditSet, FilterCond, FilterMode, FilterOp, ProtectionKind, SortDir,
    SortSpec,
};
use dntdatabase::edit_plans::ApplyOutcome;
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
        ai: Default::default(),
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
    redis_fixture_with(None, false).await
}

/// [`redis_fixture`] with an env tag and read-only flag.
async fn redis_fixture_with(env_tag: Option<EnvTag>, read_only: bool) -> (Fixture, FakeStore) {
    redis_fixture_on(
        FakeCommandDriver::with_kind(DbKind::Redis),
        env_tag,
        read_only,
    )
    .await
}

/// [`redis_fixture`] whose engine has transactions (`execute_atomic`), so collection
/// edits are atomic.
async fn atomic_fixture() -> (Fixture, FakeStore) {
    let driver = FakeCommandDriver::with_kind(DbKind::Redis);
    driver.store().with_transactions();
    redis_fixture_on(driver, None, false).await
}

/// [`redis_fixture_with`] on `driver`.
async fn redis_fixture_on(
    driver: FakeCommandDriver,
    env_tag: Option<EnvTag>,
    read_only: bool,
) -> (Fixture, FakeStore) {
    let store = driver.store();
    let mut cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: "Fake Redis".into(),
        port: 6379,
        default_db: Some("0".into()),
        secret_mode: SecretMode::Saved,
        env_tag,
        read_only,
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

/// A `COUNT(*)` gives 7, `@@character_set_client` `utf8mb4`, a write affects 1 row and
/// returns none; any other read gives rows `id = 1, 2`.
#[async_trait]
impl SqlSession for RecordingSqlSession {
    async fn execute_one(
        &self,
        stmt: &str,
        kind: StatementKind,
        _opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        lock(&self.executed).push(stmt.to_string());
        let write = kind == StatementKind::Write;
        let (cols, rows) = if stmt.contains("COUNT(*)") {
            (vec![int_col("COUNT(*)")], vec![vec![Value::Int(7)]])
        } else if stmt.contains("@@character_set_client") {
            let col = ColumnMeta {
                logical: LogicalType::Text,
                ..int_col("@@character_set_client")
            };
            (vec![col], vec![vec![Value::Text("utf8mb4".into())]])
        } else if write {
            (vec![], vec![])
        } else {
            (
                vec![int_col("id")],
                vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            )
        };
        let summary = ResultSummary {
            affected: write.then_some(1),
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
            key_count: None,
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

// ---------------------------------------------------------------------------------
// Data-view edits of the Command family (R4a Task 6).

/// The row `id` of `users` as the grid holds it.
fn user_row(id: &str, value: &str) -> RowRef {
    RowRef {
        key: vec![("_id".into(), text(id))],
        original: vec![("value".into(), text(value))],
        xmin: None,
        loaded: vec![],
    }
}

fn typed(s: &str) -> CellInput {
    CellInput::Text { text: s.into() }
}

/// `u1` becomes `alicia`, `u4 = dave` is added, `u2` is deleted.
fn user_edits() -> EditSet {
    EditSet {
        object: users(),
        edits: vec![
            RowEdit::Update {
                row: user_row("u1", "alice"),
                set: vec![("value".into(), typed("alicia"))],
            },
            RowEdit::Insert {
                values: vec![("_id".into(), typed("u4")), ("value".into(), typed("dave"))],
            },
            RowEdit::Delete {
                row: user_row("u2", "bob"),
            },
        ],
        overwrite: false,
    }
}

fn history(f: &Fixture) -> Vec<HistoryEntry> {
    let mut rows = f
        .state
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(f.id),
            search: None,
            origin: None,
            limit: 1000,
        })
        .unwrap();
    rows.sort_by_key(|r| r.id);
    rows
}

fn users_hash(store: &FakeStore) -> BTreeMap<String, String> {
    match store.get("2", &collection_key("users")) {
        Some(FakeValue::Hash(h)) => h,
        other => panic!("expected the users hash, got {other:?}"),
    }
}

fn original_users() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("u1".into(), "alice".into()),
        ("u2".into(), "bob".into()),
        ("u3".into(), "carol".into()),
    ])
}

#[tokio::test]
async fn command_collection_is_editable() {
    let (f, store) = redis_fixture().await;
    let info = data_editable_impl(&f.state, f.id, users()).await.unwrap();
    assert!(info.editable, "{info:?}");
    assert_eq!(info.reason, None);
    assert_eq!(info.key, ["_id"]);
    assert!(info.hidden_key.is_empty());
    assert_eq!(info.editable_columns, ["value"]);
    // The fake has no transactions.
    assert_eq!(info.atomic, Atomicity::No);
    assert_eq!(info.protection, ProtectionKind::Key);

    // Only collections are edited.
    let key = ObjectRef {
        kind: ObjectKind::Key,
        ..users()
    };
    let info = data_editable_impl(&f.state, f.id, key).await.unwrap();
    assert!(!info.editable);
    assert_eq!(
        info.reason.as_deref(),
        Some("only collections can be edited")
    );
    assert!(info.editable_columns.is_empty());
    assert_eq!(info.protection, ProtectionKind::Key);
    assert!(store.executed().is_empty());
}

#[tokio::test]
async fn command_edit_preview_lists_commands() {
    let (f, store) = redis_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(
        p.statements,
        [
            "HSET h:users u1 alicia",
            "HSET h:users u4 dave",
            "HDEL h:users u2"
        ]
    );
    assert_eq!(p.verdict, Verdict::Allow);
    assert_eq!(p.atomic, Atomicity::No);
    assert_eq!(p.protections, vec![Protection::KeyOnly; 3]);
    assert_eq!(p.reason, None);
    // A preview sends nothing.
    assert!(store.executed().is_empty());
    assert_eq!(users_hash(&store), original_users());
}

#[tokio::test]
async fn command_edit_apply_writes_and_logs_history() {
    let (f, store) = redis_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    assert!(r.error.is_none());
    let sent: Vec<_> = r
        .statements
        .iter()
        .map(|s| (s.index, s.affected, s.error.is_none(), s.conflict))
        .collect();
    assert_eq!(
        sent,
        [
            (0, Some(0.0), true, false),
            (1, Some(1.0), true, false),
            (2, Some(1.0), true, false)
        ]
    );
    assert_eq!(store.executed(), ["HSET", "HSET", "HDEL"]);
    assert_eq!(
        users_hash(&store),
        BTreeMap::from([
            ("u1".into(), "alicia".into()),
            ("u3".into(), "carol".into()),
            ("u4".into(), "dave".into()),
        ])
    );

    let rows = history(&f);
    let texts: Vec<_> = rows.iter().map(|r| r.sql_text.as_str()).collect();
    assert_eq!(texts, p.statements);
    for row in &rows {
        assert_eq!(row.origin, HistoryOrigin::DataEdit);
        assert_eq!(row.kind, "write");
        assert_eq!(row.status, HistoryStatus::Ok);
        assert_eq!(row.database.as_deref(), Some("2"));
        assert!(!row.redacted);
        assert!(!row.outcome_unknown);
    }

    // A plan runs once: never sent again.
    let e = data_apply_impl(&f.state, p.plan_id, false)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::PlanExpired), "{e:?}");
    assert_eq!(store.executed().len(), 3);
}

/// What `data_apply` of a Production plan for a collection without transactions says.
const NOT_ATOMIC: &str = "Changes to this collection cannot be rolled back as a unit, so the \
     data view does not edit it on a Production connection. Use the console instead.";

#[tokio::test]
async fn command_edit_blocked_on_production_when_not_atomic() {
    let (f, store) = redis_fixture_with(Some(EnvTag::Production), false).await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(p.verdict, Verdict::Block);
    assert_eq!(p.atomic, Atomicity::No);
    assert_eq!(p.reason.as_deref(), Some(NOT_ATOMIC));
    // Confirming does not lift it.
    let e = data_apply_impl(&f.state, p.plan_id, true)
        .await
        .unwrap_err();
    match e {
        DbError::Config { field, message } => {
            assert_eq!(field.as_deref(), Some("edit"));
            assert_eq!(message, NOT_ATOMIC);
        }
        other => panic!("expected the not-atomic refusal, got {other:?}"),
    }
    assert!(store.executed().is_empty());
    assert_eq!(users_hash(&store), original_users());
}

#[tokio::test]
async fn command_edit_refused_read_only() {
    let (f, store) = redis_fixture_with(None, true).await;
    let info = data_editable_impl(&f.state, f.id, users()).await.unwrap();
    assert!(!info.editable);
    assert_eq!(info.reason.as_deref(), Some("the connection is read-only"));
    // The key still identifies the rows.
    assert_eq!(info.key, ["_id"]);
    assert!(info.editable_columns.is_empty());

    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(p.verdict, Verdict::Block);
    assert_eq!(p.reason.as_deref(), Some("the connection is read-only"));
    let e = data_apply_impl(&f.state, p.plan_id, true)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::Blocked { count: 3 }), "{e:?}");
    assert!(store.executed().is_empty());

    // Made read-only after a preview: the stored plan is judged again at apply.
    let (f, store) = redis_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(p.verdict, Verdict::Allow);
    let mut cfg = f.state.repo.get(f.id).unwrap();
    cfg.read_only = true;
    f.state
        .repo
        .update(
            cfg,
            SecretUpdate {
                db_password: SecretChange::Keep,
                ssh_password: SecretChange::Keep,
                ssh_key_passphrase: SecretChange::Keep,
            },
        )
        .unwrap();
    let e = data_apply_impl(&f.state, p.plan_id, true)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::Blocked { count: 3 }), "{e:?}");
    assert!(store.executed().is_empty());
    assert_eq!(users_hash(&store), original_users());
    assert!(history(&f).is_empty());
}

#[tokio::test]
async fn sql_edit_unchanged() {
    let (f, executed) = sql_fixture().await;
    let info = data_editable_impl(&f.state, f.id, orders()).await.unwrap();
    assert!(info.editable, "{info:?}");
    assert_eq!(info.key, ["id"]);
    assert_eq!(info.protection, ProtectionKind::Snapshot);

    lock(&executed).clear();
    let set = EditSet {
        object: orders(),
        edits: vec![RowEdit::Insert {
            values: vec![("id".into(), typed("3"))],
        }],
        overwrite: false,
    };
    let p = data_preview_impl(&f.state, f.id, set).await.unwrap();
    assert_eq!(p.verdict, Verdict::Allow);
    assert_eq!(p.statements.len(), 1);
    assert!(
        p.statements[0].starts_with("INSERT INTO `shop`.`orders`"),
        "{:?}",
        p.statements
    );
    assert!(!p.protections.contains(&Protection::KeyOnly));
    // The preview read the column types only.
    assert_eq!(
        *lock(&executed),
        ["SELECT `id` FROM `shop`.`orders` LIMIT 0"]
    );
}

// ---------------------------------------------------------------------------------
// Atomic Command edits, the Command fingerprint and shortened command text (Mongo
// driver Task 2).

/// `(index, affected, has error, conflict)` of each statement of `r`.
fn sent(r: &dntdatabase::edit_plans::ApplyReport) -> Vec<(u32, Option<f64>, bool, bool)> {
    r.statements
        .iter()
        .map(|s| (s.index, s.affected, s.error.is_some(), s.conflict))
        .collect()
}

#[tokio::test]
async fn atomic_plan_uses_execute_atomic() {
    let (f, store) = atomic_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(p.atomic, Atomicity::Yes);
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    assert!(r.error.is_none());
    assert_eq!(
        sent(&r),
        [
            (0, Some(0.0), false, false),
            (1, Some(1.0), false, false),
            (2, Some(1.0), false, false)
        ]
    );
    // One atomic run, and each command ran once (inside it): no per-command execute.
    assert_eq!(store.atomic_calls(), 1);
    assert_eq!(store.executed(), ["HSET", "HSET", "HDEL"]);
    assert_eq!(
        users_hash(&store),
        BTreeMap::from([
            ("u1".into(), "alicia".into()),
            ("u3".into(), "carol".into()),
            ("u4".into(), "dave".into()),
        ])
    );
    let rows = history(&f);
    let texts: Vec<_> = rows.iter().map(|r| r.sql_text.as_str()).collect();
    assert_eq!(texts, p.statements);
    assert!(rows.iter().all(|r| r.status == HistoryStatus::Ok));

    // Without transactions the commands still run one by one.
    let (f, store) = redis_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    assert_eq!(store.atomic_calls(), 0);
}

#[tokio::test]
async fn atomic_failure_is_rolled_back() {
    let (f, store) = atomic_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    store.fail_atomic_at(1);
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{r:?}");
    // The first command ran (then was rolled back); the second failed; the third was
    // never sent.
    assert_eq!(sent(&r), [(0, None, false, false), (1, None, true, false)]);
    assert_eq!(store.atomic_calls(), 1);
    assert_eq!(users_hash(&store), original_users());

    let rows = history(&f);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].sql_text, p.statements[0]);
    assert_eq!(rows[0].status, HistoryStatus::Error);
    assert_eq!(rows[0].error_kind.as_deref(), Some("rolled_back"));
    assert_eq!(rows[1].sql_text, p.statements[1]);
    assert_eq!(rows[1].status, HistoryStatus::Error);
    assert_ne!(rows[1].error_kind.as_deref(), Some("rolled_back"));
    assert!(rows.iter().all(|r| !r.outcome_unknown));
}

#[tokio::test]
async fn atomic_failure_not_rolled_back_is_unknown() {
    let (f, store) = atomic_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    // The answer is lost at the last command (as a commit without an answer).
    store.fail_atomic_unknown_at(2);
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    // The whole unit is unknown, not just the command that failed.
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 3 }, "{r:?}");
    assert_eq!(
        sent(&r),
        [
            (0, None, false, false),
            (1, None, false, false),
            (2, None, true, false)
        ]
    );
    let rows = history(&f);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.iter().all(|r| r.outcome_unknown), "{rows:?}");
    // No row claims it was applied or rolled back.
    assert_eq!(rows[0].status, HistoryStatus::Ok);
    assert!(rows
        .iter()
        .all(|r| r.error_kind.as_deref() != Some("rolled_back")));

    // An earlier command too: still the whole unit.
    let (f, store) = atomic_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    store.fail_atomic_unknown_at(1);
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 3 }, "{r:?}");
    assert_eq!(sent(&r), [(0, None, false, false), (1, None, true, false)]);
    assert!(history(&f).iter().all(|r| r.outcome_unknown));
}

#[tokio::test]
async fn atomic_unsupported_applies_nothing() {
    let (f, store) = atomic_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    store.atomic_unsupported();
    let e = data_apply_impl(&f.state, p.plan_id, false)
        .await
        .unwrap_err();
    assert!(
        matches!(&e, DbError::Unsupported { feature, .. } if feature == "transactions"),
        "{e:?}"
    );
    // Nothing was sent, nothing is unknown, and there is nothing to record.
    assert_eq!(store.atomic_calls(), 1);
    assert!(store.executed().is_empty(), "{:?}", store.executed());
    assert_eq!(users_hash(&store), original_users());
    assert!(history(&f).is_empty());
}

#[tokio::test]
async fn transaction_support_change_makes_plan_stale() {
    // Previewed without transactions, applied where there are: never run atomically.
    let (f, store) = redis_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(p.atomic, Atomicity::No);
    store.set_transactions(true);
    let e = data_apply_impl(&f.state, p.plan_id, false)
        .await
        .unwrap_err();
    match &e {
        DbError::PlanStale { reason } => {
            assert_eq!(reason, "the server's transaction support changed")
        }
        other => panic!("expected PlanStale, got {other:?}"),
    }
    assert!(store.executed().is_empty());
    assert_eq!(store.atomic_calls(), 0);

    // Previewed with transactions, applied where there are none: never one by one.
    let (f, store) = atomic_fixture().await;
    let p = data_preview_impl(&f.state, f.id, user_edits())
        .await
        .unwrap();
    assert_eq!(p.atomic, Atomicity::Yes);
    store.set_transactions(false);
    let e = data_apply_impl(&f.state, p.plan_id, false)
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    assert!(store.executed().is_empty());
    assert_eq!(store.atomic_calls(), 0);
    assert_eq!(users_hash(&store), original_users());
    assert!(history(&f).is_empty());
}

/// `u1` becomes `alicia`, `u2` is deleted.
fn update_then_delete() -> EditSet {
    EditSet {
        object: users(),
        edits: vec![
            RowEdit::Update {
                row: user_row("u1", "alice"),
                set: vec![("value".into(), typed("alicia"))],
            },
            RowEdit::Delete {
                row: user_row("u2", "bob"),
            },
        ],
        overwrite: false,
    }
}

/// The users without `u2` (deleted by someone else after the preview).
fn users_without_u2() -> BTreeMap<String, String> {
    BTreeMap::from([("u1".into(), "alice".into()), ("u3".into(), "carol".into())])
}

#[tokio::test]
async fn document_not_found_sets_conflict() {
    // Atomic: the transaction is rolled back and the command is a conflict.
    let (f, store) = atomic_fixture().await;
    store.missing_documents();
    let p = data_preview_impl(&f.state, f.id, update_then_delete())
        .await
        .unwrap();
    store.set(
        "2",
        &collection_key("users"),
        FakeValue::Hash(users_without_u2()),
    );
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{r:?}");
    assert_eq!(sent(&r), [(0, None, false, false), (1, None, true, true)]);
    let e = r.statements[1].error.as_ref().unwrap();
    assert_eq!(e.server_code.as_deref(), Some("DocumentNotFound"));
    assert_eq!(users_hash(&store), users_without_u2());
    let rows = history(&f);
    assert_eq!(rows[1].error_kind.as_deref(), Some("conflict"));
    assert_eq!(rows[1].error_code.as_deref(), Some("DocumentNotFound"));

    // One by one: the apply stops at the command, which is a conflict.
    let (f, store) = redis_fixture().await;
    store.missing_documents();
    let p = data_preview_impl(&f.state, f.id, update_then_delete())
        .await
        .unwrap();
    store.set(
        "2",
        &collection_key("users"),
        FakeValue::Hash(users_without_u2()),
    );
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(
        r.outcome,
        ApplyOutcome::PartiallyApplied { applied: 1 },
        "{r:?}"
    );
    assert_eq!(
        sent(&r),
        [(0, Some(0.0), false, false), (1, None, true, true)]
    );
    let rows = history(&f);
    assert_eq!(rows[1].error_kind.as_deref(), Some("conflict"));
}

/// A collection's detail as a Command driver samples it: `_id` (the key, of type
/// `id_type`), the sampled `columns` (all `string`) and one index on `name`.
fn sampled_detail(id_type: &str, columns: &[&str]) -> TableDetail {
    let column = |name: &str, ordinal: u32, db_type: &str| ColumnDetail {
        name: name.into(),
        ordinal,
        db_type: db_type.into(),
        nullable: ordinal != 1,
        ..ColumnDetail::default()
    };
    let mut cols = vec![column("_id", 1, id_type)];
    for (i, c) in columns.iter().enumerate() {
        cols.push(column(c, i as u32 + 2, "string"));
    }
    TableDetail {
        object: users(),
        columns: cols,
        primary_key: Some(IndexDetail {
            name: "_id_".into(),
            unique: true,
            primary: true,
            columns: vec!["_id".into()],
            ..IndexDetail::default()
        }),
        indexes: vec![IndexDetail {
            name: "name_1".into(),
            unique: false,
            columns: vec!["name".into()],
            ..IndexDetail::default()
        }],
        foreign_keys: vec![],
        checks: vec![],
        triggers: vec![],
        options: Default::default(),
        row_estimate: None,
        size_bytes: None,
        comment: None,
    }
}

#[test]
fn command_fingerprint_ignores_sampled_columns() {
    let base = command_fingerprint(&sampled_detail("objectId", &["name"]));
    // A new sampled column, or another type of one, changes nothing.
    assert_eq!(
        command_fingerprint(&sampled_detail("objectId", &["name", "age"])),
        base
    );
    let mut retyped = sampled_detail("objectId", &["name"]);
    retyped.columns[1].db_type = "int".into();
    assert_eq!(command_fingerprint(&retyped), base);
    // The `_id` type and the index list do.
    assert_ne!(
        command_fingerprint(&sampled_detail("string", &["name"])),
        base
    );
    let mut unique = sampled_detail("objectId", &["name"]);
    unique.indexes[0].unique = true;
    assert_ne!(command_fingerprint(&unique), base);
    let mut renamed = sampled_detail("objectId", &["name"]);
    renamed.indexes[0].name = "name_-1".into();
    assert_ne!(command_fingerprint(&renamed), base);
    let mut more = sampled_detail("objectId", &["name"]);
    more.indexes[0].columns.push("age".into());
    assert_ne!(command_fingerprint(&more), base);
    let mut dropped = sampled_detail("objectId", &["name"]);
    dropped.indexes.clear();
    assert_ne!(command_fingerprint(&dropped), base);
}

#[tokio::test]
async fn long_command_text_is_shortened_in_history() {
    let (f, store) = redis_fixture().await;
    let long = "x".repeat(5000);
    let set = EditSet {
        object: users(),
        edits: vec![RowEdit::Update {
            row: user_row("u1", "alice"),
            set: vec![("value".into(), typed(&long))],
        }],
        overwrite: false,
    };
    let p = data_preview_impl(&f.state, f.id, set).await.unwrap();
    let full = format!("HSET h:users u1 {long}");
    let (shown, shortened) = display_command(&full);
    assert!(shortened);
    assert_eq!(p.statements, std::slice::from_ref(&shown));
    assert!(shown.ends_with('…'), "{shown}");
    assert_eq!(shown, format!("{}…", &full[..4096]));

    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    // The full value was written.
    assert_eq!(users_hash(&store).get("u1"), Some(&long));
    let rows = history(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sql_text, shown);
    // A shortened row is not the command that ran: it is marked like a redacted one.
    assert!(rows[0].redacted);

    // A short command stays whole and unmarked.
    assert_eq!(
        display_command("HDEL h:users u1"),
        ("HDEL h:users u1".into(), false)
    );
}

#[tokio::test]
async fn sql_apply_unchanged() {
    let (f, executed) = sql_fixture().await;
    let set = EditSet {
        object: orders(),
        edits: vec![RowEdit::Insert {
            values: vec![("id".into(), typed("3"))],
        }],
        overwrite: false,
    };
    let p = data_preview_impl(&f.state, f.id, set).await.unwrap();
    lock(&executed).clear();
    let r = data_apply_impl(&f.state, p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    assert_eq!(sent(&r), [(0, Some(1.0), false, false)]);
    assert_eq!(r.inserted, [vec![Value::Int(1)]]);
    // The SQL path: the session check, the statement, its read-back; no fingerprint
    // change made it stale.
    assert_eq!(
        *lock(&executed),
        [
            "SELECT @@character_set_client",
            "INSERT INTO `shop`.`orders` (`id`) VALUES (3)",
            "SELECT `id` FROM `shop`.`orders` WHERE `id` = 3",
        ]
    );
    let rows = history(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].sql_text,
        "INSERT INTO `shop`.`orders` (`id`) VALUES (3)"
    );
    assert_eq!(rows[0].sql_text, p.statements[0]);
    assert_eq!(rows[0].status, HistoryStatus::Ok);
    assert!(!rows[0].redacted);
}
