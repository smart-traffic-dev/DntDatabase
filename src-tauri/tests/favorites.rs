//! Favorites and snippets commands, called through their `*_impl` functions.

use dnt_core::config::{ConnectionConfig, DriverOptions, SecretMode};
use dnt_core::driver::Driver;
use dnt_core::error::{AppError, DbError};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::DbKind;
use dnt_core::test_util::sample_config;
use dnt_store::db::open_in_memory;
use dnt_store::favorites::{FavoriteInput, SnippetInput};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::fake_command::FakeCommandDriver;
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::favorites::{
    favorites_create_impl, favorites_delete_impl, favorites_list_impl, favorites_move_impl,
    favorites_rename_impl, favorites_update_impl, snippets_create_impl, snippets_delete_impl,
    snippets_list_impl, snippets_update_impl,
};
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

const SECRET: &str = "Fav-S3cret-never-stored-91c";

fn app_state(dir: &std::path::Path) -> (AppState, AppPaths) {
    let paths = AppPaths::in_dir(dir.join("data"));
    let state = AppState::open(paths.clone(), Arc::new(MemorySecretStore::default())).unwrap();
    (state, paths)
}

/// Deserialized like an IPC payload.
fn favorite(v: serde_json::Value) -> FavoriteInput {
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn favorites_commands_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_state(dir.path());
    let a = favorites_create_impl(
        &state,
        favorite(json!({
            "name": "Orders", "folder": "Reports/Daily", "sql": "SELECT * FROM orders",
            "connection_id": null, "database": null
        })),
    )
    .await
    .unwrap();
    let b = favorites_create_impl(
        &state,
        favorite(json!({
            "name": "Users", "folder": null, "sql": "SELECT * FROM users",
            "connection_id": null, "database": "shop"
        })),
    )
    .await
    .unwrap();
    let listed = favorites_list_impl(&state).await.unwrap();
    assert_eq!(listed, vec![b.clone(), a.clone()]);

    let renamed = favorites_rename_impl(&state, a.id, "All orders".into())
        .await
        .unwrap();
    assert_eq!(renamed.name, "All orders");
    let moved = favorites_move_impl(&state, a.id, None).await.unwrap();
    assert_eq!(moved.folder, None);
    let updated = favorites_update_impl(
        &state,
        b.id,
        favorite(json!({
            "name": "Users", "folder": "Admin", "sql": "SELECT 1",
            "connection_id": null, "database": null
        })),
    )
    .await
    .unwrap();
    assert_eq!(updated.folder.as_deref(), Some("Admin"));

    favorites_delete_impl(&state, a.id).await.unwrap();
    let left = favorites_list_impl(&state).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, b.id);
}

#[tokio::test]
async fn favorite_with_secret_is_refused_and_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let (state, paths) = app_state(dir.path());
    let err = favorites_create_impl(
        &state,
        favorite(json!({
            "name": "Create user", "folder": null,
            "sql": format!("CREATE USER 'app'@'%' IDENTIFIED BY '{SECRET}';"),
            "connection_id": null, "database": null
        })),
    )
    .await
    .unwrap_err();
    let app = AppError::from(err);
    assert_eq!(app.kind, "config");
    assert_eq!(app.field.as_deref(), Some("sql"));
    assert!(!app.message.contains(SECRET), "{}", app.message);
    assert!(favorites_list_impl(&state).await.unwrap().is_empty());
    drop(state);

    for entry in std::fs::read_dir(&paths.data_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
                "secret found in {}",
                path.display()
            );
        }
    }
}

/// An `AppState` whose only driver is the fake Command driver as Redis, with one saved
/// (not opened) Redis connection.
fn redis_state(dir: &std::path::Path) -> (AppState, Uuid) {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let driver: Arc<dyn Driver> = Arc::new(FakeCommandDriver::with_kind(DbKind::Redis));
    let sessions = SessionManager::new(
        Arc::clone(&repo),
        Arc::clone(&secrets),
        KnownHosts::new(dir.join("known_hosts")),
        HashMap::from([(DbKind::Redis, driver)]),
    );
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
    let new_secrets = NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    let id = repo.create(cfg, new_secrets).unwrap().id;
    let state = AppState {
        repo,
        secrets,
        sessions,
        paths: AppPaths::in_dir(dir.join("data")),
        export_paths: ExportPaths::default(),
        file_picks: FilePicks::default(),
        ai: Default::default(),
    };
    (state, id)
}

fn assert_sql_refusal(err: DbError) {
    let app = AppError::from(err);
    assert_eq!(app.kind, "config");
    assert_eq!(app.field.as_deref(), Some("sql"));
    assert!(
        app.message.contains("Favorites never store secrets"),
        "{}",
        app.message
    );
}

