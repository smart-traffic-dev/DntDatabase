//! Browse commands against the Docker test matrix (`DNT_TEST_DB=1`).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::secret::MemorySecretStore;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::browse::{
    list_databases_impl, list_objects_impl, list_schemas_impl, object_ddl_impl, refresh_node_impl,
};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
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
