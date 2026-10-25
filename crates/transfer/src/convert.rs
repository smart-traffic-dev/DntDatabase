//! Source cells to target values (spec §6.2): one [`convert`] per cell, by the target column's
//! logical type (R13). The result is a [`FullValue`] the SQL builder turns into a literal; nothing
//! here formats SQL.
//!
//! A [`CellError`] names the column and a reason, never the cell value (R6).

use std::fmt;

use base64::alphabet;
use base64::engine::general_purpose::GeneralPurposeConfig;
use base64::engine::{DecodePaddingMode, GeneralPurpose};
use base64::Engine as _;
use dnt_core::meta::{ColumnDetail, IdentityKind};
use dnt_core::server::DbKind;
use dnt_core::sql::build::ColumnTarget;
use dnt_core::value::{FullValue, LogicalType, Value};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::read::Cell;

/// A column of the import target as conversion and mapping need it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TargetColumn {
    pub name: String,
    pub db_type: String,
    pub logical: LogicalType,
    pub nullable: bool,
    /// A default, `AUTO_INCREMENT` or an identity: the column may be left out of the `INSERT`.
    pub has_default: bool,
    pub auto_increment: bool,
    pub identity: Option<IdentityKind>,
    pub generated: bool,
    pub enum_values: Vec<String>,
    /// `char(n)` / `varchar(n)` / `character(n)` / `character varying(n)`.
    pub max_chars: Option<u32>,
    pub collation: Option<String>,
    /// MySQL `… UNSIGNED` integer: negative values are refused.
    #[serde(default)]
    pub unsigned: bool,
}

impl TargetColumn {
    /// From `table_detail`'s column and the logical type the driver reports for it (R13).
    pub fn from_detail(cd: &ColumnDetail, logical: LogicalType, kind: DbKind) -> TargetColumn {
        let identity = cd.identity_kind.or_else(|| {
            cd.identity.as_deref().map(|s| {
                if s.eq_ignore_ascii_case("always") {
                    IdentityKind::Always
                } else {
                    IdentityKind::ByDefault
                }
            })
        });
        let unsigned = kind == DbKind::MySql
            && (cd.unsigned || cd.db_type.to_ascii_lowercase().contains("unsigned"));
        TargetColumn {
            name: cd.name.clone(),
            db_type: cd.db_type.clone(),
            logical,
            nullable: cd.nullable,
            has_default: cd.default.is_some() || cd.auto_increment || identity.is_some(),
            auto_increment: cd.auto_increment,
            identity,
            generated: cd.generated.is_some(),
            enum_values: cd.enum_values.clone(),
            max_chars: max_chars(&cd.db_type),
            collation: cd.collation.clone(),
            unsigned,
        }
    }

    /// The column as the SQL builder sees it.
    pub fn column_target(&self) -> ColumnTarget {
        ColumnTarget {
            name: self.name.clone(),
            db_type: self.db_type.clone(),
            logical: self.logical,
            collation: self.collation.clone(),
        }
    }

    /// `timestamp with time zone` / `timestamptz` / `time with time zone` / `timetz`.
    fn has_zone(&self) -> bool {
        let t = self.db_type.to_ascii_lowercase();
        t.contains("with time zone") || t.starts_with("timestamptz") || t.starts_with("timetz")
    }
}

/// `n` of `char(n)`, `varchar(n)`, `character(n)`, `character varying(n)` (any case).
fn max_chars(db_type: &str) -> Option<u32> {
    let t = db_type.trim().to_ascii_lowercase();
    let rest = ["varchar(", "char(", "character varying(", "character("]
        .iter()
        .find_map(|p| t.strip_prefix(p))?;
    let (n, _) = rest.split_once(')')?;
    n.trim().parse().ok()
}

/// What an empty text cell becomes in a text column (other columns always get NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum EmptyAs {
    #[default]
    Null,
    EmptyString,
}

/// How binary columns read their text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum BinaryEncoding {
    /// `0x…` or bare hex digits.
    #[default]
    Hex,
    /// Standard alphabet, padding optional.
    Base64,
    /// The UTF-8 bytes of the text.
    Utf8Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, Type)]
