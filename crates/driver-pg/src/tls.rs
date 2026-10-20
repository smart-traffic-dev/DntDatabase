//! `SslMode` → tokio-postgres `SslMode` + a rustls `ClientConfig` (ring provider only).
//!
//! tokio-postgres-rustls hands the dialled host to rustls, which is `127.0.0.1` behind an
//! SSH tunnel; every verifier here ignores that name and checks `tls_server_name` instead.
//! - `Disable` / `Prefer` / `Require`: encrypt only, the certificate is not checked
//!   (handshake signatures still are).
//! - `VerifyCa`: chain against `ca_path` only, no host-name check (works with SAN-less
//!   certificates since no name is ever matched).
//! - `VerifyFull`: chain against `ca_path` + host name = `tls_server_name`.

use dnt_core::config::{SslConfig, SslMode};
use dnt_core::error::DbError;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, WebPkiServerVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::path::Path;
use std::sync::Arc;
use tokio_postgres::config::SslMode as PgSslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

/// How to negotiate TLS for one connection.
#[derive(Clone)]
pub(crate) struct Tls {
    pub(crate) mode: PgSslMode,
    pub(crate) connector: MakeRustlsConnect,
}

/// Builds the TLS setup for `ssl` (`None` = plaintext).
pub(crate) fn tls_setup(ssl: Option<&SslConfig>, server_name: &str) -> Result<Tls, DbError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let algs = provider.signature_verification_algorithms;
    let (mode, verifier): (PgSslMode, Arc<dyn ServerCertVerifier>) = match ssl.map(|s| s.mode) {
        None | Some(SslMode::Disable) => {
            return Ok(Tls {
                mode: PgSslMode::Disable,
                connector: MakeRustlsConnect::new(client_config(
                    provider,
                    Arc::new(NoVerification { algs }),
                    None,
                )?),
            })
        }
        Some(SslMode::Prefer) => (PgSslMode::Prefer, Arc::new(NoVerification { algs })),
        Some(SslMode::Require) => (PgSslMode::Require, Arc::new(NoVerification { algs })),
        Some(SslMode::VerifyCa) => (
            PgSslMode::Require,
            Arc::new(ChainOnly {
                roots: Arc::new(roots(ssl)?),
                algs,
            }),
        ),
        Some(SslMode::VerifyFull) => {
            let name =
                ServerName::try_from(server_name.to_string()).map_err(|_| DbError::Config {
                    field: Some("host".into()),
                    message: format!("{server_name:?} is not a valid TLS server name"),
                })?;
            let inner = WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots(ssl)?),
                provider.clone(),
            )
            .build()
            .map_err(|e| DbError::Tls {
                message: e.to_string(),
            })?;
            (PgSslMode::Require, Arc::new(FixedName { inner, name }))
        }
    };
    let ssl = ssl.expect("TLS modes have an SslConfig");
    let identity = client_identity(ssl)?;
    Ok(Tls {
        mode,
        connector: MakeRustlsConnect::new(client_config(provider, verifier, identity)?),
    })
}

type Identity = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

fn client_config(
    provider: Arc<rustls::crypto::CryptoProvider>,
    verifier: Arc<dyn ServerCertVerifier>,
    identity: Option<Identity>,
) -> Result<ClientConfig, DbError> {
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    match identity {
        Some((certs, key)) => builder.with_client_auth_cert(certs, key).map_err(tls_err),
        None => Ok(builder.with_no_client_auth()),
    }
}

fn tls_err(e: rustls::Error) -> DbError {
    DbError::Tls {
        message: e.to_string(),
    }
}

/// Trust anchors from `ca_path` only; the public web roots are never trusted.
fn roots(ssl: Option<&SslConfig>) -> Result<RootCertStore, DbError> {
    let ca = ssl
        .and_then(|s| s.ca_path.as_ref())
        .ok_or_else(|| DbError::Config {
            field: Some("ssl.ca_path".into()),
            message: "SSL verification requires a CA certificate file".into(),
        })?;
    let mut store = RootCertStore::empty();
    for cert in read_certs(ca, "CA certificate", "ssl.ca_path")? {
        store.add(cert).map_err(|e| DbError::Tls {
            message: format!("invalid CA certificate in {}: {e}", ca.display()),
        })?;
    }
    Ok(store)
}

