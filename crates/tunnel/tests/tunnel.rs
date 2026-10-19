use dnt_core::config::{ResolvedConfig, SshAuth, SshConfig};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::{DbError, SshStage};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, ssh_config, testdata, Target};
use dnt_tunnel::{fingerprint, KnownHosts, SshSecrets, Tunnel, TunnelOptions};
use russh::keys::PublicKey;
use secrecy::SecretString;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const SSH_CONTAINER: &str = "dnt-test-ssh-1";
const TIMEOUT: Duration = Duration::from_secs(10);

/// `tunnel_dead_flag` restarts the shared `ssh` container: it takes the write lock,
/// every other test the read lock.
static SSH_SERVER: RwLock<()> = RwLock::const_new(());

fn secret(s: &str) -> Option<SecretString> {
    Some(SecretString::from(s.to_string()))
}

fn password_setup() -> (SshConfig, SshSecrets) {
    let (cfg, s) = ssh_config(SshAuth::Password);
    (
        cfg,
        SshSecrets {
            password: s.ssh_password,
            key_passphrase: None,
        },
    )
}

fn key_setup(key: &str, passphrase: Option<&str>) -> (SshConfig, SshSecrets) {
    let (mut cfg, _) = ssh_config(SshAuth::Key);
    cfg.key_path = Some(testdata(key));
    (
        cfg,
        SshSecrets {
            password: None,
            key_passphrase: passphrase.and_then(secret),
        },
    )
}

fn target(t: Target) -> (String, u16) {
    let (host, port) = t.ssh_target_host();
    (host.to_string(), port)
}

fn temp_known_hosts() -> (tempfile::TempDir, KnownHosts) {
    let dir = tempfile::tempdir().unwrap();
    let kh = KnownHosts::new(dir.path().join("known_hosts"));
    (dir, kh)
}

/// A fresh known_hosts file that already trusts the test server's host key.
async fn trusted_known_hosts() -> (tempfile::TempDir, KnownHosts) {
    let (dir, kh) = temp_known_hosts();
    let (cfg, secrets) = password_setup();
    match Tunnel::open(&cfg, secrets, target(Target::MySql8), &kh, TIMEOUT).await {
        Err(DbError::SshHostKeyUnknown {
            host,
            port,
            fingerprint,
            key_openssh,
        }) => kh.trust(&host, port, &fingerprint, &key_openssh).unwrap(),
        Err(e) => panic!("expected SshHostKeyUnknown, got {e}"),
        Ok(_) => panic!("expected SshHostKeyUnknown, got a tunnel"),
    }
    (dir, kh)
}

async fn open_err(cfg: &SshConfig, secrets: SshSecrets, kh: &KnownHosts) -> DbError {
    match Tunnel::open(cfg, secrets, target(Target::Pg17), kh, TIMEOUT).await {
        Ok(_) => panic!("expected an error, got a tunnel"),
        Err(e) => e,
    }
}

#[derive(Default)]
struct Rows(Vec<Vec<Value>>);

impl RowSink for Rows {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.0.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Connects `driver` to `t` through `tunnel` and checks `SELECT 1`.
async fn select_one_through(tunnel: &Tunnel, driver: &dyn Driver, t: Target) {
    let cfg = ResolvedConfig {
        host: "127.0.0.1".into(),
        port: tunnel.local_addr().port(),
        ..t.resolved(None)
    };
    let session = driver
        .connect(&cfg)
        .await
        .unwrap_or_else(|e| panic!("{t:?} via tunnel: {e}"));
    let mut rows = Rows::default();
    session
        .execute("SELECT 1", ExecOptions::default(), &mut rows)
        .await
        .unwrap();
    assert_eq!(rows.0.len(), 1, "{t:?}");
    assert!(
        matches!(rows.0[0][0], Value::Int(1) | Value::UInt(1)),
        "{t:?}: {:?}",
        rows.0[0][0]
    );
    session.close().await;
}

#[tokio::test]
async fn unknown_host_key_reported_then_trusted() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = temp_known_hosts();
    let (cfg, secrets) = password_setup();
    let err = Tunnel::open(&cfg, secrets, target(Target::MySql8), &kh, TIMEOUT)
        .await
        .err()
        .expect("first open must not trust an unknown key");
    let DbError::SshHostKeyUnknown {
        host,
        port,
        fingerprint: fp,
        key_openssh,
    } = err
    else {
        panic!("expected SshHostKeyUnknown, got {err}");
    };
    assert_eq!((host.as_str(), port), ("127.0.0.1", 2222));
    let key = PublicKey::from_openssh(&key_openssh).unwrap();
    assert_eq!(fp, fingerprint(&key));
    assert!(fp.starts_with("SHA256:"), "{fp}");
    kh.trust(&host, port, &fp, &key_openssh).unwrap();

    let (cfg, secrets) = password_setup();
    let tunnel = Tunnel::open(&cfg, secrets, target(Target::MySql8), &kh, TIMEOUT)
        .await
        .unwrap();
    assert!(tunnel.is_alive());
    let addr = tunnel.local_addr();
    assert!(addr.ip().is_loopback());
    tunnel.close().await;
    // Closing stops the listener.
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn password_auth_forwards_to_mysql() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, secrets) = password_setup();
    let tunnel = Tunnel::open(&cfg, secrets, target(Target::MySql8), &kh, TIMEOUT)
        .await
        .unwrap();
    select_one_through(&tunnel, &MySqlDriver, Target::MySql8).await;
    // Several connections share one tunnel, each on its own channel.
    select_one_through(&tunnel, &MySqlDriver, Target::MySql8).await;
    assert!(tunnel.is_alive());
    tunnel.close().await;
}

