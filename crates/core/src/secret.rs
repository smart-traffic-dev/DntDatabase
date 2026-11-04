use crate::error::DbError;
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    DbPassword,
    SshPassword,
    SshKeyPassphrase,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::DbPassword => "db_password",
            SecretKind::SshPassword => "ssh_password",
            SecretKind::SshKeyPassphrase => "ssh_key_passphrase",
        }
    }

    pub const ALL: [SecretKind; 3] = [
        SecretKind::DbPassword,
        SecretKind::SshPassword,
        SecretKind::SshKeyPassphrase,
    ];
}

/// A secret of the app itself, not of one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppSecret {
    /// The API key of the AI endpoint (AI editor completion spec §6.1).
    AiApiKey,
}

impl AppSecret {
    pub fn as_str(self) -> &'static str {
        match self {
            AppSecret::AiApiKey => "ai_api_key",
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, conn: Uuid, kind: SecretKind) -> Result<Option<SecretString>, DbError>;
    /// Whether a secret is stored. Stores that can check presence without reading the
    /// value should; the default reads it and drops it right away.
    fn has(&self, conn: Uuid, kind: SecretKind) -> Result<bool, DbError> {
        Ok(self.get(conn, kind)?.is_some())
    }
    fn set(&self, conn: Uuid, kind: SecretKind, value: &SecretString) -> Result<(), DbError>;
    fn delete(&self, conn: Uuid, kind: SecretKind) -> Result<(), DbError>;
    fn delete_all(&self, conn: Uuid) -> Result<(), DbError>;

    fn get_app(&self, secret: AppSecret) -> Result<Option<SecretString>, DbError>;
    /// Whether an app secret is stored; the default reads it and drops it right away.
    fn has_app(&self, secret: AppSecret) -> Result<bool, DbError> {
        Ok(self.get_app(secret)?.is_some())
    }
    fn set_app(&self, secret: AppSecret, value: &SecretString) -> Result<(), DbError>;
    /// Deleting a missing secret is not an error.
    fn delete_app(&self, secret: AppSecret) -> Result<(), DbError>;
}

/// Keychain entry name: `DntDatabase/<conn_uuid>/<kind>`.
pub fn secret_key(conn: Uuid, kind: SecretKind) -> String {
    format!("DntDatabase/{conn}/{}", kind.as_str())
}

/// Keychain entry name of an app secret: `DntDatabase/app/<name>` (never a UUID, so it
/// cannot meet a connection's entry).
pub fn app_secret_key(secret: AppSecret) -> String {
    format!("DntDatabase/app/{}", secret.as_str())
}

/// In-memory `SecretStore` used by tests.
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    entries: Mutex<HashMap<String, SecretString>>,
}

impl MemorySecretStore {
    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, SecretString>>, DbError> {
        self.entries.lock().map_err(|_| DbError::Secret {
            message: "in-memory secret store is poisoned".into(),
        })
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, conn: Uuid, kind: SecretKind) -> Result<Option<SecretString>, DbError> {
        Ok(self
            .lock()?
            .get(&secret_key(conn, kind))
            .map(|s| SecretString::from(s.expose_secret())))
    }
    fn set(&self, conn: Uuid, kind: SecretKind, value: &SecretString) -> Result<(), DbError> {
        self.lock()?.insert(
            secret_key(conn, kind),
            SecretString::from(value.expose_secret()),
        );
        Ok(())
    }
    fn delete(&self, conn: Uuid, kind: SecretKind) -> Result<(), DbError> {
        self.lock()?.remove(&secret_key(conn, kind));
        Ok(())
    }
    fn delete_all(&self, conn: Uuid) -> Result<(), DbError> {
        let mut map = self.lock()?;
        for kind in SecretKind::ALL {
            map.remove(&secret_key(conn, kind));
        }
        Ok(())
    }
    fn get_app(&self, secret: AppSecret) -> Result<Option<SecretString>, DbError> {
        Ok(self
            .lock()?
            .get(&app_secret_key(secret))
            .map(|s| SecretString::from(s.expose_secret())))
    }
    fn set_app(&self, secret: AppSecret, value: &SecretString) -> Result<(), DbError> {
        self.lock()?.insert(
            app_secret_key(secret),
            SecretString::from(value.expose_secret()),
        );
        Ok(())
    }
    fn delete_app(&self, secret: AppSecret) -> Result<(), DbError> {
        self.lock()?.remove(&app_secret_key(secret));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_secret_store_roundtrip() {
        let store = MemorySecretStore::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        store
            .set(a, SecretKind::DbPassword, &SecretString::from("pw-a"))
            .unwrap();
        store
            .set(a, SecretKind::SshPassword, &SecretString::from("ssh-a"))
            .unwrap();
        store
            .set(b, SecretKind::DbPassword, &SecretString::from("pw-b"))
            .unwrap();
        let got = store.get(a, SecretKind::DbPassword).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "pw-a");
        assert!(store
            .get(a, SecretKind::SshKeyPassphrase)
            .unwrap()
            .is_none());

        store.delete(a, SecretKind::SshPassword).unwrap();
        assert!(store.get(a, SecretKind::SshPassword).unwrap().is_none());

        store.delete_all(a).unwrap();
        assert!(store.get(a, SecretKind::DbPassword).unwrap().is_none());
        // Other connections are untouched.
        let other = store.get(b, SecretKind::DbPassword).unwrap().unwrap();
        assert_eq!(other.expose_secret(), "pw-b");
    }

    #[test]
    fn app_secrets_are_apart_from_connections() {
        let store = MemorySecretStore::default();
        let a = Uuid::new_v4();
        store
            .set(a, SecretKind::DbPassword, &SecretString::from("pw-a"))
            .unwrap();
        assert!(!store.has_app(AppSecret::AiApiKey).unwrap());
        store
            .set_app(AppSecret::AiApiKey, &SecretString::from("sk-1"))
            .unwrap();
        let got = store.get_app(AppSecret::AiApiKey).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "sk-1");
        assert!(store.has_app(AppSecret::AiApiKey).unwrap());

        // Deleting a connection's secrets keeps the app secret, and the reverse.
        store.delete_all(a).unwrap();
        assert!(store.has_app(AppSecret::AiApiKey).unwrap());
        store
            .set(a, SecretKind::DbPassword, &SecretString::from("pw-a"))
            .unwrap();
        store.delete_app(AppSecret::AiApiKey).unwrap();
        store.delete_app(AppSecret::AiApiKey).unwrap();
        assert!(store.get_app(AppSecret::AiApiKey).unwrap().is_none());
        assert!(store.get(a, SecretKind::DbPassword).unwrap().is_some());
        assert_eq!(
            app_secret_key(AppSecret::AiApiKey),
            "DntDatabase/app/ai_api_key"
        );
    }

    #[test]
    fn secret_key_format() {
        assert_eq!(
            secret_key(Uuid::nil(), SecretKind::SshPassword),
            "DntDatabase/00000000-0000-0000-0000-000000000000/ssh_password"
        );
        assert_eq!(SecretKind::DbPassword.as_str(), "db_password");
        assert_eq!(SecretKind::SshKeyPassphrase.as_str(), "ssh_key_passphrase");
    }
}
