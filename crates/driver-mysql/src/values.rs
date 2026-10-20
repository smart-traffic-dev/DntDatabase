//! Column metadata and value mapping from `mysql_async` to `dnt_core::value`.
//!
//! Queries run over the text protocol, so most values arrive as `Value::Bytes` and are
//! interpreted by column type; binary-protocol values (`Int`, `Date`, …) are handled too.
//! Dates and times are formatted from their numeric parts, never through a date library,
//! so zero dates (`0000-00-00`) and zero components survive unchanged.

use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::Column;
use mysql_async::Value as My;

/// Character set number MySQL uses for binary strings.
const BINARY_CHARSET: u16 = 63;

pub(crate) fn column_meta(col: &Column) -> ColumnMeta {
    let flags = col.flags();
    let org_table = col.org_table_str();
    let org_name = col.org_name_str();
    ColumnMeta {
        name: col.name_str().into_owned(),
        db_type: type_name(col).into(),
        logical: logical(col),
        nullable: Some(!flags.contains(ColumnFlags::NOT_NULL_FLAG)),
        origin_table: (!org_table.is_empty()).then(|| ObjectRef {
            database: col.schema_str().into_owned(),
            schema: None,
            name: org_table.into_owned(),
            kind: ObjectKind::Table,
        }),
        origin_column: (!org_name.is_empty()).then(|| org_name.into_owned()),
    }
}

/// The `LogicalType` a column maps to (see the type table in the task brief).
fn logical(col: &Column) -> LogicalType {
    use ColumnType::*;
    if is_enum_or_set(col) {
        return LogicalType::Enum;
    }
    match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => LogicalType::Integer,
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => LogicalType::Float,
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => LogicalType::Decimal,
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => LogicalType::Date,
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => LogicalType::DateTime,
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => LogicalType::Time,
        MYSQL_TYPE_JSON => LogicalType::Json,
        MYSQL_TYPE_BIT => LogicalType::Binary,
        MYSQL_TYPE_GEOMETRY => LogicalType::Other,
        t if is_string_type(t) => {
            if col.character_set() == BINARY_CHARSET {
                LogicalType::Binary
            } else {
                LogicalType::Text
            }
        }
        _ => LogicalType::Other,
    }
}

fn is_string_type(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_STRING
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
    )
}

fn is_enum_or_set(col: &Column) -> bool {
    matches!(
        col.column_type(),
        ColumnType::MYSQL_TYPE_ENUM | ColumnType::MYSQL_TYPE_SET
    ) || col
        .flags()
        .intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG)
}

/// Approximate SQL type name from the protocol metadata (exact names come from the introspector).
fn type_name(col: &Column) -> &'static str {
    use ColumnType::*;
    let flags = col.flags();
    let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = col.character_set() == BINARY_CHARSET;
    if flags.contains(ColumnFlags::ENUM_FLAG) || col.column_type() == MYSQL_TYPE_ENUM {
        return "enum";
    }
    if flags.contains(ColumnFlags::SET_FLAG) || col.column_type() == MYSQL_TYPE_SET {
        return "set";
    }
    match (col.column_type(), unsigned) {
        (MYSQL_TYPE_TINY, false) => "tinyint",
        (MYSQL_TYPE_TINY, true) => "tinyint unsigned",
        (MYSQL_TYPE_SHORT, false) => "smallint",
        (MYSQL_TYPE_SHORT, true) => "smallint unsigned",
        (MYSQL_TYPE_INT24, false) => "mediumint",
        (MYSQL_TYPE_INT24, true) => "mediumint unsigned",
        (MYSQL_TYPE_LONG, false) => "int",
        (MYSQL_TYPE_LONG, true) => "int unsigned",
        (MYSQL_TYPE_LONGLONG, false) => "bigint",
        (MYSQL_TYPE_LONGLONG, true) => "bigint unsigned",
        (MYSQL_TYPE_YEAR, _) => "year",
        (MYSQL_TYPE_FLOAT, _) => "float",
        (MYSQL_TYPE_DOUBLE, _) => "double",
        (MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL, _) => "decimal",
        (MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE, _) => "date",
        (MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2, _) => "datetime",
        (MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2, _) => "timestamp",
        (MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2, _) => "time",
        (MYSQL_TYPE_JSON, _) => "json",
        (MYSQL_TYPE_BIT, _) => "bit",
        (MYSQL_TYPE_GEOMETRY, _) => "geometry",
        (MYSQL_TYPE_STRING, _) if binary => "binary",
        (MYSQL_TYPE_STRING, _) => "char",
        (MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR, _) if binary => "varbinary",
        (MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR, _) => "varchar",
        (t, _) if is_string_type(t) && binary => "blob",
        (t, _) if is_string_type(t) => "text",
        (MYSQL_TYPE_NULL, _) => "null",
        _ => "unknown",
    }
}

