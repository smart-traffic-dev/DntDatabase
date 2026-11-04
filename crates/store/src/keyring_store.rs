//! `SecretStore` backed by the OS keychain (Windows Credential Manager).

use dnt_core::error::DbError;
use dnt_core::secret::{app_secret_key, secret_key, AppSecret, SecretKind, SecretStore};
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
            .map_err(|e| secret_err("open", kind.as_str(), e))
    }

    fn app_entry(&self, secret: AppSecret) -> Result<Entry, DbError> {
        Entry::new(&self.service, &app_secret_key(secret))
            .map_err(|e| secret_err("open", secret.as_str(), e))
    }
}

/// Error text names the operation and secret kind only; keyring errors never carry the value.
fn secret_err(op: &str, kind: &str, err: keyring::Error) -> DbError {
    DbError::Secret {
        message: format!("{op} {kind} failed: {err}"),
    }
}

impl SecretStore for KeyringSecretStore {
    fn get(&self, conn: Uuid, kind: SecretKind) -> Result<Option<SecretString>, DbError> {
        match self.entry(conn, kind)?.get_password() {
            Ok(value) => Ok(Some(SecretString::from(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(secret_err("read", kind.as_str(), e)),
        }
    }

    /// Reads the entry's attributes, not its secret: the password never reaches our memory.
    fn has(&self, conn: Uuid, kind: SecretKind) -> Result<bool, DbError> {
        match self.entry(conn, kind)?.get_attributes() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(secret_err("check", kind.as_str(), e)),
        }
    }

    fn set(&self, conn: Uuid, kind: SecretKind, value: &SecretString) -> Result<(), DbError> {
        self.entry(conn, kind)?
            .set_password(value.expose_secret())
            .map_err(|e| secret_err("write", kind.as_str(), e))
    }

    fn delete(&self, conn: Uuid, kind: SecretKind) -> Result<(), DbError> {
        match self.entry(conn, kind)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(secret_err("delete", kind.as_str(), e)),
        }
    }

    fn delete_all(&self, conn: Uuid) -> Result<(), DbError> {
        for kind in SecretKind::ALL {
            self.delete(conn, kind)?;
        }
        Ok(())
    }

    fn get_app(&self, secret: AppSecret) -> Result<Option<SecretString>, DbError> {
        match self.app_entry(secret)?.get_password() {
            Ok(value) => Ok(Some(SecretString::from(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(secret_err("read", secret.as_str(), e)),
        }
    }

    /// Reads the entry's attributes, not its secret.
    fn has_app(&self, secret: AppSecret) -> Result<bool, DbError> {
        match self.app_entry(secret)?.get_attributes() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(secret_err("check", secret.as_str(), e)),
        }
    }

    fn set_app(&self, secret: AppSecret, value: &SecretString) -> Result<(), DbError> {
        self.app_entry(secret)?
            .set_password(value.expose_secret())
            .map_err(|e| secret_err("write", secret.as_str(), e))
    }

    fn delete_app(&self, secret: AppSecret) -> Result<(), DbError> {
        match self.app_entry(secret)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(secret_err("delete", secret.as_str(), e)),
        }
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

    #[test]
    #[ignore = "touches the real OS credential manager"]
    fn real_credential_manager_app_secret_roundtrip() {
        // A throwaway service, so the user's real AI key is never touched.
        let store = KeyringSecretStore {
            service: format!("DntDatabase-test-{}", Uuid::new_v4()),
        };
        assert!(!store.has_app(AppSecret::AiApiKey).unwrap());
        store
            .set_app(AppSecret::AiApiKey, &SecretString::from("sk-1"))
            .unwrap();
        assert!(store.has_app(AppSecret::AiApiKey).unwrap());
        let got = store.get_app(AppSecret::AiApiKey).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "sk-1");
        store.delete_app(AppSecret::AiApiKey).unwrap();
        store.delete_app(AppSecret::AiApiKey).unwrap();
        assert!(store.get_app(AppSecret::AiApiKey).unwrap().is_none());
    }
}
