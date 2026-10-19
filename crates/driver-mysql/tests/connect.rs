use dnt_core::config::{ResolvedConfig, SslConfig, SslMode};
use dnt_core::driver::{Driver, Session};
use dnt_core::error::{hint, DbError};
use dnt_core::server::ServerVariant;
use dnt_driver_mysql::{MySqlDriver, MySqlSession};
use dnt_testkit::{require_db, testdata, Target, FIXTURE_DB};

fn expected_variant(t: Target) -> ServerVariant {
    match t {
        Target::MySql57 => ServerVariant::MySql57,
        Target::MySql8 => ServerVariant::MySql8,
        Target::MariaDb106 | Target::MariaDb114 => ServerVariant::MariaDb,
        Target::Pg12 | Target::Pg17 => unreachable!("not a MySQL-family target"),
    }
}

fn ssl(mode: SslMode, ca: Option<&str>, client_cert: bool) -> Option<SslConfig> {
    Some(SslConfig {
        version: 1,
        mode,
        ca_path: ca.map(testdata),
        cert_path: client_cert.then(|| testdata("certs/client.pem")),
        key_path: client_cert.then(|| testdata("certs/client-key.pem")),
    })
}

fn with_ssl(t: Target, ssl: Option<SslConfig>, server_name: &str) -> ResolvedConfig {
    ResolvedConfig {
        ssl,
        tls_server_name: server_name.into(),
        ..t.resolved(None)
    }
}

async fn ssl_cipher(s: &MySqlSession) -> String {
    let row = s
        .query_row_text("SHOW SESSION STATUS LIKE 'Ssl_cipher'")
        .await
        .unwrap()
        .expect("Ssl_cipher row");
    row[1].clone().unwrap_or_default()
}

