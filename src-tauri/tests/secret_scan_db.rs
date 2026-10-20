//! Spec §10: secrets live only in the secret store. Creates connections with known
//! secrets, opens one successfully and fails several (DB auth, SSH auth, key passphrase)
//! at Debug log level, exports without passwords, then scans `app.db`, every log file
//! and the export for the secrets (`DNT_TEST_DB=1`; own binary: it installs the global
//! log subscriber).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode, SshAuth};
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_store::repo::NodeId;
use dnt_testkit::{require_db, ssh_config, testdata, Target, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, test_connection_impl, trust_host_key_impl,
    SecretsInput,
};
use dntdatabase::commands::settings::set_log_level_impl;
use dntdatabase::commands::transfer::export_connections_impl;
use dntdatabase::logging::{self, LogLevel};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const DB_SECRET: &str = "Db-S3cret-fa1ling-9f2c";
const SSH_SECRET: &str = "Ssh-S3cret-wr0ng-71ab";
const PASSPHRASE_SECRET: &str = "Key-Passphrase-wr0ng-c0de";
const TYPED_SECRET: &str = "Typed-S3cret-test-44e1";

fn mysql8(name: &str) -> ConnectionConfig {
    let t = Target::MySql8;
    ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: name.into(),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: None,
        env_tag: None,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    }
}

fn secrets(v: serde_json::Value) -> SecretsInput {
    serde_json::from_value(v).unwrap()
}

/// `mysql8` through the compose SSH server.
fn via_ssh(name: &str, auth: SshAuth) -> ConnectionConfig {
    let (mut ssh, _) = ssh_config(auth);
    if auth == SshAuth::Key {
        ssh.key_path = Some(testdata("ssh/id_ed25519_enc"));
    }
    let (host, port) = Target::MySql8.ssh_target_host();
    ConnectionConfig {
        host: host.into(),
        port,
        ssh: Some(ssh),
        ..mysql8(name)
    }
}

/// Every file below `dir` whose bytes contain one of `secrets`, with the secret.
fn files_containing(dir: &Path, secrets: &[&str]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_containing(&path, secrets));
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        for secret in secrets {
            if bytes.windows(secret.len()).any(|w| w == secret.as_bytes()) {
                found.push((path.display().to_string(), secret.to_string()));
            }
        }
    }
    found
}

#[tokio::test]
async fn no_secret_in_app_db_logs_or_export() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::in_dir(dir.path().join("data"));
    std::fs::create_dir_all(&paths.logs_dir).unwrap();
    let log = logging::init(&paths.logs_dir);
    let state = AppState::open(paths.clone(), Arc::new(MemorySecretStore::default())).unwrap();
    set_log_level_impl(&state, &log, LogLevel::Debug)
        .await
        .unwrap();

    // Opens successfully (the real password is a known secret too).
    let ok = create_connection_impl(
        &state,
        mysql8("ok"),
        secrets(json!({ "db_password": TEST_PASSWORD })),
    )
    .await
    .unwrap();
    open_connection_impl(&state, ok.id, None).await.unwrap();

    // Fails at the database: wrong password.
    let bad_db = create_connection_impl(
        &state,
        mysql8("bad-db"),
        secrets(json!({ "db_password": DB_SECRET })),
    )
    .await
    .unwrap();
    let err = open_connection_impl(&state, bad_db.id, None)
        .await
        .unwrap_err();
    assert!(matches!(err.unstaged(), DbError::Auth { .. }), "{err:?}");

    // Fails at SSH authentication: wrong SSH password (after trusting the host key).
    let bad_ssh = create_connection_impl(
        &state,
        via_ssh("bad-ssh", SshAuth::Password),
        secrets(json!({ "db_password": DB_SECRET, "ssh_password": SSH_SECRET })),
    )
    .await
    .unwrap();
    let mut err = open_connection_impl(&state, bad_ssh.id, None)
        .await
        .unwrap_err()
        .into_unstaged();
    if let DbError::SshHostKeyUnknown {
        host,
        port,
        fingerprint,
        key_openssh,
    } = err
    {
        trust_host_key_impl(&state, host, port, fingerprint, key_openssh)
            .await
            .unwrap();
        err = open_connection_impl(&state, bad_ssh.id, None)
            .await
            .unwrap_err()
            .into_unstaged();
    }
    assert!(matches!(err, DbError::Ssh { .. }), "{err:?}");

    // Fails decrypting the SSH key: wrong stored passphrase.
    let bad_key = create_connection_impl(
        &state,
        via_ssh("bad-key", SshAuth::Key),
        secrets(json!({ "db_password": DB_SECRET, "ssh_key_passphrase": PASSPHRASE_SECRET })),
    )
    .await
    .unwrap();
    let err = open_connection_impl(&state, bad_key.id, None)
        .await
        .unwrap_err();
    assert!(matches!(err.unstaged(), DbError::Ssh { .. }), "{err:?}");

    // A failing Test with typed secrets.
    test_connection_impl(
        &state,
        mysql8("typed"),
        secrets(json!({ "db_password": TYPED_SECRET })),
    )
    .await
    .unwrap_err();

    // Export without passwords.
    let export = dir.path().join("data").join("export.dntconn");
    export_connections_impl(
        &state,
        [ok.id, bad_db.id, bad_ssh.id, bad_key.id]
            .map(NodeId::Connection)
            .to_vec(),
        None,
        export.to_string_lossy().into_owned(),
    )
    .await
    .unwrap();

    state.sessions.close(ok.id).await;
    drop(state);
    // Flushes the log writer.
    drop(log);

    let logs: String = std::fs::read_dir(&paths.logs_dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap_or_default())
        .collect();
    assert!(
        logs.contains("connect failed"),
        "debug logs were written: {logs}"
    );
    assert!(paths.db_file.is_file() && export.is_file());

    let secrets = [
        TEST_PASSWORD,
        DB_SECRET,
        SSH_SECRET,
        PASSPHRASE_SECRET,
        TYPED_SECRET,
    ];
    // The scan itself finds a planted secret.
    let decoy = dir.path().join("decoy");
    std::fs::create_dir(&decoy).unwrap();
    std::fs::write(decoy.join("f"), format!("x{SSH_SECRET}x")).unwrap();
    assert_eq!(files_containing(&decoy, &secrets).len(), 1);

    let leaks = files_containing(&paths.data_dir, &secrets);
    assert!(leaks.is_empty(), "secrets found: {leaks:?}");
}