fn client_identity(ssl: &SslConfig) -> Result<Option<Identity>, DbError> {
    match (&ssl.cert_path, &ssl.key_path) {
        (Some(cert), Some(key)) => {
            let certs = read_certs(cert, "client certificate", "ssl.cert_path")?;
            let key = PrivateKeyDer::from_pem_file(key).map_err(|e| DbError::Config {
                field: Some("ssl.key_path".into()),
                message: format!("cannot read the client key file {}: {e}", key.display()),
            })?;
            Ok(Some((certs, key)))
        }
        (None, None) => Ok(None),
        _ => Err(DbError::Config {
            field: Some("ssl.cert_path".into()),
            message: "Client certificate and key must both be set".into(),
        }),
    }
}

/// PEM certificates from a file the user chose; a file that cannot be read (or holds no
/// certificate) is reported at `field`.
fn read_certs(
    path: &Path,
    what: &str,
    field: &str,
) -> Result<Vec<CertificateDer<'static>>, DbError> {
    let err = |e: &dyn std::fmt::Display| DbError::Config {
        field: Some(field.into()),
        message: format!("cannot read the {what} file {}: {e}", path.display()),
    };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| err(&e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| err(&e))?;
    if certs.is_empty() {
        return Err(err(&"no PEM certificate found"));
    }
    Ok(certs)
}

