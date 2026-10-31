//! Navicat `.ncx` connection import (Navicat 12+ password encryption: AES-128-CBC).

use crate::repo::NewSecrets;
use crate::transfer::{ImportBundle, ImportSource, SkippedItem};
use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, KeyIvInit};
use dnt_core::config::{
    ConnOptions, ConnectionConfig, SecretMode, SshAuth, SshConfig, SslConfig, SslMode,
};
use dnt_core::error::DbError;
use dnt_core::server::DbKind;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use secrecy::SecretString;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use uuid::Uuid;

#[cfg(test)]
use aes::cipher::BlockEncryptMut;

const KEY: &[u8; 16] = b"libcckeylibcckey";
const IV: &[u8; 16] = b"libcciv libcciv ";
const DECRYPT_WARNING: &str = "Password could not be decrypted";

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
#[cfg(test)]
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Decrypts a Navicat 12+ hex ciphertext. Errors never include the input.
pub fn navicat12_decrypt(hex: &str) -> Result<String, DbError> {
    let fail = || invalid("The Navicat password could not be decrypted.");
    let bytes = hex::decode(hex.trim()).map_err(|_| fail())?;
    let plain = Aes128CbcDec::new(KEY.into(), IV.into())
        .decrypt_padded_vec_mut::<Pkcs7>(&bytes)
        .map_err(|_| fail())?;
    String::from_utf8(plain).map_err(|_| fail())
}

#[cfg(test)]
fn navicat12_encrypt(plain: &str) -> String {
    let bytes =
        Aes128CbcEnc::new(KEY.into(), IV.into()).encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    hex::encode_upper(bytes)
}

/// Case-insensitive view of one `<Connection>` element's attributes.
struct Attrs(HashMap<String, String>);

impl Attrs {
    fn from_element(e: &BytesStart<'_>) -> Result<Self, DbError> {
        let mut map = HashMap::new();
        for attr in e.attributes() {
            let attr = attr.map_err(|e| invalid(format!("Invalid .ncx attribute: {e}")))?;
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_ascii_lowercase();
            let value = attr
                .unescape_value()
                .map_err(|e| invalid(format!("Invalid .ncx attribute value: {e}")))?;
            map.insert(key, value.into_owned());
        }
        Ok(Attrs(map))
    }

    /// The attribute value, `None` when missing or empty.
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn flag(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.get(name).map(PathBuf::from)
    }
}

