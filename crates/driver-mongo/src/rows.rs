//! BSON <-> `Value` mapping (spec §7), canonical Extended JSON, and document -> column/row
//! building for result grids.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use dnt_core::driver::CellMode;
use dnt_core::error::DbError;
use dnt_core::value::Value;
use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp};

/// Smallest and largest `DateTime` millis shown as `Value::DateTime`:
/// 0000-01-01T00:00:00.000Z ..= 9999-12-31T23:59:59.999Z.
const MIN_DISPLAY_MILLIS: i64 = -62_167_219_200_000;
const MAX_DISPLAY_MILLIS: i64 = 253_402_300_799_999;

fn other(type_name: &str, text: impl Into<String>) -> Value {
    Value::Other {
        type_name: type_name.into(),
        text: text.into(),
    }
}

/// BSON -> display value (§7, left column). Every cell this produces is complete:
/// binary data keeps every byte (inside `binData` text), and the cutting of long text
/// belongs to the data page, so `cells` does not change the result.
pub fn bson_to_value(b: &Bson, _cells: &CellMode) -> Value {
    match b {
        Bson::ObjectId(o) => other("objectId", o.to_hex()),
        Bson::Decimal128(d) => other("decimal", d.to_string()),
        Bson::Timestamp(t) => other("timestamp", format!("{},{}", t.time, t.increment)),
        Bson::MinKey => other("minKey", ""),
        Bson::MaxKey => other("maxKey", ""),
        Bson::RegularExpression(r) => other("regex", format!("/{}/{}", r.pattern, r.options)),
        Bson::Binary(bin) => match (bin.subtype, bin.bytes.len()) {
            (BinarySubtype::Uuid, 16) => Value::Uuid(uuid_text(&bin.bytes)),
            (subtype, _) => other(
                "binData",
                format!("{:02x}:{}", u8::from(subtype), B64.encode(&bin.bytes)),
            ),
        },
        Bson::DateTime(d) => {
            let ms = d.timestamp_millis();
            if (MIN_DISPLAY_MILLIS..=MAX_DISPLAY_MILLIS).contains(&ms) {
                Value::DateTime(format_millis(ms))
            } else {
                other("date", ms.to_string())
            }
        }
        Bson::Int32(n) => Value::Int(i64::from(*n)),
        Bson::Int64(n) => Value::Int(*n),
        Bson::Double(f) if f.is_finite() => Value::Float(*f),
        Bson::Double(f) if f.is_nan() => other("double", "NaN"),
        Bson::Double(f) if *f > 0.0 => other("double", "Infinity"),
        Bson::Double(_) => other("double", "-Infinity"),
        Bson::Boolean(v) => Value::Bool(*v),
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::String(s) | Bson::Symbol(s) => Value::Text(s.clone()),
        Bson::JavaScriptCode(code) => other("javascript", code.clone()),
        Bson::JavaScriptCodeWithScope(c) => other("codeWithScope", c.code.clone()),
        Bson::DbPointer(_) => other("dbPointer", b.to_string()),
        Bson::Document(_) | Bson::Array(_) => Value::Json(canonical_ejson(b)),
    }
}

/// Value -> BSON for an edited cell (§7, right column). `db_type` is the column's
/// type name (`int` / `long`) and picks the integer width.
pub fn value_to_bson(v: &Value, db_type: Option<&str>) -> Result<Bson, DbError> {
    Ok(match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(n) => int_to_bson(*n, db_type),
        Value::UInt(n) => match i64::try_from(*n) {
            Ok(n) => int_to_bson(n, db_type),
            Err(_) => {
                return Err(cfg(format!(
                    "integer {n} does not fit in a 64-bit signed value"
                )))
            }
        },
        Value::Float(f) => Bson::Double(*f),
        Value::Decimal(s) => Bson::Decimal128(parse_decimal(s)?),
        Value::Text(s) => Bson::String(s.clone()),
        Value::Json(s) => json_to_bson(s)?,
        Value::Uuid(s) => Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: parse_uuid(s)?,
        }),
        Value::Bytes { len, preview } if preview.len() as u64 >= *len => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: preview.clone(),
        }),
        Value::Bytes { .. } => return Err(cfg("binary value is cut by its preview".into())),
        Value::DateTime(s) | Value::TimestampTz(s) => Bson::DateTime(parse_datetime(s)?),
        Value::Date(_) | Value::Time(_) => {
            return Err(cfg("date and time values are not BSON types".into()))
        }
        Value::Array(items) => Bson::Array(
            items
                .iter()
                .map(|i| value_to_bson(i, None))
                .collect::<Result<_, _>>()?,
        ),
        Value::Other { type_name, text } => other_to_bson(type_name, text)?,
    })
}

