use crate::meta::ObjectRef;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use specta::Type;
use std::fmt;
use std::sync::Arc;

/// Largest integer magnitude a JS number represents exactly (2^53).
const MAX_SAFE: u64 = 9_007_199_254_740_992;
/// Maximum number of bytes kept in `Value::Bytes::preview`.
const BYTES_PREVIEW_LEN: usize = 64;

/// TypeScript type of `Value::Int` / `Value::UInt`: a number when |x| ≤ 2^53, else a string.
/// Only used for the bindings (`#[specta(type = …)]`, exported as
/// `type JsInteger = number | string`), never constructed.
#[derive(Type)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum JsInteger {
    Number(f64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    Null,
    Bool(bool),
    Int(
        #[serde(serialize_with = "serialize_int", deserialize_with = "deserialize_int")]
        #[specta(type = JsInteger)]
        i64,
    ),
    UInt(
        #[serde(
            serialize_with = "serialize_uint",
            deserialize_with = "deserialize_uint"
        )]
        #[specta(type = JsInteger)]
        u64,
    ),
    Float(f64),
    Decimal(String),
    Text(String),
    Json(String),
    Uuid(String),
    Bytes {
        /// A byte length; far below 2^53 in practice, so a plain number.
        #[specta(type = f64)]
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

    /// False when the value lost data: a `Bytes` whose preview is shorter than its
    /// length, also inside an `Array`.
    pub fn is_complete(&self) -> bool {
        match self {
            Value::Bytes { len, preview } => preview.len() as u64 >= *len,
            Value::Array(items) => items.iter().all(Value::is_complete),
            _ => true,
        }
    }

    /// True for a `Text` / `Json` long enough (at least `PAGE_CELL_CAP - 3` bytes) that
    /// a data page may have cut it; such a value must not be written back or compared.
    pub fn may_be_page_truncated(&self) -> bool {
        match self {
            Value::Text(s) | Value::Json(s) => s.len() >= PAGE_CELL_CAP - 3,
            _ => false,
        }
    }
}

/// Largest cell a data page sends, in bytes; longer text is cut (spec §2 rule 8).
pub const PAGE_CELL_CAP: usize = 64 * 1024;

/// A cell value known to be complete: binary data with every byte, never a 64-byte
/// preview. Built only from complete data (no `From<Value>`), so SQL built from it never
/// carries a cut value. Not sent to the UI as is: see [`FullValue::to_display`].
#[derive(Debug, Clone, PartialEq)]
pub struct FullValue(Repr);

#[derive(Debug, Clone, PartialEq)]
enum Repr {
    /// Any value but `Bytes`, or an `Array` whose `Bytes` are all complete.
    Plain(Value),
    Bytes(Arc<[u8]>),
}

/// [`FullValue::try_from_value`] was given a value that lost data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("value is not complete")]
pub struct NotFull;

impl FullValue {
    pub fn bytes(b: impl Into<Arc<[u8]>>) -> FullValue {
        FullValue(Repr::Bytes(b.into()))
    }

    /// Accepts a value only if [`Value::is_complete`]: a complete `Bytes` becomes the
    /// bytes themselves, anything else is kept as is.
    pub fn try_from_value(v: Value) -> Result<FullValue, NotFull> {
        if !v.is_complete() {
            return Err(NotFull);
        }
        Ok(match v {
            Value::Bytes { preview, .. } => FullValue::bytes(preview),
            v => FullValue(Repr::Plain(v)),
        })
    }

    /// The value as the grid shows it: bytes become a `Value::Bytes` with a 64-byte
    /// preview.
    pub fn to_display(&self) -> Value {
        match &self.0 {
            Repr::Plain(v) => v.clone(),
            Repr::Bytes(b) => Value::bytes(b),
        }
    }

    /// The value, unless it is binary data (see [`FullValue::as_bytes`]).
    pub fn as_value(&self) -> Option<&Value> {
        match &self.0 {
            Repr::Plain(v) => Some(v),
            Repr::Bytes(_) => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.0 {
            Repr::Plain(_) => None,
            Repr::Bytes(b) => Some(b),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self.0, Repr::Plain(Value::Null))
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
    fn truncated_bytes_are_not_full() {
        assert_eq!(
            FullValue::try_from_value(Value::bytes(&[7; 100])),
            Err(NotFull)
        );
        assert!(FullValue::try_from_value(Value::bytes(&[7; 64])).is_ok());
        assert_eq!(
            FullValue::try_from_value(Value::Array(vec![Value::bytes(&[1; 65])])),
            Err(NotFull)
        );
    }
    #[test]
    fn full_bytes_display_as_preview() {
        let f = FullValue::bytes(vec![9u8; 1000]);
        assert_eq!(f.as_bytes().unwrap().len(), 1000);
        assert_eq!(f.as_value(), None);
        assert!(!f.is_null());
        assert_eq!(f.to_display(), Value::bytes(&[9; 1000]));
    }
    /// Complete `Bytes` become the `Bytes` repr; other values stay plain.
    #[test]
    fn complete_values_convert() {
        let b = FullValue::try_from_value(Value::bytes(&[3; 10])).unwrap();
        assert_eq!(b.as_bytes(), Some(&[3u8; 10][..]));
        assert_eq!(b, FullValue::bytes(vec![3u8; 10]));
        let t = FullValue::try_from_value(Value::Text("x".into())).unwrap();
        assert_eq!(t.as_value(), Some(&Value::Text("x".into())));
        assert_eq!(t.as_bytes(), None);
        assert_eq!(t.to_display(), Value::Text("x".into()));
        assert!(FullValue::try_from_value(Value::Null).unwrap().is_null());
        let arr = Value::Array(vec![Value::bytes(&[1; 64]), Value::Int(1)]);
        assert_eq!(
            FullValue::try_from_value(arr.clone()).unwrap().as_value(),
            Some(&arr)
        );
    }
    #[test]
    fn is_complete_looks_into_arrays() {
        assert!(Value::bytes(&[0; 64]).is_complete());
        assert!(!Value::bytes(&[0; 65]).is_complete());
        assert!(Value::Text("a".repeat(PAGE_CELL_CAP)).is_complete());
        assert!(!Value::Array(vec![Value::Null, Value::bytes(&[0; 65])]).is_complete());
        assert!(!Value::Array(vec![Value::Array(vec![Value::bytes(&[0; 65])])]).is_complete());
    }
    #[test]
    fn long_text_may_be_page_truncated() {
        assert_eq!(PAGE_CELL_CAP, 64 * 1024);
        let at = PAGE_CELL_CAP - 3;
        assert!(Value::Text("a".repeat(at)).may_be_page_truncated());
        assert!(!Value::Text("a".repeat(at - 1)).may_be_page_truncated());
        assert!(Value::Json("a".repeat(at)).may_be_page_truncated());
        assert!(!Value::Json("a".repeat(at - 1)).may_be_page_truncated());
        assert!(!Value::Decimal("1".repeat(at)).may_be_page_truncated());
        assert!(!Value::bytes(&[0; 100]).may_be_page_truncated());
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