pub struct ConvertOptions {
    pub empty: EmptyAs,
    /// A text cell equal to this (after trimming when `trim`) is NULL, e.g. `\N` or `NULL`.
    pub null_text: Option<String>,
    /// Trim spaces around text cells first.
    pub trim: bool,
    pub binary: BinaryEncoding,
}

/// Why a cell was refused. Never holds the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellReason {
    /// NULL (or an empty cell) into a NOT NULL column without default.
    NotNull,
    /// Not a value of the type: `expected` says what is ("an integer", "a date (YYYY-MM-DD)").
    Invalid {
        expected: &'static str,
    },
    OutOfRange,
    /// A negative number into an unsigned column.
    Negative,
    TooLong {
        max: u32,
    },
    NotInEnum,
    /// Arrays and other types the import cannot write (PG text with a cast is a follow-up).
    Unsupported,
}

impl fmt::Display for CellReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellReason::NotNull => {
                f.write_str("NULL is not allowed (the column is NOT NULL and has no default)")
            }
            CellReason::Invalid { expected } => write!(f, "not {expected}"),
            CellReason::OutOfRange => f.write_str("the number is out of range"),
            CellReason::Negative => f.write_str("a negative number for an unsigned column"),
            CellReason::TooLong { max } => write!(f, "longer than {max} characters"),
            CellReason::NotInEnum => f.write_str("not one of the column's values"),
            CellReason::Unsupported => f.write_str("the column's type cannot be imported"),
        }
    }
}

/// A refused cell: the target column and the reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("column {column}: {reason}")]
pub struct CellError {
    pub column: String,
    pub reason: CellReason,
}

/// `cell` as a value of column `col`.
///
/// Text cells are trimmed (`opts.trim`), then a `null_text` match is NULL and an empty cell is
/// [`EmptyAs`] in a text column, NULL elsewhere. Numbers, JSON and date cells keep their text
/// verbatim. NULL into a NOT NULL column without default is refused.
pub fn convert(
    cell: &Cell,
    col: &TargetColumn,
    opts: &ConvertOptions,
    kind: DbKind,
) -> Result<FullValue, CellError> {
    let fail = |reason| CellError {
        column: col.name.clone(),
        reason,
    };
    let value = match cell {
        Cell::Null => None,
        Cell::Text(s) => {
            let s = if opts.trim { s.trim() } else { s.as_str() };
            if opts.null_text.as_deref() == Some(s) {
                None
            } else if s.is_empty() {
                match (col.logical, opts.empty) {
                    (LogicalType::Text, EmptyAs::EmptyString) => {
                        Some(FullValue::try_from_value(Value::Text(String::new())).expect("plain"))
                    }
                    _ => None,
                }
            } else {
                Some(from_text(s, col, opts, kind).map_err(fail)?)
            }
        }
        Cell::Bool(b) => Some(from_bool(*b, col, opts, kind).map_err(fail)?),
        Cell::Number(s) | Cell::Json(s) | Cell::DateTime(s) => {
            Some(from_text(s, col, opts, kind).map_err(fail)?)
        }
    };
    match value {
        Some(v) => Ok(v),
        None if !col.nullable && !col.has_default => Err(fail(CellReason::NotNull)),
        None => Ok(plain(Value::Null)),
    }
}

fn plain(v: Value) -> FullValue {
    FullValue::try_from_value(v).expect("a value without bytes is complete")
}

fn from_bool(
    b: bool,
    col: &TargetColumn,
    opts: &ConvertOptions,
    kind: DbKind,
) -> Result<FullValue, CellReason> {
    match col.logical {
        LogicalType::Bool => Ok(plain(Value::Bool(b))),
        LogicalType::Integer => Ok(plain(Value::Int(i64::from(b)))),
        _ => from_text(if b { "true" } else { "false" }, col, opts, kind),
    }
}

