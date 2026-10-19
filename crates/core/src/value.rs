use crate::meta::ObjectRef;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use specta::Type;
use std::fmt;

/// Largest integer magnitude a JS number represents exactly (2^53).
const MAX_SAFE: u64 = 9_007_199_254_740_992;
/// Maximum number of bytes kept in `Value::Bytes::preview`.
const BYTES_PREVIEW_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    Null,
    Bool(bool),
    Int(#[serde(serialize_with = "serialize_int", deserialize_with = "deserialize_int")] i64),
    UInt(
        #[serde(
            serialize_with = "serialize_uint",
            deserialize_with = "deserialize_uint"
        )]
        u64,
    ),
    Float(f64),
    Decimal(String),
    Text(String),
    Json(String),
    Uuid(String),
    Bytes {
        len: u64,
        preview: Vec<u8>,
    },
    Date(String),
    Time(String),
    DateTime(String),
    TimestampTz(String),
    Array(Vec<Value>),
    Other {
        type_name: String,
        text: String,
    },
}

impl Value {
    /// Builds a `Bytes` value keeping at most 64 bytes of preview.
    pub fn bytes(full: &[u8]) -> Value {
        Value::Bytes {
            len: full.len() as u64,
            preview: full[..full.len().min(BYTES_PREVIEW_LEN)].to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub enum LogicalType {
    Integer,
    Decimal,
    Float,
    Text,
    Binary,
    Bool,
    Date,
    Time,
    DateTime,
    Json,
    Uuid,
    Array,
    Enum,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct ColumnMeta {
    pub name: String,
    pub db_type: String,
    pub logical: LogicalType,
    pub nullable: Option<bool>,
    pub origin_table: Option<ObjectRef>,
    pub origin_column: Option<String>,
}

fn serialize_int<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    if v.unsigned_abs() <= MAX_SAFE {
        s.serialize_i64(*v)
    } else {
        s.serialize_str(&v.to_string())
    }
}

fn serialize_uint<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    if *v <= MAX_SAFE {
        s.serialize_u64(*v)
    } else {
        s.serialize_str(&v.to_string())
    }
}

struct IntVisitor;

impl<'de> Visitor<'de> for IntVisitor {
    type Value = i128;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an integer or an integer string")
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<i128, E> {
        Ok(v as i128)
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<i128, E> {
        Ok(v as i128)
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<i128, E> {
        v.parse::<i128>().map_err(E::custom)
    }
}

fn deserialize_int<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let n = d.deserialize_any(IntVisitor)?;
    i64::try_from(n).map_err(de::Error::custom)
}

fn deserialize_uint<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let n = d.deserialize_any(IntVisitor)?;
    u64::try_from(n).map_err(de::Error::custom)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_ints_serialize_as_numbers() {
        assert_eq!(
            serde_json::to_string(&Value::Int(42)).unwrap(),
            r#"{"t":"Int","v":42}"#
        );
    }
    #[test]
    fn big_ints_serialize_as_strings() {
        assert_eq!(
            serde_json::to_string(&Value::Int(9_007_199_254_740_993)).unwrap(),
            r#"{"t":"Int","v":"9007199254740993"}"#
        );
        assert_eq!(
            serde_json::to_string(&Value::UInt(u64::MAX)).unwrap(),
            r#"{"t":"UInt","v":"18446744073709551615"}"#
        );
        assert_eq!(
            serde_json::to_string(&Value::Int(-9_007_199_254_740_993)).unwrap(),
            r#"{"t":"Int","v":"-9007199254740993"}"#
        );
    }
    #[test]
    fn boundary_int_stays_number() {
        assert_eq!(
            serde_json::to_string(&Value::Int(9_007_199_254_740_992)).unwrap(),
            r#"{"t":"Int","v":9007199254740992}"#
        );
    }
    #[test]
    fn bytes_preview_is_capped_at_64() {
        let v = Value::bytes(&[7u8; 1000]);
        assert!(matches!(v, Value::Bytes { len: 1000, ref preview } if preview.len() == 64));
    }
    #[test]
    fn decimal_is_kept_verbatim() {
        assert_eq!(
            serde_json::to_string(&Value::Decimal("12345678901234567890.123456".into())).unwrap(),
            r#"{"t":"Decimal","v":"12345678901234567890.123456"}"#
        );
    }
    #[test]
    fn ints_deserialize_from_number_or_string() {
        let a: Value = serde_json::from_str(r#"{"t":"Int","v":42}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"t":"Int","v":"-9007199254740993"}"#).unwrap();
        let c: Value = serde_json::from_str(r#"{"t":"UInt","v":"18446744073709551615"}"#).unwrap();
        assert_eq!(a, Value::Int(42));
        assert_eq!(b, Value::Int(-9_007_199_254_740_993));
        assert_eq!(c, Value::UInt(u64::MAX));
    }
}
