//! Column metadata and value mapping from PostgreSQL's binary result format to
//! `dnt_core::value`. tokio-postgres always asks for binary results, so every cell is
//! decoded from its wire bytes (see the type table in the task brief). Dates and times are
//! formatted from their integer encodings, never through a date library, so `infinity`,
//! BC dates and far-future values survive unchanged.

use crate::numeric::decode_numeric;
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use tokio_postgres::types::{FromSql, Kind, Type};
use tokio_postgres::Column;

/// Microseconds per day.
const USECS_PER_DAY: i64 = 86_400_000_000;
/// Days from 1970-01-01 (Unix epoch) to 2000-01-01 (PostgreSQL epoch).
const PG_EPOCH_DAYS: i64 = 10_957;

/// A cell's raw wire bytes (`None` for SQL NULL), accepted for any column type.
pub(crate) struct Raw<'a>(pub(crate) Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

pub(crate) fn column_meta(col: &Column) -> ColumnMeta {
    ColumnMeta {
        name: col.name().to_string(),
        db_type: type_name(col.type_()),
        logical: logical(col.type_()),
        // RowDescription carries neither nullability nor names of the origin table
        // (only its OID); the introspector supplies those.
        nullable: None,
        origin_table: None,
        origin_column: None,
    }
}

/// `int4`, `order_status`, arrays as `int4[]`.
fn type_name(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(elem) => format!("{}[]", elem.name()),
        _ => ty.name().to_string(),
    }
}

fn is_text_type(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN
    ) || ty.name() == "citext"
}

pub(crate) fn logical(ty: &Type) -> LogicalType {
    match ty.kind() {
        Kind::Domain(base) => return logical(base),
        Kind::Enum(_) => return LogicalType::Enum,
        Kind::Array(_) => return LogicalType::Array,
        _ => {}
    }
    match *ty {
        Type::BOOL => LogicalType::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => LogicalType::Integer,
        Type::FLOAT4 | Type::FLOAT8 => LogicalType::Float,
        Type::NUMERIC => LogicalType::Decimal,
        Type::JSON | Type::JSONB => LogicalType::Json,
        Type::UUID => LogicalType::Uuid,
        Type::BYTEA => LogicalType::Binary,
        Type::DATE => LogicalType::Date,
        Type::TIME => LogicalType::Time,
        Type::TIMESTAMP | Type::TIMESTAMPTZ => LogicalType::DateTime,
        _ if is_text_type(ty) => LogicalType::Text,
        _ => LogicalType::Other,
    }
}

pub(crate) fn to_value(ty: &Type, raw: Option<&[u8]>) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    decode(ty, raw).unwrap_or_else(|| other(ty, raw))
}

/// `None` when the type is not in the mapping table or the bytes are malformed.
fn decode(ty: &Type, raw: &[u8]) -> Option<Value> {
    match ty.kind() {
        // A domain is sent in its base type's format.
        Kind::Domain(base) => return Some(to_value(base, Some(raw))),
        Kind::Enum(_) => return Some(Value::Text(text(raw))),
        Kind::Array(elem) => return decode_array(elem, raw),
        _ => {}
    }
    Some(match *ty {
        Type::BOOL => Value::Bool(fixed::<1>(raw)?[0] != 0),
        Type::INT2 => Value::Int(i16::from_be_bytes(fixed(raw)?).into()),
        Type::INT4 => Value::Int(i32::from_be_bytes(fixed(raw)?).into()),
        Type::INT8 => Value::Int(i64::from_be_bytes(fixed(raw)?)),
        Type::OID => Value::Int(u32::from_be_bytes(fixed(raw)?).into()),
        // Widen through the shortest decimal form so 3.4028235e38f32 gains no noise digits.
        Type::FLOAT4 => float(
            ty,
            f32::from_be_bytes(fixed(raw)?).to_string().parse().ok()?,
        ),
        Type::FLOAT8 => float(ty, f64::from_be_bytes(fixed(raw)?)),
        Type::NUMERIC => Value::Decimal(decode_numeric(raw)),
        Type::JSON => Value::Json(text(raw)),
        // jsonb: a version byte (1) followed by the JSON text.
        Type::JSONB => match raw.split_first() {
            Some((1, json)) => Value::Json(text(json)),
            _ => return None,
        },
        Type::UUID => Value::Uuid(uuid(&fixed::<16>(raw)?)),
        Type::BYTEA => Value::bytes(raw),
        Type::DATE => Value::Date(date(i32::from_be_bytes(fixed(raw)?))),
        Type::TIME => Value::Time(time_of_day(i64::from_be_bytes(fixed(raw)?))?),
        Type::TIMESTAMP => Value::DateTime(timestamp(i64::from_be_bytes(fixed(raw)?), false)),
        Type::TIMESTAMPTZ => Value::TimestampTz(timestamp(i64::from_be_bytes(fixed(raw)?), true)),
        Type::INTERVAL => {
            let b = fixed::<16>(raw)?;
            Value::Other {
                type_name: "interval".into(),
                text: iso_interval(
                    i64::from_be_bytes(b[..8].try_into().ok()?),
                    i32::from_be_bytes(b[8..12].try_into().ok()?),
                    i32::from_be_bytes(b[12..].try_into().ok()?),
                ),
            }
        }
        _ if is_text_type(ty) => Value::Text(text(raw)),
        _ => return None,
    })
}

