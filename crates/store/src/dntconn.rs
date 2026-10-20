//! `.dntconn` export/import: groups and connection configs as JSON, with secrets optionally
//! encrypted under a passphrase (Argon2id key derivation, XChaCha20-Poly1305).

use crate::db::internal;
use crate::repo::{now_rfc3339, ConnectionRepo, Group, NewSecrets, NodeId};
use crate::transfer::{ImportBundle, ImportSource, SkippedItem};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use dnt_core::config::ConnectionConfig;
use dnt_core::error::DbError;
use dnt_core::secret::{SecretKind, SecretStore};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const FORMAT: &str = "dntdatabase-connections";
const VERSION: u32 = 1;
const KDF: &str = "argon2id";
const CIPHER: &str = "xchacha20poly1305";
const M_COST_KIB: u32 = 65_536;
const T_COST: u32 = 3;
const P_COST: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const MIN_PASSPHRASE_CHARS: usize = 8;
/// Upper bounds accepted from a file, so a crafted file cannot demand unbounded memory/time.
const MAX_M_COST_KIB: u32 = 1 << 20;
const MAX_T_COST: u32 = 16;
const MAX_P_COST: u32 = 16;
const PASSPHRASE_INCORRECT: &str = "Passphrase is incorrect.";

#[derive(Serialize, Deserialize)]
struct DntconnFile {
    format: String,
    version: u32,
    exported_at: String,
    groups: Vec<Group>,
    connections: Vec<ConnectionConfig>,
    secrets: Option<EncryptedSecrets>,
}

#[derive(Serialize, Deserialize)]
struct EncryptedSecrets {
    kdf: String,
    m: u32,
    t: u32,
    p: u32,
    salt: String,
    cipher: String,
    nonce: String,
    ciphertext: String,
}

/// One connection's entry in the encrypted plaintext; wiped on drop.
#[derive(Serialize, Deserialize, Default)]
struct PlainSecrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    db_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssh_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssh_key_passphrase: Option<String>,
}

impl Drop for PlainSecrets {
    fn drop(&mut self) {
        self.db_password.zeroize();
        self.ssh_password.zeroize();
        self.ssh_key_passphrase.zeroize();
    }
}

/// Exports the selected nodes. A group brings its whole subtree; every exported connection or
/// group brings its ancestor groups. Secrets are included (encrypted) only with a passphrase.
pub fn export_dntconn(
    repo: &ConnectionRepo,
    ids: &[NodeId],
    passphrase: Option<&SecretString>,
) -> Result<String, DbError> {
    if let Some(p) = passphrase {
        if p.expose_secret().chars().count() < MIN_PASSPHRASE_CHARS {
            return Err(DbError::Config {
                field: Some("passphrase".into()),
                message: format!("Passphrase must be at least {MIN_PASSPHRASE_CHARS} characters."),
            });
        }
    }
    let (groups, configs) = repo.snapshot()?;
    let (groups, connections) = select(groups, configs, ids)?;
    let secrets = passphrase
        .map(|p| encrypt_secrets(repo.secret_store(), &connections, p))
        .transpose()?;
    let file = DntconnFile {
        format: FORMAT.into(),
        version: VERSION,
        exported_at: now_rfc3339()?,
        groups,
        connections,
        secrets,
    };
    serde_json::to_string_pretty(&file).map_err(|e| internal("serialize .dntconn", e))
}

/// Parses a `.dntconn` file. Without a passphrase any encrypted secrets are ignored.
/// Connections with an invalid config are reported as skipped.
pub fn parse_dntconn(
    json: &str,
    passphrase: Option<&SecretString>,
) -> Result<ImportBundle, DbError> {
    let file: DntconnFile = serde_json::from_value(parse_header(json)?)
        .map_err(|e| invalid(format!("The file is not a valid .dntconn file: {e}")))?;
    let mut secrets = match (&file.secrets, passphrase) {
        (Some(enc), Some(p)) => decrypt_secrets(enc, p)?,
        _ => HashMap::new(),
    };
    let mut connections = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for c in file.connections {
        let problem = if c.id.is_nil() {
            Some("Invalid connection id".to_string())
        } else if !seen.insert(c.id) {
            Some("Duplicate connection id".to_string())
        } else {
            c.validate().err().map(|e| match e {
                DbError::Config { message, .. } => message,
                other => other.to_string(),
            })
        };
        match problem {
            None => connections.push(c),
            Some(reason) => skipped.push(SkippedItem {
                name: c.name,
                reason,
            }),
        }
    }
    let kept: HashSet<Uuid> = connections.iter().map(|c| c.id).collect();
    secrets.retain(|id, _| kept.contains(id));
    Ok(ImportBundle {
        source: ImportSource::Dntconn,
        groups: file.groups,
        connections,
        secrets,
        skipped,
        warnings: HashMap::new(),
    })
}

