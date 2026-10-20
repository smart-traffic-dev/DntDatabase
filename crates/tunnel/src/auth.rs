//! Loading the client private key for public-key authentication.

use dnt_core::error::{DbError, SshStage};
use russh::keys::{decode_secret_key, Error as KeyError, PrivateKey};
use secrecy::{ExposeSecret, SecretString};
use std::io::ErrorKind;
use std::path::Path;
use zeroize::Zeroizing;

/// `Ssh{Auth}` message when the private key is encrypted and no passphrase was given:
/// the app asks for the passphrase instead of reporting a failure.
pub const KEY_PASSPHRASE_REQUIRED: &str =
    "the SSH private key is encrypted: a passphrase is required";

fn key_path_err(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some("ssh.key_path".into()),
        message: message.into(),
    }
}

pub(crate) fn auth_err(message: impl Into<String>) -> DbError {
    DbError::Ssh {
        stage: SshStage::Auth,
        message: message.into(),
    }
}

/// Reads and (if needed) decrypts the private key. File permissions are not checked:
/// NTFS ACLs must not block the user the way the OpenSSH client does.
/// Errors never contain key material or the passphrase.
pub(crate) fn load_private_key(
    path: Option<&Path>,
    passphrase: Option<&SecretString>,
) -> Result<PrivateKey, DbError> {
    let path =
        path.ok_or_else(|| key_path_err("SSH key authentication requires a private key file"))?;
    let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            key_path_err(format!(
                "SSH private key file not found: {}",
                path.display()
            ))
        } else {
            key_path_err(format!(
                "cannot read SSH private key file {}: {e}",
                path.display()
            ))
        }
    })?);
    let decrypt = |p: &SecretString| decode_secret_key(&text, Some(p.expose_secret()));
    match decode_secret_key(&text, None) {
        Ok(key) => Ok(key),
        Err(KeyError::KeyIsEncrypted) => match passphrase {
            None => Err(auth_err(KEY_PASSPHRASE_REQUIRED)),
            Some(p) => {
                decrypt(p).map_err(|_| auth_err("wrong passphrase for the SSH private key"))
            }
        },
        // Some legacy formats only parse once decrypted.
        Err(_) => match passphrase {
            Some(p) => decrypt(p).map_err(|_| {
                auth_err("cannot decrypt the SSH private key: wrong passphrase or unsupported key format")
            }),
            None => Err(key_path_err("unsupported or corrupt SSH private key file")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/ssh")
            .join(name)
    }

    fn pass(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    fn auth_message(r: Result<PrivateKey, DbError>) -> String {
        match r {
            Err(DbError::Ssh {
                stage: SshStage::Auth,
                message,
            }) => message,
            Err(e) => panic!("expected Ssh/Auth, got {e:?}"),
            Ok(_) => panic!("expected Ssh/Auth, got a key"),
        }
    }

    #[test]
    fn loads_plain_and_encrypted_keys() {
        let plain = load_private_key(Some(&fixture("id_ed25519")), None).unwrap();
        assert!(!plain.is_encrypted());
        // A passphrase given for an unencrypted key is ignored.
        load_private_key(Some(&fixture("id_ed25519")), Some(&pass("x"))).unwrap();
        let enc =
            load_private_key(Some(&fixture("id_ed25519_enc")), Some(&pass("dnt_pass"))).unwrap();
        assert!(!enc.is_encrypted());
    }

    #[test]
    fn encrypted_key_needs_the_right_passphrase() {
        let path = fixture("id_ed25519_enc");
        let m = auth_message(load_private_key(Some(&path), None));
        assert!(m.contains("passphrase"), "{m}");
        let m = auth_message(load_private_key(Some(&path), Some(&pass("wrong_pass"))));
        assert!(m.contains("passphrase") && !m.contains("wrong_pass"), "{m}");
    }

    #[test]
    fn missing_or_bad_key_file_is_config_error() {
        let field = |r: Result<PrivateKey, DbError>| match r {
            Err(DbError::Config { field, .. }) => field,
            other => panic!("expected Config, got {:?}", other.map(|_| ())),
        };
        assert_eq!(
            field(load_private_key(Some(&fixture("nope")), None)).as_deref(),
            Some("ssh.key_path")
        );
        assert_eq!(
            field(load_private_key(None, None)).as_deref(),
            Some("ssh.key_path")
        );
        assert_eq!(
            field(load_private_key(Some(&fixture("id_ed25519.pub")), None)).as_deref(),
            Some("ssh.key_path")
        );
    }
}