fn from_text(
    s: &str,
    col: &TargetColumn,
    opts: &ConvertOptions,
    kind: DbKind,
) -> Result<FullValue, CellReason> {
    use LogicalType as L;
    let pg = kind == DbKind::Postgres;
    let invalid = |expected| Err(CellReason::Invalid { expected });
    let v = match col.logical {
        L::Text => {
            if let Some(max) = col.max_chars {
                if s.chars().count() > max as usize {
                    return Err(CellReason::TooLong { max });
                }
            }
            Value::Text(s.into())
        }
        L::Enum => {
            if !col.enum_values.iter().any(|e| e == s) {
                return Err(CellReason::NotInEnum);
            }
            Value::Text(s.into())
        }
        L::Integer => integer(s, col.unsigned)?,
        L::Decimal => {
            if is_decimal(s) || (pg && matches!(s, "NaN" | "Infinity" | "-Infinity")) {
                Value::Decimal(s.into())
            } else {
                return invalid("a decimal number");
            }
        }
        L::Float => Value::Float(float(s)?),
        L::Bool => match s.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Value::Bool(true),
            "false" | "f" | "no" | "n" | "0" => Value::Bool(false),
            _ => return invalid("true or false"),
        },
        L::Date if is_date(s) || (pg && is_pg_infinity(s)) => Value::Date(s.into()),
        L::Date => return invalid("a date (YYYY-MM-DD)"),
        L::Time => {
            let body = if col.has_zone() { strip_zone(s) } else { s };
            if !is_time(body) {
                return invalid("a time (HH:MM:SS)");
            }
            Value::Time(s.into())
        }
        L::DateTime => {
            let zone = col.has_zone();
            let normal = if pg && is_pg_infinity(s) {
                s.to_string()
            } else {
                datetime(s, zone).ok_or(CellReason::Invalid {
                    expected: "a date and time (YYYY-MM-DD HH:MM:SS)",
                })?
            };
            if zone {
                Value::TimestampTz(normal)
            } else {
                Value::DateTime(normal)
            }
        }
        L::Json => {
            if serde_json::from_str::<&serde_json::value::RawValue>(s).is_err() {
                return invalid("valid JSON");
            }
            Value::Json(s.into())
        }
        L::Uuid if is_uuid(s) => Value::Uuid(s.into()),
        L::Uuid => return invalid("a UUID"),
        L::Binary => return binary(s, opts.binary).map(FullValue::bytes),
        L::Array | L::Other => return Err(CellReason::Unsupported),
    };
    Ok(plain(v))
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `-?\d+` within i64, else u64 (non-negative), else out of range.
fn integer(s: &str, unsigned: bool) -> Result<Value, CellReason> {
    let neg = s.starts_with('-');
    if !digits(s.strip_prefix('-').unwrap_or(s)) {
        return Err(CellReason::Invalid {
            expected: "an integer",
        });
    }
    let v = match s.parse::<i64>() {
        Ok(i) => Value::Int(i),
        Err(_) if !neg => s
            .parse::<u64>()
            .map(Value::UInt)
            .map_err(|_| CellReason::OutOfRange)?,
        Err(_) => return Err(CellReason::OutOfRange),
    };
    if unsigned && matches!(v, Value::Int(i) if i < 0) {
        return Err(CellReason::Negative);
    }
    Ok(v)
}

/// `-?\d+(\.\d+)?([eE][-+]?\d+)?` (the builder's decimal syntax).
fn is_decimal(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    digits(int)
        && frac.is_none_or(digits)
        && exp.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

/// A decimal number, or NaN / ±Infinity (any case, `inf` too). A finite text that overflows
/// f64 is out of range. NaN is kept: MySQL refuses it when the literal is built.
fn float(s: &str) -> Result<f64, CellReason> {
    match s.to_ascii_lowercase().as_str() {
        "nan" => return Ok(f64::NAN),
        "infinity" | "inf" => return Ok(f64::INFINITY),
        "-infinity" | "-inf" => return Ok(f64::NEG_INFINITY),
        _ => {}
    }
    if !is_decimal(s) {
        return Err(CellReason::Invalid {
            expected: "a number",
        });
    }
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(f),
        _ => Err(CellReason::OutOfRange),
    }
}

