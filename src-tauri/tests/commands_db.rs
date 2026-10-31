//! Browse commands against the Docker test matrix (`DNT_TEST_DB=1`).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::{DbKind, SqlKind};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_store::repo::NodeId;
use dnt_store::transfer::ConflictPolicy;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::cache::SchemaCache;
use dntdatabase::commands::browse::{
    get_schema_cache_impl, list_databases_impl, list_objects_impl, list_schemas_impl,
    object_ddl_impl, refresh_node_impl,
};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::query::{completion_objects_impl, completion_scope_impl};
use dntdatabase::commands::transfer::{apply_import_impl, export_connections_impl};
use dntdatabase::file_picks::FileAccess;
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open(state: &AppState, t: Target) -> Uuid {
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

#[tokio::test]
async fn mysql_system_databases_hidden_unless_requested() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state, Target::MySql8).await;

    let names = |dbs: Vec<dnt_core::meta::DatabaseInfo>| -> Vec<String> {
        dbs.into_iter().map(|d| d.name).collect()
    };
    let user = names(list_databases_impl(&state, id, false).await.unwrap());
    assert!(user.iter().any(|n| n == FIXTURE_DB), "{user:?}");
    assert!(!user.iter().any(|n| n == "mysql" || n == "sys"), "{user:?}");
    let all = names(list_databases_impl(&state, id, true).await.unwrap());
    assert!(all.iter().any(|n| n == "information_schema"), "{all:?}");

    let scope = Scope {
        database: FIXTURE_DB.into(),
        schema: None,
    };
    let tables = list_objects_impl(&state, id, scope, ObjectKind::Table)
        .await
        .unwrap();
    assert!(tables.iter().any(|t| t.name == "customers"), "{tables:?}");
    let ddl = object_ddl_impl(
        &state,
        id,
        ObjectRef {
            database: FIXTURE_DB.into(),
            schema: None,
            name: "customers".into(),
            kind: ObjectKind::Table,
        },
    )
    .await
    .unwrap();
    assert!(ddl.contains("CREATE TABLE"), "{ddl}");
    refresh_node_impl(&state, id, vec![format!("db:{FIXTURE_DB}")])
        .await
        .unwrap();
    close_connection_impl(&state, id).await.unwrap();
}

#[tokio::test]
async fn overwrite_import_closes_the_open_connection() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state, Target::MySql8).await;
    assert_eq!(state.sessions.open_connections(), [id]);

    let file = dir.path().join("self.dntconn");
    let path = file.to_string_lossy().into_owned();
    for access in [FileAccess::ConnectionsExport, FileAccess::ConnectionsImport] {
        state.file_picks.admit(access, file.clone()).unwrap();
    }
    export_connections_impl(&state, vec![NodeId::Connection(id)], None, path.clone())
        .await
        .unwrap();
    let result = apply_import_impl(&state, path, None, ConflictPolicy::Overwrite)
        .await
        .unwrap();
    assert_eq!(result.overwritten_ids, [id]);
    // The runtime kept the old settings: it is closed, the next browse says so.
    assert!(state.sessions.open_connections().is_empty());
    let err = list_databases_impl(&state, id, false).await.unwrap_err();
    assert!(matches!(err, dnt_core::error::DbError::NotOpen), "{err:?}");
}

/// Discards every result.
struct NullSink;

impl RowSink for NullSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Runs each statement of `sqls` on a fresh admin session to `db` of `t`.
async fn exec(t: Target, db: Option<&str>, sqls: &[&str]) {
    let driver: Box<dyn Driver> = match t.sql_kind() {
        SqlKind::MySql => Box::new(MySqlDriver),
        SqlKind::Postgres => Box::new(PgDriver),
    };
    let session = driver.connect(&t.resolved(db)).await.unwrap();
    for sql in sqls {
        session
            .sql()
            .unwrap()
            .execute(sql, ExecOptions::default(), &mut NullSink)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {sql}: {e}"));
    }
    session.close().await;
}

/// Creates database `db` (if missing) with table `table`, and drops `drop` there.
async fn ensure_database(t: Target, db: &str, table: &str, drop: &str) {
    match t.sql_kind() {
        SqlKind::MySql => {
            exec(
                t,
                None,
                &[
                    &format!("CREATE DATABASE IF NOT EXISTS {db}"),
                    &format!("CREATE TABLE IF NOT EXISTS {db}.{table} (id int)"),
                    &format!("DROP TABLE IF EXISTS {db}.{drop}"),
                ],
            )
            .await
        }
        SqlKind::Postgres => {
            // No `CREATE DATABASE IF NOT EXISTS` on PostgreSQL: a failure because it
            // exists is fine.
            let driver = PgDriver;
            let session = driver.connect(&t.resolved(Some("postgres"))).await.unwrap();
            let _ = session
                .sql()
                .unwrap()
                .execute(
                    &format!("CREATE DATABASE {db}"),
                    ExecOptions::default(),
                    &mut NullSink,
                )
                .await;
            session.close().await;
            exec(
                t,
                Some(db),
                &[
                    &format!("CREATE TABLE IF NOT EXISTS {table} (id int)"),
                    &format!("DROP TABLE IF EXISTS {drop}"),
                ],
            )
            .await
        }
    }
}

