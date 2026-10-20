//! Favorites and snippets commands, called through their `*_impl` functions.

use dnt_core::error::AppError;
use dnt_core::secret::MemorySecretStore;
use dnt_store::favorites::{FavoriteInput, SnippetInput};
use dntdatabase::commands::favorites::{
    favorites_create_impl, favorites_delete_impl, favorites_list_impl, favorites_move_impl,
    favorites_rename_impl, favorites_update_impl, snippets_create_impl, snippets_delete_impl,
    snippets_list_impl, snippets_update_impl,
};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::sync::Arc;

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