/// Parses a Navicat `.ncx` export. Unsupported connection types and connections failing
/// validation are reported as skipped; undecryptable passwords become per-connection warnings.
pub fn parse_ncx(xml: &str) -> Result<ImportBundle, DbError> {
    let mut reader = Reader::from_str(xml);
    let mut elements = Vec::new();
    let mut depth = 0usize;
    let mut saw_root = false;
    loop {
        match reader
            .read_event()
            .map_err(|e| invalid(format!("The file is not valid XML: {e}")))?
        {
            Event::Start(e) => {
                depth += 1;
                saw_root |= e.name().as_ref().eq_ignore_ascii_case(b"Connections");
                if e.name().as_ref().eq_ignore_ascii_case(b"Connection") {
                    elements.push(Attrs::from_element(&e)?);
                }
            }
            Event::Empty(e) => {
                saw_root |= e.name().as_ref().eq_ignore_ascii_case(b"Connections");
                if e.name().as_ref().eq_ignore_ascii_case(b"Connection") {
                    elements.push(Attrs::from_element(&e)?);
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 || !saw_root {
        return Err(invalid("The file is not a Navicat connections export."));
    }

    let mut bundle = ImportBundle {
        source: ImportSource::Navicat,
        groups: Vec::new(),
        connections: Vec::new(),
        secrets: HashMap::new(),
        skipped: Vec::new(),
        warnings: HashMap::new(),
    };
    let mut seen = HashSet::new();
    for attrs in &elements {
        let name = attrs.get("ConnectionName").unwrap_or_default().to_string();
        let conn_type = attrs.get("ConnType").unwrap_or_default();
        let Some(kind) = db_kind(conn_type) else {
            bundle.skipped.push(SkippedItem {
                name,
                reason: format!("Unsupported type {conn_type}"),
            });
            continue;
        };

        let mut id = Uuid::new_v4();
        while !seen.insert(id) {
            id = Uuid::new_v4();
        }
        let mut warnings = Vec::new();
        let mut decrypt = |field: &str| -> Option<SecretString> {
            let cipher = attrs.get(field)?;
            match navicat12_decrypt(cipher) {
                Ok(plain) => Some(SecretString::from(plain)),
                Err(_) => {
                    if !warnings.iter().any(|w| w == DECRYPT_WARNING) {
                        warnings.push(DECRYPT_WARNING.to_string());
                    }
                    None
                }
            }
        };
        let secrets = NewSecrets {
            db_password: decrypt("Password"),
            ssh_password: decrypt("SSH_Password"),
            ssh_key_passphrase: decrypt("SSH_Passphrase"),
        };

        let config = ConnectionConfig {
            id,
            group_id: None,
            name: name.clone(),
            sort_order: 0,
            kind,
            host: attrs.get("Host").unwrap_or_default().to_string(),
            port: port(attrs.get("Port"), default_port(kind)),
            username: attrs.get("UserName").unwrap_or_default().to_string(),
            default_db: attrs.get("Database").map(str::to_string),
            env_tag: None,
            read_only: false,
            secret_mode: if secrets.db_password.is_some() {
                SecretMode::Saved
            } else {
                SecretMode::Ask
            },
            ssh: attrs.flag("SSH").then(|| ssh_config(attrs)),
            ssl: ssl_config(attrs),
            options: ConnOptions::default(),
        };
        if let Err(e) = config.validate() {
            bundle.skipped.push(SkippedItem {
                name,
                reason: match e {
                    DbError::Config { message, .. } => message,
                    other => other.to_string(),
                },
            });
            continue;
        }
        if secrets.db_password.is_some()
            || secrets.ssh_password.is_some()
            || secrets.ssh_key_passphrase.is_some()
        {
            bundle.secrets.insert(id, secrets);
        }
        if !warnings.is_empty() {
            bundle.warnings.insert(id, warnings);
        }
        bundle.connections.push(config);
    }
    Ok(bundle)
}

fn db_kind(conn_type: &str) -> Option<DbKind> {
    match conn_type.to_ascii_uppercase().as_str() {
        "MYSQL" | "MARIADB" => Some(DbKind::MySql),
        "POSTGRESQL" => Some(DbKind::Postgres),
        _ => None,
    }
}

fn default_port(kind: DbKind) -> u16 {
    match kind {
        DbKind::MySql => 3306,
        DbKind::Postgres => 5432,
        DbKind::Mongo => 27017,
        DbKind::Redis => 6379,
    }
}

/// A missing port uses the default; an unparsable one becomes 0 so validation rejects it.
fn port(value: Option<&str>, default: u16) -> u16 {
    value.map_or(default, |v| v.trim().parse().unwrap_or(0))
}

fn ssh_config(attrs: &Attrs) -> SshConfig {
    let auth = match attrs.get("SSH_AuthenMethod") {
        Some(m) if m.eq_ignore_ascii_case("PUBLICKEY") => SshAuth::Key,
        _ => SshAuth::Password,
    };
    SshConfig {
        version: 1,
        host: attrs.get("SSH_Host").unwrap_or_default().to_string(),
        port: port(attrs.get("SSH_Port"), 22),
        user: attrs.get("SSH_UserName").unwrap_or_default().to_string(),
        auth,
        key_path: attrs.path("SSH_PrivateKey"),
    }
}

fn ssl_config(attrs: &Attrs) -> Option<SslConfig> {
    if !attrs.flag("SSL") {
        return None;
    }
    let mode = match attrs.get("SSL_PGSSLMode").map(str::to_ascii_lowercase) {
        Some(m) if m == "verify-ca" => SslMode::VerifyCa,
        Some(m) if m == "verify-full" => SslMode::VerifyFull,
        _ => SslMode::Require,
    };
    Some(SslConfig {
        version: 1,
        mode,
        ca_path: attrs.path("SSL_CACert"),
        cert_path: attrs.path("SSL_ClientCert"),
        key_path: attrs.path("SSL_ClientKey"),
    })
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: None,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypt_roundtrip() {
        for p in ["P@ssw0rd!", "đèn-xanh"] {
            assert_eq!(navicat12_decrypt(&navicat12_encrypt(p)).unwrap(), p);
        }
    }

    #[test]
    fn known_vector() {
        // printf 123456 | openssl enc -aes-128-cbc -K 6c696263636b65796c696263636b6579 \
        //   -iv 6c696263636976206c69626363697620 | xxd -p | tr a-f A-F
        const EXPECTED: &str = "833E4ABBC56C89041A9070F043641E3B";
        assert_eq!(navicat12_decrypt(EXPECTED).unwrap(), "123456");
    }
}
