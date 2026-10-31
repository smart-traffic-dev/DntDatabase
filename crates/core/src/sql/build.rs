//! SQL text built from values: the one place a cell value becomes SQL (spec §4.1).
//!
//! v1 renders literals instead of bind parameters, so the statement a preview shows is
//! the statement sent. Every value goes through [`literal`]; nothing else formats a value
//! into SQL.

use crate::server::SqlKind;
use crate::sql::filter::WhereClause;
use crate::sql::ident::{qualified_name, quote_ident};
use crate::sql::Dialect;
use crate::sql::TokKind;
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
///   composite, …) are [`BuildError::Unsupported`] (a dump writes PG arrays and
///   `Other` values with [`dump_literal`]).
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

/// Lower-case hex digits of `b`.
fn hex(b: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(b.len() * 2);
    for &x in b {
        hex.push(DIGITS[usize::from(x >> 4)] as char);
        hex.push(DIGITS[usize::from(x & 0xf)] as char);
    }
    hex
}

fn binary(b: &[u8], d: Dialect) -> String {
    let hex = hex(b);
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
    let quoted = mysql_quote(s, d).map_err(|e| match e {
        BuildError::Invalid { reason, .. } => invalid(col, &reason),
        other => other,
    })?;
    Ok(format!("_utf8mb4{quoted}"))
}

/// `s` as a plain MySQL string literal `'…'` (no character set introducer), escaped as
/// [`literal`] escapes strings for `d`: backslash-escaped (`\\`, `\'`, `\0`) unless
/// `NO_BACKSLASH_ESCAPES`, where only `'` is doubled and NUL is refused. For DDL
/// positions that take no introducer (`COMMENT '…'`, `ENUM('…')`, a column `DEFAULT`).
pub fn mysql_quote(s: &str, d: Dialect) -> Result<String, BuildError> {
    let backslash = matches!(d, Dialect::MySql(m) if !m.no_backslash_escapes);
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' if backslash => out.push_str("\\\\"),
            '\'' if backslash => out.push_str("\\'"),
            '\0' if backslash => out.push_str("\\0"),
            '\'' => out.push_str("''"),
            '\0' => {
                return Err(BuildError::Invalid {
                    column: String::new(),
                    reason: "a NUL character cannot be written under NO_BACKSLASH_ESCAPES".into(),
                })
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
        crate::sql::ident::quote_ident(crate::server::SqlKind::Postgres, name)
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

// ---------------------------------------------------------------------------------------
// Dump literals: what a dump writes beyond `literal` (spec §6.4).
// ---------------------------------------------------------------------------------------

/// [`literal`], plus on PostgreSQL a `Value::Array` as `'<array text>'::<db_type>`
/// ([`pg_array_text`]) and a `Value::Other` as `'<text>'::<db_type>` (the value's text as
/// the server printed it). The cast target is rendered like every PG cast (from the
/// type's parsed structure); a NUL anywhere in the text is `Invalid`, an incomplete
/// binary array item `NotFull`. MySQL `Value::Other` / `Value::Array` stay `Unsupported`.
pub fn dump_literal(v: &FullValue, col: &ColumnTarget, d: Dialect) -> Result<String, BuildError> {
    if d == Dialect::Postgres {
        let text = match v.as_value() {
            Some(Value::Array(items)) => Some(pg_array_text(items).ok_or(BuildError::NotFull)?),
            Some(Value::Other { text, .. }) => Some(text.clone()),
            _ => None,
        };
        if let Some(text) = text {
            if text.contains('\0') {
                return Err(invalid(col, "PostgreSQL text cannot hold a NUL character"));
            }
            return Ok(format!(
                "'{}'::{}",
                text.replace('\'', "''"),
                pg_cast_target(col)?
            ));
        }
    }
    literal(v, col, d)
}

/// A PostgreSQL array value in its text form (`{1,NULL,"a b",{2,3}}`), the same quoting
/// as the CSV export's arrays: an item is written bare unless it is empty, reads as
/// `NULL` (any case), or holds `,` `{` `}` `"` `\` or whitespace; then it is
/// double-quoted with `\` and `"` backslash-escaped. `Value::Null` is `NULL`, a nested
/// array its own `{…}`, a binary item its text form `\x…` (so always quoted), a float
/// the shortest text that reads back the same (or `NaN` / `Infinity` / `-Infinity`).
/// `None` when a binary item (at any depth) is not complete.
///
/// The result is array *text*, not SQL: [`dump_literal`] quotes it as a string literal.
pub fn pg_array_text(items: &[Value]) -> Option<String> {
    let mut out = String::from("{");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            Value::Null => out.push_str("NULL"),
            Value::Array(inner) => out.push_str(&pg_array_text(inner)?),
            Value::Bytes { len, preview } => {
                if (preview.len() as u64) < *len {
                    return None;
                }
                push_array_item(&mut out, &format!("\\x{}", hex(preview)));
            }
            other => push_array_item(&mut out, &array_item_text(other)),
        }
    }
    out.push('}');
    Some(out)
}

/// The text of a scalar array item (not `Null`, `Array` or `Bytes`).
fn array_item_text(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) if f.is_nan() => "NaN".into(),
        Value::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "Infinity" } else { "-Infinity" }.into()
        }
        Value::Float(f) => format!("{f:?}"),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s)
        | Value::Other { text: s, .. } => s.clone(),
        Value::Null | Value::Array(_) | Value::Bytes { .. } => String::new(),
    }
}

fn push_array_item(out: &mut String, t: &str) {
    let plain = !t.is_empty()
        && !t.eq_ignore_ascii_case("null")
        && !t
            .chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\') || c.is_whitespace());
    if plain {
        out.push_str(t);
    } else {
        out.push('"');
        for c in t.chars() {
            if matches!(c, '"' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }
}

// ---------------------------------------------------------------------------------------
// Statements: key-based DML and the raw WHERE check.
// ---------------------------------------------------------------------------------------

/// The table a statement reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub kind: SqlKind,
    pub database: String,
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    /// The quoted, qualified name ([`qualified_name`]).
    pub fn relation(&self) -> String {
        qualified_name(
            self.kind,
            &self.database,
            self.schema.as_deref(),
            &self.name,
        )
    }
}

/// The columns and values that identify one row. Not empty and with no NULL value:
/// build it with [`RowKey::new`]; the builders check it again.
#[derive(Debug, Clone, PartialEq)]
pub struct RowKey(pub Vec<(ColumnTarget, FullValue)>);

impl RowKey {
    pub fn new(cols: Vec<(ColumnTarget, FullValue)>) -> Result<RowKey, BuildError> {
        let key = RowKey(cols);
        key.check()?;
        Ok(key)
    }

    fn check(&self) -> Result<(), BuildError> {
        if self.0.is_empty() {
            return Err(BuildError::Invalid {
                column: String::new(),
                reason: "a row key needs at least one column".into(),
            });
        }
        match self.0.iter().find(|(_, v)| v.is_null()) {
            Some((col, _)) => Err(invalid(col, "a key column cannot be NULL")),
            None => Ok(()),
        }
    }
}

/// The conflict check added to a key-based `UPDATE` / `DELETE` (spec §5.2).
#[derive(Debug, Clone, PartialEq)]
pub enum RowGuard {
    /// PostgreSQL: the row version (`xmin::text`) read with the page.
    PgXmin(String),
    /// The comparable columns' values as loaded, compared null-safe.
    Snapshot(Vec<(ColumnTarget, FullValue)>),
    /// The key only (no comparable column, or an explicit overwrite).
    KeyOnly,
}

/// Words that start a write or DDL statement: refused at depth 0 of a raw `WHERE`.
const RAW_WHERE_FORBIDDEN: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP", "ALTER", "TRUNCATE", "GRANT",
    "REVOKE", "CALL", "DO", "COPY", "SET",
];