#[tokio::test]
async fn every_target_works_through_the_tunnel() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    for t in Target::all() {
        let (cfg, secrets) = password_setup();
        let tunnel = Tunnel::open(&cfg, secrets, target(t), &kh, TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let driver: &dyn Driver = match t.kind() {
            dnt_core::server::DbKind::MySql => &MySqlDriver,
            dnt_core::server::DbKind::Postgres => &PgDriver,
        };
        select_one_through(&tunnel, driver, t).await;
        assert_eq!(tunnel.last_channel_error(), None, "{t:?}");
        tunnel.close().await;
    }
}

#[tokio::test]
async fn unreachable_target_records_the_channel_error() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, secrets) = password_setup();
    // Nothing listens on this port of the mysql8 container.
    let tunnel = Tunnel::open(&cfg, secrets, ("mysql8".into(), 3399), &kh, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(tunnel.last_channel_error(), None);
    let mut db = Target::MySql8.resolved(None);
    db.host = "127.0.0.1".into();
    db.port = tunnel.local_addr().port();
    let err = MySqlDriver
        .connect(&db)
        .await
        .err()
        .expect("nothing listens there");
    let recorded = tunnel.last_channel_error();
    assert!(
        recorded.is_some(),
        "driver saw {err:?}, no channel error recorded"
    );
    tunnel.close().await;
}

#[tokio::test]
async fn wrong_password_is_auth_error() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, _) = password_setup();
    let secrets = SshSecrets {
        password: secret("not-the-password"),
        key_passphrase: None,
    };
    let err = open_err(&cfg, secrets, &kh).await;
    assert!(
        matches!(
            err,
            DbError::Ssh {
                stage: SshStage::Auth,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.to_string().contains("not-the-password"));
}

#[tokio::test]
async fn key_auth_forwards_to_pg() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, secrets) = key_setup("ssh/id_ed25519", None);
    let tunnel = Tunnel::open(&cfg, secrets, target(Target::Pg17), &kh, TIMEOUT)
        .await
        .unwrap();
    select_one_through(&tunnel, &PgDriver, Target::Pg17).await;
    tunnel.close().await;
}

#[tokio::test]
async fn encrypted_key_with_passphrase() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, secrets) = key_setup("ssh/id_ed25519_enc", Some("dnt_pass"));
    let tunnel = Tunnel::open(&cfg, secrets, target(Target::Pg17), &kh, TIMEOUT)
        .await
        .unwrap();
    select_one_through(&tunnel, &PgDriver, Target::Pg17).await;
    tunnel.close().await;
}

#[tokio::test]
async fn wrong_key_passphrase_is_auth_error() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    for pass in [Some("wrong_pass"), None] {
        let (cfg, secrets) = key_setup("ssh/id_ed25519_enc", pass);
        let err = open_err(&cfg, secrets, &kh).await;
        let DbError::Ssh {
            stage: SshStage::Auth,
            message,
        } = &err
        else {
            panic!("{pass:?}: expected Ssh/Auth, got {err:?}");
        };
        assert!(message.contains("passphrase"), "{pass:?}: {message}");
        assert!(!message.contains("wrong_pass"), "{message}");
    }
}

#[tokio::test]
async fn missing_key_file_is_config_error() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, secrets) = key_setup("ssh/does_not_exist", None);
    let err = open_err(&cfg, secrets, &kh).await;
    assert!(
        matches!(&err, DbError::Config { field: Some(f), .. } if f == "ssh.key_path"),
        "{err:?}"
    );
}

#[tokio::test]
async fn host_key_mismatch_blocks() {
    require_db!();
    let _g = SSH_SERVER.read().await;
    let (_d, kh) = temp_known_hosts();
    // A key the server does not have: the client test key.
    let fake = std::fs::read_to_string(testdata("ssh/id_ed25519.pub")).unwrap();
    let fake_fp = fingerprint(&PublicKey::from_openssh(&fake).unwrap());
    kh.trust("127.0.0.1", 2222, &fake_fp, &fake).unwrap();
    let (cfg, secrets) = password_setup();
    let err = open_err(&cfg, secrets, &kh).await;
    let DbError::Ssh {
        stage: SshStage::HostKeyMismatch,
        message,
    } = &err
    else {
        panic!("expected Ssh/HostKeyMismatch, got {err:?}");
    };
    assert!(message.contains(&fake_fp), "{message}");
}

/// Waits until the restarted `ssh` container accepts tunnels again.
async fn wait_ssh_ready(kh: &KnownHosts) {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let (cfg, secrets) = password_setup();
        let res = Tunnel::open(
            &cfg,
            secrets,
            target(Target::MySql8),
            kh,
            Duration::from_secs(5),
        )
        .await;
        match res {
            Ok(t) => {
                t.close().await;
                return;
            }
            Err(e) if Instant::now() > deadline => panic!("ssh container not back: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

#[tokio::test]
async fn tunnel_dead_flag() {
    require_db!();
    let _g = SSH_SERVER.write().await;
    let (_d, kh) = trusted_known_hosts().await;
    let (cfg, secrets) = password_setup();
    let opts = TunnelOptions {
        keepalive_interval: Duration::from_secs(1),
    };
    let tunnel = Tunnel::open_with(&cfg, secrets, target(Target::MySql8), &kh, TIMEOUT, opts)
        .await
        .unwrap();
    assert!(tunnel.is_alive());

    let mut restart = std::process::Command::new("docker")
        .args(["restart", SSH_CONTAINER])
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("docker restart");
    let started = Instant::now();
    while tunnel.is_alive() && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let dead = !tunnel.is_alive();
    let status = restart.wait().unwrap();
    assert!(status.success(), "docker restart failed: {status}");
    tunnel.close().await;
    wait_ssh_ready(&kh).await;
    assert!(
        dead,
        "tunnel still alive 5 s after the ssh server went down"
    );
}