fn table_names(cache: &SchemaCache) -> Vec<&str> {
    cache.tables.iter().map(|t| t.name.as_str()).collect()
}

#[tokio::test]
async fn schema_cache_per_database_loads_lazily() {
    require_db!();
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;

        let fixture = completion_scope_impl(&state, id, FIXTURE_DB.into())
            .await
            .unwrap();
        assert_eq!(fixture.database, FIXTURE_DB);
        let orders = fixture
            .tables
            .iter()
            .find(|t| t.name == "orders")
            .unwrap_or_else(|| panic!("{t:?}: {:?}", table_names(&fixture)));
        assert!(
            orders.columns.iter().any(|c| c.name == "customer_id"),
            "{t:?}: {orders:?}"
        );
        // The P1 command now returns the home database's cache, with column types.
        let home = get_schema_cache_impl(&state, id).await.unwrap().unwrap();
        assert_eq!(home, fixture, "{t:?}");

        ensure_database(t, "dnt_p2_other", "t2", "t_none").await;
        let other = completion_scope_impl(&state, id, "dnt_p2_other".into())
            .await
            .unwrap();
        let names = table_names(&other);
        assert!(names.contains(&"t2"), "{t:?}: {names:?}");
        assert!(!names.contains(&"orders"), "{t:?}: {names:?}");
        if t.kind() == DbKind::Postgres {
            assert!(other
                .tables
                .iter()
                .all(|t| t.schema.as_deref() == Some("public")));
        }

        let schema = (t.kind() == DbKind::Postgres).then(|| "public".to_string());
        let objects = completion_objects_impl(&state, id, FIXTURE_DB.into(), schema.clone())
            .await
            .unwrap();
        assert!(
            objects
                .iter()
                .any(|o| o.name == "orders" && o.kind == ObjectKind::Table),
            "{t:?}: {objects:?}"
        );
        assert!(
            objects.iter().all(|o| o.schema == schema),
            "{t:?}: {objects:?}"
        );
        assert!(
            objects.iter().any(|o| o.kind == ObjectKind::View),
            "{t:?}: {objects:?}"
        );
        close_connection_impl(&state, id).await.unwrap();
    }
}

#[tokio::test]
async fn refresh_after_ddl_sees_new_table() {
    require_db!();
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let db = "dnt_p2_refresh";
        ensure_database(t, db, "t_base", "t_new").await;

        let before = completion_scope_impl(&state, id, db.into()).await.unwrap();
        assert!(!table_names(&before).contains(&"t_new"), "{t:?}");
        let create = match t.sql_kind() {
            SqlKind::MySql => format!("CREATE TABLE {db}.t_new (id int, note varchar(10))"),
            SqlKind::Postgres => "CREATE TABLE t_new (id int, note varchar(10))".to_string(),
        };
        let at = (t.kind() == DbKind::Postgres).then_some(db);
        exec(t, at, &[&create]).await;
        // Still the cached result until refreshed.
        let cached = completion_scope_impl(&state, id, db.into()).await.unwrap();
        assert_eq!(cached, before, "{t:?}");

        state.sessions.refresh_schema_cache(id, db);
        let after = completion_scope_impl(&state, id, db.into()).await.unwrap();
        let new = after
            .tables
            .iter()
            .find(|t| t.name == "t_new")
            .unwrap_or_else(|| panic!("{t:?}: {:?}", table_names(&after)));
        let note = new.columns.iter().find(|c| c.name == "note").unwrap();
        assert!(
            note.data_type.starts_with("varchar(10)") || note.data_type == "character varying(10)",
            "{t:?}: {note:?}"
        );
        close_connection_impl(&state, id).await.unwrap();
    }
}

#[tokio::test]
async fn pg_system_schemas_hidden_unless_requested() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state, Target::Pg17).await;

    let user: Vec<String> = list_schemas_impl(&state, id, FIXTURE_DB.into(), false)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(user.iter().any(|n| n == "public"), "{user:?}");
    assert!(
        !user
            .iter()
            .any(|n| n == "information_schema" || n.starts_with("pg_")),
        "{user:?}"
    );
    let all: Vec<String> = list_schemas_impl(&state, id, FIXTURE_DB.into(), true)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(all.iter().any(|n| n == "pg_catalog"), "{all:?}");
    close_connection_impl(&state, id).await.unwrap();
}