/// Accepts any certificate; the handshake signature is still verified.
#[derive(Debug)]
struct NoVerification {
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// `VerifyCa`: the chain must lead to a configured root; the name is never checked.
#[derive(Debug)]
struct ChainOnly {
    roots: Arc<RootCertStore>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algs.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// `VerifyFull`: full webpki verification against `tls_server_name`, whatever host was dialled.
#[derive(Debug)]
struct FixedName {
    inner: Arc<WebPkiServerVerifier>,
    name: ServerName<'static>,
}

impl ServerCertVerifier for FixedName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner
            .verify_server_cert(end_entity, intermediates, &self.name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The rustls error behind a failed handshake, searching the error's source chain
/// (tokio-rustls reports it as an `io::Error` wrapping the `rustls::Error`).
pub(crate) fn rustls_error<'a>(
    e: &'a (dyn std::error::Error + 'static),
) -> Option<&'a rustls::Error> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(t) = err.downcast_ref::<rustls::Error>() {
            return Some(t);
        }
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if let Some(t) = io.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                return Some(t);
            }
        }
        cur = err.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    fn config_error(r: Result<Tls, DbError>) -> (Option<String>, String) {
        match r {
            Err(DbError::Config { field, message }) => (field, message),
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected Config error, got Ok"),
        }
    }

    #[test]
    fn ssl_mode_mapping() {
        let mode = |s: Option<SslConfig>| tls_setup(s.as_ref(), "localhost").unwrap().mode;
        assert_eq!(mode(None), PgSslMode::Disable);
        assert_eq!(mode(Some(ssl(SslMode::Disable, None))), PgSslMode::Disable);
        assert_eq!(mode(Some(ssl(SslMode::Prefer, None))), PgSslMode::Prefer);
        assert_eq!(mode(Some(ssl(SslMode::Require, None))), PgSslMode::Require);
        for m in [SslMode::VerifyCa, SslMode::VerifyFull] {
            assert_eq!(
                mode(Some(ssl(m, Some(certs("ca.pem"))))),
                PgSslMode::Require
            );
        }
        let with_identity = SslConfig {
            cert_path: Some(certs("client.pem")),
            key_path: Some(certs("client-key.pem")),
            ..ssl(SslMode::Require, None)
        };
        assert!(tls_setup(Some(&with_identity), "localhost").is_ok());
    }

    #[test]
    fn verify_modes_require_ca() {
        for m in [SslMode::VerifyCa, SslMode::VerifyFull] {
            let (field, _) = config_error(tls_setup(Some(&ssl(m, None)), "db"));
            assert_eq!(field.as_deref(), Some("ssl.ca_path"), "{m:?}");
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
            let (field, message) = config_error(tls_setup(Some(&cfg), "db"));
            assert_eq!(field.as_deref(), Some("ssl.cert_path"));
            assert_eq!(message, "Client certificate and key must both be set");
        }
    }

    #[test]
    fn unreadable_files_are_field_errors() {
        let missing = PathBuf::from("Z:\\nope\\missing.pem");
        let field = |r| config_error(r).0;
        assert_eq!(
            field(tls_setup(
                Some(&ssl(SslMode::VerifyFull, Some(missing.clone()))),
                "db"
            ))
            .as_deref(),
            Some("ssl.ca_path")
        );
        // A key file is not a certificate.
        assert_eq!(
            field(tls_setup(
                Some(&ssl(SslMode::VerifyCa, Some(certs("client-key.pem")))),
                "db",
            ))
            .as_deref(),
            Some("ssl.ca_path")
        );
        let bad_cert = SslConfig {
            cert_path: Some(missing.clone()),
            key_path: Some(certs("client-key.pem")),
            ..ssl(SslMode::Require, None)
        };
        assert_eq!(
            field(tls_setup(Some(&bad_cert), "db")).as_deref(),
            Some("ssl.cert_path")
        );
        let bad_key = SslConfig {
            cert_path: Some(certs("client.pem")),
            key_path: Some(missing),
            ..ssl(SslMode::Require, None)
        };
        assert_eq!(
            field(tls_setup(Some(&bad_key), "db")).as_deref(),
            Some("ssl.key_path")
        );
    }

    fn leaf(name: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_file(certs(name)).unwrap()
    }

    fn chain_only(ca: &str) -> ChainOnly {
        let mut roots = RootCertStore::empty();
        roots.add(leaf(ca)).unwrap();
        ChainOnly {
            roots: Arc::new(roots),
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }

    fn verify(v: &dyn ServerCertVerifier, cert: &str, name: &str) -> Result<(), rustls::Error> {
        v.verify_server_cert(
            &leaf(cert),
            &[],
            &ServerName::try_from(name).unwrap(),
            &[],
            UnixTime::now(),
        )
        .map(|_| ())
    }

    #[test]
    fn verify_ca_checks_chain_but_not_name() {
        let v = chain_only("ca.pem");
        verify(&v, "server.pem", "wrong.example").expect("SAN cert, any name");
        // SAN-less certificate (like auto-generated server certs): no name to match.
        verify(&v, "server-nosan.pem", "wrong.example").expect("SAN-less cert");
        verify(&v, "server-nosan.pem", "127.0.0.1").expect("SAN-less cert, IP");
        // A chain that does not lead to the configured root is rejected.
        let untrusted = chain_only("client.pem");
        assert!(matches!(
            verify(&untrusted, "server.pem", "localhost"),
            Err(rustls::Error::InvalidCertificate(_))
        ));
    }

    #[test]
    fn verify_full_checks_the_configured_name() {
        let mut roots = RootCertStore::empty();
        roots.add(leaf("ca.pem")).unwrap();
        let inner = WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        let full = |name: &str| FixedName {
            inner: inner.clone(),
            name: ServerName::try_from(name.to_string()).unwrap(),
        };
        // The dialled host (`127.0.0.1` through a tunnel) is ignored.
        verify(&full("localhost"), "server.pem", "127.0.0.1").expect("name matches");
        assert!(verify(&full("wrong.example"), "server.pem", "localhost").is_err());
        assert!(verify(&full("localhost"), "server-nosan.pem", "localhost").is_err());
    }

    #[test]
    fn finds_rustls_error_in_source_chain() {
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        assert!(rustls_error(&io).is_some());
        let plain = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(rustls_error(&plain).is_none());
    }
}
