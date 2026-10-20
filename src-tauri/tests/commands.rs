//! Command layer called directly through the `*_impl` functions (no Tauri runtime).

use dnt_core::secret::{MemorySecretStore, SecretKind, SecretStore};
use dnt_core::test_util::sample_config;
use dnt_store::repo::NodeId;
use dnt_store::transfer::ConflictPolicy;
use dntdatabase::commands::connections::{
    create_connection_impl, list_tree_impl, update_connection_impl, SecretsInput,
    SecretsUpdateInput,
};
use dntdatabase::commands::settings::{
    get_confirm_unbounded_writes_impl, get_idle_tx_warn_minutes_impl, get_log_level_impl,
    get_table_page_size_impl, saved_confirm_unbounded, saved_idle_tx_warn_minutes, saved_log_level,
    saved_table_page_size, set_confirm_unbounded_writes_impl, set_idle_tx_warn_minutes_impl,
    set_log_level_impl, set_table_page_size_impl, CONFIRM_UNBOUNDED_KEY, IDLE_TX_WARN_KEY,
};
use dntdatabase::commands::transfer::{
    apply_import_impl, export_connections_impl, import_needs_passphrase_impl, preview_import_impl,
};
use dntdatabase::commands::workspace::{
    file_read_impl, file_write_impl, history_clear_impl, history_delete_impl, history_list_impl,
    tabs_delete_impl, tabs_load_impl, tabs_save_impl,
};
use dntdatabase::commands::{export_bindings, BINDINGS_PATH};
use dntdatabase::file_picks::FileAccess;
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
    // Picked in the save dialog of the source app.
    source
        .file_picks
        .admit(FileAccess::ConnectionsExport, file.clone())
        .unwrap();
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

    let store = Arc::new(MemorySecretStore::default());
    let target = app_state(&dir.path().join("b"), Arc::clone(&store));
    // Picked in the open dialog of the target app.
    target
        .file_picks
        .admit(FileAccess::ConnectionsImport, file.clone())
        .unwrap();
    assert!(import_needs_passphrase_impl(&target, path.clone())
        .await
        .unwrap());
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
    target
        .file_picks
        .admit(FileAccess::ConnectionsImport, junk.clone())
        .unwrap();
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
    let missing = state
        .file_picks
        .admit(
            FileAccess::ConnectionsImport,
            dir.path().join("missing.dntconn"),
        )
        .unwrap();
    for err in [
        preview_import_impl(&state, missing.clone(), None)
            .await
            .unwrap_err(),
        import_needs_passphrase_impl(&state, missing.clone())
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

#[tokio::test]
async fn confirm_unbounded_writes_defaults_on_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert!(get_confirm_unbounded_writes_impl(&state).await.unwrap());
    set_confirm_unbounded_writes_impl(&state, false)
        .await
        .unwrap();
    assert!(!get_confirm_unbounded_writes_impl(&state).await.unwrap());
    // Survives a restart.
    drop(state);
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert!(!saved_confirm_unbounded(&state.repo));
    set_confirm_unbounded_writes_impl(&state, true)
        .await
        .unwrap();
    assert!(saved_confirm_unbounded(&state.repo));
    // Anything but `0` stored keeps the confirmation on.
    state
        .repo
        .set_app_setting(CONFIRM_UNBOUNDED_KEY, "garbage")
        .unwrap();
    assert!(saved_confirm_unbounded(&state.repo));
}

#[tokio::test]
async fn idle_tx_warn_minutes_defaults_to_5_and_is_validated() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert_eq!(get_idle_tx_warn_minutes_impl(&state).await.unwrap(), 5);
    for good in [1, 30, 120] {
        set_idle_tx_warn_minutes_impl(&state, good).await.unwrap();
        assert_eq!(get_idle_tx_warn_minutes_impl(&state).await.unwrap(), good);
    }
    for bad in [0, 121] {
        let err = set_idle_tx_warn_minutes_impl(&state, bad)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, dnt_core::error::DbError::Config { field: Some(f), .. } if f == "minutes"),
            "{err:?}"
        );
    }
    assert_eq!(get_idle_tx_warn_minutes_impl(&state).await.unwrap(), 120);
    // Survives a restart; a corrupt stored value falls back to the default.
    drop(state);
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert_eq!(saved_idle_tx_warn_minutes(&state.repo), 120);
    state.repo.set_app_setting(IDLE_TX_WARN_KEY, "999").unwrap();
    assert_eq!(saved_idle_tx_warn_minutes(&state.repo), 5);
}

