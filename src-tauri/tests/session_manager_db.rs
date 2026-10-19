//! `SessionManager` against the Docker test matrix (`DNT_TEST_DB=1`).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode, SshAuth};
use dnt_core::driver::{Driver, Introspector};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{ObjectKind, Scope};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{DbKind, ServerVariant};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::{require_db, ssh_config, Target, FIXTURE_DB, TEST_PASSWORD};
use dnt_tunnel::KnownHosts;
use dntdatabase::cache::{NodeKey, SchemaCache};
use dntdatabase::session_manager::SessionManager;
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

struct Fixture {
    sm: SessionManager,
    repo: Arc<ConnectionRepo>,
    known_hosts: KnownHosts,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = KnownHosts::new(dir.path().join("known_hosts"));
    let drivers: HashMap<DbKind, Arc<dyn Driver>> = HashMap::from([
        (DbKind::MySql, Arc::new(MySqlDriver) as Arc<dyn Driver>),
        (DbKind::Postgres, Arc::new(PgDriver) as Arc<dyn Driver>),
    ]);
    let sm = SessionManager::new(Arc::clone(&repo), secrets, known_hosts.clone(), drivers);
    Fixture {
        sm,
        repo,
        known_hosts,
        _dir: dir,
    }
}

/// Direct (no SSH) config for `t` as its superuser.
fn direct(t: Target, db: Option<&str>) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: db.map(str::to_string),
        env_tag: None,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    }
}

fn db_password() -> NewSecrets {
    NewSecrets {
        db_password: Some(SecretString::from(TEST_PASSWORD)),
        ssh_password: None,
        ssh_key_passphrase: None,
    }
}

/// Polls the SchemaCache of `id` for up to 5 s until it holds `table` with `column`.
async fn wait_schema_cache(
    sm: &SessionManager,
    id: Uuid,
    table: &str,
    column: &str,
) -> SchemaCache {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(cache) = sm.schema_cache(id) {
            let found = cache
                .tables
                .iter()
                .any(|t| t.name == table && t.columns.iter().any(|c| c == column));
            assert!(found, "{table}.{column} missing from {cache:?}");
            return cache;
        }
        assert!(Instant::now() < deadline, "SchemaCache not ready in 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn open_mysql8_via_ssh_reports_timings() {
    require_db!();
    let f = fixture();
    let (ssh, ssh_secrets) = ssh_config(SshAuth::Password);
    let (target_host, target_port) = Target::MySql8.ssh_target_host();
    let cfg = ConnectionConfig {
        host: target_host.into(),
        port: target_port,
        ssh: Some(ssh),
        ..direct(Target::MySql8, None)
    };
    let id = f
        .repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(SecretString::from(TEST_PASSWORD)),
                ..ssh_secrets
            },
        )
        .unwrap()
        .id;

    // The fresh known_hosts does not trust the server yet: the UI asks, trusts, retries.
    match f.sm.open(id, None).await.map_err(DbError::into_unstaged) {
        Err(DbError::SshHostKeyUnknown {
            host,
            port,
            fingerprint,
            key_openssh,
        }) => f
            .known_hosts
            .trust(&host, port, &fingerprint, &key_openssh)
            .unwrap(),
        other => panic!("expected SshHostKeyUnknown, got {other:?}"),
    }

    let res = f.sm.open(id, None).await.unwrap();
    assert_eq!(res.info.variant, ServerVariant::MySql8);
    let stages: Vec<_> = res.timings.iter().map(|t| t.stage).collect();
    assert_eq!(
        stages,
        [
            ConnectStage::ResolveSecrets,
            ConnectStage::OpenTunnel,
            ConnectStage::ConnectDb,
            ConnectStage::DetectServer
        ]
    );

    // Browsing goes through the tunnel too.
    let dbs =
        f.sm.browse(id, None, NodeKey(vec![]), |i: &dyn Introspector| {
            i.list_databases()
        })
        .await
        .unwrap();
    assert!(dbs.iter().any(|d| d.name == FIXTURE_DB), "{dbs:?}");
    f.sm.close(id).await;
}

#[tokio::test]
async fn schema_cache_populated() {
    require_db!();
    let f = fixture();
    let id = f
        .repo
        .create(direct(Target::MySql8, Some(FIXTURE_DB)), db_password())
        .unwrap()
        .id;
    f.sm.open(id, None).await.unwrap();
    let cache = wait_schema_cache(&f.sm, id, "customers", "email").await;
    assert_eq!(cache.database, FIXTURE_DB);
    f.sm.close(id).await;
}

#[tokio::test]
async fn pg_schema_cache_and_browse_other_database() {
    require_db!();
    let f = fixture();
    let id = f
        .repo
        .create(direct(Target::Pg17, Some(FIXTURE_DB)), db_password())
        .unwrap()
        .id;
    f.sm.open(id, None).await.unwrap();
    let cache = wait_schema_cache(&f.sm, id, "customers", "email").await;
    assert_eq!(cache.database, FIXTURE_DB);
    assert!(cache
        .tables
        .iter()
        .all(|t| t.schema.as_deref() == Some("public")));

    // Another database gets its own metadata session instead of `Unsupported`.
    let scope = Scope {
        database: "postgres".into(),
        schema: Some("public".into()),
    };
    let tables =
        f.sm.browse(
            id,
            Some("postgres"),
            NodeKey(vec!["db:postgres".into(), "schema:public".into()]),
            move |i: &dyn Introspector| {
                let scope = scope.clone();
                Box::pin(async move { i.list_objects(&scope, ObjectKind::Table).await })
            },
        )
        .await
        .unwrap();
    assert!(tables.iter().all(|t| t.name != "customers"), "{tables:?}");
    f.sm.close(id).await;
}

#[tokio::test]
async fn test_connection_does_not_persist() {
    require_db!();
    let f = fixture();
    let before = f.repo.tree().unwrap();
    let cfg = ConnectionConfig {
        id: Uuid::new_v4(),
        ..direct(Target::Pg17, Some(FIXTURE_DB))
    };
    let res = f.sm.test(&cfg, db_password()).await.unwrap();
    assert_eq!(res.info.variant, ServerVariant::Postgres);
    assert_eq!(f.repo.tree().unwrap(), before);
    assert!(f.sm.open_connections().is_empty());

    // A failing test leaves nothing behind either.
    let bad = NewSecrets {
        db_password: Some(SecretString::from("wrong")),
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    let err = f.sm.test(&cfg, bad).await.unwrap_err();
    assert_eq!(err.stage(), Some(ConnectStage::ConnectDb));
    assert!(matches!(err.unstaged(), DbError::Auth { .. }), "{err:?}");
    assert_eq!(f.repo.tree().unwrap(), before);
    assert!(f.sm.open_connections().is_empty());
}