fn kind_of(d: Dialect) -> SqlKind {
    match d {
        Dialect::MySql(_) => SqlKind::MySql,
        Dialect::Postgres => SqlKind::Postgres,
    }
}

/// The table's relation, after checking that it belongs to dialect `d`.
fn relation_for(t: &TableRef, d: Dialect) -> Result<String, BuildError> {
    if t.kind != kind_of(d) {
        return Err(BuildError::Invalid {
            column: t.name.clone(),
            reason: "the SQL dialect does not match the table's server".into(),
        });
    }
    Ok(t.relation())
}

/// One `INSERT INTO <relation> (<cols>) VALUES (…), (…)` for every row (each row has a
/// value per column). With no columns every value is the default: MySQL `() VALUES (),
/// …`, PostgreSQL `DEFAULT VALUES` (one row only). A non-empty `returning` adds
/// `RETURNING <cols>` on PostgreSQL; MySQL adds nothing (the caller reads back by
/// `LAST_INSERT_ID()`).
pub fn insert_sql(
    t: &TableRef,
    cols: &[ColumnTarget],
    rows: &[Vec<FullValue>],
    returning: &[ColumnTarget],
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    let kind = kind_of(d);
    if rows.is_empty() {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "an INSERT needs at least one row".into(),
        });
    }
    let mut sql = format!("INSERT INTO {relation}");
    if cols.is_empty() && d == Dialect::Postgres {
        if rows.len() != 1 || !rows[0].is_empty() {
            return Err(BuildError::Invalid {
                column: String::new(),
                reason: "PostgreSQL inserts one all-default row at a time".into(),
            });
        }
        sql.push_str(" DEFAULT VALUES");
    } else {
        let names: Vec<String> = cols.iter().map(|c| quote_ident(kind, &c.name)).collect();
        let mut tuples = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != cols.len() {
                return Err(BuildError::Invalid {
                    column: String::new(),
                    reason: format!("a row has {} values for {} columns", row.len(), cols.len()),
                });
            }
            let values = cols
                .iter()
                .zip(row)
                .map(|(c, v)| literal(v, c, d))
                .collect::<Result<Vec<_>, _>>()?;
            tuples.push(format!("({})", values.join(", ")));
        }
        sql.push_str(&format!(
            " ({}) VALUES {}",
            names.join(", "),
            tuples.join(", ")
        ));
    }
    if !returning.is_empty() && d == Dialect::Postgres {
        let names: Vec<String> = returning
            .iter()
            .map(|c| quote_ident(kind, &c.name))
            .collect();
        sql.push_str(&format!(" RETURNING {}", names.join(", ")));
    }
    Ok(sql)
}

/// What an `INSERT` does when a row collides with an existing key (R14).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// No clause: the server reports the duplicate.
    #[default]
    Error,
    /// Upsert on `key` (the columns of the primary key or a NOT NULL unique index),
    /// setting the `update` columns from the new row. With no `update` column: PG
    /// `DO NOTHING`, MySQL `<first key column> = <first key column>`. Every `key` and
    /// `update` column must be one of the inserted columns.
    Update {
        key: Vec<String>,
        update: Vec<String>,
    },
}

/// How [`insert_sql_with`] / [`insert_head`] / [`insert_tail`] write an `INSERT`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InsertOptions {
    /// PostgreSQL `OVERRIDING SYSTEM VALUE` (write an `IDENTITY ALWAYS` column);
    /// `Invalid` on MySQL.
    pub overriding_system_value: bool,
    pub on_conflict: OnConflict,
}

/// `INSERT INTO <relation> (<cols>)[ OVERRIDING SYSTEM VALUE] VALUES ` (with the trailing
/// space): the part of [`insert_sql_with`] before the row tuples. MySQL with no columns:
/// `INSERT INTO <relation> () VALUES ` (all-default rows `()`); PostgreSQL needs at
/// least one column (its all-default form `DEFAULT VALUES` takes no tuples).
pub fn insert_head(
    t: &TableRef,
    cols: &[ColumnTarget],
    opts: &InsertOptions,
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    if opts.overriding_system_value && d != Dialect::Postgres {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "OVERRIDING SYSTEM VALUE is PostgreSQL only".into(),
        });
    }
    if cols.is_empty() && d == Dialect::Postgres {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "a PostgreSQL multi-row INSERT needs at least one column".into(),
        });
    }
    let kind = kind_of(d);
    let names: Vec<String> = cols.iter().map(|c| quote_ident(kind, &c.name)).collect();
    let overriding = if opts.overriding_system_value {
        " OVERRIDING SYSTEM VALUE"
    } else {
        ""
    };
    Ok(format!(
        "INSERT INTO {relation} ({}){overriding} VALUES ",
        names.join(", ")
    ))
}

/// The part of [`insert_sql_with`] after the row tuples: `""`, MySQL
/// ` ON DUPLICATE KEY UPDATE `c` = VALUES(`c`), …` or PostgreSQL
/// ` ON CONFLICT ("k", …) DO UPDATE SET "c" = EXCLUDED."c", …` / ` … DO NOTHING`.
pub fn insert_tail(
    cols: &[ColumnTarget],
    opts: &InsertOptions,
    d: Dialect,
) -> Result<String, BuildError> {
    let OnConflict::Update { key, update } = &opts.on_conflict else {
        return Ok(String::new());
    };
    if key.is_empty() {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "an upsert needs at least one key column".into(),
        });
    }
    for name in key.iter().chain(update) {
        if !cols.iter().any(|c| &c.name == name) {
            return Err(BuildError::Invalid {
                column: name.clone(),
                reason: "an upsert column must be one of the inserted columns".into(),
            });
        }
    }
    let kind = kind_of(d);
    let q = |n: &str| quote_ident(kind, n);
    Ok(match d {
        Dialect::MySql(_) => {
            let sets: Vec<String> = if update.is_empty() {
                vec![format!("{0} = {0}", q(&key[0]))]
            } else {
                update
                    .iter()
                    .map(|n| format!("{0} = VALUES({0})", q(n)))
                    .collect()
            };
            format!(" ON DUPLICATE KEY UPDATE {}", sets.join(", "))
        }
        Dialect::Postgres => {
            let keys: Vec<String> = key.iter().map(|n| q(n)).collect();
            if update.is_empty() {
                format!(" ON CONFLICT ({}) DO NOTHING", keys.join(", "))
            } else {
                let sets: Vec<String> = update
                    .iter()
                    .map(|n| format!("{0} = EXCLUDED.{0}", q(n)))
                    .collect();
                format!(
                    " ON CONFLICT ({}) DO UPDATE SET {}",
                    keys.join(", "),
                    sets.join(", ")
                )
            }
        }
    })
}

/// One row as `(<lit>, <lit>, …)` with [`literal`] (one value per column).
pub fn row_tuple(
    cols: &[ColumnTarget],
    row: &[FullValue],
    d: Dialect,
) -> Result<String, BuildError> {
    tuple_with(cols, row, d, literal)
}