#[tokio::test]
async fn table_page_size_defaults_to_100_and_is_validated() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert_eq!(get_table_page_size_impl(&state).await.unwrap(), 100);
    set_table_page_size_impl(&state, 500).await.unwrap();
    assert_eq!(get_table_page_size_impl(&state).await.unwrap(), 500);
    for bad in [0, 9, 10_001] {
        let err = set_table_page_size_impl(&state, bad).await.unwrap_err();
        assert!(
            matches!(&err, dnt_core::error::DbError::Config { field: Some(f), .. } if f == "size"),
            "{err:?}"
        );
    }
    assert_eq!(get_table_page_size_impl(&state).await.unwrap(), 500);
    // Survives a restart; a corrupt stored value falls back to the default.
    drop(state);
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert_eq!(saved_table_page_size(&state.repo), 500);
    state
        .repo
        .set_app_setting("table_page_size", "abc")
        .unwrap();
    assert_eq!(saved_table_page_size(&state.repo), 100);
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

/// The committed `ui/src/bindings.ts` matches what the commands export. With
/// `DNT_UPDATE_FIXTURES=1` the committed file is rewritten instead.
#[test]
fn bindings_are_current() {
    let committed = std::path::Path::new(BINDINGS_PATH);
    if std::env::var("DNT_UPDATE_FIXTURES").as_deref() == Ok("1") {
        export_bindings(committed).unwrap();
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("bindings.ts");
    export_bindings(&fresh).unwrap();
    // Line endings may differ with git's autocrlf on Windows.
    let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap().replace("\r\n", "\n");
    assert!(
        read(&fresh) == read(committed),
        "ui/src/bindings.ts is stale; regenerate it with \
         DNT_UPDATE_FIXTURES=1 cargo test -p dntdatabase --test commands bindings_are_current"
    );
}

/// A `.sql` path as picked in the backend's dialog.
fn picked_sql(state: &AppState, file: &std::path::Path) -> String {
    state
        .file_picks
        .admit(FileAccess::Sql, file.to_path_buf())
        .unwrap()
}

#[tokio::test]
async fn file_roundtrip_utf8_without_bom() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let file = dir.path().join("query.sql");
    let path = picked_sql(&state, &file);
    let text = "SELECT 'Tiếng Việt 😀';\r\n";
    file_write_impl(&state, path.clone(), text.to_string())
        .await
        .unwrap();
    let bytes = std::fs::read(&file).unwrap();
    assert!(!bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
    assert_eq!(bytes, text.as_bytes());
    assert_eq!(file_read_impl(&state, path.clone()).await.unwrap(), text);

    // A leading BOM is never written, and is stripped on read.
    file_write_impl(&state, path.clone(), format!("\u{feff}{text}"))
        .await
        .unwrap();
    assert!(!std::fs::read(&file)
        .unwrap()
        .starts_with(&[0xEF, 0xBB, 0xBF]));
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend_from_slice(text.as_bytes());
    std::fs::write(&file, with_bom).unwrap();
    assert_eq!(file_read_impl(&state, path).await.unwrap(), text);
}

#[tokio::test]
async fn file_write_replaces_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let file = work.join("existing.sql");
    std::fs::write(&file, "OLD CONTENT THAT IS LONGER THAN THE NEW ONE").unwrap();
    let path = picked_sql(&state, &file);
    file_write_impl(&state, path.clone(), "SELECT 2;".into())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "SELECT 2;");
    // Writing into a missing directory fails and leaves nothing behind either.
    let bad = picked_sql(&state, &work.join("missing").join("x.sql"));
    assert!(file_write_impl(&state, bad, "x".into()).await.is_err());
    let names: Vec<String> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["existing.sql"], "temporary file left behind");
}

