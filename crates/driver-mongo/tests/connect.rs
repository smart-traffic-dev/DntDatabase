//! Connect against the compose services `mongo60` (standalone) and `mongo80` (replica set
//! `rs0`, TLS allowed). Needs `DNT_TEST_DB=1`.

use dnt_core::config::{DriverOptions, ResolvedConfig, SslConfig, SslMode};
use dnt_core::error::DbError;
use dnt_core::server::{DbKind, ServerVariant};
use dnt_driver_mongo::test_helpers::{open, Opened};
use dnt_testkit::{require_db, testdata, Target, FIXTURE_DB, TEST_PASSWORD};
use mongodb::bson::doc;
use std::path::PathBuf;
use std::time::Duration;

fn cfg(t: Target, user: &str) -> ResolvedConfig {
    t.resolved_as(user, TEST_PASSWORD, Some(FIXTURE_DB))
}

fn with_replica_set(mut c: ResolvedConfig, name: &str, direct: bool) -> ResolvedConfig {
    if let DriverOptions::Mongo {
        replica_set,
        direct_connection,
        ..
    } = &mut c.driver
    {
        *replica_set = Some(name.into());
        *direct_connection = direct;
    }
    c
}

fn ssl(mode: SslMode, ca: Option<PathBuf>, cert: Option<PathBuf>) -> Option<SslConfig> {
    Some(SslConfig {
        version: 1,
        mode,
        ca_path: ca,
        cert_path: cert,
        key_path: None,
    })
}

fn ca() -> Option<PathBuf> {
    Some(testdata("certs/ca.pem"))
}

async fn opened(c: &ResolvedConfig) -> Opened {
    open(c).await.unwrap_or_else(|e| panic!("{c:?}: {e}"))
}

async fn open_err(c: &ResolvedConfig) -> DbError {
    match open(c).await {
        Ok(o) => {
            o.client.shutdown().immediate(true).await;
            panic!("{c:?}: connected, expected an error")
        }
        Err(e) => e,
    }
}

async fn ping(o: &Opened) {
    o.client
        .database("admin")
        .run_command(doc! { "ping": 1 })
        .await
        .expect("ping");
}

async fn close(o: Opened) {
    o.client.shutdown().immediate(true).await;
}

#[tokio::test]
async fn connects_standalone_60() {
    require_db!();
    for user in ["root", "dnt"] {
        let o = opened(&cfg(Target::Mongo60, user)).await;
        assert_eq!(
            (
                o.info.kind,
                o.info.variant,
                o.info.version.0,
                o.info.version.1
            ),
            (DbKind::Mongo, ServerVariant::Mongo, 6, 0)
        );
        assert!(
            o.info.version_string.starts_with("6.0."),
            "{}",
            o.info.version_string
        );
        assert_eq!(o.db, FIXTURE_DB);
        let n = o
            .client
            .database(FIXTURE_DB)
            .collection::<mongodb::bson::Document>("orders")
            .count_documents(doc! {})
            .await
            .unwrap();
        assert_eq!(n, 5);
        close(o).await;
    }

    // No database: the session starts in `test`, credentials checked against `admin`.
    let mut c = cfg(Target::Mongo60, "root");
    c.database = None;
    if let DriverOptions::Mongo { auth_source, .. } = &mut c.driver {
        *auth_source = None;
    }
    let o = opened(&c).await;
    assert_eq!(o.db, "test");
    close(o).await;

    // No auth source but a database: the database is the auth source (no user there).
    c.database = Some(FIXTURE_DB.into());
    assert!(matches!(open_err(&c).await, DbError::Auth { .. }));
}

