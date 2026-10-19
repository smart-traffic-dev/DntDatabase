//! `SslMode` → `mysql_async::SslOpts`, plus helpers for reading rustls handshake errors.

use dnt_core::config::{SslConfig, SslMode};
use dnt_core::error::DbError;
use mysql_async::{ClientIdentity, Error as MyError, IoError, SslOpts};
use rustls::pki_types::ServerName;
use rustls::{AlertDescription, CertificateError};
use std::path::Path;

/// Shown when `VerifyCa` meets a certificate without subjectAltName (e.g. MySQL's
/// auto-generated server certificates), which the retry in [`verify_ca_retry`] cannot handle.
pub(crate) const NO_SAN_MESSAGE: &str = "Server certificate has no subjectAltName; VerifyCa \
     cannot skip host-name checking for it with this driver. Use Require, or a certificate \
     with SANs.";

/// Maps `SslConfig` to `mysql_async` TLS options. The certificate host name is always
/// `server_name` (the original host when the connection goes through an SSH tunnel).
pub(crate) fn ssl_opts(ssl: &SslConfig, server_name: &str) -> Result<Option<SslOpts>, DbError> {
    let mut opts = match ssl.mode {
        SslMode::Disable => return Ok(None),
        // Encrypt only: the certificate is not checked at all.
        SslMode::Prefer | SslMode::Require => SslOpts::default()
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true),
        // mysql_async 0.37 fails to honour this flag with rustls 0.23 (it string-matches
        // "NotValidForName"); `verify_ca_retry` below recovers from the resulting error.
        SslMode::VerifyCa => verifying(ssl)?.with_danger_skip_domain_validation(true),
        SslMode::VerifyFull => verifying(ssl)?,
    };
    match (&ssl.cert_path, &ssl.key_path) {
        (Some(cert), Some(key)) => {
            opts = opts.with_client_identity(Some(ClientIdentity::new(
                read_file(cert, "client certificate")?.into(),
                read_file(key, "client key")?.into(),
            )));
        }
        (None, None) => {}
        _ => {
            return Err(DbError::Config {
                field: Some("ssl.cert_path".into()),
                message: "Client certificate and key must both be set".into(),
            })
        }
    }
    if !server_name.is_empty() {
        opts = opts.with_danger_tls_hostname_override(Some(server_name.to_string()));
    }
    Ok(Some(opts))
}

/// Chain verification against `ca_path` only; the public web roots are never trusted.
fn verifying(ssl: &SslConfig) -> Result<SslOpts, DbError> {
    let ca = ssl.ca_path.as_ref().ok_or_else(|| DbError::Config {
        field: Some("ssl.ca_path".into()),
        message: "SSL verification requires a CA certificate file".into(),
    })?;
    Ok(SslOpts::default()
        .with_root_certs(vec![read_file(ca, "CA certificate")?.into()])
        .with_disable_built_in_roots(true))
}

fn read_file(path: &Path, what: &str) -> Result<Vec<u8>, DbError> {
    std::fs::read(path).map_err(|e| DbError::Tls {
        message: format!("cannot read {what} file {}: {e}", path.display()),
    })
}

/// The rustls error behind a failed handshake. tokio-rustls reports it as an `io::Error`
/// wrapping the `rustls::Error`; certificate loading reports it as `IoError::Tls`.
pub(crate) fn rustls_error(e: &MyError) -> Option<&rustls::Error> {
    match e {
        MyError::Io(IoError::Io(io)) => io.get_ref()?.downcast_ref::<rustls::Error>(),
        MyError::Io(IoError::Tls(mysql_async::TlsError::Tls(t))) => Some(t),
        _ => None,
    }
}

/// Message for a rustls handshake failure, with a hint for the common cipher mismatch.
pub(crate) fn handshake_message(t: &rustls::Error) -> String {
    match t {
        rustls::Error::AlertReceived(AlertDescription::HandshakeFailure) => format!(
            "{t} (the server offers no TLS 1.2+ ECDHE cipher suite; \
             e.g. MySQL 5.7 builds linked against OpenSSL 1.0.x)"
        ),
        _ => t.to_string(),
    }
}

