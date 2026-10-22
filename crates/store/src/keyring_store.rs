//! `SecretStore` backed by the OS keychain (Windows Credential Manager).

use dnt_core::error::DbError;
use dnt_core::secret::{secret_key, SecretKind, SecretStore};
use keyring::Entry;
use secrecy::{ExposeSecret, SecretString};
use uuid::Uuid;

const SERVICE: &str = "DntDatabase";

/// Overrides the keychain service name, so the E2E run keeps its secrets apart from the
/// user's real ones (its cleanup only ever touches that service).
pub const SERVICE_ENV: &str = "DNT_KEYRING_SERVICE";

#[derive(Debug, Clone)]
pub struct KeyringSecretStore {
    service: String,
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        KeyringSecretStore {
            service: SERVICE.to_owned(),
        }
    }
}

impl KeyringSecretStore {
    /// `$DNT_KEYRING_SERVICE` when set, otherwise `DntDatabase`.
    pub fn from_env() -> Self {
        match std::env::var(SERVICE_ENV) {
            Ok(service) if !service.is_empty() => KeyringSecretStore { service },
            _ => KeyringSecretStore::default(),
        }
    }

    fn entry(&self, conn: Uuid, kind: SecretKind) -> Result<Entry, DbError> {
        Entry::new(&self.service, &secret_key(conn, kind))
            .map_err(|e| secret_err("open", kind, e))
    }
}

/// Error text names the operation and secret kind only; keyring errors never carry the value.
fn secret_err(op: &str, kind: SecretKind, err: keyring::Error) -> DbError {
    DbError::Secret {
        message: format!("{op} {} failed: {err}", kind.as_str()),
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, conn: Uuid, kind: SecretKind) -> Result<Option<SecretString>, DbError> {
        match self.entry(conn, kind)?.get_password() {
            Ok(value) => Ok(Some(SecretString::from(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(secret_err("read", kind, e)),
        }
    }

    /// Reads the entry's attributes, not its secret: the password never reaches our memory.
    fn has(&self, conn: Uuid, kind: SecretKind) -> Result<bool, DbError> {
        match self.entry(conn, kind)?.get_attributes() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(secret_err("check", kind, e)),
        }
    }

    fn set(&self, conn: Uuid, kind: SecretKind, value: &SecretString) -> Result<(), DbError> {
        self.entry(conn, kind)?
            .set_password(value.expose_secret())
            .map_err(|e| secret_err("write", kind, e))
    }

    fn delete(&self, conn: Uuid, kind: SecretKind) -> Result<(), DbError> {
        match self.entry(conn, kind)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(secret_err("delete", kind, e)),
        }
    }

    fn delete_all(&self, conn: Uuid) -> Result<(), DbError> {
        for kind in SecretKind::ALL {
            self.delete(conn, kind)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "touches the real OS credential manager"]
    fn real_credential_manager_roundtrip() {
        let store = KeyringSecretStore::default();
        let id = Uuid::new_v4();
        assert!(store.get(id, SecretKind::DbPassword).unwrap().is_none());

        store
            .set(id, SecretKind::DbPassword, &SecretString::from("pw-1"))
            .unwrap();
        store
            .set(id, SecretKind::SshPassword, &SecretString::from("ssh-1"))
            .unwrap();
        let got = store.get(id, SecretKind::DbPassword).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "pw-1");
        assert!(store.has(id, SecretKind::DbPassword).unwrap());
        assert!(!store.has(id, SecretKind::SshKeyPassphrase).unwrap());

        store.delete(id, SecretKind::DbPassword).unwrap();
        assert!(store.get(id, SecretKind::DbPassword).unwrap().is_none());
        // Deleting a missing entry is not an error.
        store.delete(id, SecretKind::DbPassword).unwrap();

        store.delete_all(id).unwrap();
        assert!(store.get(id, SecretKind::SshPassword).unwrap().is_none());
    }
}