/// [`to_value`], with every byte of a binary value kept when `full_bytes` (export).
pub(crate) fn to_value_full(v: My, col: &Column, full_bytes: bool) -> Value {
    match v {
        My::Bytes(b) if full_bytes && logical(col) == LogicalType::Binary => Value::Bytes {
            len: b.len() as u64,
            preview: b,
        },
        v => to_value(v, col),
    }
}

/// [`to_value`] as a complete value ([`CellMode::Full`](dnt_core::driver::CellMode)): a
/// binary value keeps every byte (the export path of [`to_value_full`]).
pub(crate) fn to_full_value(v: My, col: &Column) -> FullValue {
    // `to_value` builds `Bytes` only for binary columns, which `full_bytes` keeps whole,
    // so the result is always complete.
    FullValue::try_from_value(to_value_full(v, col, true))
        .expect("binary values are read whole with full_bytes")
}

pub(crate) fn to_value(v: My, col: &Column) -> Value {
    match v {
        My::NULL => Value::Null,
        My::Bytes(b) => from_bytes(b, col),
        My::Int(i) => Value::Int(i),
        My::UInt(u) if is_unsigned(col) => Value::UInt(u),
        My::UInt(u) => i64::try_from(u).map_or(Value::UInt(u), Value::Int),
        // Widen through the shortest decimal form so 3.4e38f32 does not gain noise digits.
        My::Float(f) => Value::Float(f.to_string().parse().unwrap_or(f as f64)),
        My::Double(d) => Value::Float(d),
        My::Date(y, mo, d, h, mi, s, us) => match logical(col) {
            LogicalType::Date => Value::Date(fmt_date(y, mo, d)),
            _ => Value::DateTime(fmt_datetime(y, mo, d, h, mi, s, us)),
        },
        My::Time(neg, days, h, mi, s, us) => Value::Time(fmt_time(
            neg,
            u64::from(days) * 24 + u64::from(h),
            mi,
            s,
            us,
        )),
    }
}

/// Interprets a text-protocol value by its column type.
fn from_bytes(b: Vec<u8>, col: &Column) -> Value {
    let text = || String::from_utf8_lossy(&b).into_owned();
    match logical(col) {
        LogicalType::Integer if is_unsigned(col) => text()
            .parse::<u64>()
            .map_or_else(|_| Value::Text(text()), Value::UInt),
        LogicalType::Integer => text()
            .parse::<i64>()
            .map_or_else(|_| Value::Text(text()), Value::Int),
        LogicalType::Float => text()
            .parse::<f64>()
            .map_or_else(|_| Value::Text(text()), Value::Float),
        LogicalType::Decimal => Value::Decimal(text()),
        LogicalType::Date => {
            let s = text();
            match parse_datetime(&s) {
                Some((y, mo, d, ..)) => Value::Date(fmt_date(y, mo, d)),
                None => Value::Date(s),
            }
        }
        LogicalType::DateTime => {
            let s = text();
            match parse_datetime(&s) {
                Some((y, mo, d, h, mi, sec, us)) => {
                    Value::DateTime(fmt_datetime(y, mo, d, h, mi, sec, us))
                }
                None => Value::DateTime(s),
            }
        }
        LogicalType::Time => {
            let s = text();
            match parse_time(&s) {
                Some((neg, h, mi, sec, us)) => Value::Time(fmt_time(neg, h, mi, sec, us)),
                None => Value::Time(s),
            }
        }
        LogicalType::Json => Value::Json(text()),
        LogicalType::Text | LogicalType::Enum => Value::Text(text()),
        LogicalType::Binary => Value::bytes(&b),
        _ if col.column_type() == ColumnType::MYSQL_TYPE_GEOMETRY => Value::Other {
            type_name: "geometry".into(),
            text: hex(&b),
        },
        _ => Value::Other {
            type_name: type_name(col).into(),
            text: text(),
        },
    }
}

/// A value as plain text, independent of column type (`None` for SQL NULL). Used for
/// metadata rows, whose cells are names, numbers and SQL fragments.
pub(crate) fn value_text(v: My) -> Option<String> {
    match v {
        My::NULL => None,
        My::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        other => Some(other.as_sql(true)),
    }
}