/// For `VerifyCa`, after a failed handshake:
/// - `None`: not a host-name mismatch; report the error as usual.
/// - `Some(Ok(name))`: the chain verified but the name did not match; `name` is one the
///   certificate *is* valid for, so the connection is retried with full verification against
///   it (chain + name are still checked, which is at least as strict as `VerifyCa`).
/// - `Some(Err(_))`: the certificate has no subjectAltName to retry with.
///
/// mysql_async exposes no way to plug in a custom rustls verifier, hence this workaround.
pub(crate) fn verify_ca_retry(e: &MyError) -> Option<Result<String, DbError>> {
    match rustls_error(e)? {
        rustls::Error::InvalidCertificate(CertificateError::NotValidForNameContext {
            presented,
            ..
        }) => {
            if presented.is_empty() {
                return Some(Err(DbError::Tls {
                    message: NO_SAN_MESSAGE.into(),
                }));
            }
            presented.iter().find_map(|p| presented_name(p)).map(Ok)
        }
        _ => None,
    }
}

/// Parses one entry of `NotValidForNameContext::presented` (`DnsName("db.example")`,
/// `IpAddress(10.0.0.1)`) into a name usable as `ServerName`. Wildcards get a concrete label.
fn presented_name(p: &str) -> Option<String> {
    let name = if let Some(dns) = p
        .strip_prefix("DnsName(\"")
        .and_then(|s| s.strip_suffix("\")"))
    {
        match dns.strip_prefix("*.") {
            Some(rest) => format!("dnt-verify-ca.{rest}"),
            None => dns.to_string(),
        }
    } else {
        p.strip_prefix("IpAddress(")?.strip_suffix(')')?.to_string()
    };
    ServerName::try_from(name.as_str()).ok()?;
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, UnixTime};
    use rustls::RootCertStore;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ssl(mode: SslMode, ca: Option<PathBuf>) -> SslConfig {
        SslConfig {
            version: 1,
            mode,
            ca_path: ca,
            cert_path: None,
            key_path: None,
        }
    }

    fn certs(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/certs")
            .join(name)
    }

    fn config_field(r: Result<Option<SslOpts>, DbError>) -> Option<String> {
        match r {
            Err(DbError::Config { field, .. }) => field,
            other => panic!(
                "expected Config error, got {:?}",
                other.map(|o| o.is_some())
            ),
        }
    }

    #[test]
    fn ssl_mode_mapping() {
        assert!(ssl_opts(&ssl(SslMode::Disable, None), "db")
            .unwrap()
            .is_none());
        for mode in [SslMode::Prefer, SslMode::Require] {
            let o = ssl_opts(&ssl(mode, None), "db").unwrap().unwrap();
            assert!(o.accept_invalid_certs() && o.skip_domain_validation());
            assert_eq!(o.tls_hostname_override(), Some("db"));
        }
        let o = ssl_opts(&ssl(SslMode::VerifyCa, Some(certs("ca.pem"))), "db")
            .unwrap()
            .unwrap();
        assert!(!o.accept_invalid_certs() && o.skip_domain_validation());
        assert!(o.disable_built_in_roots() && o.root_certs().len() == 1);
        let o = ssl_opts(
            &ssl(SslMode::VerifyFull, Some(certs("ca.pem"))),
            "db.example",
        )
        .unwrap()
        .unwrap();
        assert!(!o.accept_invalid_certs() && !o.skip_domain_validation());
        assert!(o.disable_built_in_roots() && o.root_certs().len() == 1);
        assert_eq!(o.tls_hostname_override(), Some("db.example"));
        assert!(o.client_identity().is_none());

        let with_identity = SslConfig {
            cert_path: Some(certs("client.pem")),
            key_path: Some(certs("client-key.pem")),
            ..ssl(SslMode::Require, None)
        };
        let o = ssl_opts(&with_identity, "db").unwrap().unwrap();
        assert!(o.client_identity().is_some());
    }

    #[test]
    fn verify_modes_require_ca() {
        for mode in [SslMode::VerifyCa, SslMode::VerifyFull] {
            assert_eq!(
                config_field(ssl_opts(&ssl(mode, None), "db")).as_deref(),
                Some("ssl.ca_path"),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn client_cert_and_key_must_be_paired() {
        let only_cert = SslConfig {
            cert_path: Some(certs("client.pem")),
            ..ssl(SslMode::Require, None)
        };
        let only_key = SslConfig {
            key_path: Some(certs("client-key.pem")),
            ..ssl(SslMode::Require, None)
        };
        for cfg in [only_cert, only_key] {
            match ssl_opts(&cfg, "db") {
                Err(DbError::Config { field, message }) => {
                    assert_eq!(field.as_deref(), Some("ssl.cert_path"));
                    assert_eq!(message, "Client certificate and key must both be set");
                }
                other => panic!(
                    "expected Config error, got {:?}",
                    other.map(|o| o.is_some())
                ),
            }
        }
    }

    #[test]
    fn unreadable_files_are_tls_errors() {
        let missing = PathBuf::from("Z:\\nope\\ca.pem");
        let err = ssl_opts(&ssl(SslMode::VerifyFull, Some(missing.clone())), "db").unwrap_err();
        assert!(matches!(err, DbError::Tls { .. }), "{err:?}");
        let bad_identity = SslConfig {
            cert_path: Some(missing),
            key_path: Some(certs("client-key.pem")),
            ..ssl(SslMode::Require, None)
        };
        assert!(matches!(
            ssl_opts(&bad_identity, "db"),
            Err(DbError::Tls { .. })
        ));
    }

    fn wrapped(t: rustls::Error) -> MyError {
        std::io::Error::new(std::io::ErrorKind::InvalidData, t).into()
    }

    fn retry_name(e: &MyError) -> Option<String> {
        verify_ca_retry(e).map(|r| r.expect("retry name"))
    }

    #[test]
    fn verify_ca_retry_from_mismatch() {
        let mismatch = |presented: Vec<&str>| {
            wrapped(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForNameContext {
                    expected: ServerName::try_from("wrong.example").unwrap().to_owned(),
                    presented: presented.into_iter().map(String::from).collect(),
                },
            ))
        };
        assert_eq!(
            retry_name(&mismatch(vec![
                "DnsName(\"localhost\")",
                "IpAddress(127.0.0.1)"
            ]))
            .as_deref(),
            Some("localhost")
        );
        assert_eq!(
            retry_name(&mismatch(vec!["IpAddress(10.0.0.5)"])).as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            retry_name(&mismatch(vec!["DnsName(\"*.db.example\")"])).as_deref(),
            Some("dnt-verify-ca.db.example")
        );
        assert_eq!(retry_name(&mismatch(vec!["Uri(\"x\")"])), None);
        match verify_ca_retry(&mismatch(vec![])) {
            Some(Err(DbError::Tls { message })) => assert_eq!(message, NO_SAN_MESSAGE),
            other => panic!("expected no-SAN Tls error, got {other:?}"),
        }
        let untrusted = wrapped(rustls::Error::InvalidCertificate(
            CertificateError::UnknownIssuer,
        ));
        assert!(verify_ca_retry(&untrusted).is_none());
    }

    /// Runs the same rustls verifier mysql_async uses (chain against `ca.pem`, name
    /// `wrong.example`) on a fixture certificate and returns the resulting error.
    fn verify_fixture(leaf: &str) -> MyError {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_file(certs("ca.pem")).unwrap())
            .unwrap();
        let verifier = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        let leaf = CertificateDer::from_pem_file(certs(leaf)).unwrap();
        let err = verifier
            .verify_server_cert(
                &leaf,
                &[],
                &ServerName::try_from("wrong.example").unwrap(),
                &[],
                UnixTime::now(),
            )
            .expect_err("wrong.example must not verify");
        wrapped(err)
    }

    #[test]
    fn verify_ca_retry_with_real_certificates() {
        // Cert with SANs: retried against a presented name.
        assert_eq!(
            retry_name(&verify_fixture("server.pem")).as_deref(),
            Some("localhost")
        );
        // SAN-less cert (like MySQL's auto-generated ones): explicit, actionable error.
        match verify_ca_retry(&verify_fixture("server-nosan.pem")) {
            Some(Err(DbError::Tls { message })) => assert_eq!(message, NO_SAN_MESSAGE),
            other => panic!("expected no-SAN Tls error, got {other:?}"),
        }
    }

    #[test]
    fn handshake_failure_message_has_hint() {
        let m = handshake_message(&rustls::Error::AlertReceived(
            AlertDescription::HandshakeFailure,
        ));
        assert!(m.contains("cipher suite"), "{m}");
    }
}
