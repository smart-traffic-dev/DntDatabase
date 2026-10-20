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

/// Most digits a MySQL `DECIMAL` (and an exact decimal literal) holds.
const MYSQL_DECIMAL_DIGITS: usize = 65;

fn decimal(s: &str, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    if is_decimal(s) {
        return match d {
            // MySQL reads `1.5e3` as an approximate (DOUBLE) literal: write the digits.
            Dialect::MySql(_) if s.contains(['e', 'E']) => expand_exponent(s)
                .ok_or_else(|| invalid(col, "too many digits for an exact decimal")),
            _ => Ok(s.into()),
        };
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

/// `s` (already [`is_decimal`], with an exponent) as plain digits, by moving the decimal
/// point (string arithmetic, never a float). `None` when the result has more than
/// [`MYSQL_DECIMAL_DIGITS`] digits, not counting leading zeros of the integer part.
fn expand_exponent(s: &str) -> Option<String> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (mantissa, exp) = s.split_once(['e', 'E'])?;
    let exp = exp.strip_prefix('+').unwrap_or(exp);
    // Any exponent this large expands past the digit limit (or is all zeros, which
    // is not worth a huge string): refuse before allocating.
    if exp.trim_start_matches('-').len() > 4 {
        return None;
    }
    let exp: i64 = exp.parse().ok()?;
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let point = int.len() as i64 + exp;
    let len = digits.len() as i64;
    let (int_part, frac_part) = if point <= 0 {
        (
            "0".to_string(),
            "0".repeat(usize::try_from(-point).ok()?) + &digits,
        )
    } else if point >= len {
        (
            digits.clone() + &"0".repeat(usize::try_from(point - len).ok()?),
            String::new(),
        )
    } else {
        let p = usize::try_from(point).ok()?;
        (digits[..p].to_string(), digits[p..].to_string())
    };
    let int_part = match int_part.trim_start_matches('0') {
        "" => "0",
        t => t,
    };
    let significant = int_part.trim_start_matches('0').len() + frac_part.len();
    if significant > MYSQL_DECIMAL_DIGITS {
        return None;
    }
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    out.push_str(int_part);
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(&frac_part);
    }
    Some(out)
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

/// SQL type names PG prints as several words: `(head, tail)`, with an optional type
/// modifier between them as PG prints it (`character varying(10)`,
/// `timestamp(3) with time zone`).
const PG_MULTI_WORD_TYPES: &[(&str, &str)] = &[
    ("character varying", ""),
    ("double precision", ""),
    ("bit varying", ""),
    ("timestamp", " with time zone"),
    ("timestamp", " without time zone"),
    ("time", " with time zone"),
    ("time", " without time zone"),
];

/// The column's type as a PG cast target, rendered from its parsed structure and never
/// spliced in as free text (`db_type` may be a raw catalog name such as `bool or true`).
/// Accepted: `name` or `schema.name`, each part an unquoted identifier (emitted as is
/// when it is a lower-case plain identifier, else through `quote_ident`) or a
/// double-quoted one (`""` un-doubled, then always re-quoted); or one of
/// [`PG_MULTI_WORD_TYPES`]. Either may be followed by a type modifier of digits and
/// commas in parentheses and any number of `[]`. Anything else is `Unsupported`.
fn pg_cast_target(col: &ColumnTarget) -> Result<String, BuildError> {
    render_pg_type(&col.db_type).ok_or_else(|| unsupported(col))
}

fn render_pg_type(t: &str) -> Option<String> {
    for (head, tail) in PG_MULTI_WORD_TYPES {
        if let Some(rest) = t.strip_prefix(head) {
            let (typmod, rest) = take_typmod(rest);
            if let Some(rest) = rest.strip_prefix(tail) {
                // Space-separated, so `timestamp` alone never matches here.
                if let Some(arrays) = take_arrays(rest) {
                    return Some(format!("{head}{typmod}{tail}{arrays}"));
                }
            }
        }
    }
    let (first, rest) = take_ident_part(t)?;
    let (second, rest) = match rest.strip_prefix('.') {
        Some(after) => {
            let (p, r) = take_ident_part(after)?;
            (Some(p), r)
        }
        None => (None, rest),
    };
    let (typmod, rest) = take_typmod(rest);
    let arrays = take_arrays(rest)?;
    let mut out = render_pg_part(&first);
    if let Some(second) = &second {
        out.push('.');
        out.push_str(&render_pg_part(second));
    }
    out.push_str(typmod);
    out.push_str(&arrays);
    Some(out)
}

/// One parsed name part of a type: its text and whether the input quoted it.
struct NamePart {
    name: String,
    quoted: bool,
}

/// One name part at the start of `s`: `"…"` (with `""` un-doubled) or an unquoted
/// identifier. Returns the part and what follows it.
fn take_ident_part(s: &str) -> Option<(NamePart, &str)> {
    if let Some(body) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if body[i + 1..].starts_with('"') {
                    chars.next();
                    name.push('"');
                } else {
                    return (!name.is_empty() && !name.contains('\0')).then(|| {
                        let part = NamePart { name, quoted: true };
                        (part, &body[i + 1..])
                    });
                }
            } else {
                name.push(c);
            }
        }
        return None; // unterminated
    }
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_alphabetic() || c == '_' => {}
        _ => return None,
    }
    let end = chars
        .find(|&(_, c)| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .map_or(s.len(), |(i, _)| i);
    let part = NamePart {
        name: s[..end].to_string(),
        quoted: false,
    };
    Some((part, &s[end..]))
}

