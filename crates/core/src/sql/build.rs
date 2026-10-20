//! SQL text built from values: the one place a cell value becomes SQL (spec §4.1).
//!
//! v1 renders literals instead of bind parameters, so the statement a preview shows is
//! the statement sent. Every value goes through [`literal`]; nothing else formats a value
//! into SQL.

use crate::sql::Dialect;
use crate::value::{FullValue, LogicalType, Value};

/// The column a value is written to or compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnTarget {
    pub name: String,
    /// The column's type as introspection reports it (PG: the cast target, e.g. `uuid`).
    pub db_type: String,
    pub logical: LogicalType,
    pub collation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("unsupported value for {column} ({db_type})")]
    Unsupported { column: String, db_type: String },
    #[error("invalid value for {column}: {reason}")]
    Invalid { column: String, reason: String },
    #[error("value is not complete")]
    NotFull,
}

/// `v` as a SQL literal for column `col` under dialect `d`.
///
/// - `NULL` for a null of any column.
/// - Strings: MySQL `_utf8mb4'…'`, backslash-escaped (`\\`, `\'`, `\0`) unless
///   `NO_BACKSLASH_ESCAPES`, where only `'` is doubled and NUL is refused. PostgreSQL
///   `'…'` with `'` doubled (never `E'…'`, so it relies on `standard_conforming_strings`
///   being on), NUL refused, and a `::<db_type>` cast unless the column is text.
/// - Numbers verbatim (a decimal must look like one); float NaN/±Infinity only on PG.
/// - Booleans: MySQL `1`/`0`, PG `TRUE`/`FALSE`. Binary: MySQL `X'…'`, PG `'\x…'::bytea`.
/// - MySQL JSON: `CAST(_utf8mb4'…' AS JSON)`.
/// - Arrays, `Value::Other` and columns of logical type `Other` (spatial, range,
///   composite, …) are [`BuildError::Unsupported`].
pub fn literal(v: &FullValue, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    if v.is_null() {
        return Ok("NULL".into());
    }
    if let Some(b) = v.as_bytes() {
        return Ok(binary(b, d));
    }
    let v = v.as_value().ok_or(BuildError::NotFull)?;
    if col.logical == LogicalType::Other {
        return Err(unsupported(col));
    }
    match v {
        Value::Null => Ok("NULL".into()),
        Value::Bool(b) => Ok(match (d, b) {
            (Dialect::MySql(_), true) => "1",
            (Dialect::MySql(_), false) => "0",
            (Dialect::Postgres, true) => "TRUE",
            (Dialect::Postgres, false) => "FALSE",
        }
        .into()),
        Value::Int(i) => Ok(i.to_string()),
        Value::UInt(u) => Ok(u.to_string()),
        Value::Float(f) => float(*f, col, d),
        Value::Decimal(s) => decimal(s, col, d),
        Value::Text(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s) => string(s, col, d),
        Value::Json(s) => match d {
            Dialect::MySql(_) => Ok(format!("CAST({} AS JSON)", mysql_string(s, col, d)?)),
            Dialect::Postgres => string(s, col, d),
        },
        Value::Bytes { len, preview } if preview.len() as u64 >= *len => Ok(binary(preview, d)),
        Value::Bytes { .. } => Err(BuildError::NotFull),
        Value::Array(_) | Value::Other { .. } => Err(unsupported(col)),
    }
}

fn unsupported(col: &ColumnTarget) -> BuildError {
    BuildError::Unsupported {
        column: col.name.clone(),
        db_type: col.db_type.clone(),
    }
}

fn invalid(col: &ColumnTarget, reason: &str) -> BuildError {
    BuildError::Invalid {
        column: col.name.clone(),
        reason: reason.into(),
    }
}

fn binary(b: &[u8], d: Dialect) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(b.len() * 2);
    for &x in b {
        hex.push(DIGITS[usize::from(x >> 4)] as char);
        hex.push(DIGITS[usize::from(x & 0xf)] as char);
    }
    match d {
        Dialect::MySql(_) => format!("X'{hex}'"),
        Dialect::Postgres => format!("'\\x{hex}'::bytea"),
    }
}