/// Whether the file carries encrypted secrets (so the UI should ask for the passphrase).
pub fn dntconn_has_secrets(json: &str) -> Result<bool, DbError> {
    Ok(parse_header(json)?
        .get("secrets")
        .is_some_and(|s| !s.is_null()))
}

/// Parses the JSON and checks the `format` and `version` markers.
fn parse_header(json: &str) -> Result<Value, DbError> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| invalid(format!("The file is not valid JSON: {e}")))?;
    if value.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(invalid("The file is not a DntDatabase connections export."));
    }
    match value.get("version") {
        Some(v) if v.as_u64() == Some(u64::from(VERSION)) => Ok(value),
        other => Err(DbError::Unsupported {
            feature: "dntconn version".into(),
            version: match other {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => "missing".into(),
            },
        }),
    }
}

fn select(
    groups: Vec<Group>,
    configs: Vec<ConnectionConfig>,
    ids: &[NodeId],
) -> Result<(Vec<Group>, Vec<ConnectionConfig>), DbError> {
    let parent_of: HashMap<Uuid, Option<Uuid>> =
        groups.iter().map(|g| (g.id, g.parent_id)).collect();
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for g in &groups {
        if let Some(p) = g.parent_id {
            children.entry(p).or_default().push(g.id);
        }
    }
    let mut want_groups = HashSet::new();
    let mut want_conns = HashSet::new();
    // Keeps `want_groups` closed under "parent of": stop at the first group already in it.
    let add_with_ancestors = |start: Option<Uuid>, want: &mut HashSet<Uuid>| {
        let mut cur = start;
        while let Some(g) = cur {
            if !want.insert(g) {
                break;
            }
            cur = parent_of.get(&g).copied().flatten();
        }
    };
    for node in ids {
        match *node {
            NodeId::Group(id) => {
                if !parent_of.contains_key(&id) {
                    return Err(not_found("group", id));
                }
                add_with_ancestors(Some(id), &mut want_groups);
                let mut subtree = HashSet::from([id]);
                let mut stack = vec![id];
                while let Some(g) = stack.pop() {
                    for &child in children.get(&g).into_iter().flatten() {
                        if subtree.insert(child) {
                            stack.push(child);
                        }
                    }
                }
                want_conns.extend(
                    configs
                        .iter()
                        .filter(|c| c.group_id.is_some_and(|g| subtree.contains(&g)))
                        .map(|c| c.id),
                );
                want_groups.extend(subtree);
            }
            NodeId::Connection(id) => {
                let cfg = configs
                    .iter()
                    .find(|c| c.id == id)
                    .ok_or_else(|| not_found("connection", id))?;
                want_conns.insert(id);
                add_with_ancestors(cfg.group_id, &mut want_groups);
            }
        }
    }
    Ok((
        groups
            .into_iter()
            .filter(|g| want_groups.contains(&g.id))
            .collect(),
        configs
            .into_iter()
            .filter(|c| want_conns.contains(&c.id))
            .collect(),
    ))
}