/// A part quoted in the input is always re-quoted (`"char"` is not `char`, `"int"` is
/// not `int`); an unquoted one is emitted bare only when it is a lower-case plain
/// identifier.
fn render_pg_part(part: &NamePart) -> String {
    let name = part.name.as_str();
    let mut chars = name.chars();
    let plain = !part.quoted
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if plain {
        name.to_string()
    } else {
        crate::sql::ident::quote_ident(crate::server::DbKind::Postgres, name)
    }
}

/// An optional `(digits[,digits…])` at the start of `s`: (the modifier, the rest).
fn take_typmod(s: &str) -> (&str, &str) {
    if let Some(body) = s.strip_prefix('(') {
        if let Some(close) = body.find(')') {
            let inner = &body[..close];
            if inner
                .split(',')
                .all(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            {
                return s.split_at(close + 2);
            }
        }
    }
    ("", s)
}

/// `s` must be zero or more `[]` and nothing else.
fn take_arrays(s: &str) -> Option<String> {
    let mut rest = s;
    let mut n = 0;
    while let Some(r) = rest.strip_prefix("[]") {
        rest = r;
        n += 1;
    }
    rest.is_empty().then(|| "[]".repeat(n))
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

    fn pg_cast(db_type: &str) -> Result<String, BuildError> {
        let c = ColumnTarget {
            name: "c".into(),
            db_type: db_type.into(),
            logical: LogicalType::Enum,
            collation: None,
        };
        literal(&t("x"), &c, Dialect::Postgres)
    }

    #[test]
    fn pg_cast_target_is_rendered_from_structure() {
        for bad in [
            "bool or true",
            "text is not null",
            "numeric(1) or true",
            "int); DROP TABLE x; --",
            "uuid; DROP TABLE t",
            "x'y",
            "a--b",
            "a\"b",
            "\"unterminated",
            "\"\"",
            "a.b.c",
            "int4[",
            "numeric( 1)",
            "timestamp with time zone or true",
            "character varying(10) x",
            "1abc",
            "",
        ] {
            assert!(
                matches!(pg_cast(bad), Err(BuildError::Unsupported { .. })),
                "{bad:?}: {:?}",
                pg_cast(bad)
            );
        }
        for (db_type, cast) in [
            ("OrderStatus", "\"OrderStatus\""),
            (
                "\"My Schema\".\"Weird\"\"T\"",
                "\"My Schema\".\"Weird\"\"T\"",
            ),
            // A quoted part stays quoted, even when it looks plain or is a keyword.
            ("\"order_status\"", "\"order_status\""),
            ("\"char\"", "\"char\""),
            ("public.\"int\"", "public.\"int\""),
            ("\"user\"", "\"user\""),
            ("public.order_status", "public.order_status"),
            ("Sales.order_status", "\"Sales\".order_status"),
            ("đơn_hàng", "\"đơn_hàng\""),
            ("int4[]", "int4[]"),
            ("numeric(40,10)", "numeric(40,10)"),
            ("character varying(10)", "character varying(10)"),
            ("character varying", "character varying"),
            ("double precision[]", "double precision[]"),
            ("timestamp(3) with time zone", "timestamp(3) with time zone"),
            ("time without time zone", "time without time zone"),
            ("timestamptz", "timestamptz"),
        ] {
            assert_eq!(
                pg_cast(db_type).unwrap(),
                format!("'x'::{cast}"),
                "{db_type:?}"
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
            assert_eq!(lit(dec(good), PG_NUM), good);
        }
        for good in ["0", "-12", "123456789012345678901234567890.1234567890"] {
            assert_eq!(lit(dec(good), MY_DEC), good);
        }
    }

    /// MySQL reads an exponent literal as DOUBLE: the digits are written out exactly.
    #[test]
    fn mysql_expands_decimal_exponents() {
        for (input, out) in [
            ("1234567890.0123456789e0", "1234567890.0123456789"),
            ("1.5e3", "1500"),
            ("-2.5e-3", "-0.0025"),
            ("1e5", "100000"),
            ("-1.5E-3", "-0.0015"),
            ("1.50e1", "15.0"),
            ("007e1", "70"),
            ("12.5E+1", "125"),
            ("0e0", "0"),
            ("1e64", &format!("1{}", "0".repeat(64))),
            ("1e-65", &format!("0.{}1", "0".repeat(64))),
        ] {
            assert_eq!(lit(dec(input), MY_DEC), out, "{input}");
            // PG reads exponent literals as exact numeric: unchanged.
            assert_eq!(lit(dec(input), PG_NUM), input, "{input}");
        }
        // 66 digits, however reached, is beyond an exact MySQL decimal.
        for too_long in [
            "1e65",
            "1e-66",
            "1.5e65",
            "123456789012345678901234567890123456789012345678901234567890.123456e1",
            "1e99999999999999999999",
            "1e-99999",
        ] {
            assert!(
                matches!(lit_err(dec(too_long), MY_DEC), BuildError::Invalid { .. }),
                "{too_long}"
            );
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