#[tokio::test]
async fn favorite_with_redis_secret_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (state, conn) = redis_state(dir.path());
    let err = favorites_create_impl(
        &state,
        favorite(json!({
            "name": "Login", "folder": null, "sql": "AUTH s3cret",
            "connection_id": conn, "database": "0"
        })),
    )
    .await
    .unwrap_err();
    assert_sql_refusal(err);
    assert!(favorites_list_impl(&state).await.unwrap().is_empty());

    // The update path refuses too, and leaves the row unchanged.
    let ok = favorites_create_impl(
        &state,
        favorite(json!({
            "name": "Get", "folder": null, "sql": "GET a",
            "connection_id": conn, "database": "0"
        })),
    )
    .await
    .unwrap();
    let err = favorites_update_impl(
        &state,
        ok.id,
        favorite(json!({
            "name": "Get", "folder": null, "sql": "GET a\nAUTH admin s3cret",
            "connection_id": conn, "database": "0"
        })),
    )
    .await
    .unwrap_err();
    assert_sql_refusal(err);
    assert_eq!(favorites_list_impl(&state).await.unwrap(), vec![ok]);
}

#[tokio::test]
async fn favorite_without_connection_uses_sql_check() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = redis_state(dir.path());
    // No connection, or one that does not exist: the SQL check of both dialects.
    for connection_id in [None, Some(Uuid::new_v4())] {
        let err = favorites_create_impl(
            &state,
            favorite(json!({
                "name": "Create user", "folder": null,
                "sql": format!("CREATE USER u IDENTIFIED BY '{SECRET}'"),
                "connection_id": connection_id, "database": null
            })),
        )
        .await
        .unwrap_err();
        assert_sql_refusal(err);
        // Text the SQL check finds no secret in is saved.
        favorites_create_impl(
            &state,
            favorite(json!({
                "name": "Auth", "folder": null, "sql": "AUTH s3cret",
                "connection_id": connection_id, "database": null
            })),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn sql_favorite_rules_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_state(dir.path());
    let mut ids = Vec::new();
    for (kind, port) in [(DbKind::MySql, 3306), (DbKind::Postgres, 5432)] {
        let cfg = ConnectionConfig {
            id: Uuid::nil(),
            kind,
            name: format!("{kind:?}"),
            port,
            ..sample_config()
        };
        let new_secrets = NewSecrets {
            db_password: None,
            ssh_password: None,
            ssh_key_passphrase: None,
        };
        ids.push(Some(state.repo.create(cfg, new_secrets).unwrap().id));
    }
    ids.push(None);
    for connection_id in ids {
        // Either dialect's secrets are refused, whatever the connection's kind.
        for sql in [
            "CREATE USER 'u'@'%' IDENTIFIED BY 's3cret';",
            "ALTER ROLE app PASSWORD 'hunter2'",
            "SET PASSWORD FOR u = 'x'",
            "SELECT dblink_connect('postgresql://u:pw@h/db')",
        ] {
            let err = favorites_create_impl(
                &state,
                favorite(json!({
                    "name": "Secret", "folder": null, "sql": sql,
                    "connection_id": connection_id, "database": null
                })),
            )
            .await
            .unwrap_err();
            assert_sql_refusal(err);
        }
        let saved = favorites_create_impl(
            &state,
            favorite(json!({
                "name": "Users", "folder": null, "sql": "SELECT * FROM users WHERE id = 1",
                "connection_id": connection_id, "database": null
            })),
        )
        .await
        .unwrap();
        assert_eq!(saved.connection_id, connection_id);
    }
}

#[tokio::test]
async fn snippets_commands_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = app_state(dir.path());
    let input: SnippetInput = serde_json::from_value(json!({
        "prefix": "sel", "label": "Select", "body": "SELECT ${1:*} FROM ${2:t};",
        "description": "Select from a table"
    }))
    .unwrap();
    let s = snippets_create_impl(&state, input.clone()).await.unwrap();
    let dup = snippets_create_impl(&state, input.clone())
        .await
        .unwrap_err();
    let app = AppError::from(dup);
    assert_eq!(app.field.as_deref(), Some("prefix"));

    let updated = snippets_update_impl(
        &state,
        s.id,
        SnippetInput {
            label: "Select rows".into(),
            ..input
        },
    )
    .await
    .unwrap();
    assert_eq!(updated.label, "Select rows");
    assert_eq!(snippets_list_impl(&state).await.unwrap(), vec![updated]);
    snippets_delete_impl(&state, s.id).await.unwrap();
    assert!(snippets_list_impl(&state).await.unwrap().is_empty());
}