fn other_to_bson(type_name: &str, text: &str) -> Result<Bson, DbError> {
    let bad = || cfg(format!("invalid {type_name} value: {text}"));
    Ok(match type_name {
        "objectId" => Bson::ObjectId(ObjectId::parse_str(text).map_err(|_| bad())?),
        "decimal" => Bson::Decimal128(parse_decimal(text)?),
        "timestamp" => {
            let (t, i) = text.split_once(',').ok_or_else(bad)?;
            Bson::Timestamp(Timestamp {
                time: t.trim().parse().map_err(|_| bad())?,
                increment: i.trim().parse().map_err(|_| bad())?,
            })
        }
        "minKey" => Bson::MinKey,
        "maxKey" => Bson::MaxKey,
        "regex" => {
            let rest = text.strip_prefix('/').ok_or_else(bad)?;
            let (pattern, options) = rest.rsplit_once('/').ok_or_else(bad)?;
            Bson::RegularExpression(Regex {
                pattern: pattern.into(),
                options: options.into(),
            })
        }
        "binData" => {
            let (sub, data) = text.split_once(':').ok_or_else(bad)?;
            let subtype = u8::from_str_radix(sub, 16).map_err(|_| bad())?;
            Bson::Binary(Binary {
                subtype: BinarySubtype::from(subtype),
                bytes: B64.decode(data).map_err(|_| bad())?,
            })
        }
        "date" => Bson::DateTime(DateTime::from_millis(text.parse().map_err(|_| bad())?)),
        "double" => Bson::Double(match text {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => return Err(bad()),
        }),
        "javascript" | "codeWithScope" | "dbPointer" => {
            return Err(cfg(format!("{type_name} values are read-only")))
        }
        _ => return Err(cfg(format!("{type_name} values cannot be written"))),
    })
}

fn cfg(message: String) -> DbError {
    DbError::Config {
        field: None,
        message,
    }
}

fn int_to_bson(n: i64, db_type: Option<&str>) -> Bson {
    match (db_type, i32::try_from(n)) {
        (Some("long"), _) | (_, Err(_)) => Bson::Int64(n),
        (_, Ok(small)) => Bson::Int32(small),
    }
}

fn parse_decimal(s: &str) -> Result<Decimal128, DbError> {
    s.trim()
        .parse()
        .map_err(|_| cfg(format!("invalid decimal value: {s}")))
}

/// Cell JSON is canonical Extended JSON (what [`canonical_ejson`] writes), so the BSON
/// types survive the edit. Task 6's shell literal parser also accepts relaxed forms; the
/// cell editor only needs to read back its own output.
fn json_to_bson(s: &str) -> Result<Bson, DbError> {
    let json: serde_json::Value =
        serde_json::from_str(s).map_err(|e| cfg(format!("invalid JSON value: {e}")))?;
    Bson::try_from(json).map_err(|e| cfg(format!("invalid Extended JSON value: {e}")))
}

fn parse_uuid(s: &str) -> Result<Vec<u8>, DbError> {
    let hex_digits: String = s.chars().filter(|c| *c != '-').collect();
    match hex::decode(&hex_digits) {
        Ok(b) if b.len() == 16 => Ok(b),
        _ => Err(cfg(format!("invalid uuid value: {s}"))),
    }
}

