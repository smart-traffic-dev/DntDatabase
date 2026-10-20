//! Fixtures shared by tests in this crate and downstream crates (feature `test-util`).

use crate::config::{
    ConnOptions, ConnectionConfig, ResolvedConfig, SecretMode, SshAuth, SshConfig,
};
use crate::server::DbKind;
use secrecy::SecretString;
use std::time::Duration;
use uuid::Uuid;

/// A valid MySQL connection config without SSH/SSL.
pub fn sample_config() -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888),
        group_id: None,
        name: "Local MySQL".into(),
        sort_order: 0,
        kind: DbKind::MySql,
        host: "127.0.0.1".into(),
        port: 3306,
        username: "root".into(),
        default_db: Some("app".into()),
        env_tag: None,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    }
}

/// A valid password-authenticated SSH config.
pub fn sample_ssh() -> SshConfig {
    SshConfig {
        version: 1,
        host: "bastion.example.com".into(),
        port: 22,
        user: "deploy".into(),
        auth: SshAuth::Password,
        key_path: None,
    }
}

/// A resolved config pointing at `sample_config()`'s server.
pub fn sample_resolved(pw: Option<&str>) -> ResolvedConfig {
    ResolvedConfig {
        kind: DbKind::MySql,
        host: "127.0.0.1".into(),
        port: 3306,
        tls_server_name: "127.0.0.1".into(),
        username: "root".into(),
        password: pw.map(SecretString::from),
        database: Some("app".into()),
        ssl: None,
        connect_timeout: Duration::from_secs(10),
        read_only: false,
        application_name: "DntDatabase".into(),
    }
}
