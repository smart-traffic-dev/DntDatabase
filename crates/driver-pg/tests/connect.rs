use dnt_core::config::{ResolvedConfig, SslConfig, SslMode};
use dnt_core::driver::{Driver, Session};
use dnt_core::error::{hint, DbError};
use dnt_core::server::{DbKind, ServerVariant};
use dnt_driver_pg::{PgDriver, PgSession};
use dnt_testkit::{require_db, testdata, Target, FIXTURE_DB};

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
        ..t.resolved(Some(FIXTURE_DB))
    }
}

/// Whether the session's own backend runs over TLS (`pg_stat_ssl`).
async fn uses_ssl(s: &PgSession) -> bool {
    let row = s
        .query_row_text("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
        .await
        .unwrap()
        .expect("pg_stat_ssl row");
    row[0].as_deref() == Some("t")
}

async fn connect_ok(t: Target, cfg: &ResolvedConfig, what: &str) -> PgSession {
    PgDriver
        .connect_session(cfg)
        .await
        .unwrap_or_else(|e| panic!("{t:?} {what}: {e}"))
}

async fn connect_err(t: Target, cfg: &ResolvedConfig, what: &str) -> DbError {
    PgDriver
        .connect(cfg)
        .await
        .err()
        .unwrap_or_else(|| panic!("{t:?} {what}: connected"))
}

#[tokio::test]
async fn connects_and_detects_variant() {
    require_db!();
    for t in Target::pg() {
        let s = PgDriver
            .connect(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let info = s.server_info();
        assert_eq!(info.kind, DbKind::Postgres, "{t:?}");
        assert_eq!(info.variant, ServerVariant::Postgres, "{t:?}");
        let major = match t {
            Target::Pg12 => 12,
            _ => 17,
        };
        assert_eq!(info.version.0, major, "{t:?}: {info:?}");
        assert!(
            info.version_string.starts_with("PostgreSQL"),
            "{t:?}: {info:?}"
        );
        assert!(!s.sql().unwrap().in_transaction(), "{t:?}");
        s.close().await;
    }
}

#[tokio::test]
async fn session_sets_application_name_and_database() {
    require_db!();
    for t in Target::pg() {
        let s = connect_ok(t, &t.resolved(Some(FIXTURE_DB)), "connect").await;
        let row = s
            .query_row_text("SELECT current_setting('application_name'), current_database()")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row[0].as_deref(), Some("DntDatabase"), "{t:?}");
        assert_eq!(row[1].as_deref(), Some(FIXTURE_DB), "{t:?}");
        Box::new(s).close().await;
    }
}

#[tokio::test]
async fn wrong_password_is_auth_error() {
    require_db!();
    let bad = "not-the-password-x9";
    for t in Target::pg() {
        let err = connect_err(t, &t.resolved_as("postgres", bad, None), "wrong password").await;
        match &err {
            DbError::Auth { message } => assert!(message.contains("28P01"), "{t:?}: {message}"),
            other => panic!("{t:?}: expected Auth, got {other:?}"),
        }
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
        ..Target::Pg17.resolved(None)
    };
    let err = connect_err(Target::Pg17, &cfg, "port 1").await;
    assert!(matches!(err, DbError::Connect { .. }), "{err:?}");
    assert!(hint(&err).is_some(), "{err:?}");
}

/// A host name that does not resolve (`.invalid` never does) gets the DNS hint.
#[tokio::test]
async fn unresolvable_host_has_dns_hint() {
    require_db!();
    let cfg = ResolvedConfig {
        host: "no-such-host.invalid".into(),
        ..Target::Pg17.resolved(None)
    };
    let err = connect_err(Target::Pg17, &cfg, "unresolvable host").await;
    assert!(matches!(err, DbError::Connect { .. }), "{err:?}");
    let hint = hint(&err).unwrap_or_default();
    assert!(hint.contains("could not be resolved"), "{err:?}: {hint}");
}

#[tokio::test]
async fn tls_modes() {
    require_db!();
    for t in Target::pg() {
        // Disable: plaintext.
        let s = connect_ok(
            t,
            &with_ssl(t, ssl(SslMode::Disable, None, false), "localhost"),
            "disable",
        )
        .await;
        assert!(!uses_ssl(&s).await, "{t:?} disable");

        // No SSL config at all: plaintext.
        let s = connect_ok(t, &with_ssl(t, None, "localhost"), "none").await;
        assert!(!uses_ssl(&s).await, "{t:?} none");

        // Prefer: TLS when the server offers it.
        let s = connect_ok(
            t,
            &with_ssl(t, ssl(SslMode::Prefer, None, false), "localhost"),
            "prefer",
        )
        .await;
        assert!(uses_ssl(&s).await, "{t:?} prefer");

        // Require: encrypted, certificate not checked (wrong name, no CA).
        let s = connect_ok(
            t,
            &with_ssl(t, ssl(SslMode::Require, None, false), "wrong.example"),
            "require",
        )
        .await;
        assert!(uses_ssl(&s).await, "{t:?} require");

        // VerifyCa: chain checked, host name ignored.
        let s = connect_ok(
            t,
            &with_ssl(
                t,
                ssl(SslMode::VerifyCa, Some("certs/ca.pem"), false),
                "wrong.example",
            ),
            "verify_ca",
        )
        .await;
        assert!(uses_ssl(&s).await, "{t:?} verify_ca");

        // VerifyCa against a certificate that is not the issuing CA fails.
        let err = connect_err(
            t,
            &with_ssl(
                t,
                ssl(SslMode::VerifyCa, Some("certs/client.pem"), false),
                "localhost",
            ),
            "verify_ca untrusted",
        )
        .await;
        assert!(matches!(err, DbError::Tls { .. }), "{t:?}: {err:?}");

        // VerifyFull with the right name and a client certificate.
        let s = connect_ok(
            t,
            &with_ssl(
                t,
                ssl(SslMode::VerifyFull, Some("certs/ca.pem"), true),
                "localhost",
            ),
            "verify_full",
        )
        .await;
        assert!(uses_ssl(&s).await, "{t:?} verify_full");

        // VerifyFull with a name the certificate does not cover.
        let err = connect_err(
            t,
            &with_ssl(
                t,
                ssl(SslMode::VerifyFull, Some("certs/ca.pem"), false),
                "wrong.example",
            ),
            "verify_full wrong name",
        )
        .await;
        assert!(matches!(err, DbError::Tls { .. }), "{t:?}: {err:?}");

        // Verify modes without a CA file are a config error, before any network I/O.
        let err = connect_err(
            t,
            &with_ssl(t, ssl(SslMode::VerifyFull, None, false), "localhost"),
            "no ca",
        )
        .await;
        assert!(
            matches!(&err, DbError::Config { field: Some(f), .. } if f == "ssl.ca_path"),
            "{t:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn read_only_blocks_writes() {
    require_db!();
    for t in Target::pg() {
        let cfg = ResolvedConfig {
            read_only: true,
            ..t.resolved(Some(FIXTURE_DB))
        };
        let s = connect_ok(t, &cfg, "read-only").await;
        let err = s
            .query_row_text(
                "INSERT INTO customers (name, email) VALUES ('ro', 'read-only@example.com')",
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{t:?}: write succeeded on a read-only session"));
        match err {
            DbError::Server { sqlstate, .. } => {
                assert_eq!(sqlstate.as_deref(), Some("25006"), "{t:?}")
            }
            other => panic!("{t:?}: expected Server error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn ping_works() {
    require_db!();
    for t in Target::pg() {
        let s = PgDriver
            .connect(&t.resolved(None))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        s.ping().await.unwrap_or_else(|e| panic!("{t:?}: {e}"));
        s.close().await;
    }
}

#[tokio::test]
async fn introspector_is_the_session() {
    require_db!();
    let s = PgDriver
        .connect(&Target::Pg17.resolved(None))
        .await
        .unwrap();
    let dbs = s.introspector().list_databases().await.unwrap();
    assert!(dbs.iter().any(|d| d.name == FIXTURE_DB), "{dbs:?}");
    s.close().await;
}