/// Unsigned integer column. YEAR carries `UNSIGNED_FLAG` on the wire but maps to `Int`.
fn is_unsigned(col: &Column) -> bool {
    col.flags().contains(ColumnFlags::UNSIGNED_FLAG)
        && col.column_type() != ColumnType::MYSQL_TYPE_YEAR
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn fmt_frac(us: u32) -> String {
    if us == 0 {
        String::new()
    } else {
        format!(".{us:06}")
    }
}

fn fmt_date(y: u16, mo: u8, d: u8) -> String {
    format!("{y:04}-{mo:02}-{d:02}")
}

fn fmt_datetime(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> String {
    format!(
        "{} {h:02}:{mi:02}:{s:02}{}",
        fmt_date(y, mo, d),
        fmt_frac(us)
    )
}

fn fmt_time(neg: bool, hours: u64, mi: u8, s: u8, us: u32) -> String {
    let sign = if neg { "-" } else { "" };
    format!("{sign}{hours:02}:{mi:02}:{s:02}{}", fmt_frac(us))
}

/// `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` into numeric parts; `None` for anything else.
fn parse_datetime(s: &str) -> Option<(u16, u8, u8, u8, u8, u8, u32)> {
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let y = parts.next()?.parse().ok()?;
    let mo = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let (h, mi, sec, us) = match time {
        Some(t) => {
            let (neg, h, mi, sec, us) = parse_time(t)?;
            if neg {
                return None;
            }
            (u8::try_from(h).ok()?, mi, sec, us)
        }
        None => (0, 0, 0, 0),
    };
    Some((y, mo, d, h, mi, sec, us))
}

/// `[-]H+:MM:SS[.f{1,6}]` into (negative, hours, minutes, seconds, microseconds).
fn parse_time(s: &str) -> Option<(bool, u64, u8, u8, u32)> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (hms, frac) = match s.split_once('.') {
        Some((a, f)) => (a, Some(f)),
        None => (s, None),
    };
    let mut parts = hms.split(':');
    let h = parts.next()?.parse().ok()?;
    let mi = parts.next()?.parse().ok()?;
    let sec = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let us = match frac {
        Some(f) if !f.is_empty() && f.len() <= 6 && f.bytes().all(|c| c.is_ascii_digit()) => {
            format!("{f:0<6}").parse().ok()?
        }
        Some(_) => return None,
        None => 0,
    };
    Some((neg, h, mi, sec, us))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(t: ColumnType) -> Column {
        Column::new(t)
    }

    #[test]
    fn zero_datetime_is_formatted_from_parts() {
        let v = to_value(
            My::Date(0, 0, 0, 0, 0, 0, 0),
            &col(ColumnType::MYSQL_TYPE_DATETIME),
        );
        assert_eq!(v, Value::DateTime("0000-00-00 00:00:00".into()));
    }

    #[test]
    fn zero_month_date_keeps_its_parts() {
        let v = to_value(
            My::Date(2024, 0, 10, 0, 0, 0, 0),
            &col(ColumnType::MYSQL_TYPE_DATE),
        );
        assert_eq!(v, Value::Date("2024-00-10".into()));
    }

    #[test]
    fn negative_time_with_days_and_micros() {
        let v = to_value(
            My::Time(true, 1, 2, 3, 4, 5),
            &col(ColumnType::MYSQL_TYPE_TIME),
        );
        assert_eq!(v, Value::Time("-26:03:04.000005".into()));
    }

    #[test]
    fn unsigned_bigint_max_stays_uint() {
        let c = col(ColumnType::MYSQL_TYPE_LONGLONG).with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(to_value(My::UInt(u64::MAX), &c), Value::UInt(u64::MAX));
        // Text protocol: the same value arrives as its decimal text.
        let text = My::Bytes(u64::MAX.to_string().into_bytes());
        assert_eq!(to_value(text, &c), Value::UInt(u64::MAX));
        assert_eq!(column_meta(&c).logical, LogicalType::Integer);
    }

    fn bytes(s: &str) -> My {
        My::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn text_protocol_temporals_drop_zero_fractions_only() {
        let dt = col(ColumnType::MYSQL_TYPE_DATETIME);
        assert_eq!(
            to_value(bytes("2024-01-01 10:00:00.000000"), &dt),
            Value::DateTime("2024-01-01 10:00:00".into())
        );
        assert_eq!(
            to_value(bytes("9999-12-31 23:59:59.999999"), &dt),
            Value::DateTime("9999-12-31 23:59:59.999999".into())
        );
        assert_eq!(
            to_value(bytes("2020-00-00 00:00:00.5"), &dt),
            Value::DateTime("2020-00-00 00:00:00.500000".into())
        );
        let t = col(ColumnType::MYSQL_TYPE_TIME);
        assert_eq!(
            to_value(bytes("-838:59:59.000000"), &t),
            Value::Time("-838:59:59".into())
        );
        assert_eq!(
            to_value(bytes("00:00:01.000100"), &t),
            Value::Time("00:00:01.000100".into())
        );
        let d = col(ColumnType::MYSQL_TYPE_DATE);
        assert_eq!(
            to_value(bytes("0000-00-00"), &d),
            Value::Date("0000-00-00".into())
        );
        // Unparseable text is passed through rather than failing.
        assert_eq!(to_value(bytes("junk"), &d), Value::Date("junk".into()));
    }

    #[test]
    fn strings_by_charset_and_flags() {
        let text = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(45);
        assert_eq!(
            to_value(bytes("Nguyễn"), &text),
            Value::Text("Nguyễn".into())
        );
        let bin = col(ColumnType::MYSQL_TYPE_BLOB).with_character_set(BINARY_CHARSET);
        assert_eq!(
            to_value(My::Bytes(vec![7; 100]), &bin),
            Value::bytes(&[7; 100])
        );
        assert_eq!(column_meta(&bin).logical, LogicalType::Binary);
        let en = col(ColumnType::MYSQL_TYPE_STRING)
            .with_character_set(45)
            .with_flags(ColumnFlags::ENUM_FLAG);
        assert_eq!(to_value(bytes("large"), &en), Value::Text("large".into()));
        assert_eq!(column_meta(&en).logical, LogicalType::Enum);
        // Export keeps every byte.
        assert_eq!(
            to_value_full(My::Bytes(vec![7; 100]), &bin, true),
            Value::Bytes {
                len: 100,
                preview: vec![7; 100]
            }
        );
        assert_eq!(
            to_value_full(bytes("Nguyễn"), &text, true),
            Value::Text("Nguyễn".into())
        );
        let bit = col(ColumnType::MYSQL_TYPE_BIT).with_character_set(BINARY_CHARSET);
        assert_eq!(to_value(My::Bytes(vec![0xAA]), &bit), Value::bytes(&[0xAA]));
        let geo = col(ColumnType::MYSQL_TYPE_GEOMETRY).with_character_set(BINARY_CHARSET);
        assert_eq!(
            to_value(My::Bytes(vec![0, 0x1F]), &geo),
            Value::Other {
                type_name: "geometry".into(),
                text: "001f".into()
            }
        );
    }

    #[test]
    fn to_full_value_keeps_all_bytes() {
        let bin = col(ColumnType::MYSQL_TYPE_BLOB).with_character_set(BINARY_CHARSET);
        let f = to_full_value(My::Bytes(vec![7; 100]), &bin);
        assert_eq!(f.as_bytes().unwrap().len(), 100);
        // Everything else decodes exactly like `to_value`.
        let text = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(45);
        assert_eq!(
            to_full_value(bytes("Nguyễn"), &text).as_value(),
            Some(&Value::Text("Nguyễn".into()))
        );
        assert!(to_full_value(My::NULL, &bin).is_null());
        let bit = col(ColumnType::MYSQL_TYPE_BIT).with_character_set(BINARY_CHARSET);
        assert_eq!(
            to_full_value(My::Bytes(vec![0xAA]), &bit).as_bytes(),
            Some(&[0xAA][..])
        );
        let geo = col(ColumnType::MYSQL_TYPE_GEOMETRY).with_character_set(BINARY_CHARSET);
        assert_eq!(
            to_full_value(My::Bytes(vec![0, 0x1F]), &geo).to_display(),
            to_value(My::Bytes(vec![0, 0x1F]), &geo)
        );
    }

    #[test]
    fn numbers_keep_full_precision() {
        let tiny1 = col(ColumnType::MYSQL_TYPE_TINY)
            .with_column_length(1)
            .with_flags(ColumnFlags::NUM_FLAG);
        assert_eq!(to_value(bytes("1"), &tiny1), Value::Int(1));
        let dec = col(ColumnType::MYSQL_TYPE_NEWDECIMAL);
        assert_eq!(
            to_value(bytes("123456789012345678901234567890.1234567890"), &dec),
            Value::Decimal("123456789012345678901234567890.1234567890".into())
        );
        let year = col(ColumnType::MYSQL_TYPE_YEAR)
            .with_flags(ColumnFlags::UNSIGNED_FLAG | ColumnFlags::ZEROFILL_FLAG);
        assert_eq!(to_value(bytes("2155"), &year), Value::Int(2155));
        assert_eq!(to_value(My::UInt(2155), &year), Value::Int(2155));
        let f = col(ColumnType::MYSQL_TYPE_FLOAT);
        assert_eq!(
            to_value(My::Float(3.40282e38), &f),
            Value::Float(3.40282e38)
        );
        assert_eq!(to_value(bytes("3.40282e38"), &f), Value::Float(3.40282e38));
    }
}
