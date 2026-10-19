//! Command layer called directly through the `*_impl` functions (no Tauri runtime).

use dnt_core::secret::{MemorySecretStore, SecretKind, SecretStore};
use dnt_core::test_util::sample_config;
use dnt_store::repo::NodeId;
use dnt_store::transfer::ConflictPolicy;
use dntdatabase::commands::connections::{
    create_connection_impl, list_tree_impl, update_connection_impl, SecretsInput,
    SecretsUpdateInput,
};
use dntdatabase::commands::export_bindings;
use dntdatabase::commands::settings::{get_log_level_impl, saved_log_level, set_log_level_impl};
use dntdatabase::commands::transfer::{
    apply_import_impl, export_connections_impl, import_needs_passphrase_impl, preview_import_impl,
};
use dntdatabase::logging::LogLevel;
use dntdatabase::state::{AppPaths, AppState};
use secrecy::ExposeSecret;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const DB_PASSWORD: &str = "db-hunter2-secret";
const SSH_PASSWORD: &str = "ssh-correct-horse";
const KEY_PASSPHRASE: &str = "key-battery-staple";

fn app_state(dir: &std::path::Path, secrets: Arc<MemorySecretStore>) -> AppState {
    AppState::open(AppPaths::in_dir(dir.join("data")), secrets).unwrap()
}

#[tokio::test]
async fn secrets_never_in_command_output() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemorySecretStore::default());
    let state = app_state(dir.path(), Arc::clone(&store));

    // Deserialized like an IPC payload.
    let secrets: SecretsInput = serde_json::from_value(json!({
        "db_password": DB_PASSWORD,
        "ssh_password": SSH_PASSWORD,
        "ssh_key_passphrase": KEY_PASSPHRASE,
    }))
    .unwrap();
    let mut cfg = sample_config();
    cfg.id = Uuid::nil();
    let created = create_connection_impl(&state, cfg, secrets).await.unwrap();
    let tree = list_tree_impl(&state).await.unwrap();

    let out = format!(
        "{}\n{}",
        serde_json::to_string(&created).unwrap(),
        serde_json::to_string(&tree).unwrap()
    );
    for secret in [DB_PASSWORD, SSH_PASSWORD, KEY_PASSPHRASE] {
        assert!(!out.contains(secret), "{secret} leaked into {out}");
    }
    // The secrets did reach the secret store, and the tree reports their presence.
    let stored = store
        .get(created.id, SecretKind::DbPassword)
        .unwrap()
        .unwrap();
    assert_eq!(stored.expose_secret(), DB_PASSWORD);
    assert!(tree.connections[0].has_db_password);
    assert!(tree.connections[0].has_ssh_secret);
}