#[tokio::test]
async fn file_read_rejects_non_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let file = dir.path().join("latin1.sql");
    std::fs::write(&file, b"SELECT '\xE9t\xE9';").unwrap();
    let err = file_read_impl(&state, picked_sql(&state, &file))
        .await
        .unwrap_err();
    let app = dnt_core::error::AppError::from(err);
    assert_eq!(app.kind, "config");
    assert_eq!(app.field.as_deref(), Some("path"));
    assert_eq!(app.message, "The file is not UTF-8 text.");

    let missing = picked_sql(&state, &dir.path().join("missing.sql"));
    let app = dnt_core::error::AppError::from(file_read_impl(&state, missing).await.unwrap_err());
    assert_eq!(app.kind, "config");
    assert!(
        app.message.starts_with("Could not read missing.sql: "),
        "{app:?}"
    );
}

#[tokio::test]
async fn tabs_commands_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    // Deserialized like an IPC payload.
    let tab: dnt_store::tabs::QueryTabRecord = serde_json::from_value(json!({
        "id": "tab-1", "title": "Query 1", "sort_order": 0, "connection_id": null,
        "database": null, "sql_text": "SELECT 1", "cursor": 8, "file_path": null,
        "dirty": false, "continue_on_error": true, "row_limit": 100000, "updated_at": ""
    }))
    .unwrap();
    tabs_save_impl(&state, tab.clone()).await.unwrap();
    let loaded = tabs_load_impl(&state).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].sql_text, "SELECT 1");
    assert!(loaded[0].continue_on_error);
    assert_eq!(loaded[0].row_limit, 100_000);

    // Restored after a restart.
    drop(state);
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    assert_eq!(tabs_load_impl(&state).await.unwrap().len(), 1);
    tabs_delete_impl(&state, "tab-1".into()).await.unwrap();
    assert!(tabs_load_impl(&state).await.unwrap().is_empty());
}

fn is_path_refusal(err: &dnt_core::error::DbError) -> bool {
    matches!(err, dnt_core::error::DbError::Config { field: Some(f), message }
        if f == "path" && message.starts_with("Choose the file"))
}

/// `file_read` / `file_write` and the connection import / export never touch a path
/// the UI names unless it was picked in the backend's dialog for that use.
#[tokio::test]
async fn unpicked_paths_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "do not read").unwrap();
    let victim = dir.path().join("victim.sql");
    std::fs::write(&victim, "keep").unwrap();
    let s = |p: &std::path::Path| p.to_string_lossy().into_owned();

    let err = file_read_impl(&state, s(&secret)).await.unwrap_err();
    assert!(is_path_refusal(&err), "{err:?}");
    let err = file_write_impl(&state, s(&victim), "x".into())
        .await
        .unwrap_err();
    assert!(is_path_refusal(&err), "{err:?}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    // An empty path is still "no file chosen".
    let err = file_read_impl(&state, String::new()).await.unwrap_err();
    assert!(
        matches!(&err, dnt_core::error::DbError::Config { message, .. } if message == "No file was chosen."),
        "{err:?}"
    );

    // A connections file picked to import is not writable, and the reverse.
    let import = state
        .file_picks
        .admit(FileAccess::ConnectionsImport, victim.clone())
        .unwrap();
    let err = export_connections_impl(&state, vec![], None, import.clone())
        .await
        .unwrap_err();
    assert!(is_path_refusal(&err), "{err:?}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    let err = file_write_impl(&state, import, "x".into())
        .await
        .unwrap_err();
    assert!(is_path_refusal(&err), "{err:?}");
    let export = state
        .file_picks
        .admit(FileAccess::ConnectionsExport, secret.clone())
        .unwrap();
    for err in [
        preview_import_impl(&state, export.clone(), None)
            .await
            .unwrap_err(),
        apply_import_impl(&state, export.clone(), None, ConflictPolicy::Skip)
            .await
            .unwrap_err(),
        import_needs_passphrase_impl(&state, export.clone())
            .await
            .unwrap_err(),
        file_read_impl(&state, export).await.unwrap_err(),
    ] {
        assert!(is_path_refusal(&err), "{err:?}");
    }
}

/// A query tab's file keeps working after a restart (`tabs_load` re-admits the paths
/// stored in the app database), but the UI cannot plant a path through `tabs_save`.
#[tokio::test]
async fn tab_files_are_admitted_from_the_stored_tabs_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let file = dir.path().join("saved.sql");
    let tab = |id: &str, path: &std::path::Path| dnt_store::tabs::QueryTabRecord {
        id: id.into(),
        title: "saved.sql".into(),
        sort_order: 0,
        connection_id: None,
        database: None,
        sql_text: "SELECT 1".into(),
        cursor: 0,
        file_path: Some(path.to_string_lossy().into_owned()),
        dirty: false,
        continue_on_error: false,
        row_limit: 1000,
        updated_at: String::new(),
    };

    // Not picked: refused, and nothing is stored.
    let planted = dir.path().join("planted.sql");
    let err = tabs_save_impl(&state, tab("tab-1", &planted))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, dnt_core::error::DbError::Config { field: Some(f), .. } if f == "file_path"),
        "{err:?}"
    );
    assert!(tabs_load_impl(&state).await.unwrap().is_empty());

    // Picked (Save as): stored, written.
    let path = picked_sql(&state, &file);
    file_write_impl(&state, path.clone(), "SELECT 1".into())
        .await
        .unwrap();
    tabs_save_impl(&state, tab("tab-1", &file)).await.unwrap();

    // After a restart the stored file is admitted again by `tabs_load` (Ctrl+S).
    drop(state);
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let err = file_write_impl(&state, path.clone(), "SELECT 2".into())
        .await
        .unwrap_err();
    assert!(
        is_path_refusal(&err),
        "not before the tabs are loaded: {err:?}"
    );
    assert_eq!(tabs_load_impl(&state).await.unwrap().len(), 1);
    file_write_impl(&state, path.clone(), "SELECT 2".into())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "SELECT 2");
    assert_eq!(file_read_impl(&state, path).await.unwrap(), "SELECT 2");
    // Saving the tab again keeps its stored file; another tab cannot take a planted one.
    tabs_save_impl(&state, tab("tab-1", &file)).await.unwrap();
    assert!(tabs_save_impl(&state, tab("tab-2", &planted))
        .await
        .is_err());
    let err = file_read_impl(&state, planted.to_string_lossy().into_owned())
        .await
        .unwrap_err();
    assert!(is_path_refusal(&err), "{err:?}");
}