fn is_pg_infinity(s: &str) -> bool {
    matches!(s, "infinity" | "-infinity")
}

fn two_digits_in(s: &str, lo: u32, hi: u32) -> bool {
    s.len() == 2 && digits(s) && s.parse::<u32>().is_ok_and(|n| (lo..=hi).contains(&n))
}

/// `YYYY-MM-DD` (month 01–12, day 01–31; the server checks the calendar).
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && digits(&s[..4])
        && two_digits_in(&s[5..7], 1, 12)
        && two_digits_in(&s[8..10], 1, 31)
}

/// `HH:MM[:SS[.f{1,6}]]` (HH 00–23).
fn is_time(s: &str) -> bool {
    let (hms, frac) = match s.split_once('.') {
        Some((a, f)) => (a, Some(f)),
        None => (s, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let ok_parts = match parts.as_slice() {
        [h, m] => frac.is_none() && two_digits_in(h, 0, 23) && two_digits_in(m, 0, 59),
        [h, m, sec] => {
            two_digits_in(h, 0, 23) && two_digits_in(m, 0, 59) && two_digits_in(sec, 0, 60)
        }
        _ => false,
    };
    ok_parts && frac.is_none_or(|f| digits(f) && f.len() <= 6)
}

/// `s` without a trailing `Z` / `±HH[:MM]` / `±HHMM` zone.
fn strip_zone(s: &str) -> &str {
    if let Some(r) = s.strip_suffix(['Z', 'z']) {
        return r;
    }
    if let Some(i) = s.rfind(['+', '-']) {
        let zone = &s[i + 1..];
        let ok = match zone.split_once(':') {
            Some((h, m)) => two_digits_in(h, 0, 23) && two_digits_in(m, 0, 59),
            None => digits(zone) && (zone.len() == 2 || zone.len() == 4),
        };
        if ok {
            return &s[..i];
        }
    }
    s
}

/// `YYYY-MM-DD[( |T)HH:MM[:SS[.f{1,6}]]]` with `T` written as a space; an offset only when
/// `zone` (kept verbatim).
fn datetime(s: &str, zone: bool) -> Option<String> {
    if s.len() == 10 {
        return is_date(s).then(|| s.to_string());
    }
    let date = s.get(..10)?;
    let sep = s.as_bytes().get(10)?;
    if !is_date(date) || !matches!(sep, b' ' | b'T' | b't') {
        return None;
    }
    // Byte 10 is ASCII, so 11 is a char boundary.
    let time = &s[11..];
    let body = if zone { strip_zone(time) } else { time };
    is_time(body).then(|| format!("{date} {time}"))
}

/// `8-4-4-4-12` hex digits.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn binary(s: &str, enc: BinaryEncoding) -> Result<Vec<u8>, CellReason> {
    match enc {
        BinaryEncoding::Hex => {
            let body = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(s);
            hex::decode(body).map_err(|_| CellReason::Invalid {
                expected: "hexadecimal bytes",
            })
        }
        BinaryEncoding::Base64 => BASE64.decode(s).map_err(|_| CellReason::Invalid {
            expected: "base64 bytes",
        }),
        BinaryEncoding::Utf8Text => Ok(s.as_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_stripping() {
        assert_eq!(strip_zone("03:04:05+07:00"), "03:04:05");
        assert_eq!(strip_zone("03:04:05-0330"), "03:04:05");
        assert_eq!(strip_zone("03:04:05Z"), "03:04:05");
        assert_eq!(strip_zone("03:04:05"), "03:04:05");
    }

    #[test]
    fn datetime_shapes() {
        assert_eq!(datetime("2024-01-02", false).as_deref(), Some("2024-01-02"));
        assert_eq!(datetime("2024-01-02X03:04", false), None);
        assert_eq!(datetime("2024-01-02 ", false), None);
        assert_eq!(datetime("2024-13-02 03:04", false), None);
    }
}