#[tokio::test]
async fn update_applies_secret_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemorySecretStore::default());
    let state = app_state(dir.path(), Arc::clone(&store));
    let secrets: SecretsInput =
        serde_json::from_value(json!({ "db_password": DB_PASSWORD, "ssh_password": SSH_PASSWORD }))
            .unwrap();
    let cfg = create_connection_impl(&state, sample_config(), secrets)
        .await
        .unwrap();

    let changes: SecretsUpdateInput = serde_json::from_value(json!({
        "db_password": { "set": "new-password" },
        "ssh_password": "clear",
        "ssh_key_passphrase": "keep",
    }))
    .unwrap();
    update_connection_impl(&state, cfg.clone(), changes)
        .await
        .unwrap();

    let db = store.get(cfg.id, SecretKind::DbPassword).unwrap().unwrap();
    assert_eq!(db.expose_secret(), "new-password");
    assert!(store
        .get(cfg.id, SecretKind::SshPassword)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn export_then_import_into_another_app() {
    let dir = tempfile::tempdir().unwrap();
    let source = app_state(
        &dir.path().join("a"),
        Arc::new(MemorySecretStore::default()),
    );
    let secrets: SecretsInput =
        serde_json::from_value(json!({ "db_password": DB_PASSWORD })).unwrap();
    let cfg = create_connection_impl(&source, sample_config(), secrets)
        .await
        .unwrap();
    let file = dir.path().join("export.dntconn");
    let path = file.to_string_lossy().into_owned();
    let passphrase = Some("long enough passphrase".to_string());
    export_connections_impl(
        &source,
        vec![NodeId::Connection(cfg.id)],
        passphrase.clone(),
        path.clone(),
    )
    .await
    .unwrap();
    // Encrypted at rest.
    assert!(!std::fs::read_to_string(&file)
        .unwrap()
        .contains(DB_PASSWORD));
    assert!(import_needs_passphrase_impl(path.clone()).await.unwrap());

    let store = Arc::new(MemorySecretStore::default());
    let target = app_state(&dir.path().join("b"), Arc::clone(&store));
    let preview = preview_import_impl(&target, path.clone(), passphrase.clone())
        .await
        .unwrap();
    assert_eq!(preview.rows.len(), 1);
    assert!(preview.rows[0].has_password);
    let result = apply_import_impl(&target, path, passphrase, ConflictPolicy::Skip)
        .await
        .unwrap();
    assert_eq!(result.imported, 1);
    let stored = store.get(cfg.id, SecretKind::DbPassword).unwrap().unwrap();
    assert_eq!(stored.expose_secret(), DB_PASSWORD);

    // Something that is neither format is a config error.
    let junk = dir.path().join("junk.txt");
    std::fs::write(&junk, "hello").unwrap();
    let err = preview_import_impl(&target, junk.to_string_lossy().into_owned(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, dnt_core::error::DbError::Config { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn unreadable_import_file_is_a_path_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let missing = dir
        .path()
        .join("missing.dntconn")
        .to_string_lossy()
        .into_owned();
    for err in [
        preview_import_impl(&state, missing.clone(), None)
            .await
            .unwrap_err(),
        import_needs_passphrase_impl(missing.clone())
            .await
            .unwrap_err(),
    ] {
        let app = dnt_core::error::AppError::from(err);
        assert_eq!(app.kind, "config");
        assert_eq!(app.field.as_deref(), Some("path"));
        assert!(
            app.message.starts_with("Could not read missing.dntconn: "),
            "{app:?}"
        );
    }
}

#[tokio::test]
async fn log_level_is_persisted_and_restored() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let log = dntdatabase::logging::init(&state.paths.logs_dir);
    assert_eq!(get_log_level_impl(&state).await.unwrap(), LogLevel::Info);

    set_log_level_impl(&state, &log, LogLevel::Debug)
        .await
        .unwrap();
    assert_eq!(get_log_level_impl(&state).await.unwrap(), LogLevel::Debug);

    // A restart reads it back from app.db and applies it.
    drop(state);
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert_eq!(saved_log_level(&state.repo), LogLevel::Debug);
    set_log_level_impl(&state, &log, LogLevel::Info)
        .await
        .unwrap();
    assert_eq!(saved_log_level(&state.repo), LogLevel::Info);
}

#[test]
fn bindings_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bindings.ts");
    export_bindings(&path).unwrap();
    let ts = std::fs::read_to_string(&path).unwrap();
    assert!(ts.contains("export type AppError"), "{ts}");
    assert!(ts.contains("listTree"), "{ts}");
    // 64-bit integers are exported as numbers / number-or-string, never as bigint.
    assert!(!ts.contains("bigint"), "{ts}");
    assert!(
        ts.contains("export type JsInteger = number | string"),
        "{ts}"
    );
    assert!(ts.contains(r#"{ t: "Int"; v: JsInteger }"#), "{ts}");
    assert!(ts.contains(r#"{ t: "UInt"; v: JsInteger }"#), "{ts}");
    assert!(ts.contains("len: number"), "{ts}");
}
