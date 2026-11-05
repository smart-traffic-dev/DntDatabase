//! Saving Mongo/Redis connections through the command layer (R3), before their drivers exist.

use dnt_core::config::{ConnectionConfig, DriverOptions};
use dnt_core::error::{AppError, DbError};
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::test_util::sample_config;
use dntdatabase::commands::connections::{create_connection_impl, SecretsInput};
use dntdatabase::state::{AppPaths, AppState};
use std::sync::Arc;
use uuid::Uuid;

/// A Redis connection is saved like any other; opening it fails cleanly until R4 registers a
/// Redis driver.
#[tokio::test]
async fn redis_connection_saves_but_has_no_driver() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(
        AppPaths::in_dir(dir.path().join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap();
    let mut cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: "Local Redis".into(),
        port: 6379,
        default_db: None,
        ..sample_config()
    };
    cfg.options.driver = DriverOptions::Redis { db: 0 };
    let secrets: SecretsInput = serde_json::from_value(serde_json::json!({
        "db_password": "redis-pw",
        "ssh_password": null,
        "ssh_key_passphrase": null,
    }))
    .unwrap();

    let created = create_connection_impl(&state, cfg, secrets).await.unwrap();
    assert_eq!(created.kind, DbKind::Redis);
    assert_eq!(state.repo.get(created.id).unwrap(), created);

    let error = match state.sessions.open(created.id, None).await {
        Err(e) => e,
        Ok(_) => panic!("a Redis connection opened without a Redis driver"),
    };
    match &error {
        DbError::Config { field, message } => {
            assert_eq!(field, &None);
            assert_eq!(message, "This build has no Redis driver yet.");
        }
        other => panic!("expected Config, got {other:?}"),
    }
    // A user-facing kind (the UI shows no internal-error toast), the message as is.
    let app: AppError = error.into();
    assert_eq!(app.kind, "config");
    assert_eq!(app.message, "This build has no Redis driver yet.");
}