fn encrypt_secrets(
    store: &dyn SecretStore,
    connections: &[ConnectionConfig],
    passphrase: &SecretString,
) -> Result<EncryptedSecrets, DbError> {
    let mut plain = BTreeMap::new();
    for c in connections {
        // Fill a zeroize-on-drop entry field by field, so an early `?` never drops a plain
        // `String` holding a secret.
        let mut entry = PlainSecrets::default();
        for (kind, slot) in [
            (SecretKind::DbPassword, &mut entry.db_password),
            (SecretKind::SshPassword, &mut entry.ssh_password),
            (SecretKind::SshKeyPassphrase, &mut entry.ssh_key_passphrase),
        ] {
            *slot = store
                .get(c.id, kind)?
                .map(|s| s.expose_secret().to_string());
        }
        if entry.db_password.is_some()
            || entry.ssh_password.is_some()
            || entry.ssh_key_passphrase.is_some()
        {
            plain.insert(c.id, entry);
        }
    }
    let plaintext =
        Zeroizing::new(serde_json::to_vec(&plain).map_err(|e| internal("serialize secrets", e))?);
    drop(plain);

    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let key = derive_key(passphrase, &salt, M_COST_KIB, T_COST, P_COST)?;
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = XChaCha20Poly1305::new(Key::from_slice(&key[..]))
        .encrypt(&nonce, plaintext.as_slice())
        .map_err(|_| DbError::Internal {
            message: "encrypting secrets failed".into(),
        })?;
    Ok(EncryptedSecrets {
        kdf: KDF.into(),
        m: M_COST_KIB,
        t: T_COST,
        p: P_COST,
        salt: BASE64.encode(salt),
        cipher: CIPHER.into(),
        nonce: BASE64.encode(nonce),
        ciphertext: BASE64.encode(ciphertext),
    })
}

fn decrypt_secrets(
    enc: &EncryptedSecrets,
    passphrase: &SecretString,
) -> Result<HashMap<Uuid, NewSecrets>, DbError> {
    if enc.kdf != KDF {
        return Err(invalid(format!(
            "Unsupported key derivation function {:?}.",
            enc.kdf
        )));
    }
    if enc.cipher != CIPHER {
        return Err(invalid(format!("Unsupported cipher {:?}.", enc.cipher)));
    }
    if enc.m > MAX_M_COST_KIB || enc.t > MAX_T_COST || enc.p > MAX_P_COST {
        return Err(invalid("Unsupported key derivation parameters."));
    }
    let decode = |what: &str, s: &str| {
        BASE64
            .decode(s)
            .map_err(|e| invalid(format!("The {what} is not valid base64: {e}")))
    };
    let salt = decode("salt", &enc.salt)?;
    let nonce = decode("nonce", &enc.nonce)?;
    let ciphertext = decode("ciphertext", &enc.ciphertext)?;
    if nonce.len() != NONCE_LEN {
        return Err(invalid(format!("The nonce must be {NONCE_LEN} bytes.")));
    }
    let key = derive_key(passphrase, &salt, enc.m, enc.t, enc.p)?;
    let plaintext = Zeroizing::new(
        XChaCha20Poly1305::new(Key::from_slice(&key[..]))
            .decrypt(XNonce::from_slice(&nonce), ciphertext.as_slice())
            .map_err(|_| DbError::Config {
                field: Some("passphrase".into()),
                message: PASSPHRASE_INCORRECT.into(),
            })?,
    );
    // The serde error is dropped on purpose: it could quote a secret value.
    let entries: HashMap<Uuid, PlainSecrets> = serde_json::from_slice(&plaintext)
        .map_err(|_| invalid("The encrypted secrets are malformed."))?;
    Ok(entries
        .into_iter()
        .map(|(id, e)| {
            let secret = |s: &Option<String>| s.as_deref().map(SecretString::from);
            let secrets = NewSecrets {
                db_password: secret(&e.db_password),
                ssh_password: secret(&e.ssh_password),
                ssh_key_passphrase: secret(&e.ssh_key_passphrase),
            };
            (id, secrets)
        })
        .collect())
}

fn derive_key(
    passphrase: &SecretString,
    salt: &[u8],
    m: u32,
    t: u32,
    p: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, DbError> {
    let params = Params::new(m, t, p, Some(KEY_LEN))
        .map_err(|e| invalid(format!("Invalid key derivation parameters: {e}")))?;
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.expose_secret().as_bytes(), salt, &mut key[..])
        .map_err(|e| invalid(format!("Key derivation failed: {e}")))?;
    Ok(key)
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: None,
        message: message.into(),
    }
}

fn not_found(what: &str, id: Uuid) -> DbError {
    DbError::Internal {
        message: format!("{what} {id} not found"),
    }
}