fn float(f: f64, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    if f.is_finite() {
        // `{:?}` is the shortest text that reads back as the same f64, with an exponent
        // for very large / small values: a valid numeric literal in both dialects.
        return Ok(format!("{f:?}"));
    }
    let special = if f.is_nan() {
        "NaN"
    } else if f > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    };
    match d {
        Dialect::Postgres => Ok(format!("'{special}'::float8")),
        Dialect::MySql(_) => Err(invalid(col, &format!("{special} cannot be stored"))),
    }
}

fn decimal(s: &str, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    if is_decimal(s) {
        return Ok(s.into());
    }
    // PG `numeric` also holds NaN and (PG 14+) ±Infinity.
    if d == Dialect::Postgres && matches!(s, "NaN" | "Infinity" | "-Infinity") {
        return Ok(format!("'{s}'::{}", pg_cast_target(col)?));
    }
    Err(invalid(col, "not a decimal number"))
}

/// `^-?\d+(\.\d+)?([eE][-+]?\d+)?$` (ASCII digits only).
fn is_decimal(s: &str) -> bool {
    fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
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

/// A text-like value: MySQL string literal; PG string literal cast to the column's type
/// unless the column is text.
fn string(s: &str, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    match d {
        Dialect::MySql(_) => mysql_string(s, col, d),
        Dialect::Postgres => {
            if s.contains('\0') {
                return Err(invalid(col, "PostgreSQL text cannot hold a NUL character"));
            }
            let quoted = format!("'{}'", s.replace('\'', "''"));
            if col.logical == LogicalType::Text {
                Ok(quoted)
            } else {
                Ok(format!("{quoted}::{}", pg_cast_target(col)?))
            }
        }
    }
}

fn mysql_string(s: &str, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    let backslash = matches!(d, Dialect::MySql(m) if !m.no_backslash_escapes);
    let mut out = String::with_capacity(s.len() + 11);
    out.push_str("_utf8mb4'");
    for c in s.chars() {
        match c {
            '\\' if backslash => out.push_str("\\\\"),
            '\'' if backslash => out.push_str("\\'"),
            '\0' if backslash => out.push_str("\\0"),
            '\'' => out.push_str("''"),
            '\0' => {
                return Err(invalid(
                    col,
                    "a NUL character cannot be written under NO_BACKSLASH_ESCAPES",
                ))
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    Ok(out)
}

/// The column's type as a PG cast target. It comes from the server's catalog, but it is
/// still only accepted when it is a plain type name: words joined by `.` or a space,
/// numeric modifiers `(10)` / `(40,10)`, and `[]` suffixes. Anything else (quotes,
/// operators, comments, `;`) is refused rather than spliced into SQL.
fn pg_cast_target(col: &ColumnTarget) -> Result<&str, BuildError> {
    if is_plain_type_name(&col.db_type) {
        Ok(&col.db_type)
    } else {
        Err(unsupported(col))
    }
}

fn is_plain_type_name(t: &str) -> bool {
    let b = t.as_bytes();
    let word_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let word_byte = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    // Each step: a word, then any modifiers / `[]`, then a separator or the end.
    loop {
        if !b.get(i).copied().is_some_and(word_start) {
            return false;
        }
        while b.get(i).copied().is_some_and(word_byte) {
            i += 1;
        }
        if b.get(i) == Some(&b'(') {
            let close = match b[i..].iter().position(|&c| c == b')') {
                Some(p) => i + p,
                None => return false,
            };
            let inner = &t[i + 1..close];
            if !inner
                .split(',')
                .all(|n| !n.trim().is_empty() && n.trim().bytes().all(|c| c.is_ascii_digit()))
            {
                return false;
            }
            i = close + 1;
        }
        while b[i..].starts_with(b"[]") {
            i += 2;
        }
        match b.get(i) {
            None => return true,
            Some(b'.' | b' ') => i += 1,
            Some(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::MySqlMode;

    /// A column and dialect to render for.
    struct Ctx(&'static str, LogicalType, Dialect);

    const MY_NBE_MODE: MySqlMode = MySqlMode {
        ansi_quotes: false,
        no_backslash_escapes: true,
    };
    const MY: Ctx = Ctx("varchar", LogicalType::Text, Dialect::MYSQL);
    const MY_NBE: Ctx = Ctx("varchar", LogicalType::Text, Dialect::MySql(MY_NBE_MODE));
    const MY_BIN: Ctx = Ctx("varbinary", LogicalType::Binary, Dialect::MYSQL);
    const MY_JSON: Ctx = Ctx("json", LogicalType::Json, Dialect::MYSQL);
    const MY_INT: Ctx = Ctx("int", LogicalType::Integer, Dialect::MYSQL);
    const MY_DEC: Ctx = Ctx("decimal", LogicalType::Decimal, Dialect::MYSQL);
    const MY_FLOAT: Ctx = Ctx("double", LogicalType::Float, Dialect::MYSQL);
    const MY_BOOL: Ctx = Ctx("tinyint", LogicalType::Integer, Dialect::MYSQL);
    const MY_DATE: Ctx = Ctx("date", LogicalType::Date, Dialect::MYSQL);
    const MY_GEOM: Ctx = Ctx("geometry", LogicalType::Other, Dialect::MYSQL);
    const PG: Ctx = Ctx("text", LogicalType::Text, Dialect::Postgres);
    const PG_BYTEA: Ctx = Ctx("bytea", LogicalType::Binary, Dialect::Postgres);
    const PG_UUID: Ctx = Ctx("uuid", LogicalType::Uuid, Dialect::Postgres);
    const PG_JSONB: Ctx = Ctx("jsonb", LogicalType::Json, Dialect::Postgres);
    const PG_TSTZ: Ctx = Ctx("timestamptz", LogicalType::DateTime, Dialect::Postgres);
    const PG_ENUM: Ctx = Ctx("order_status", LogicalType::Enum, Dialect::Postgres);
    const PG_NUM: Ctx = Ctx("numeric", LogicalType::Decimal, Dialect::Postgres);
    const PG_FLOAT: Ctx = Ctx("float4", LogicalType::Float, Dialect::Postgres);
    const PG_BOOL: Ctx = Ctx("bool", LogicalType::Bool, Dialect::Postgres);
    const PG_INT: Ctx = Ctx("int8", LogicalType::Integer, Dialect::Postgres);
    const PG_ARR: Ctx = Ctx("int4[]", LogicalType::Array, Dialect::Postgres);
    const PG_INTERVAL: Ctx = Ctx("interval", LogicalType::Other, Dialect::Postgres);

    fn col(c: &Ctx) -> ColumnTarget {
        ColumnTarget {
            name: "c".into(),
            db_type: c.0.into(),
            logical: c.1,
            collation: None,
        }
    }
    fn full(v: Value) -> FullValue {
        FullValue::try_from_value(v).unwrap()
    }
    fn t(s: &str) -> FullValue {
        full(Value::Text(s.into()))
    }
    fn dec(s: &str) -> FullValue {
        full(Value::Decimal(s.into()))
    }
    fn uuid(s: &str) -> FullValue {
        full(Value::Uuid(s.into()))
    }
    fn lit(v: FullValue, c: Ctx) -> String {
        literal(&v, &col(&c), c.2).unwrap_or_else(|e| panic!("{e}"))
    }
    fn lit_err(v: FullValue, c: Ctx) -> BuildError {
        literal(&v, &col(&c), c.2).expect_err("expected an error")
    }

    #[test]
    fn mysql_escapes_backslash_and_quote() {
        assert_eq!(lit(t("it's C:\\x"), MY), r"_utf8mb4'it\'s C:\\x'");
        assert_eq!(lit(t("it's C:\\x"), MY_NBE), r"_utf8mb4'it''s C:\x'");
        assert_eq!(lit(t("a\0b"), MY), r"_utf8mb4'a\0b'");
        // ANSI_QUOTES only changes `"…"`; single-quoted strings are the same.
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        });
        assert_eq!(
            lit(t("\"x\""), Ctx("varchar", LogicalType::Text, ansi)),
            "_utf8mb4'\"x\"'"
        );
    }

    #[test]
    fn pg_never_uses_e_strings() {
        assert_eq!(lit(t(r"a\'b"), PG), r"'a\''b'");
        assert_eq!(lit(t("Nguyễn"), PG), "'Nguyễn'");
    }

    #[test]
    fn nul_in_text_is_rejected_where_it_cannot_be_written() {
        for c in [PG, MY_NBE] {
            assert!(matches!(
                lit_err(t("a\0b"), c),
                BuildError::Invalid { ref column, .. } if column == "c"
            ));
        }
        assert!(matches!(
            lit_err(full(Value::Json("\"\0\"".into())), PG_JSONB),
            BuildError::Invalid { .. }
        ));
    }

    #[test]
    fn binary_literals() {
        assert_eq!(lit(FullValue::bytes(vec![0, 0xff]), MY_BIN), "X'00ff'");
        assert_eq!(
            lit(FullValue::bytes(vec![0, 0xff]), PG_BYTEA),
            r"'\x00ff'::bytea"
        );
        assert_eq!(lit(FullValue::bytes(Vec::new()), MY_BIN), "X''");
        assert_eq!(lit(FullValue::bytes(Vec::new()), PG_BYTEA), r"'\x'::bytea");
    }

    #[test]
    fn pg_casts_non_text_types() {
        let u = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(lit(uuid(u), PG_UUID), format!("'{u}'::uuid"));
        assert_eq!(
            lit(full(Value::Json(r#"{"a":"it's"}"#.into())), PG_JSONB),
            r#"'{"a":"it''s"}'::jsonb"#
        );
        assert_eq!(
            lit(
                full(Value::TimestampTz("2038-01-19T03:14:07+00:00".into())),
                PG_TSTZ
            ),
            "'2038-01-19T03:14:07+00:00'::timestamptz"
        );
        assert_eq!(lit(t("shipped"), PG_ENUM), "'shipped'::order_status");
        // MySQL has no casts for text-like types.
        assert_eq!(uuid_mysql(u), format!("_utf8mb4'{u}'"));
        assert_eq!(
            lit(full(Value::Date("9999-12-31".into())), MY_DATE),
            "_utf8mb4'9999-12-31'"
        );
    }

    fn uuid_mysql(u: &str) -> String {
        lit(uuid(u), Ctx("char", LogicalType::Text, Dialect::MYSQL))
    }

    #[test]
    fn pg_cast_target_must_be_a_plain_type_name() {
        for bad in ["uuid; DROP TABLE t", "x'y", "a--b", "\"weird\"", ""] {
            let c = ColumnTarget {
                name: "c".into(),
                db_type: bad.into(),
                logical: LogicalType::Enum,
                collation: None,
            };
            assert!(
                matches!(
                    literal(&t("x"), &c, Dialect::Postgres),
                    Err(BuildError::Unsupported { .. })
                ),
                "{bad}"
            );
        }
        for good in [
            "character varying(10)",
            "public.order_status",
            "numeric(40,10)",
        ] {
            let c = ColumnTarget {
                name: "c".into(),
                db_type: good.into(),
                logical: LogicalType::Enum,
                collation: None,
            };
            assert_eq!(
                literal(&t("x"), &c, Dialect::Postgres).unwrap(),
                format!("'x'::{good}")
            );
        }
    }

    #[test]
    fn mysql_json_is_cast() {
        assert_eq!(
            lit(full(Value::Json(r#"{"a": 1}"#.into())), MY_JSON),
            r#"CAST(_utf8mb4'{"a": 1}' AS JSON)"#
        );
    }

    #[test]
    fn decimal_must_be_numeric() {
        assert!(matches!(
            lit_err(dec("1;DROP"), MY),
            BuildError::Invalid { .. }
        ));
        for bad in [
            "", "-", "1.", ".5", "1e", "1e+", "--1", "1 ", "0x10", "1.2.3", "١",
        ] {
            assert!(
                matches!(lit_err(dec(bad), MY_DEC), BuildError::Invalid { .. }),
                "{bad:?}"
            );
        }
        for good in [
            "0",
            "-12",
            "123456789012345678901234567890.1234567890",
            "1e5",
            "-1.5E-3",
        ] {
            assert_eq!(lit(dec(good), MY_DEC), good);
            assert_eq!(lit(dec(good), PG_NUM), good);
        }
    }

    #[test]
    fn pg_numeric_specials_are_quoted_mysql_rejects_them() {
        for s in ["NaN", "Infinity", "-Infinity"] {
            assert_eq!(lit(dec(s), PG_NUM), format!("'{s}'::numeric"));
            assert!(matches!(
                lit_err(dec(s), MY_DEC),
                BuildError::Invalid { .. }
            ));
        }
    }

    #[test]
    fn numbers_and_bools() {
        assert_eq!(
            lit(full(Value::Int(i64::MIN)), MY_INT),
            "-9223372036854775808"
        );
        assert_eq!(
            lit(full(Value::UInt(u64::MAX)), MY_INT),
            "18446744073709551615"
        );
        assert_eq!(lit(full(Value::Int(7)), PG_INT), "7");
        assert_eq!(lit(full(Value::Bool(true)), MY_BOOL), "1");
        assert_eq!(lit(full(Value::Bool(false)), MY_BOOL), "0");
        assert_eq!(lit(full(Value::Bool(true)), PG_BOOL), "TRUE");
        assert_eq!(lit(full(Value::Bool(false)), PG_BOOL), "FALSE");
    }

    #[test]
    fn floats_round_trip_and_specials() {
        for f in [
            0.1,
            -1.5,
            1.7976931348623157e308,
            5e-324,
            3.4028234663852886e38,
            0.0,
        ] {
            let s = lit(full(Value::Float(f)), MY_FLOAT);
            assert_eq!(s.parse::<f64>().unwrap(), f, "{s}");
            assert_eq!(lit(full(Value::Float(f)), PG_FLOAT), s);
        }
        assert_eq!(lit(full(Value::Float(f64::NAN)), PG_FLOAT), "'NaN'::float8");
        assert_eq!(
            lit(full(Value::Float(f64::INFINITY)), PG_FLOAT),
            "'Infinity'::float8"
        );
        assert_eq!(
            lit(full(Value::Float(f64::NEG_INFINITY)), PG_FLOAT),
            "'-Infinity'::float8"
        );
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                lit_err(full(Value::Float(f)), MY_FLOAT),
                BuildError::Invalid { .. }
            ));
        }
    }

    #[test]
    fn null_is_null_for_every_column() {
        let n = full(Value::Null);
        for c in [MY, MY_GEOM, PG, PG_ARR, PG_INTERVAL, PG_BYTEA] {
            assert_eq!(lit(n.clone(), c), "NULL");
        }
    }

    #[test]
    fn arrays_and_other_are_unsupported() {
        let arr = full(Value::Array(vec![Value::Int(1)]));
        assert!(matches!(
            lit_err(arr, PG_ARR),
            BuildError::Unsupported { .. }
        ));
        let other = full(Value::Other {
            type_name: "interval".into(),
            text: "P1D".into(),
        });
        assert!(matches!(
            lit_err(other, PG_INTERVAL),
            BuildError::Unsupported { .. }
        ));
        // An `Other` column (spatial, range, composite …) is never written from text.
        assert!(matches!(
            lit_err(t("POINT(1 2)"), MY_GEOM),
            BuildError::Unsupported { .. }
        ));
    }
}