/// One row as `(<lit>, …)` with [`dump_literal`] (PG arrays and `Other` values too).
pub fn dump_row_tuple(
    cols: &[ColumnTarget],
    row: &[FullValue],
    d: Dialect,
) -> Result<String, BuildError> {
    tuple_with(cols, row, d, dump_literal)
}

type LiteralFn = fn(&FullValue, &ColumnTarget, Dialect) -> Result<String, BuildError>;

fn tuple_with(
    cols: &[ColumnTarget],
    row: &[FullValue],
    d: Dialect,
    lit: LiteralFn,
) -> Result<String, BuildError> {
    if row.len() != cols.len() {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: format!("a row has {} values for {} columns", row.len(), cols.len()),
        });
    }
    let values = cols
        .iter()
        .zip(row)
        .map(|(c, v)| lit(v, c, d))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("({})", values.join(", ")))
}

/// [`insert_head`] + the rows' [`row_tuple`]s joined with `", "` + [`insert_tail`]. With
/// default options this is byte for byte [`insert_sql`] without `RETURNING` (except
/// that PostgreSQL with no columns is refused, see [`insert_head`]).
pub fn insert_sql_with(
    t: &TableRef,
    cols: &[ColumnTarget],
    rows: &[Vec<FullValue>],
    opts: &InsertOptions,
    d: Dialect,
) -> Result<String, BuildError> {
    let head = insert_head(t, cols, opts, d)?;
    if rows.is_empty() {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "an INSERT needs at least one row".into(),
        });
    }
    let tail = insert_tail(cols, opts, d)?;
    let tuples = rows
        .iter()
        .map(|r| row_tuple(cols, r, d))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("{head}{}{tail}", tuples.join(", ")))
}

/// `UPDATE <relation> SET <col> = <lit>, … WHERE <key>[ AND <guard>]`.
pub fn update_sql(
    t: &TableRef,
    key: &RowKey,
    set: &[(ColumnTarget, FullValue)],
    guard: &RowGuard,
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    if set.is_empty() {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "an UPDATE needs at least one column to set".into(),
        });
    }
    let kind = kind_of(d);
    let assignments = set
        .iter()
        .map(|(c, v)| {
            Ok(format!(
                "{} = {}",
                quote_ident(kind, &c.name),
                literal(v, c, d)?
            ))
        })
        .collect::<Result<Vec<_>, BuildError>>()?;
    let cond = row_condition(key, guard, d)?;
    Ok(format!(
        "UPDATE {relation} SET {} WHERE {cond}",
        assignments.join(", ")
    ))
}

/// `DELETE FROM <relation> WHERE <key>[ AND <guard>]`.
pub fn delete_sql(
    t: &TableRef,
    key: &RowKey,
    guard: &RowGuard,
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    let cond = row_condition(key, guard, d)?;
    Ok(format!("DELETE FROM {relation} WHERE {cond}"))
}

/// `SELECT 1 FROM <relation> WHERE <key>[ AND <guard>]`: whether the row an `UPDATE` /
/// `DELETE` with the same key and guard targets is there (the same condition).
pub fn exists_sql(
    t: &TableRef,
    key: &RowKey,
    guard: &RowGuard,
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    let cond = row_condition(key, guard, d)?;
    Ok(format!("SELECT 1 FROM {relation} WHERE {cond}"))
}

/// `SELECT <cols> FROM <relation> WHERE <key>`: columns of the one row a key names (a
/// cell's complete value, `data_cell_fetch`).
pub fn select_by_key_sql(
    t: &TableRef,
    cols: &[ColumnTarget],
    key: &RowKey,
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    if cols.is_empty() {
        return Err(BuildError::Invalid {
            column: String::new(),
            reason: "a SELECT needs at least one column".into(),
        });
    }
    let kind = kind_of(d);
    let names: Vec<String> = cols.iter().map(|c| quote_ident(kind, &c.name)).collect();
    let cond = row_condition(key, &RowGuard::KeyOnly, d)?;
    Ok(format!(
        "SELECT {} FROM {relation} WHERE {cond}",
        names.join(", ")
    ))
}

/// `<key col> = <lit> AND …` then the guard's comparisons, all joined with `AND`.
fn row_condition(key: &RowKey, guard: &RowGuard, d: Dialect) -> Result<String, BuildError> {
    key.check()?;
    let kind = kind_of(d);
    let mut parts = Vec::new();
    for (c, v) in &key.0 {
        parts.push(format!(
            "{} = {}",
            quote_ident(kind, &c.name),
            literal(v, c, d)?
        ));
    }
    match guard {
        RowGuard::KeyOnly => {}
        RowGuard::PgXmin(x) => {
            if d != Dialect::Postgres {
                return Err(BuildError::Invalid {
                    column: "xmin".into(),
                    reason: "a row version guard is PostgreSQL only".into(),
                });
            }
            if x.is_empty() || !x.bytes().all(|b| b.is_ascii_digit()) {
                return Err(BuildError::Invalid {
                    column: "xmin".into(),
                    reason: "a row version is a transaction id (digits)".into(),
                });
            }
            let xid = ColumnTarget {
                name: "xmin".into(),
                db_type: "xid".into(),
                logical: LogicalType::Integer,
                collation: None,
            };
            let lit = literal(&text_value(x)?, &xid, d)?;
            parts.push(format!("xmin = {lit}"));
        }
        RowGuard::Snapshot(cols) => {
            for (c, v) in cols {
                let q = quote_ident(kind, &c.name);
                let lit = literal(v, c, d)?;
                parts.push(match d {
                    Dialect::Postgres => format!("{q} IS NOT DISTINCT FROM {lit}"),
                    Dialect::MySql(_) => match binary_charset(c)? {
                        Some(cs) if !v.is_null() => {
                            format!("{q} <=> CONVERT({lit} USING {cs}) COLLATE {cs}_bin")
                        }
                        _ => format!("{q} <=> {lit}"),
                    },
                });
            }
        }
    }
    Ok(parts.join(" AND "))
}

/// For a MySQL text column whose collation is not already binary (`<cs>_…`, not ending
/// in `_bin`): its character set `<cs>`, so a snapshot compares with `<cs>_bin` and a
/// case-only (or accent-only) change is still a conflict. `None` when the plain `<=>`
/// already compares exactly.
fn binary_charset(c: &ColumnTarget) -> Result<Option<&str>, BuildError> {
    if c.logical != LogicalType::Text {
        return Ok(None);
    }
    let Some(collation) = c.collation.as_deref() else {
        return Ok(None);
    };
    if collation == "binary" || collation.ends_with("_bin") {
        return Ok(None);
    }
    let plain = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    match collation.split_once('_') {
        Some((cs, rest)) if plain(cs) && plain(rest) => Ok(Some(cs)),
        _ => Err(invalid(c, &format!("unexpected collation {collation:?}"))),
    }
}

/// `s` as a complete text value (text is always complete).
fn text_value(s: &str) -> Result<FullValue, BuildError> {
    FullValue::try_from_value(Value::Text(s.into())).map_err(|_| BuildError::NotFull)
}

