//! Browsing a key-value keyspace (Redis): spec §5.2, for `capabilities.paged_objects`.

use crate::error::DbError;
use crate::value::Value;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use specta::Type;

#[async_trait]
pub trait Keyspace: Send + Sync {
    /// One page of keys of `db`. `req.count` is at most 1000; `req.pattern` is a Redis
    /// glob and never becomes a `KEYS`.
    async fn scan(&self, db: &str, req: &ScanRequest) -> Result<KeyPage, DbError>;
    /// One page (`range`) of the value of `key`, with its TTL and memory use.
    async fn key_value(
        &self,
        db: &str,
        key: &KeyName,
        range: &ValueRange,
    ) -> Result<KeyValueView, DbError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ScanRequest {
    pub cursor: Option<String>,
    pub pattern: Option<String>,
    pub key_type: Option<KeyType>,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeyPage {
    pub keys: Vec<KeySummary>,
    /// `None`: the scan is complete.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeySummary {
    pub name: KeyName,
    pub key_type: KeyType,
    pub ttl_ms: Option<i64>,
}

/// A key as the server stores it (`raw`, serialized as standard base64) and as the UI
/// shows it (`display`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeyName {
    pub display: String,
    #[serde(with = "raw_base64")]
    #[specta(type = String)]
    pub raw: Vec<u8>,
}

impl KeyName {
    /// `display` is `raw` as UTF-8, every invalid byte shown as `\xNN`.
    pub fn from_bytes(raw: Vec<u8>) -> KeyName {
        let mut display = String::with_capacity(raw.len());
        for chunk in raw.utf8_chunks() {
            display.push_str(chunk.valid());
            for b in chunk.invalid() {
                display.push_str(&format!("\\x{b:02x}"));
            }
        }
        KeyName { display, raw }
    }
}

mod raw_base64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(raw: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(raw))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum KeyType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    Json,
    Other(String),
}

/// A page of a hash / list / set / zset / stream value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ValueRange {
    pub offset: u64,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub enum KeyValue {
    String {
        value: Value,
    },
    Hash {
        fields: Vec<(Value, Value)>,
        total: u64,
        next: Option<String>,
    },
    List {
        items: Vec<Value>,
        total: u64,
    },
    Set {
        members: Vec<Value>,
        total: u64,
        next: Option<String>,
    },
    ZSet {
        members: Vec<(Value, f64)>,
        total: u64,
    },
    Stream {
        entries: Vec<StreamEntry>,
        total: u64,
    },
    Other {
        type_name: String,
        text: String,
    },
}

/// What [`Keyspace::key_value`] returns: the value plus its TTL and (best-effort,
/// `MEMORY USAGE`) memory use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct KeyValueView {
    pub value: KeyValue,
    pub ttl_ms: Option<i64>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<(Value, Value)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_name_shows_invalid_bytes() {
        let k = KeyName::from_bytes(vec![b'a', 0xff, b'b']);
        assert_eq!(k.display, "a\\xffb");
        assert_eq!(serde_json::to_value(&k).unwrap()["raw"], "Yf9i");
        let back: KeyName = serde_json::from_value(serde_json::to_value(&k).unwrap()).unwrap();
        assert_eq!(back, k);
    }
}
