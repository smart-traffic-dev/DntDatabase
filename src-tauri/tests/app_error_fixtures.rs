//! Rust → UI error contract: real `DbError → AppError` values, written to
//! `ui/src/test/fixtures/app-errors.json`, which the UI tests use instead of hand-written
//! error shapes. The test fails when the committed file is stale; regenerate it with
//! `DNT_UPDATE_FIXTURES=1 cargo test -p dntdatabase --test app_error_fixtures`.
//!
//! Errors produced by app code paths that need no server (validation, import, secrets,
//! not-open, panics) come from those paths; driver / tunnel errors are built as the
//! drivers build them and staged the way the connect pipeline stages them.

use dnt_core::config::SecretMode;
use dnt_core::error::{ssh_channel_error, AppError, ConnectStage, DbError, SshStage, StageTiming};
use dnt_core::secret::MemorySecretStore;
use dnt_core::test_util::sample_config;
use dnt_store::repo::NodeId;
use dntdatabase::commands::browse::list_databases_impl;
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, trust_host_key_impl, SecretsInput,
};
use dntdatabase::commands::guard;
use dntdatabase::commands::transfer::{export_connections_impl, preview_import_impl};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

const UPDATE_ENV: &str = "DNT_UPDATE_FIXTURES";

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ui/src/test/fixtures/app-errors.json")
}

fn no_secrets() -> SecretsInput {
    serde_json::from_value(json!({})).unwrap()
}

fn timing(stage: ConnectStage, ms: u64) -> StageTiming {
    StageTiming { stage, ms }
}

/// Timings of the stages before `stage`, as the pipeline records them.
fn before(stage: ConnectStage, ssh: bool) -> Vec<StageTiming> {
    let mut all = vec![timing(ConnectStage::ResolveSecrets, 1)];
    if ssh {
        all.push(timing(ConnectStage::OpenTunnel, 120));
    }
    all.push(timing(ConnectStage::ConnectDb, 35));
    all.into_iter().take_while(|t| t.stage != stage).collect()
}

fn staged(stage: ConnectStage, ssh: bool, err: DbError) -> AppError {
    DbError::at_stage(stage, before(stage, ssh), err).into()
}