#[tokio::test]
async fn history_commands() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path(), Arc::new(MemorySecretStore::default()));
    let cfg = create_connection_impl(
        &state,
        sample_config(),
        serde_json::from_value(json!({})).unwrap(),
    )
    .await
    .unwrap();
    let mut ids = Vec::new();
    for sql in ["SELECT 1", "SELECT 2", "SELECT 3"] {
        let id = state
            .repo
            .history_insert(&dnt_store::history::NewHistoryEntry {
                connection_id: cfg.id,
                database: None,
                sql_text: sql.into(),
                redacted: false,
                kind: "select".into(),
                started_at: "2026-10-20T10:00:00Z".into(),
                elapsed_ms: Some(1),
                status: dnt_store::history::HistoryStatus::Ok,
                row_count: Some(1),
                affected: None,
                error_kind: None,
                error_code: None,
                origin: dnt_store::history::HistoryOrigin::Query,
                outcome_unknown: false,
            })
            .unwrap();
        ids.push(id);
    }
    let filter: dnt_store::history::HistoryFilter =
        serde_json::from_value(json!({ "connection_id": cfg.id, "search": "2", "limit": 50 }))
            .unwrap();
    let found = history_list_impl(&state, filter).await.unwrap();
    assert_eq!(found.len(), 1);
    // The status is serialized in snake_case.
    assert_eq!(serde_json::to_value(&found[0]).unwrap()["status"], "ok");
    assert_eq!(serde_json::to_value(&found[0]).unwrap()["origin"], "query");
    assert_eq!(
        serde_json::to_value(&found[0]).unwrap()["outcome_unknown"],
        false
    );

    let all: dnt_store::history::HistoryFilter =
        serde_json::from_value(json!({ "connection_id": null, "search": null, "limit": 50 }))
            .unwrap();
    let delete: dnt_store::history::HistoryIds = serde_json::from_value(json!([ids[0]])).unwrap();
    history_delete_impl(&state, delete).await.unwrap();
    assert_eq!(
        history_list_impl(&state, all.clone()).await.unwrap().len(),
        2
    );
    history_clear_impl(&state).await.unwrap();
    assert!(history_list_impl(&state, all).await.unwrap().is_empty());
}