/// Checks a raw `WHERE` text from the user before it is wrapped as ` WHERE (<text>\n)`.
///
/// Refused (with the dialect's lexer, so strings, quoted names and comments never
/// count): an empty condition; a `;` anywhere; unbalanced parentheses; a string, quoted
/// name or block comment left open (it would swallow the closing parenthesis); a MySQL
/// executable comment `/*! … */` (the server runs its body); a word that starts a write
/// or DDL statement at depth 0 ([`RAW_WHERE_FORBIDDEN`]); `INTO` anywhere. The data view
/// also runs the query read-only, so this is a first line, not the only one.
pub fn check_raw_where(where_sql: &str, d: Dialect) -> Result<(), BuildError> {
    check_fragment(where_sql, d, &RAW_WHERE_RULES)
}

/// Checks a user-written SQL expression that the table designer places into DDL: a
/// column default, a generated column, a `USING` conversion, a `CHECK`, an index
/// expression or a custom type name.
///
/// Same rules as [`check_raw_where`], plus: `SELECT` anywhere (no subqueries), a `,`
/// at depth 0 (one fragment must not carry several DDL clauses) and comments of any
/// kind (the text is not followed by a newline in DDL, so a `--` comment would
/// swallow the rest of the line). The server stays the final check.
///
/// For MySQL the text must pass under all four lexing modes (`ANSI_QUOTES` ×
/// `NO_BACKSLASH_ESCAPES`), whatever mode `d` carries: the server's `sql_mode` may
/// differ from the one assumed, and a literal that hides a clause in one mode must
/// not slip through.
pub fn check_expression(text: &str, d: Dialect) -> Result<(), BuildError> {
    match d {
        Dialect::Postgres => check_fragment(text, d, &EXPRESSION_RULES),
        Dialect::MySql(_) => {
            for ansi_quotes in [false, true] {
                for no_backslash_escapes in [false, true] {
                    let mode = crate::sql::MySqlMode {
                        ansi_quotes,
                        no_backslash_escapes,
                    };
                    check_fragment(text, Dialect::MySql(mode), &EXPRESSION_RULES)?;
                }
            }
            Ok(())
        }
    }
}

/// What [`check_fragment`] refuses beyond the shared rules.
struct FragmentRules {
    /// `BuildError::Invalid::column` of a refusal.
    label: &'static str,
    /// The noun in messages ("a filter", "an expression").
    noun: &'static str,
    /// The empty-text message.
    empty: &'static str,
    /// Words refused at any depth (besides `INTO`).
    forbidden_anywhere: &'static [&'static str],
    /// Refuse comments (any text between tokens that is not whitespace).
    no_comments: bool,
    /// Refuse a `,` at depth 0.
    no_top_level_comma: bool,
}

const RAW_WHERE_RULES: FragmentRules = FragmentRules {
    label: "WHERE",
    noun: "a filter",
    empty: "the condition is empty",
    forbidden_anywhere: &[],
    no_comments: false,
    no_top_level_comma: false,
};

const EXPRESSION_RULES: FragmentRules = FragmentRules {
    label: "expression",
    noun: "an expression",
    empty: "the expression is empty",
    forbidden_anywhere: &["SELECT"],
    no_comments: true,
    no_top_level_comma: true,
};