fn uuid_text(b: &[u8]) -> String {
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// `YYYY-MM-DD HH:MM:SS.mmm` (UTC) for millis in the displayable range.
fn format_millis(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// Accepts what [`format_millis`] writes, and RFC 3339 (`TimestampTz`).
fn parse_datetime(s: &str) -> Result<DateTime, DbError> {
    if let Ok(d) = DateTime::parse_rfc3339_str(s) {
        return Ok(d);
    }
    parse_millis(s)
        .map(DateTime::from_millis)
        .ok_or_else(|| cfg(format!("invalid date value: {s}")))
}

fn parse_millis(s: &str) -> Option<i64> {
    let (date, time) = s.split_once([' ', 'T'])?;
    let mut d = date.splitn(3, '-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.splitn(3, ':');
    let (h, mi, sec): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let frac3: String = format!("{frac:0<3}").chars().take(3).collect();
    let millis: i64 = frac3.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(y, m, day);
    Some(days * 86_400_000 + ((h * 60 + mi) * 60 + sec) * 1000 + millis)
}

// Howard Hinnant's civil-calendar algorithms (proleptic Gregorian, days since 1970-01-01).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Canonical Extended JSON (v2) text of a value, keys in document order. Documents and
/// arrays use it as their `Value::Json` text, so editing a cell loses no BSON type.
pub fn canonical_ejson(b: &Bson) -> String {
    b.clone().into_canonical_extjson().to_string()
}

/// Columns of a result: `_id` first (when any document has it), then field names in
/// order of first appearance.
pub fn columns_of(docs: &[Document]) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    if docs.iter().any(|d| d.contains_key("_id")) {
        cols.push("_id".into());
    }
    for d in docs {
        for k in d.keys() {
            if !cols.iter().any(|c| c == k) {
                cols.push(k.clone());
            }
        }
    }
    cols
}

/// One row for `cols`; a field the document lacks is `Null`.
pub fn row_of(doc: &Document, cols: &[String], cells: &CellMode) -> Vec<Value> {
    cols.iter()
        .map(|c| doc.get(c).map_or(Value::Null, |b| bson_to_value(b, cells)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{
        doc, oid::ObjectId, Binary, Decimal128, JavaScriptCodeWithScope, Regex, Timestamp,
    };

    fn other(t: &str, s: &str) -> Value {
        Value::Other {
            type_name: t.into(),
            text: s.into(),
        }
    }

    fn to_value(b: &Bson) -> Value {
        bson_to_value(b, &CellMode::Preview)
    }

    fn read_only(r: Result<Bson, DbError>, kind: &str) {
        match r {
            Err(DbError::Config {
                field: None,
                message,
            }) => {
                assert_eq!(message, format!("{kind} values are read-only"))
            }
            other => panic!("expected read-only error, got {other:?}"),
        }
    }

    /// A `$dbPointer` cannot be built directly (private fields); go through Extended JSON.
    fn db_pointer() -> Bson {
        let j = serde_json::json!({"$dbPointer": {"$ref": "users", "$id": {"$oid": "507f1f77bcf86cd799439011"}}});
        Bson::try_from(j).unwrap()
    }

    #[test]
    fn every_bson_kind_maps() {
        let oid = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        assert_eq!(
            to_value(&Bson::ObjectId(oid)),
            other("objectId", "507f1f77bcf86cd799439011")
        );
        let dec: Decimal128 = "1.50".parse().unwrap();
        assert_eq!(to_value(&Bson::Decimal128(dec)), other("decimal", "1.50"));
        assert_eq!(
            to_value(&Bson::Timestamp(Timestamp {
                time: 7,
                increment: 3
            })),
            other("timestamp", "7,3")
        );
        assert_eq!(to_value(&Bson::MinKey), other("minKey", ""));
        assert_eq!(to_value(&Bson::MaxKey), other("maxKey", ""));
        assert_eq!(
            to_value(&Bson::RegularExpression(Regex {
                pattern: "^a.b".into(),
                options: "i".into()
            })),
            other("regex", "/^a.b/i")
        );
        let uuid_bytes: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            to_value(&Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: uuid_bytes
            })),
            Value::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into())
        );
        assert_eq!(
            to_value(&Bson::DateTime(mongodb::bson::DateTime::from_millis(
                1_700_000_000_123
            ))),
            Value::DateTime("2023-11-14 22:13:20.123".into())
        );
        assert_eq!(to_value(&Bson::Int32(5)), Value::Int(5));
        assert_eq!(to_value(&Bson::Int64(-9)), Value::Int(-9));
        assert_eq!(to_value(&Bson::Double(1.5)), Value::Float(1.5));
        assert_eq!(to_value(&Bson::Boolean(true)), Value::Bool(true));
        assert_eq!(to_value(&Bson::Null), Value::Null);
        assert_eq!(to_value(&Bson::Undefined), Value::Null);
        assert_eq!(to_value(&Bson::Symbol("s".into())), Value::Text("s".into()));
        assert_eq!(to_value(&Bson::String("x".into())), Value::Text("x".into()));
        assert_eq!(
            to_value(&Bson::JavaScriptCode("f()".into())),
            other("javascript", "f()")
        );
        assert_eq!(
            to_value(&Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope {
                code: "g()".into(),
                scope: doc! {"a": 1}
            })),
            other("codeWithScope", "g()")
        );
        match to_value(&db_pointer()) {
            Value::Other { type_name, text } => {
                assert_eq!(type_name, "dbPointer");
                assert!(text.contains("users"), "{text}");
            }
            v => panic!("{v:?}"),
        }
        assert_eq!(
            to_value(&Bson::Document(doc! {"a": 1_i64})),
            Value::Json(r#"{"a":{"$numberLong":"1"}}"#.into())
        );
        assert_eq!(
            to_value(&Bson::Array(vec![Bson::Int32(1), Bson::Null])),
            Value::Json(r#"[{"$numberInt":"1"},null]"#.into())
        );
        // Value -> BSON for the writable kinds.
        let back = |v: &Value| value_to_bson(v, None).unwrap();
        assert_eq!(
            back(&other("objectId", "507f1f77bcf86cd799439011")),
            Bson::ObjectId(oid)
        );
        assert_eq!(back(&other("decimal", "1.50")), Bson::Decimal128(dec));
        assert_eq!(
            back(&other("timestamp", "7,3")),
            Bson::Timestamp(Timestamp {
                time: 7,
                increment: 3
            })
        );
        assert_eq!(back(&other("minKey", "")), Bson::MinKey);
        assert_eq!(back(&other("maxKey", "")), Bson::MaxKey);
        assert_eq!(
            back(&other("regex", "/^a.b/i")),
            Bson::RegularExpression(Regex {
                pattern: "^a.b".into(),
                options: "i".into()
            })
        );
        assert_eq!(
            back(&Value::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into())),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: (0u8..16).collect()
            })
        );
        assert_eq!(
            back(&Value::DateTime("2023-11-14 22:13:20.123".into())),
            Bson::DateTime(mongodb::bson::DateTime::from_millis(1_700_000_000_123))
        );
        assert_eq!(
            back(&other("date", "-62198755200000")),
            Bson::DateTime(mongodb::bson::DateTime::from_millis(-62_198_755_200_000))
        );
        assert_eq!(back(&Value::Float(1.5)), Bson::Double(1.5));
        assert_eq!(back(&Value::Bool(false)), Bson::Boolean(false));
        assert_eq!(back(&Value::Null), Bson::Null);
        assert_eq!(back(&Value::Text("t".into())), Bson::String("t".into()));
        assert!(matches!(
            back(&other("double", "NaN")),
            Bson::Double(d) if d.is_nan()
        ));
        assert_eq!(
            back(&other("double", "-Infinity")),
            Bson::Double(f64::NEG_INFINITY)
        );
        assert_eq!(
            back(&Value::Json(r#"{"a":{"$numberLong":"1"}}"#.into())),
            Bson::Document(doc! {"a": 1_i64})
        );
        read_only(
            value_to_bson(&other("javascript", "f()"), None),
            "javascript",
        );
        read_only(
            value_to_bson(&other("codeWithScope", "g()"), None),
            "codeWithScope",
        );
        read_only(value_to_bson(&other("dbPointer", "x"), None), "dbPointer");
    }

    #[test]
    fn binary_keeps_subtype_and_bytes() {
        let bytes: Vec<u8> = (0..200u8).collect();
        let v = to_value(&Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: bytes.clone(),
        }));
        let Value::Other { type_name, text } = &v else {
            panic!("{v:?}")
        };
        assert_eq!(type_name, "binData");
        let (sub, b64) = text.split_once(':').unwrap();
        assert_eq!(sub, "00");
        assert_eq!(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap(),
            bytes
        );
        // A user-defined subtype and a non-16-byte subtype 4 stay binData.
        let user = to_value(&Bson::Binary(Binary {
            subtype: BinarySubtype::UserDefined(0x80),
            bytes: vec![1, 2],
        }));
        assert_eq!(user, other("binData", "80:AQI="));
        let short_uuid = to_value(&Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: vec![1, 2],
        }));
        assert_eq!(short_uuid, other("binData", "04:AQI="));
        // Back: exact subtype and bytes.
        assert_eq!(
            value_to_bson(&v, None).unwrap(),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes
            })
        );
        assert_eq!(
            value_to_bson(&user, None).unwrap(),
            Bson::Binary(Binary {
                subtype: BinarySubtype::UserDefined(0x80),
                bytes: vec![1, 2]
            })
        );
        // Complete core bytes become generic binary; cut previews are refused.
        assert_eq!(
            value_to_bson(&Value::bytes(&[9, 9]), None).unwrap(),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![9, 9]
            })
        );
        assert!(value_to_bson(&Value::bytes(&[9; 100]), None).is_err());
    }

    #[test]
    fn nan_and_out_of_range_dates_are_other() {
        assert_eq!(to_value(&Bson::Double(f64::NAN)), other("double", "NaN"));
        assert_eq!(
            to_value(&Bson::Double(f64::INFINITY)),
            other("double", "Infinity")
        );
        assert_eq!(
            to_value(&Bson::Double(f64::NEG_INFINITY)),
            other("double", "-Infinity")
        );
        let ms = |m| Bson::DateTime(mongodb::bson::DateTime::from_millis(m));
        assert_eq!(
            to_value(&ms(i64::MAX)),
            other("date", &i64::MAX.to_string())
        );
        assert_eq!(
            to_value(&ms(i64::MIN)),
            other("date", &i64::MIN.to_string())
        );
        // Year 10000 and year -1 are outside the displayable range; the edges are inside.
        assert_eq!(
            to_value(&ms(253_402_300_800_000)),
            other("date", "253402300800000")
        );
        assert_eq!(
            to_value(&ms(-62_167_219_200_001)),
            other("date", "-62167219200001")
        );
        assert_eq!(
            to_value(&ms(253_402_300_799_999)),
            Value::DateTime("9999-12-31 23:59:59.999".into())
        );
        assert_eq!(
            to_value(&ms(-62_167_219_200_000)),
            Value::DateTime("0000-01-01 00:00:00.000".into())
        );
        assert_eq!(
            to_value(&ms(-1)),
            Value::DateTime("1969-12-31 23:59:59.999".into())
        );
    }

    /// The nested document of the round trip below.
    fn nested() -> Document {
        doc! {
            "a": {
                "id": ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap(),
                "at": mongodb::bson::DateTime::from_millis(1_700_000_000_123),
                "n": 5_i64,
                "d": "1.50".parse::<Decimal128>().unwrap(),
            }
        }
    }

    const NESTED_EJSON: &str = r#"{"a":{"id":{"$oid":"507f1f77bcf86cd799439011"},"at":{"$date":{"$numberLong":"1700000000123"}},"n":{"$numberLong":"5"},"d":{"$numberDecimal":"1.50"}}}"#;

    /// Half of `nested_types_round_trip_canonical_ejson`: the canonical text is pinned.
    /// TODO(Task 6): the other half (below, commented out) needs `shell::literal`.
    #[test]
    fn nested_types_round_trip_canonical_ejson_text() {
        let text = canonical_ejson(&Bson::Document(nested()));
        assert_eq!(text, NESTED_EJSON);
    }

    // TODO(Task 6): turn this into the real `nested_types_round_trip_canonical_ejson`
    // (drop `_text` above, un-ignore) once `crate::shell::literal::parse_document` exists:
    //
    // #[test]
    // fn nested_types_round_trip_canonical_ejson() {
    //     let d = nested();
    //     let text = canonical_ejson(&Bson::Document(d.clone()));
    //     assert_eq!(text, NESTED_EJSON);
    //     let parsed = crate::shell::literal::parse_document(&text).unwrap();
    //     assert_eq!(parsed, d);
    // }

    #[test]
    fn int_kind_follows_db_type() {
        let int = |n: i64, t: Option<&str>| value_to_bson(&Value::Int(n), t).unwrap();
        assert_eq!(int(5, Some("int")), Bson::Int32(5));
        assert_eq!(int(5, Some("long")), Bson::Int64(5));
        assert_eq!(int(5, None), Bson::Int32(5));
        assert_eq!(int(5, Some("double")), Bson::Int32(5));
        assert_eq!(
            int(i64::from(i32::MAX) + 1, None),
            Bson::Int64(2_147_483_648)
        );
        assert_eq!(int(i64::from(i32::MIN), None), Bson::Int32(i32::MIN));
        assert_eq!(
            int(i64::from(i32::MIN) - 1, None),
            Bson::Int64(-2_147_483_649)
        );
        // `int` column but the value no longer fits: widen rather than wrap.
        assert_eq!(
            int(i64::from(i32::MAX) + 1, Some("int")),
            Bson::Int64(2_147_483_648)
        );
    }

    #[test]
    fn columns_id_first_then_appearance() {
        let docs = vec![
            doc! {"b": 1, "_id": 1, "a": 2},
            doc! {"c": 3, "a": 4, "_id": 2},
        ];
        assert_eq!(columns_of(&docs), vec!["_id", "b", "a", "c"]);
        // No `_id` anywhere: first appearance only.
        assert_eq!(columns_of(&[doc! {"z": 1}, doc! {"y": 1}]), vec!["z", "y"]);
        assert!(columns_of(&[]).is_empty());
        let cols = columns_of(&docs);
        assert_eq!(
            row_of(&doc! {"a": 9, "_id": 7}, &cols, &CellMode::Preview),
            vec![Value::Int(7), Value::Null, Value::Int(9), Value::Null]
        );
    }
}