#[tokio::test]
async fn connects_and_detects_variant() {
    require_db!();
    for t in Target::mysql_family() {
        let s = MySqlDriver
            .connect(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert_eq!(s.server_info().variant, expected_variant(t), "{t:?}");
        assert!(!s.in_transaction(), "{t:?}");
        s.close().await;
    }
}

#[tokio::test]
async fn session_uses_utf8mb4_and_selected_database() {
    require_db!();
    for t in Target::mysql_family() {
        let s = MySqlDriver
            .connect_session(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let row = s
            .query_row_text("SELECT @@character_set_client, DATABASE()")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row[0].as_deref(), Some("utf8mb4"), "{t:?}");
        assert_eq!(row[1].as_deref(), Some(FIXTURE_DB), "{t:?}");
        Box::new(s).close().await;
    }
}

#[tokio::test]
async fn wrong_password_is_auth_error() {
    require_db!();
    let bad = "not-the-password-x9";
    for t in Target::mysql_family() {
        let err = MySqlDriver
            .connect(&t.resolved_as("root", bad, None))
            .await
            .err()
            .unwrap_or_else(|| panic!("{t:?}: connected with a wrong password"));
        assert!(matches!(err, DbError::Auth { .. }), "{t:?}: {err:?}");
        assert!(hint(&err).is_some(), "{t:?}: no hint for {err:?}");
        assert!(
            !format!("{err} {err:?}").contains(bad),
            "{t:?}: leaks password"
        );
    }
}

#[tokio::test]
async fn refused_port_is_connect_error() {
    require_db!();
    let cfg = ResolvedConfig {
        port: 1,
        ..Target::MySql8.resolved(None)
    };
    let err = MySqlDriver
        .connect(&cfg)
        .await
        .err()
        .expect("port 1 refused");
    assert!(matches!(err, DbError::Connect { .. }), "{err:?}");
}

#[tokio::test]
async fn tls_require_and_verify_ca() {
    require_db!();
    for t in Target::mysql_family() {
        // Disable: plaintext.
        let s = MySqlDriver
            .connect_session(&with_ssl(
                t,
                ssl(SslMode::Disable, None, false),
                "localhost",
            ))
            .await
            .unwrap_or_else(|e| panic!("{t:?} disable: {e}"));
        assert_eq!(ssl_cipher(&s).await, "", "{t:?} disable");

        if t == Target::MySql57 {
            // The official mysql:5.7 image links OpenSSL 1.0.2 without ECDHE, and rustls
            // only offers ECDHE suites: TLS cannot be negotiated with this server.
            let err = MySqlDriver
                .connect(&with_ssl(
                    t,
                    ssl(SslMode::Require, None, false),
                    "localhost",
                ))
                .await
                .err()
                .expect("MySQL 5.7 require");
            assert!(matches!(err, DbError::Tls { .. }), "{t:?}: {err:?}");
            // Prefer falls back to plaintext.
            let s = MySqlDriver
                .connect_session(&with_ssl(t, ssl(SslMode::Prefer, None, false), "localhost"))
                .await
                .unwrap_or_else(|e| panic!("{t:?} prefer: {e}"));
            assert_eq!(ssl_cipher(&s).await, "", "{t:?} prefer fallback");
            continue;
        }

        // Require: encrypted, certificate not checked.
        let s = MySqlDriver
            .connect_session(&with_ssl(
                t,
                ssl(SslMode::Require, None, false),
                "localhost",
            ))
            .await
            .unwrap_or_else(|e| panic!("{t:?} require: {e}"));
        assert_ne!(ssl_cipher(&s).await, "", "{t:?} require");

        // Prefer: TLS when the server offers it.
        let s = MySqlDriver
            .connect_session(&with_ssl(t, ssl(SslMode::Prefer, None, false), "localhost"))
            .await
            .unwrap_or_else(|e| panic!("{t:?} prefer: {e}"));
        assert_ne!(ssl_cipher(&s).await, "", "{t:?} prefer");

        // VerifyCa: chain checked, host name ignored.
        let s = MySqlDriver
            .connect_session(&with_ssl(
                t,
                ssl(SslMode::VerifyCa, Some("certs/ca.pem"), false),
                "wrong.example",
            ))
            .await
            .unwrap_or_else(|e| panic!("{t:?} verify_ca: {e}"));
        assert_ne!(ssl_cipher(&s).await, "", "{t:?} verify_ca");

        // VerifyCa against a certificate that is not the issuing CA fails.
        let err = MySqlDriver
            .connect(&with_ssl(
                t,
                ssl(SslMode::VerifyCa, Some("certs/server.pem"), false),
                "localhost",
            ))
            .await
            .err()
            .unwrap_or_else(|| panic!("{t:?}: verify_ca accepted an untrusted chain"));
        assert!(matches!(err, DbError::Tls { .. }), "{t:?}: {err:?}");

        // VerifyFull with the right name and a client certificate.
        let s = MySqlDriver
            .connect_session(&with_ssl(
                t,
                ssl(SslMode::VerifyFull, Some("certs/ca.pem"), true),
                "localhost",
            ))
            .await
            .unwrap_or_else(|e| panic!("{t:?} verify_full: {e}"));
        assert_ne!(ssl_cipher(&s).await, "", "{t:?} verify_full");

        // VerifyFull with a name the certificate does not cover.
        let err = MySqlDriver
            .connect(&with_ssl(
                t,
                ssl(SslMode::VerifyFull, Some("certs/ca.pem"), false),
                "wrong.example",
            ))
            .await
            .err()
            .unwrap_or_else(|| panic!("{t:?}: verify_full accepted a wrong host name"));
        assert!(matches!(err, DbError::Tls { .. }), "{t:?}: {err:?}");
    }
}

#[tokio::test]
async fn read_only_blocks_writes() {
    require_db!();
    for t in Target::mysql_family() {
        let cfg = ResolvedConfig {
            read_only: true,
            ..t.resolved(Some(FIXTURE_DB))
        };
        let s = MySqlDriver
            .connect_session(&cfg)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let err = s
            .query_row_text(
                "INSERT INTO customers (name, email) VALUES ('ro', 'read-only@example.com')",
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{t:?}: write succeeded on a read-only session"));
        match err {
            DbError::Server { code, .. } => assert_eq!(code.as_deref(), Some("1792"), "{t:?}"),
            other => panic!("{t:?}: expected Server error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn ping_works() {
    require_db!();
    for t in Target::mysql_family() {
        let s = MySqlDriver
            .connect(&t.resolved(None))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        s.ping().await.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        s.close().await;
    }
}