fn fixed<const N: usize>(raw: &[u8]) -> Option<[u8; N]> {
    raw.try_into().ok()
}

fn text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// `Float` for finite values; NaN / ±Infinity (which JSON cannot carry) as their PG text.
fn float(ty: &Type, f: f64) -> Value {
    if f.is_finite() {
        return Value::Float(f);
    }
    let text = if f.is_nan() {
        "NaN"
    } else if f > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    };
    Value::Other {
        type_name: ty.name().into(),
        text: text.into(),
    }
}

/// Types outside the table: readable UTF-8 text as is, anything else as `\x<hex>`.
fn other(ty: &Type, raw: &[u8]) -> Value {
    let text = match std::str::from_utf8(raw) {
        Ok(s)
            if !s
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) =>
        {
            s.to_string()
        }
        _ => hex_text(raw),
    };
    Value::Other {
        type_name: type_name(ty),
        text,
    }
}

/// `\x` followed by lowercase hex, like PostgreSQL's `bytea_output = hex`.
pub(crate) fn hex_text(raw: &[u8]) -> String {
    let mut s = String::with_capacity(2 + raw.len() * 2);
    s.push_str("\\x");
    for b in raw {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn uuid(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Binary array: `int32 ndim, int32 has_null, uint32 elem_oid`, then `(int32 len,
/// int32 lbound)` per dimension, then each element as `int32 len` (-1 = NULL) + bytes.
/// Multi-dimensional arrays become nested `Array`s.
fn decode_array(elem: &Type, raw: &[u8]) -> Option<Value> {
    let int = |at: usize| -> Option<i32> {
        Some(i32::from_be_bytes(raw.get(at..at + 4)?.try_into().ok()?))
    };
    let ndim = usize::try_from(int(0)?).ok()?;
    if ndim == 0 {
        return Some(Value::Array(Vec::new()));
    }
    let dims: Vec<usize> = (0..ndim)
        .map(|d| usize::try_from(int(12 + d * 8)?).ok())
        .collect::<Option<_>>()?;
    let mut at = 12 + ndim * 8;
    let total = dims.iter().try_fold(1usize, |acc, &d| acc.checked_mul(d))?;
    let mut flat = Vec::with_capacity(total.min(raw.len()));
    for _ in 0..total {
        let len = int(at)?;
        at += 4;
        if len < 0 {
            flat.push(Value::Null);
        } else {
            let end = at.checked_add(usize::try_from(len).ok()?)?;
            flat.push(to_value(elem, Some(raw.get(at..end)?)));
            at = end;
        }
    }
    if at != raw.len() {
        return None;
    }
    Some(nest(&dims, &mut flat.into_iter()))
}

fn nest(dims: &[usize], items: &mut impl Iterator<Item = Value>) -> Value {
    match dims.split_first() {
        Some((&n, [])) => Value::Array(items.take(n).collect()),
        Some((&n, rest)) => Value::Array((0..n).map(|_| nest(rest, items)).collect()),
        None => Value::Array(Vec::new()),
    }
}

/// Proleptic Gregorian date of a day count relative to 1970-01-01 (H. Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `YYYY-MM-DD` of a day count since 2000-01-01, plus whether the year is BC.
fn ymd(days: i64) -> (String, bool) {
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    // Astronomical year 0 is 1 BC.
    let (year, bc) = if y <= 0 { (1 - y, true) } else { (y, false) };
    (format!("{year:04}-{m:02}-{d:02}"), bc)
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        _ => {
            let (s, bc) = ymd(days.into());
            if bc {
                format!("{s} BC")
            } else {
                s
            }
        }
    }
}

/// `HH:MM:SS[.ffffff]` with trailing fraction zeros removed, like PostgreSQL prints it.
fn hms(us: i64) -> String {
    let secs = us / 1_000_000;
    format!(
        "{:02}:{:02}:{:02}{}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        frac(us % 1_000_000)
    )
}

fn frac(us: i64) -> String {
    if us == 0 {
        String::new()
    } else {
        format!(".{us:06}").trim_end_matches('0').to_string()
    }
}

fn time_of_day(us: i64) -> Option<String> {
    (0..=USECS_PER_DAY).contains(&us).then(|| hms(us))
}

/// `timestamp` as `YYYY-MM-DD HH:MM:SS[.f]`; `timestamptz` (always UTC on the wire) as
/// ISO 8601 `YYYY-MM-DDTHH:MM:SS[.f]+00:00`.
fn timestamp(us: i64, tz: bool) -> String {
    match us {
        i64::MAX => return "infinity".into(),
        i64::MIN => return "-infinity".into(),
        _ => {}
    }
    let (date, bc) = ymd(us.div_euclid(USECS_PER_DAY));
    let time = hms(us.rem_euclid(USECS_PER_DAY));
    let s = if tz {
        format!("{date}T{time}+00:00")
    } else {
        format!("{date} {time}")
    };
    if bc {
        format!("{s} BC")
    } else {
        s
    }
}

/// ISO 8601 duration, matching PostgreSQL's `intervalstyle = iso_8601`: every field keeps
/// its own sign (`P-1Y-2M3DT-4H`), zero fields are omitted, all zero is `PT0S`.
fn iso_interval(us: i64, days: i32, months: i32) -> String {
    if us == 0 && days == 0 && months == 0 {
        return "PT0S".into();
    }
    let mut s = String::from("P");
    let (years, months) = (months / 12, months % 12);
    for (v, unit) in [
        (i64::from(years), 'Y'),
        (months.into(), 'M'),
        (days.into(), 'D'),
    ] {
        if v != 0 {
            s.push_str(&format!("{v}{unit}"));
        }
    }
    if us != 0 {
        s.push('T');
        let hours = us / 3_600_000_000;
        let minutes = us % 3_600_000_000 / 60_000_000;
        let sec_us = us % 60_000_000;
        if hours != 0 {
            s.push_str(&format!("{hours}H"));
        }
        if minutes != 0 {
            s.push_str(&format!("{minutes}M"));
        }
        if sec_us != 0 {
            let sign = if sec_us < 0 { "-" } else { "" };
            let abs = sec_us.unsigned_abs();
            s.push_str(&format!(
                "{sign}{}{}S",
                abs / 1_000_000,
                frac((abs % 1_000_000) as i64)
            ));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(ty: &Type, raw: &[u8]) -> Value {
        to_value(ty, Some(raw))
    }

    fn other_text(type_name: &str, text: &str) -> Value {
        Value::Other {
            type_name: type_name.into(),
            text: text.into(),
        }
    }

    #[test]
    fn null_and_scalars() {
        assert_eq!(to_value(&Type::INT4, None), Value::Null);
        assert_eq!(v(&Type::BOOL, &[1]), Value::Bool(true));
        assert_eq!(
            v(&Type::INT2, &(-32768i16).to_be_bytes()),
            Value::Int(-32768)
        );
        assert_eq!(
            v(&Type::INT4, &i32::MAX.to_be_bytes()),
            Value::Int(2147483647)
        );
        assert_eq!(
            v(&Type::INT8, &i64::MAX.to_be_bytes()),
            Value::Int(i64::MAX)
        );
        assert_eq!(
            v(&Type::OID, &u32::MAX.to_be_bytes()),
            Value::Int(4294967295)
        );
        assert_eq!(
            v(&Type::FLOAT4, &3.4028235e38f32.to_be_bytes()),
            Value::Float(3.4028235e38)
        );
        assert_eq!(v(&Type::FLOAT8, &0.1f64.to_be_bytes()), Value::Float(0.1));
        assert_eq!(
            v(&Type::FLOAT8, &f64::NAN.to_be_bytes()),
            other_text("float8", "NaN")
        );
        assert_eq!(
            v(&Type::FLOAT4, &f32::NEG_INFINITY.to_be_bytes()),
            other_text("float4", "-Infinity")
        );
        // Wrong width falls back to hex instead of panicking.
        assert_eq!(v(&Type::INT4, &[0, 1]), other_text("int4", "\\x0001"));
    }

    #[test]
    fn numeric_keeps_text_and_special_values() {
        let nan = [0, 0, 0, 0, 0xC0, 0, 0, 0];
        assert_eq!(v(&Type::NUMERIC, &nan), Value::Decimal("NaN".into()));
        let half = [0, 1, 0xff, 0xff, 0x40, 0, 0, 1, 0x13, 0x88];
        assert_eq!(v(&Type::NUMERIC, &half), Value::Decimal("-0.5".into()));
    }

    #[test]
    fn text_json_uuid_bytes() {
        for ty in [Type::TEXT, Type::VARCHAR, Type::BPCHAR, Type::NAME] {
            assert_eq!(v(&ty, "Nguyễn".as_bytes()), Value::Text("Nguyễn".into()));
        }
        let status = Type::new(
            "order_status".into(),
            90_000,
            Kind::Enum(vec!["new".into(), "paid".into()]),
            "public".into(),
        );
        assert_eq!(v(&status, b"paid"), Value::Text("paid".into()));
        assert_eq!(logical(&status), LogicalType::Enum);
        let citext = Type::new("citext".into(), 90_001, Kind::Simple, "public".into());
        assert_eq!(v(&citext, b"AbC"), Value::Text("AbC".into()));
        assert_eq!(logical(&citext), LogicalType::Text);
        assert_eq!(
            v(&Type::JSON, br#"{"a":1}"#),
            Value::Json(r#"{"a":1}"#.into())
        );
        assert_eq!(
            v(&Type::JSONB, b"\x01{\"a\": 1}"),
            Value::Json(r#"{"a": 1}"#.into())
        );
        let id = [
            0x12, 0x3e, 0x45, 0x67, 0xe8, 0x9b, 0x12, 0xd3, 0xa4, 0x56, 0x42, 0x66, 0x14, 0x17,
            0x40, 0x00,
        ];
        assert_eq!(
            v(&Type::UUID, &id),
            Value::Uuid("123e4567-e89b-12d3-a456-426614174000".into())
        );
        assert_eq!(v(&Type::BYTEA, &[0, 0xff]), Value::bytes(&[0, 0xff]));
    }

    #[test]
    fn dates_and_times() {
        let date_of = |d: i32| v(&Type::DATE, &d.to_be_bytes());
        assert_eq!(date_of(0), Value::Date("2000-01-01".into()));
        assert_eq!(date_of(2_921_939), Value::Date("9999-12-31".into()));
        assert_eq!(date_of(-10_957), Value::Date("1970-01-01".into()));
        // '0001-01-01 BC' is astronomical year 0: 730 485 days before 2000-01-01.
        assert_eq!(date_of(-730_485), Value::Date("0001-01-01 BC".into()));
        assert_eq!(date_of(i32::MAX), Value::Date("infinity".into()));
        assert_eq!(date_of(i32::MIN), Value::Date("-infinity".into()));

        let time_of = |us: i64| v(&Type::TIME, &us.to_be_bytes());
        assert_eq!(
            time_of(86_399_999_999),
            Value::Time("23:59:59.999999".into())
        );
        assert_eq!(time_of(36_000_500_000), Value::Time("10:00:00.5".into()));
        assert_eq!(time_of(USECS_PER_DAY), Value::Time("24:00:00".into()));

        let ts = |us: i64| v(&Type::TIMESTAMP, &us.to_be_bytes());
        let tstz = |us: i64| v(&Type::TIMESTAMPTZ, &us.to_be_bytes());
        // 9999-12-31 23:59:59.999999
        let max = 2_921_939 * USECS_PER_DAY + 86_399_999_999;
        assert_eq!(
            ts(max),
            Value::DateTime("9999-12-31 23:59:59.999999".into())
        );
        // 2038-01-19 03:14:07+00 = Unix 2147483647 s.
        let y2038 = (2_147_483_647 - PG_EPOCH_DAYS * 86_400) * 1_000_000;
        assert_eq!(
            tstz(y2038),
            Value::TimestampTz("2038-01-19T03:14:07+00:00".into())
        );
        // Before the PG epoch: negative counts round down to the previous day.
        assert_eq!(ts(-1), Value::DateTime("1999-12-31 23:59:59.999999".into()));
        assert_eq!(
            ts(-730_485 * USECS_PER_DAY),
            Value::DateTime("0001-01-01 00:00:00 BC".into())
        );
        assert_eq!(ts(i64::MAX), Value::DateTime("infinity".into()));
        assert_eq!(tstz(i64::MIN), Value::TimestampTz("-infinity".into()));
    }

    fn interval(us: i64, days: i32, months: i32) -> Value {
        let mut b = us.to_be_bytes().to_vec();
        b.extend_from_slice(&days.to_be_bytes());
        b.extend_from_slice(&months.to_be_bytes());
        v(&Type::INTERVAL, &b)
    }

    #[test]
    fn intervals_are_iso_8601() {
        // '1 year 2 months 3 days 04:05:06.789'
        assert_eq!(
            interval(14_706_789_000, 3, 14),
            other_text("interval", "P1Y2M3DT4H5M6.789S")
        );
        assert_eq!(interval(0, 0, 0), other_text("interval", "PT0S"));
        assert_eq!(interval(0, 30, 0), other_text("interval", "P30D"));
        // '-1 year -2 months +3 days -04:05:06.5'
        assert_eq!(
            interval(-14_706_500_000, 3, -14),
            other_text("interval", "P-1Y-2M3DT-4H-5M-6.5S")
        );
        assert_eq!(interval(-500_000, 0, 0), other_text("interval", "PT-0.5S"));
    }

    /// Binary array of int4 elements (`None` = NULL) with the given dimensions.
    fn int4_array(dims: &[i32], items: &[Option<i32>]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(dims.len() as i32).to_be_bytes());
        b.extend_from_slice(&i32::from(items.iter().any(Option::is_none)).to_be_bytes());
        b.extend_from_slice(&Type::INT4.oid().to_be_bytes());
        for d in dims {
            b.extend_from_slice(&d.to_be_bytes());
            b.extend_from_slice(&1i32.to_be_bytes());
        }
        for item in items {
            match item {
                Some(x) => {
                    b.extend_from_slice(&4i32.to_be_bytes());
                    b.extend_from_slice(&x.to_be_bytes());
                }
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        b
    }

    #[test]
    fn arrays() {
        assert_eq!(
            v(
                &Type::INT4_ARRAY,
                &int4_array(&[3], &[Some(1), None, Some(3)])
            ),
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)])
        );
        assert_eq!(
            v(&Type::INT4_ARRAY, &int4_array(&[], &[])),
            Value::Array(vec![])
        );
        assert_eq!(
            v(
                &Type::INT4_ARRAY,
                &int4_array(&[2, 2], &[Some(1), Some(2), Some(3), Some(4)])
            ),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)]),
            ])
        );
        // Truncated element data falls back to hex.
        let mut bad = int4_array(&[1], &[Some(1)]);
        bad.pop();
        assert!(matches!(v(&Type::INT4_ARRAY, &bad), Value::Other { .. }));
        let col_type = Type::TEXT_ARRAY;
        assert_eq!(type_name(&col_type), "text[]");
        assert_eq!(logical(&col_type), LogicalType::Array);
    }

    #[test]
    fn domains_map_to_their_base_type() {
        let positive = Type::new(
            "positive_int".into(),
            90_002,
            Kind::Domain(Type::INT4),
            "public".into(),
        );
        assert_eq!(v(&positive, &7i32.to_be_bytes()), Value::Int(7));
        assert_eq!(logical(&positive), LogicalType::Integer);
        assert_eq!(type_name(&positive), "positive_int");
    }

    #[test]
    fn other_types_as_text_or_hex() {
        assert_eq!(v(&Type::XML, b"<a>x</a>"), other_text("xml", "<a>x</a>"));
        // inet 127.0.0.1/32 in binary: valid UTF-8, but control bytes -> hex.
        assert_eq!(
            v(&Type::INET, &[2, 32, 0, 4, 127, 0, 0, 1]),
            other_text("inet", "\\x022000047f000001")
        );
        assert_eq!(
            v(&Type::POINT, &[0xff, 0xfe]),
            other_text("point", "\\xfffe")
        );
        assert_eq!(logical(&Type::INET), LogicalType::Other);
        assert_eq!(logical(&Type::INTERVAL), LogicalType::Other);
    }
}