async fn app_errors() -> BTreeMap<&'static str, AppError> {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(
        AppPaths::in_dir(dir.path().join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap();
    let mut out: BTreeMap<&'static str, AppError> = BTreeMap::new();

    // Field errors of the connection dialog (validation in `create_connection`).
    let field = |f: fn(&mut dnt_core::config::ConnectionConfig)| {
        let mut cfg = sample_config();
        f(&mut cfg);
        cfg
    };
    for (name, cfg) in [
        ("field_name", field(|c| c.name = " ".into())),
        ("field_host", field(|c| c.host = String::new())),
        ("field_port", field(|c| c.port = 0)),
        (
            "field_ssh_key_path",
            field(|c| {
                c.ssh = Some(dnt_core::config::SshConfig {
                    auth: dnt_core::config::SshAuth::Key,
                    key_path: None,
                    ..dnt_core::test_util::sample_ssh()
                })
            }),
        ),
        (
            "field_ssl_ca_path",
            field(|c| {
                c.ssl = Some(dnt_core::config::SslConfig {
                    version: 1,
                    mode: dnt_core::config::SslMode::VerifyFull,
                    ca_path: None,
                    cert_path: None,
                    key_path: None,
                })
            }),
        ),
    ] {
        let err = create_connection_impl(&state, cfg, no_secrets())
            .await
            .unwrap_err();
        out.insert(name, err.into());
    }

    // Import / export (`.dntconn`).
    let saved = create_connection_impl(
        &state,
        sample_config(),
        serde_json::from_value(json!({ "db_password": "pw" })).unwrap(),
    )
    .await
    .unwrap();
    let file = dir.path().join("conns.dntconn");
    let path = file.to_string_lossy().into_owned();
    let err = export_connections_impl(
        &state,
        vec![NodeId::Connection(saved.id)],
        Some("short".into()),
        path.clone(),
    )
    .await
    .unwrap_err();
    out.insert("passphrase_too_short", err.into());
    export_connections_impl(
        &state,
        vec![NodeId::Connection(saved.id)],
        Some("long enough passphrase".into()),
        path.clone(),
    )
    .await
    .unwrap();
    let err = preview_import_impl(&state, path, Some("wrong passphrase!".into()))
        .await
        .unwrap_err();
    out.insert("passphrase_incorrect", err.into());
    let missing = dir.path().join("missing.dntconn");
    let err = preview_import_impl(&state, missing.to_string_lossy().into_owned(), None)
        .await
        .unwrap_err();
    out.insert("import_unreadable", err.into());
    let junk = dir.path().join("notes.txt");
    std::fs::write(&junk, "hello").unwrap();
    let err = preview_import_impl(&state, junk.to_string_lossy().into_owned(), None)
        .await
        .unwrap_err();
    out.insert("import_unknown_format", err.into());

    // Opening: Ask mode without a prompt, and browsing a connection that is not open.
    let ask = create_connection_impl(
        &state,
        dnt_core::config::ConnectionConfig {
            id: Uuid::nil(),
            secret_mode: SecretMode::Ask,
            ..sample_config()
        },
        no_secrets(),
    )
    .await
    .unwrap();
    let err = open_connection_impl(&state, ask.id, None)
        .await
        .unwrap_err();
    out.insert("password_required", err.into());
    let err = list_databases_impl(&state, ask.id, false)
        .await
        .unwrap_err();
    out.insert("not_open", err.into());

    // Trusting a host key that is not a valid OpenSSH public key.
    let err = trust_host_key_impl(
        &state,
        "bastion".into(),
        22,
        "SHA256:abc123".into(),
        "not a key".into(),
    )
    .await
    .unwrap_err();
    out.insert("host_key_trust_invalid", err.into());

    // A command that panicked.
    let internal = guard(async {
        if true {
            panic!("boom");
        }
        Ok::<(), DbError>(())
    })
    .await
    .unwrap_err();
    out.insert("internal", internal);

    // Driver errors, as the drivers report them, staged like the connect pipeline does.
    use ConnectStage::{ConnectDb, OpenTunnel};
    out.insert(
        "connect_refused",
        staged(
            ConnectDb,
            false,
            DbError::Connect {
                message: "No connection could be made because the target machine actively \
                          refused it. (os error 10061)"
                    .into(),
            },
        ),
    );
    out.insert(
        "auth_mysql",
        staged(
            ConnectDb,
            false,
            DbError::Auth {
                message: "Access denied for user 'root'@'172.18.0.1' (using password: YES) \
                          [1045]"
                    .into(),
            },
        ),
    );
    out.insert(
        "auth_pg",
        staged(
            ConnectDb,
            false,
            DbError::Auth {
                message: "password authentication failed for user \"postgres\" [28P01]".into(),
            },
        ),
    );
    out.insert(
        "tls",
        staged(
            ConnectDb,
            false,
            DbError::Tls {
                message: "invalid peer certificate: UnknownIssuer".into(),
            },
        ),
    );
    out.insert(
        "server_db_missing",
        staged(
            ConnectDb,
            false,
            DbError::Server {
                code: Some("1049".into()),
                sqlstate: Some("42000".into()),
                message: "Unknown database 'shop'".into(),
                position: None,
            },
        ),
    );
    out.insert(
        "timeout_connect",
        staged(ConnectDb, false, DbError::Timeout { stage: ConnectDb }),
    );
    out.insert(
        "ssh_connect",
        staged(
            OpenTunnel,
            true,
            DbError::Ssh {
                stage: SshStage::Connect,
                message: "Connection refused (os error 10061)".into(),
            },
        ),
    );
    out.insert(
        "ssh_auth",
        staged(
            OpenTunnel,
            true,
            DbError::Ssh {
                stage: SshStage::Auth,
                message: "the server rejected the SSH credentials of user ops".into(),
            },
        ),
    );
    out.insert(
        "ssh_channel",
        staged(
            ConnectDb,
            true,
            ssh_channel_error("db.internal", 3306, "Channel open failure: ConnectFailed"),
        ),
    );
    out.insert(
        "ssh_host_key_mismatch",
        staged(
            OpenTunnel,
            true,
            DbError::Ssh {
                stage: SshStage::HostKeyMismatch,
                message: "the host key of [bastion]:22 changed: trusted SHA256:old, server \
                          sent SHA256:new"
                    .into(),
            },
        ),
    );
    out.insert(
        "ssh_host_key_unknown",
        staged(
            OpenTunnel,
            true,
            DbError::SshHostKeyUnknown {
                host: "bastion".into(),
                port: 22,
                fingerprint: "SHA256:abc123".into(),
                key_openssh: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample".into(),
            },
        ),
    );

    // Browsing an open connection.
    out.insert(
        "timeout_execute",
        DbError::Timeout {
            stage: ConnectStage::Execute,
        }
        .into(),
    );
    out.insert(
        "connection_lost",
        DbError::ConnectionLost {
            in_transaction: false,
        }
        .into(),
    );
    out.insert("cancelled", DbError::Cancelled.into());
    out
}

#[tokio::test]
async fn ui_error_fixtures_match_the_backend() {
    let errors = app_errors().await;
    let text = format!("{}\n", serde_json::to_string_pretty(&errors).unwrap());
    let path = fixture_path();
    if std::env::var(UPDATE_ENV).is_ok_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        committed == text,
        "{} is stale: rerun with {UPDATE_ENV}=1 and commit the result",
        path.display()
    );
}