#[tokio::test]
async fn connects_replica_set_tls_80() {
    require_db!();
    let mut c = with_replica_set(cfg(Target::Mongo80, "dnt"), "rs0", false);
    c.ssl = ssl(SslMode::VerifyFull, ca(), None);
    let o = opened(&c).await;
    assert_eq!((o.info.kind, o.info.version.0), (DbKind::Mongo, 8));
    assert!(o.info.capabilities.transactions);
    ping(&o).await;
    close(o).await;

    // The same server without TLS (it allows both).
    let o = opened(&with_replica_set(
        cfg(Target::Mongo80, "root"),
        "rs0",
        false,
    ))
    .await;
    ping(&o).await;
    close(o).await;

    // A wrong replica set name finds no server.
    let mut c = with_replica_set(cfg(Target::Mongo80, "root"), "other", false);
    c.connect_timeout = Duration::from_secs(3);
    match open_err(&c).await {
        DbError::Connect { message } => assert!(message.contains("127.0.0.1:27080"), "{message}"),
        other => panic!("expected Connect, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_password_is_auth() {
    require_db!();
    for t in Target::mongo() {
        for user in ["root", "dnt"] {
            let c = t.resolved_as(user, "not-the-password", Some(FIXTURE_DB));
            match open_err(&c).await {
                DbError::Auth { message } => {
                    assert!(!message.contains("not-the-password"), "{message}")
                }
                other => panic!("{t:?}/{user}: expected Auth, got {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn unverified_tls() {
    require_db!();
    // The test CA is not a public root: only Require (no certificate checks) connects.
    for mode in [SslMode::Prefer, SslMode::Require] {
        let mut c = cfg(Target::Mongo80, "dnt");
        c.ssl = ssl(mode, None, None);
        let o = opened(&c).await;
        ping(&o).await;
        close(o).await;
        // Same through a tunnel (direct connection to that one server).
        c.via_tunnel = true;
        let o = opened(&c).await;
        ping(&o).await;
        close(o).await;
    }

    // Verify full checks the certificate: the bundled roots do not know the test CA.
    let mut c = cfg(Target::Mongo80, "dnt");
    c.ssl = ssl(SslMode::VerifyFull, None, None);
    c.connect_timeout = Duration::from_secs(3);
    assert!(matches!(open_err(&c).await, DbError::Connect { .. }));

    // TLS really is on: the TLS-less 6.0 server cannot be reached with it.
    let mut c = cfg(Target::Mongo60, "dnt");
    c.ssl = ssl(SslMode::Require, None, None);
    c.connect_timeout = Duration::from_secs(3);
    assert!(matches!(open_err(&c).await, DbError::Connect { .. }));

    // Refused before connecting.
    let mut c = cfg(Target::Mongo80, "dnt");
    c.ssl = ssl(SslMode::VerifyCa, ca(), None);
    assert!(matches!(open_err(&c).await,
        DbError::Config { field: Some(f), .. } if f == "ssl.mode"));
    c.ssl = ssl(SslMode::VerifyFull, ca(), None);
    c.via_tunnel = true;
    assert!(matches!(open_err(&c).await,
        DbError::Config { field: Some(f), .. } if f == "ssl.mode"));
}

#[tokio::test]
async fn client_cert_pem() {
    require_db!();
    let mut c = with_replica_set(cfg(Target::Mongo80, "dnt"), "rs0", false);
    c.ssl = ssl(
        SslMode::VerifyFull,
        ca(),
        Some(testdata("certs/client-combined.pem")),
    );
    let o = opened(&c).await;
    ping(&o).await;
    close(o).await;

    // The certificate is presented and checked: mongod rejects one made for a server
    // (no clientAuth usage), in the same one-file form.
    let dir = tempfile::tempdir().unwrap();
    let server_pem = dir.path().join("server-combined.pem");
    let read = |name: &str| std::fs::read_to_string(testdata(name)).unwrap();
    std::fs::write(
        &server_pem,
        read("certs/server.pem") + &read("certs/server-key.pem"),
    )
    .unwrap();
    let mut wrong = with_replica_set(cfg(Target::Mongo80, "dnt"), "rs0", false);
    wrong.ssl = ssl(SslMode::VerifyFull, ca(), Some(server_pem));
    wrong.connect_timeout = Duration::from_secs(3);
    assert!(matches!(open_err(&wrong).await, DbError::Connect { .. }));

    // The certificate alone, or a separate key file, is refused before connecting.
    c.ssl = ssl(
        SslMode::VerifyFull,
        ca(),
        Some(testdata("certs/client.pem")),
    );
    assert!(matches!(open_err(&c).await,
        DbError::Config { field: Some(f), .. } if f == "ssl.cert_path"));
    c.ssl = Some(SslConfig {
        key_path: Some(testdata("certs/client-key.pem")),
        ..ssl(
            SslMode::VerifyFull,
            ca(),
            Some(testdata("certs/client.pem")),
        )
        .unwrap()
    });
    assert!(matches!(open_err(&c).await,
        DbError::Config { field: Some(f), .. } if f == "ssl.key_path"));
}

#[tokio::test]
async fn transactions_capability_follows_topology() {
    require_db!();
    let o = opened(&cfg(Target::Mongo60, "dnt")).await;
    assert!(!o.info.capabilities.transactions, "standalone");
    close(o).await;
    for c in [
        cfg(Target::Mongo80, "dnt"),
        with_replica_set(cfg(Target::Mongo80, "dnt"), "rs0", true),
    ] {
        let o = opened(&c).await;
        assert!(o.info.capabilities.transactions, "replica set member");
        close(o).await;
    }
}