/// The rules shared by [`check_raw_where`] and [`check_expression`] (see the former).
fn check_fragment(text: &str, d: Dialect, rules: &FragmentRules) -> Result<(), BuildError> {
    let bad = |reason: &str| {
        Err(BuildError::Invalid {
            column: rules.label.into(),
            reason: reason.into(),
        })
    };
    // Lexed as it will be sent: an open string or comment swallows the `)`.
    let wrapped = format!("({text}\n)");
    let toks = crate::sql::tokenize(&wrapped, d);
    let closes_at = wrapped.len() - 1;
    if !matches!(toks.last(), Some(t) if t.kind == TokKind::Close && t.start == closes_at) {
        return bad("a string, quoted name or comment is not closed");
    }
    if rules.no_comments {
        // The lexer drops comments: anything but whitespace between tokens is one.
        let has_comment = toks
            .windows(2)
            .any(|w| !wrapped[w[0].end..w[1].start].trim().is_empty());
        if has_comment {
            return bad(&format!("comments are not allowed in {}", rules.noun));
        }
    }
    let inner = &toks[1..toks.len() - 1];
    if inner.is_empty() {
        return bad(rules.empty);
    }
    let mut depth = 0usize;
    // With the comma rule: open `(` / `[` in order, so they must nest properly and a
    // comma counts as top level only outside both (PG `ARRAY[1, 2]`, `a[1:2]`).
    let mut nesting: Vec<u8> = Vec::new();
    let comma_rule = rules.no_top_level_comma;
    for t in inner {
        match t.kind {
            TokKind::Open => {
                depth += 1;
                nesting.push(b'(');
            }
            TokKind::Close => {
                if depth == 0 || (comma_rule && nesting.pop() != Some(b'(')) {
                    return bad("unbalanced parentheses");
                }
                depth -= 1;
            }
            TokKind::Punct(b';') => return bad(&format!("a ';' cannot appear in {}", rules.noun)),
            TokKind::Punct(b'[') if comma_rule => nesting.push(b'['),
            TokKind::Punct(b']') if comma_rule => {
                if nesting.pop() != Some(b'[') {
                    return bad("unbalanced brackets");
                }
            }
            TokKind::Punct(b',') if comma_rule && nesting.is_empty() => {
                return bad(&format!(
                    "a ',' outside parentheses cannot appear in {}",
                    rules.noun
                ))
            }
            TokKind::ExecComment => return bad("executable comments are not allowed"),
            TokKind::Word(w) => {
                let forbidden_here = (depth == 0
                    && RAW_WHERE_FORBIDDEN
                        .iter()
                        .any(|k| w.eq_ignore_ascii_case(k)))
                    || rules
                        .forbidden_anywhere
                        .iter()
                        .any(|k| w.eq_ignore_ascii_case(k));
                if w.eq_ignore_ascii_case("INTO") || forbidden_here {
                    return bad(&format!(
                        "{} is not allowed in {}",
                        w.to_ascii_uppercase(),
                        rules.noun
                    ));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return bad("unbalanced parentheses");
    }
    if comma_rule && !nesting.is_empty() {
        return bad("unbalanced brackets");
    }
    Ok(())
}

/// The data view's raw `WHERE` text as a [`WhereClause`] (same leading-space convention
/// as [`crate::sql::filter::build_where`], no parameters), after [`check_raw_where`]:
/// ` WHERE (<raw>\n)`. The newline ends a trailing line comment before the closing
/// parenthesis, so it cannot comment out the `ORDER BY` / `LIMIT` that follow.
pub fn raw_where_clause(where_sql: &str, d: Dialect) -> Result<WhereClause, BuildError> {
    check_raw_where(where_sql, d)?;
    Ok(WhereClause {
        sql: format!(" WHERE ({where_sql}\n)"),
        params: Vec::new(),
    })
}

#[cfg(test)]
mod dml_tests {
    use super::*;
    use crate::sql::MySqlMode;

    const NBE: Dialect = Dialect::MySql(MySqlMode {
        ansi_quotes: false,
        no_backslash_escapes: true,
    });

    fn my_table() -> TableRef {
        TableRef {
            kind: SqlKind::MySql,
            database: "shop".into(),
            schema: None,
            name: "users".into(),
        }
    }
    fn pg_table() -> TableRef {
        TableRef {
            kind: SqlKind::Postgres,
            database: "shop".into(),
            schema: Some("public".into()),
            name: "users".into(),
        }
    }
    fn c(name: &str, db_type: &str, logical: LogicalType, collation: Option<&str>) -> ColumnTarget {
        ColumnTarget {
            name: name.into(),
            db_type: db_type.into(),
            logical,
            collation: collation.map(Into::into),
        }
    }
    fn id_my() -> ColumnTarget {
        c("id", "int", LogicalType::Integer, None)
    }
    fn id_pg() -> ColumnTarget {
        c("id", "int4", LogicalType::Integer, None)
    }
    fn name_my(collation: &str) -> ColumnTarget {
        c("name", "varchar", LogicalType::Text, Some(collation))
    }
    fn name_pg() -> ColumnTarget {
        c("name", "text", LogicalType::Text, None)
    }
    fn fv(v: Value) -> FullValue {
        FullValue::try_from_value(v).unwrap()
    }
    fn int(i: i64) -> FullValue {
        fv(Value::Int(i))
    }
    fn text(s: &str) -> FullValue {
        fv(Value::Text(s.into()))
    }
    fn null() -> FullValue {
        fv(Value::Null)
    }
    fn key(cols: Vec<(ColumnTarget, FullValue)>) -> RowKey {
        RowKey::new(cols).unwrap()
    }

    #[test]
    fn update_by_key_with_snapshot_mysql() {
        let guard = RowGuard::Snapshot(vec![
            (name_my("utf8mb4_bin"), text("Anna")),
            (c("age", "int", LogicalType::Integer, None), null()),
        ]);
        let sql = update_sql(
            &my_table(),
            &key(vec![(id_my(), int(5))]),
            &[
                (name_my("utf8mb4_bin"), text("it's")),
                (c("note", "text", LogicalType::Text, None), null()),
            ],
            &guard,
            Dialect::MYSQL,
        )
        .unwrap();
        assert_eq!(
            sql,
            "UPDATE `shop`.`users` SET `name` = _utf8mb4'it\\'s', `note` = NULL \
             WHERE `id` = 5 AND `name` <=> _utf8mb4'Anna' AND `age` <=> NULL"
        );
    }

    #[test]
    fn update_and_delete_pg_snapshot_and_key_only() {
        let k = key(vec![(id_pg(), int(5)), (name_pg(), text("a"))]);
        let snap = RowGuard::Snapshot(vec![(c("age", "int4", LogicalType::Integer, None), null())]);
        assert_eq!(
            update_sql(
                &pg_table(),
                &k,
                &[(name_pg(), text("b"))],
                &snap,
                Dialect::Postgres
            )
            .unwrap(),
            "UPDATE \"public\".\"users\" SET \"name\" = 'b' \
             WHERE \"id\" = 5 AND \"name\" = 'a' AND \"age\" IS NOT DISTINCT FROM NULL"
        );
        assert_eq!(
            delete_sql(&pg_table(), &k, &RowGuard::KeyOnly, Dialect::Postgres).unwrap(),
            "DELETE FROM \"public\".\"users\" WHERE \"id\" = 5 AND \"name\" = 'a'"
        );
        // The same condition, as a check that the row is there.
        assert_eq!(
            exists_sql(&pg_table(), &k, &snap, Dialect::Postgres).unwrap(),
            "SELECT 1 FROM \"public\".\"users\" \
             WHERE \"id\" = 5 AND \"name\" = 'a' AND \"age\" IS NOT DISTINCT FROM NULL"
        );
        // Nothing to set is an error, not `UPDATE … SET WHERE`.
        assert!(matches!(
            update_sql(&pg_table(), &k, &[], &RowGuard::KeyOnly, Dialect::Postgres),
            Err(BuildError::Invalid { .. })
        ));
        // One row's columns by its key (a cell's complete value).
        let age = ColumnTarget {
            name: "age".into(),
            db_type: "int".into(),
            logical: LogicalType::Integer,
            collation: None,
        };
        assert_eq!(
            select_by_key_sql(
                &pg_table(),
                std::slice::from_ref(&age),
                &k,
                Dialect::Postgres
            )
            .unwrap(),
            "SELECT \"age\" FROM \"public\".\"users\" WHERE \"id\" = 5 AND \"name\" = 'a'"
        );
        assert!(matches!(
            select_by_key_sql(&pg_table(), &[], &k, Dialect::Postgres),
            Err(BuildError::Invalid { .. })
        ));
        // A dialect that does not match the table's server (with columns, so the
        // mismatch is what fails).
        assert!(matches!(
            select_by_key_sql(&pg_table(), std::slice::from_ref(&age), &k, Dialect::MYSQL),
            Err(BuildError::Invalid { reason, .. }) if reason.contains("dialect")
        ));
    }

    #[test]
    fn delete_by_key_with_xmin_pg() {
        let sql = delete_sql(
            &pg_table(),
            &key(vec![(id_pg(), int(5))]),
            &RowGuard::PgXmin("123".into()),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            sql,
            "DELETE FROM \"public\".\"users\" WHERE \"id\" = 5 AND xmin = '123'::xid"
        );
        // An xmin is digits; anything else never reaches the SQL.
        for bad in ["", "1'; DROP TABLE t; --", "12a", "-1"] {
            assert!(
                matches!(
                    delete_sql(
                        &pg_table(),
                        &key(vec![(id_pg(), int(5))]),
                        &RowGuard::PgXmin(bad.into()),
                        Dialect::Postgres,
                    ),
                    Err(BuildError::Invalid { .. })
                ),
                "{bad:?}"
            );
        }
        // xmin is PostgreSQL only.
        assert!(matches!(
            delete_sql(
                &my_table(),
                &key(vec![(id_my(), int(5))]),
                &RowGuard::PgXmin("1".into()),
                Dialect::MYSQL,
            ),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn mysql_snapshot_compares_text_binary() {
        let del = |collation: &str, v: FullValue| {
            delete_sql(
                &my_table(),
                &key(vec![(id_my(), int(1))]),
                &RowGuard::Snapshot(vec![(name_my(collation), v)]),
                Dialect::MYSQL,
            )
            .unwrap()
        };
        let prefix = "DELETE FROM `shop`.`users` WHERE `id` = 1 AND ";
        assert_eq!(
            del("utf8mb4_0900_ai_ci", text("Anna")),
            format!("{prefix}`name` <=> CONVERT(_utf8mb4'Anna' USING utf8mb4) COLLATE utf8mb4_bin")
        );
        assert_eq!(
            del("latin1_swedish_ci", text("Anna")),
            format!("{prefix}`name` <=> CONVERT(_utf8mb4'Anna' USING latin1) COLLATE latin1_bin")
        );
        assert_eq!(
            del("utf8mb4_bin", text("Anna")),
            format!("{prefix}`name` <=> _utf8mb4'Anna'")
        );
        // NULL needs no collation.
        assert_eq!(
            del("latin1_swedish_ci", null()),
            format!("{prefix}`name` <=> NULL")
        );
        // A collation that is not `<charset>_…` in plain lower-case never reaches the SQL.
        for bad in ["utf8mb4 x_ci", "x;y_ci", "_ci", "UTF8_x"] {
            assert!(
                matches!(
                    delete_sql(
                        &my_table(),
                        &key(vec![(id_my(), int(1))]),
                        &RowGuard::Snapshot(vec![(name_my(bad), text("a"))]),
                        Dialect::MYSQL,
                    ),
                    Err(BuildError::Invalid { .. })
                ),
                "{bad:?}"
            );
        }
        // Non-text columns compare as they are.
        assert_eq!(
            delete_sql(
                &my_table(),
                &key(vec![(id_my(), int(1))]),
                &RowGuard::Snapshot(vec![(
                    c("n", "int", LogicalType::Integer, Some("utf8mb4_0900_ai_ci")),
                    int(3)
                )]),
                Dialect::MYSQL,
            )
            .unwrap(),
            format!("{prefix}`n` <=> 3")
        );
    }

    #[test]
    fn row_key_rejects_null() {
        assert!(matches!(
            RowKey::new(vec![(id_my(), null())]),
            Err(BuildError::Invalid { .. })
        ));
        assert!(matches!(
            RowKey::new(vec![]),
            Err(BuildError::Invalid { .. })
        ));
        // A key built by hand is checked again by the builders.
        let bad = RowKey(vec![(id_pg(), int(1)), (name_pg(), null())]);
        assert!(matches!(
            delete_sql(&pg_table(), &bad, &RowGuard::KeyOnly, Dialect::Postgres),
            Err(BuildError::Invalid { .. })
        ));
        assert!(matches!(
            update_sql(
                &pg_table(),
                &RowKey(vec![]),
                &[(name_pg(), text("x"))],
                &RowGuard::KeyOnly,
                Dialect::Postgres
            ),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn insert_many_rows_in_one_statement() {
        let cols = [id_pg(), name_pg()];
        let rows = vec![vec![int(1), text("a")], vec![int(2), null()]];
        assert_eq!(
            insert_sql(&pg_table(), &cols, &rows, &[id_pg()], Dialect::Postgres).unwrap(),
            "INSERT INTO \"public\".\"users\" (\"id\", \"name\") VALUES (1, 'a'), (2, NULL) \
             RETURNING \"id\""
        );
        let cols = [id_my(), name_my("utf8mb4_bin")];
        // MySQL has no RETURNING: the caller reads back by LAST_INSERT_ID().
        assert_eq!(
            insert_sql(&my_table(), &cols, &rows, &[id_my()], Dialect::MYSQL).unwrap(),
            "INSERT INTO `shop`.`users` (`id`, `name`) VALUES (1, _utf8mb4'a'), (2, NULL)"
        );
        // Every column at its default.
        assert_eq!(
            insert_sql(&my_table(), &[], &[vec![], vec![]], &[], Dialect::MYSQL).unwrap(),
            "INSERT INTO `shop`.`users` () VALUES (), ()"
        );
        assert_eq!(
            insert_sql(&pg_table(), &[], &[vec![]], &[id_pg()], Dialect::Postgres).unwrap(),
            "INSERT INTO \"public\".\"users\" DEFAULT VALUES RETURNING \"id\""
        );
        for (cols, rows) in [
            (vec![id_pg()], vec![]),
            (vec![id_pg()], vec![vec![int(1), int(2)]]),
            (vec![], vec![vec![], vec![]]),
        ] {
            assert!(matches!(
                insert_sql(&pg_table(), &cols, &rows, &[], Dialect::Postgres),
                Err(BuildError::Invalid { .. })
            ));
        }
    }

    fn upsert(key: &[&str], update: &[&str]) -> InsertOptions {
        InsertOptions {
            overriding_system_value: false,
            on_conflict: OnConflict::Update {
                key: key.iter().map(|s| s.to_string()).collect(),
                update: update.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    #[test]
    fn upsert_mysql() {
        let cols = [
            id_my(),
            name_my("utf8mb4_bin"),
            c("qty", "int", LogicalType::Integer, None),
        ];
        let rows = vec![vec![int(1), text("a"), int(2)]];
        assert_eq!(
            insert_sql_with(
                &my_table(),
                &cols,
                &rows,
                &upsert(&["id"], &["name", "qty"]),
                Dialect::MYSQL
            )
            .unwrap(),
            "INSERT INTO `shop`.`users` (`id`, `name`, `qty`) VALUES (1, _utf8mb4'a', 2) \
             ON DUPLICATE KEY UPDATE `name` = VALUES(`name`), `qty` = VALUES(`qty`)"
        );
    }

    #[test]
    fn upsert_mysql_key_only() {
        let cols = [id_my(), c("k2", "int", LogicalType::Integer, None)];
        let rows = vec![vec![int(1), int(2)]];
        assert_eq!(
            insert_sql_with(
                &my_table(),
                &cols,
                &rows,
                &upsert(&["id", "k2"], &[]),
                Dialect::MYSQL
            )
            .unwrap(),
            "INSERT INTO `shop`.`users` (`id`, `k2`) VALUES (1, 2) \
             ON DUPLICATE KEY UPDATE `id` = `id`"
        );
    }

    #[test]
    fn upsert_pg() {
        let cols = [
            id_pg(),
            c("k2", "int4", LogicalType::Integer, None),
            name_pg(),
        ];
        let rows = vec![
            vec![int(1), int(2), text("a")],
            vec![int(3), int(4), null()],
        ];
        assert_eq!(
            insert_sql_with(
                &pg_table(),
                &cols,
                &rows,
                &upsert(&["id", "k2"], &["name"]),
                Dialect::Postgres
            )
            .unwrap(),
            "INSERT INTO \"public\".\"users\" (\"id\", \"k2\", \"name\") \
             VALUES (1, 2, 'a'), (3, 4, NULL) \
             ON CONFLICT (\"id\", \"k2\") DO UPDATE SET \"name\" = EXCLUDED.\"name\""
        );
    }

    #[test]
    fn upsert_pg_do_nothing() {
        let cols = [id_pg(), name_pg()];
        assert_eq!(
            insert_tail(&cols, &upsert(&["id"], &[]), Dialect::Postgres).unwrap(),
            " ON CONFLICT (\"id\") DO NOTHING"
        );
    }

    #[test]
    fn upsert_checks_its_columns() {
        let cols = [id_pg(), name_pg()];
        for opts in [
            upsert(&[], &["name"]),
            upsert(&["missing"], &["name"]),
            upsert(&["id"], &["missing"]),
        ] {
            for d in [Dialect::Postgres, Dialect::MYSQL] {
                assert!(
                    matches!(
                        insert_tail(&cols, &opts, d),
                        Err(BuildError::Invalid { .. })
                    ),
                    "{opts:?} {d:?}"
                );
            }
        }
    }

    #[test]
    fn overriding_system_value_pg() {
        let opts = InsertOptions {
            overriding_system_value: true,
            on_conflict: OnConflict::Error,
        };
        assert_eq!(
            insert_sql_with(
                &pg_table(),
                &[id_pg()],
                &[vec![int(1)]],
                &opts,
                Dialect::Postgres
            )
            .unwrap(),
            "INSERT INTO \"public\".\"users\" (\"id\") OVERRIDING SYSTEM VALUE VALUES (1)"
        );
        assert_eq!(
            insert_head(&pg_table(), &[id_pg()], &opts, Dialect::Postgres).unwrap(),
            "INSERT INTO \"public\".\"users\" (\"id\") OVERRIDING SYSTEM VALUE VALUES "
        );
    }

    #[test]
    fn overriding_refused_on_mysql() {
        let opts = InsertOptions {
            overriding_system_value: true,
            on_conflict: OnConflict::Error,
        };
        assert!(matches!(
            insert_head(&my_table(), &[id_my()], &opts, Dialect::MYSQL),
            Err(BuildError::Invalid { .. })
        ));
        assert!(matches!(
            insert_sql_with(
                &my_table(),
                &[id_my()],
                &[vec![int(1)]],
                &opts,
                Dialect::MYSQL
            ),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn head_tuples_tail_equal_insert_sql_with() {
        let pg_cols = [id_pg(), name_pg()];
        let my_cols = [id_my(), name_my("utf8mb4_bin")];
        let rows = vec![vec![int(1), text("it's")], vec![int(2), null()]];
        let cases: Vec<(TableRef, &[ColumnTarget], InsertOptions, Dialect)> = vec![
            (
                pg_table(),
                &pg_cols,
                InsertOptions::default(),
                Dialect::Postgres,
            ),
            (
                pg_table(),
                &pg_cols,
                upsert(&["id"], &["name"]),
                Dialect::Postgres,
            ),
            (
                pg_table(),
                &pg_cols,
                InsertOptions {
                    overriding_system_value: true,
                    on_conflict: OnConflict::Update {
                        key: vec!["id".into()],
                        update: vec![],
                    },
                },
                Dialect::Postgres,
            ),
            (
                my_table(),
                &my_cols,
                InsertOptions::default(),
                Dialect::MYSQL,
            ),
            (
                my_table(),
                &my_cols,
                upsert(&["id"], &["name"]),
                Dialect::MYSQL,
            ),
            (my_table(), &my_cols, upsert(&["id"], &[]), NBE),
        ];
        for (t, cols, opts, d) in cases {
            let whole = insert_sql_with(&t, cols, &rows, &opts, d).unwrap();
            let parts = format!(
                "{}{}, {}{}",
                insert_head(&t, cols, &opts, d).unwrap(),
                row_tuple(cols, &rows[0], d).unwrap(),
                row_tuple(cols, &rows[1], d).unwrap(),
                insert_tail(cols, &opts, d).unwrap()
            );
            assert_eq!(parts, whole, "{opts:?} {d:?}");
        }
        // MySQL all-default rows.
        assert_eq!(
            insert_head(&my_table(), &[], &InsertOptions::default(), Dialect::MYSQL).unwrap()
                + &row_tuple(&[], &[], Dialect::MYSQL).unwrap(),
            "INSERT INTO `shop`.`users` () VALUES ()"
        );
        // A row with the wrong number of values.
        assert!(matches!(
            row_tuple(&pg_cols, &[int(1)], Dialect::Postgres),
            Err(BuildError::Invalid { .. })
        ));
        // PG all-default rows have no tuple form.
        assert!(matches!(
            insert_head(
                &pg_table(),
                &[],
                &InsertOptions::default(),
                Dialect::Postgres
            ),
            Err(BuildError::Invalid { .. })
        ));
        assert!(matches!(
            insert_sql_with(
                &pg_table(),
                &pg_cols,
                &[],
                &InsertOptions::default(),
                Dialect::Postgres
            ),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn insert_sql_with_default_options_equals_insert_sql() {
        let opts = InsertOptions::default();
        let pg_cols = [id_pg(), name_pg()];
        let my_cols = [id_my(), name_my("utf8mb4_bin")];
        let rows = vec![vec![int(1), text("a\\'b")], vec![int(2), null()]];
        assert_eq!(
            insert_sql_with(&pg_table(), &pg_cols, &rows, &opts, Dialect::Postgres).unwrap(),
            insert_sql(&pg_table(), &pg_cols, &rows, &[], Dialect::Postgres).unwrap()
        );
        for d in [Dialect::MYSQL, NBE] {
            assert_eq!(
                insert_sql_with(&my_table(), &my_cols, &rows, &opts, d).unwrap(),
                insert_sql(&my_table(), &my_cols, &rows, &[], d).unwrap()
            );
        }
        assert_eq!(
            insert_sql_with(&my_table(), &[], &[vec![], vec![]], &opts, Dialect::MYSQL).unwrap(),
            insert_sql(&my_table(), &[], &[vec![], vec![]], &[], Dialect::MYSQL).unwrap()
        );
        // The dialect must match the table, as for `insert_sql`.
        assert!(matches!(
            insert_sql_with(&pg_table(), &pg_cols, &rows, &opts, Dialect::MYSQL),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn dump_row_tuple_uses_dump_literals() {
        let cols = [id_pg(), c("tags", "text[]", LogicalType::Array, None)];
        let row = [
            int(1),
            fv(Value::Array(vec![Value::Text("a b".into()), Value::Null])),
        ];
        assert_eq!(
            dump_row_tuple(&cols, &row, Dialect::Postgres).unwrap(),
            "(1, '{\"a b\",NULL}'::text[])"
        );
        assert!(matches!(
            row_tuple(&cols, &row, Dialect::Postgres),
            Err(BuildError::Unsupported { .. })
        ));
    }

    #[test]
    fn raw_where_clause_wraps_so_a_line_comment_cannot_eat_the_rest() {
        assert_eq!(
            raw_where_clause("id > 1 -- newest", Dialect::MYSQL).unwrap(),
            WhereClause {
                sql: " WHERE (id > 1 -- newest\n)".into(),
                params: vec![],
            }
        );
        assert!(matches!(
            raw_where_clause("1=1; DROP TABLE t", Dialect::Postgres),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn dialect_must_match_the_table() {
        assert!(matches!(
            delete_sql(
                &pg_table(),
                &key(vec![(id_pg(), int(1))]),
                &RowGuard::KeyOnly,
                Dialect::MYSQL
            ),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn raw_where_rejects_statement_breaks() {
        for d in [Dialect::MYSQL, NBE, Dialect::Postgres] {
            for bad in [
                "1=1; DROP TABLE t",
                "id IN (SELECT 1) INTO @x",
                "(a=1",
                "a=1)",
                "a=1) OR (1=1",
                "DELETE",
                "x = 1 AND delete from t",
                "id IN (SELECT id FROM u INTO @x)",
                "1=1 /* unterminated",
                "a = 'unterminated",
                "",
                "  -- only a comment",
                "set",
                "Truncate t",
            ] {
                assert!(
                    matches!(check_raw_where(bad, d), Err(BuildError::Invalid { .. })),
                    "{d:?} {bad:?}"
                );
            }
        }
        // MySQL executable comments run their body: refused.
        assert!(check_raw_where("1=1 /*!50000 ; DROP TABLE t */", Dialect::MYSQL).is_err());
        // PostgreSQL ends a `--` comment at `\r` too: what follows is live SQL.
        assert!(matches!(
            check_raw_where("1=1 --\r) ; DROP TABLE t; SELECT (1", Dialect::Postgres),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn raw_where_accepts_subqueries_and_functions() {
        for d in [Dialect::MYSQL, NBE, Dialect::Postgres] {
            for good in [
                "id IN (SELECT id FROM u WHERE x>1)",
                "lower(name) = 'a'",
                "name = 'x; DROP TABLE t'",
                "note = 'insert into'",
                "a = 1 -- trailing comment",
                "updated_at > '2024-01-01'",
            ] {
                assert_eq!(check_raw_where(good, d), Ok(()), "{d:?} {good:?}");
            }
        }
        assert_eq!(
            check_raw_where("body = $$a;b) into$$", Dialect::Postgres),
            Ok(())
        );
    }

    #[test]
    fn check_expression_rejects_statements() {
        for d in [Dialect::MYSQL, NBE, Dialect::Postgres] {
            for bad in [
                "1; DROP TABLE t",
                "(select 1)",
                "SELECT 1",
                "a > (SELECT max(b) FROM t)",
                "",
                "(a",
                "a)",
                "drop table t",
                "1 INTO @x",
                "a -- trailing comment",
                "a /* c */ + 1",
                // Several clauses in one fragment.
                "integer, DISABLE TRIGGER ALL",
                "c::int, OWNER TO x",
                "int, ADD COLUMN evil int",
                "a, b",
                // Brackets must balance and nest with parentheses.
                "a]",
                "ARRAY[1, 2",
                "ARRAY[(1], 2)",
                "ARRAY[1], 2",
            ] {
                assert!(
                    matches!(check_expression(bad, d), Err(BuildError::Invalid { .. })),
                    "{d:?} {bad:?}"
                );
            }
            for good in [
                "now()",
                "a > 0",
                "lower(name) = 'select; drop'",
                "CURRENT_TIMESTAMP",
                "(price * qty)",
                "integer[]",
                "numeric(10,2)",
                "coalesce(a, b, 0)",
                "ARRAY[1, 2]",
                "'{1,2}'::integer[]",
                "f(ARRAY[1, 2], 3)",
            ] {
                assert_eq!(check_expression(good, d), Ok(()), "{d:?} {good:?}");
            }
        }
        // MySQL: every sql_mode lexing must agree, whatever mode the caller passes.
        // Default mode reads each of these as one string literal; NO_BACKSLASH_ESCAPES
        // (backslash) or ANSI_QUOTES (double quote) ends it early and exposes a clause.
        for d in [Dialect::MYSQL, NBE] {
            for bad in [r"'\' , ADD COLUMN x int, '", r#""\" , ADD COLUMN x int, ""#] {
                assert!(
                    matches!(check_expression(bad, d), Err(BuildError::Invalid { .. })),
                    "{d:?} {bad:?}"
                );
            }
        }
        assert_eq!(
            check_expression("concat('a', \"b\")", Dialect::MYSQL),
            Ok(())
        );
        assert!(check_expression("1 /*!50000 + 1 */", Dialect::MYSQL).is_err());
        assert!(check_expression("a # comment", Dialect::MYSQL).is_err());
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
    fn mysql_quote_has_no_introducer() {
        assert_eq!(mysql_quote("it's C:\\x", MY.2).unwrap(), r"'it\'s C:\\x'");
        assert_eq!(
            mysql_quote("it's C:\\x", MY_NBE.2).unwrap(),
            r"'it''s C:\x'"
        );
        assert_eq!(mysql_quote("a\0b", MY.2).unwrap(), r"'a\0b'");
        assert!(mysql_quote("a\0b", MY_NBE.2).is_err());
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

    fn dump(v: FullValue, c: Ctx) -> Result<String, BuildError> {
        dump_literal(&v, &col(&c), c.2)
    }

    #[test]
    fn dump_literal_pg_array_and_other() {
        let ints = full(Value::Array(vec![
            Value::Int(1),
            Value::Null,
            Value::Int(3),
        ]));
        assert_eq!(
            dump(
                ints,
                Ctx("integer[]", LogicalType::Array, Dialect::Postgres)
            )
            .unwrap(),
            "'{1,NULL,3}'::integer[]"
        );
        let mixed = full(Value::Array(vec![
            Value::Int(1),
            Value::Null,
            Value::Text("a b".into()),
        ]));
        assert_eq!(
            dump(mixed, Ctx("text[]", LogicalType::Array, Dialect::Postgres)).unwrap(),
            "'{1,NULL,\"a b\"}'::text[]"
        );
        // A quote inside an item is doubled by the SQL literal, not the array text.
        let quoted = full(Value::Array(vec![Value::Text("it's".into())]));
        assert_eq!(
            dump(quoted, Ctx("text[]", LogicalType::Array, Dialect::Postgres)).unwrap(),
            "'{it''s}'::text[]"
        );
        let other = full(Value::Other {
            type_name: "interval".into(),
            text: "1 day".into(),
        });
        assert_eq!(dump(other, PG_INTERVAL).unwrap(), "'1 day'::interval");
        // Everything else is `literal`.
        assert_eq!(dump(t("x"), PG_ENUM).unwrap(), "'x'::order_status");
        assert_eq!(dump(full(Value::Null), PG_ARR).unwrap(), "NULL");
        assert_eq!(
            dump(FullValue::bytes(vec![1]), PG_BYTEA).unwrap(),
            r"'\x01'::bytea"
        );
        // The cast target is rendered from structure, never spliced in.
        let other = full(Value::Other {
            type_name: "x".into(),
            text: "1".into(),
        });
        assert!(matches!(
            dump(
                other,
                Ctx("int; DROP TABLE t", LogicalType::Other, Dialect::Postgres)
            ),
            Err(BuildError::Unsupported { .. })
        ));
        // NUL cannot be stored in PG text.
        let nul = full(Value::Array(vec![Value::Text("a\0".into())]));
        assert!(matches!(
            dump(nul, Ctx("text[]", LogicalType::Array, Dialect::Postgres)),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn dump_literal_mysql_other_unsupported() {
        let other = full(Value::Other {
            type_name: "geometry".into(),
            text: "POINT(1 2)".into(),
        });
        assert!(matches!(
            dump(other, MY_GEOM),
            Err(BuildError::Unsupported { .. })
        ));
        let arr = full(Value::Array(vec![Value::Int(1)]));
        assert!(matches!(
            dump(arr, Ctx("json", LogicalType::Json, Dialect::MYSQL)),
            Err(BuildError::Unsupported { .. })
        ));
        assert_eq!(dump(t("a"), MY).unwrap(), "_utf8mb4'a'");
    }

    #[test]
    fn pg_array_text_quoting() {
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(pg_array_text(&[]).unwrap(), "{}");
        assert_eq!(
            pg_array_text(&[
                text("plain"),
                text(""),
                text("null"),
                text("NULL"),
                text("a,b"),
                text("{x}"),
                text("say \"hi\""),
                text("back\\slash"),
                text("tab\there"),
                text("Nguyễn"),
                Value::Null,
            ])
            .unwrap(),
            r#"{plain,"","null","NULL","a,b","{x}","say \"hi\"","back\\slash","tab"#.to_string()
                + "\there\",Nguyễn,NULL}"
        );
        // Nested arrays, numbers, booleans, floats.
        assert_eq!(
            pg_array_text(&[
                Value::Array(vec![Value::Int(1), Value::UInt(2)]),
                Value::Array(vec![Value::Float(1.5), Value::Float(f64::NAN)]),
            ])
            .unwrap(),
            "{{1,2},{1.5,NaN}}"
        );
        assert_eq!(
            pg_array_text(&[Value::Bool(true), Value::Bool(false)]).unwrap(),
            "{true,false}"
        );
        // bytea items in their text form; incomplete ones refuse the whole array.
        assert_eq!(
            pg_array_text(&[Value::bytes(&[0, 0xff])]).unwrap(),
            r#"{"\\x00ff"}"#
        );
        assert_eq!(
            pg_array_text(&[Value::Array(vec![Value::Bytes {
                len: 10,
                preview: vec![1]
            }])]),
            None
        );
    }
}
