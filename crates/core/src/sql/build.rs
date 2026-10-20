//! SQL text built from values: the one place a cell value becomes SQL (spec §4.1).
//!
//! v1 renders literals instead of bind parameters, so the statement a preview shows is
//! the statement sent. Every value goes through [`literal`]; nothing else formats a value
//! into SQL.

use crate::server::DbKind;
use crate::sql::ident::{self, qualified_name, quote_ident};
use crate::sql::Dialect;
use crate::sql::TokKind;
use crate::value::{FullValue, LogicalType, Value};
use serde::{Deserialize, Serialize};

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

// ---------------------------------------------------------------------------------------
// Statements: filtered SELECT and key-based DML.
// ---------------------------------------------------------------------------------------

/// The table a statement reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub kind: DbKind,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    StartsWith,
    EndsWith,
    IsNull,
    IsNotNull,
    In,
    Between,
}

/// One condition of a structured filter: `column op values`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct FilterCond {
    pub column: String,
    pub op: FilterOp,
    pub values: Vec<Value>,
    /// Pattern operators on PostgreSQL use `ILIKE` (MySQL follows the column collation).
    pub case_insensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum FilterJoin {
    And,
    Or,
}

/// The data view's filter: structured conditions, or a raw `WHERE` text checked by
/// [`check_raw_where`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Filter {
    Conditions {
        join: FilterJoin,
        conds: Vec<FilterCond>,
    },
    Raw {
        where_sql: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub limit: u64,
    pub offset: u64,
}

/// Most conditions in one structured filter.
pub const MAX_FILTER_CONDITIONS: usize = crate::sql::filter::MAX_CONDITIONS;
/// Most values of one `In` condition.
pub const MAX_IN_VALUES: usize = 1000;

/// Words that start a write or DDL statement: refused at depth 0 of a raw `WHERE`.
const RAW_WHERE_FORBIDDEN: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP", "ALTER", "TRUNCATE", "GRANT",
    "REVOKE", "CALL", "DO", "COPY", "SET",
];

fn kind_of(d: Dialect) -> DbKind {
    match d {
        Dialect::MySql(_) => DbKind::MySql,
        Dialect::Postgres => DbKind::Postgres,
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

/// `SELECT [<extra_cols>, ]* FROM <relation>[ WHERE …][ ORDER BY …] LIMIT … OFFSET …`.
///
/// `extra_cols` are expressions the backend chose (`xmin::text`, quoted hidden key
/// columns), spliced as is and placed before `*`; on MySQL the `*` is then qualified
/// (`` `db`.`t`.* ``), since MySQL refuses a bare `*` after other items. `cols` are the
/// table's columns, which the filter's conditions must name exactly; `order` lists
/// `(column, ascending)`.
pub fn select_sql(
    t: &TableRef,
    extra_cols: &[String],
    filter: Option<&Filter>,
    cols: &[ColumnTarget],
    order: &[(String, bool)],
    page: Page,
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    let where_sql = where_clause(filter, cols, d)?;
    let list = if extra_cols.is_empty() {
        "*".to_string()
    } else {
        let star = match d {
            Dialect::MySql(_) => format!("{relation}.*"),
            Dialect::Postgres => "*".into(),
        };
        format!("{}, {star}", extra_cols.join(", "))
    };
    Ok(format!(
        "SELECT {list} FROM {relation}{where_sql}{}{}",
        ident::order_by_clause(kind_of(d), order),
        ident::page_clause(page.limit, page.offset)
    ))
}

/// `SELECT COUNT(*) FROM <relation>[ WHERE …]`, the filter as in [`select_sql`].
pub fn count_filtered_sql(
    t: &TableRef,
    filter: Option<&Filter>,
    cols: &[ColumnTarget],
    d: Dialect,
) -> Result<String, BuildError> {
    let relation = relation_for(t, d)?;
    let where_sql = where_clause(filter, cols, d)?;
    Ok(ident::count_where_sql(&relation, &where_sql))
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

/// The ` WHERE …` of a filter (with its leading space), or empty.
fn where_clause(
    filter: Option<&Filter>,
    cols: &[ColumnTarget],
    d: Dialect,
) -> Result<String, BuildError> {
    match filter {
        None => Ok(String::new()),
        Some(Filter::Raw { where_sql }) => {
            check_raw_where(where_sql, d)?;
            // The newline ends a trailing line comment before the closing parenthesis.
            Ok(format!(" WHERE ({where_sql}\n)"))
        }
        Some(Filter::Conditions { join, conds }) => {
            if conds.is_empty() {
                return Ok(String::new());
            }
            if conds.len() > MAX_FILTER_CONDITIONS {
                return Err(BuildError::Invalid {
                    column: String::new(),
                    reason: format!("a filter has at most {MAX_FILTER_CONDITIONS} conditions"),
                });
            }
            let parts = conds
                .iter()
                .map(|c| condition(c, cols, d).map(|s| format!("({s})")))
                .collect::<Result<Vec<_>, _>>()?;
            let joiner = match join {
                FilterJoin::And => " AND ",
                FilterJoin::Or => " OR ",
            };
            Ok(format!(" WHERE {}", parts.join(joiner)))
        }
    }
}

/// `s` as a complete text value (text is always complete).
fn text_value(s: &str) -> Result<FullValue, BuildError> {
    FullValue::try_from_value(Value::Text(s.into())).map_err(|_| BuildError::NotFull)
}

/// A text target named like `col`: pattern and text comparisons render the value as a
/// plain string literal (no PG cast).
fn text_target(col: &ColumnTarget) -> ColumnTarget {
    ColumnTarget {
        name: col.name.clone(),
        db_type: "text".into(),
        logical: LogicalType::Text,
        collation: None,
    }
}

fn condition(c: &FilterCond, cols: &[ColumnTarget], d: Dialect) -> Result<String, BuildError> {
    let Some(col) = cols.iter().find(|t| t.name == c.column) else {
        return Err(BuildError::Invalid {
            column: c.column.clone(),
            reason: "not a column of this table".into(),
        });
    };
    let (min, max) = match c.op {
        FilterOp::IsNull | FilterOp::IsNotNull => (0, 0),
        FilterOp::Between => (2, 2),
        FilterOp::In => (1, MAX_IN_VALUES),
        _ => (1, 1),
    };
    let n = c.values.len();
    if n < min || n > max {
        let want = match (min, max) {
            (0, 0) => "no value".to_string(),
            (a, b) if a == b => format!("{a} value{}", if a == 1 { "" } else { "s" }),
            (a, b) => format!("{a} to {b} values"),
        };
        return Err(invalid(
            col,
            &format!("the condition takes {want}, not {n}"),
        ));
    }
    if c.values.iter().any(|v| matches!(v, Value::Null)) {
        return Err(invalid(
            col,
            "compare with NULL using is null / is not null",
        ));
    }
    let pg = d == Dialect::Postgres;
    let q = quote_ident(kind_of(d), &col.name);
    // PG types without equality / ordering compare as text.
    let (cmp, target) = if pg && crate::sql::filter::pg_unordered(&col.db_type) {
        (format!("{q}::text"), text_target(col))
    } else {
        (q.clone(), col.clone())
    };
    let lit = |v: &Value| -> Result<String, BuildError> {
        let full = FullValue::try_from_value(v.clone()).map_err(|_| BuildError::NotFull)?;
        literal(&full, &target, d)
    };
    Ok(match c.op {
        FilterOp::Eq => format!("{cmp} = {}", lit(&c.values[0])?),
        FilterOp::Ne => format!("{cmp} <> {}", lit(&c.values[0])?),
        FilterOp::Lt => format!("{cmp} < {}", lit(&c.values[0])?),
        FilterOp::Le => format!("{cmp} <= {}", lit(&c.values[0])?),
        FilterOp::Gt => format!("{cmp} > {}", lit(&c.values[0])?),
        FilterOp::Ge => format!("{cmp} >= {}", lit(&c.values[0])?),
        FilterOp::In => {
            let list = c.values.iter().map(lit).collect::<Result<Vec<_>, _>>()?;
            format!("{cmp} IN ({})", list.join(", "))
        }
        FilterOp::Between => format!(
            "{cmp} BETWEEN {} AND {}",
            lit(&c.values[0])?,
            lit(&c.values[1])?
        ),
        FilterOp::IsNull => format!("{q} IS NULL"),
        FilterOp::IsNotNull => format!("{q} IS NOT NULL"),
        FilterOp::Contains | FilterOp::StartsWith | FilterOp::EndsWith => pattern(c, col, &q, d)?,
    })
}

/// `contains` / `starts with` / `ends with`: `LIKE` (PG `ILIKE` when case-insensitive)
/// on the column's text, the value matched literally. The escape character is `\`
/// (MySQL default mode writes it `'\\'`), or `!` under MySQL `NO_BACKSLASH_ESCAPES`.
fn pattern(c: &FilterCond, col: &ColumnTarget, q: &str, d: Dialect) -> Result<String, BuildError> {
    let text = match &c.values[0] {
        Value::Text(s)
        | Value::Uuid(s)
        | Value::Decimal(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s)
        | Value::Json(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        _ => return Err(invalid(col, "a pattern needs a text value")),
    };
    let (esc, esc_sql) = match d {
        Dialect::MySql(m) if m.no_backslash_escapes => ('!', "'!'"),
        Dialect::MySql(_) => ('\\', "'\\\\'"),
        Dialect::Postgres => ('\\', "'\\'"),
    };
    let mut escaped = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        if ch == esc || ch == '%' || ch == '_' {
            escaped.push(esc);
        }
        escaped.push(ch);
    }
    let pat = match c.op {
        FilterOp::Contains => format!("%{escaped}%"),
        FilterOp::StartsWith => format!("{escaped}%"),
        _ => format!("%{escaped}"),
    };
    let lit = literal(&text_value(&pat)?, &text_target(col), d)?;
    let (subject, like) = match d {
        Dialect::Postgres if c.case_insensitive => (format!("{q}::text"), "ILIKE"),
        Dialect::Postgres => (format!("{q}::text"), "LIKE"),
        Dialect::MySql(_) => (q.to_string(), "LIKE"),
    };
    Ok(format!("{subject} {like} {lit} ESCAPE {esc_sql}"))
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
    let bad = |reason: &str| {
        Err(BuildError::Invalid {
            column: "WHERE".into(),
            reason: reason.into(),
        })
    };
    // Lexed as it will be sent: an open string or comment swallows the `)`.
    let wrapped = format!("({where_sql}\n)");
    let toks = crate::sql::tokenize(&wrapped, d);
    let closes_at = wrapped.len() - 1;
    if !matches!(toks.last(), Some(t) if t.kind == TokKind::Close && t.start == closes_at) {
        return bad("a string, quoted name or comment is not closed");
    }
    let inner = &toks[1..toks.len() - 1];
    if inner.is_empty() {
        return bad("the condition is empty");
    }
    let mut depth = 0usize;
    for t in inner {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => {
                if depth == 0 {
                    return bad("unbalanced parentheses");
                }
                depth -= 1;
            }
            TokKind::Punct(b';') => return bad("a ';' cannot appear in a filter"),
            TokKind::ExecComment => return bad("executable comments are not allowed"),
            TokKind::Word(w) => {
                if w.eq_ignore_ascii_case("INTO") {
                    return bad("INTO is not allowed in a filter");
                }
                if depth == 0
                    && RAW_WHERE_FORBIDDEN
                        .iter()
                        .any(|k| w.eq_ignore_ascii_case(k))
                {
                    return bad(&format!(
                        "{} is not allowed in a filter",
                        w.to_ascii_uppercase()
                    ));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return bad("unbalanced parentheses");
    }
    Ok(())
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
            kind: DbKind::MySql,
            database: "shop".into(),
            schema: None,
            name: "users".into(),
        }
    }
    fn pg_table() -> TableRef {
        TableRef {
            kind: DbKind::Postgres,
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
    fn cond(column: &str, op: FilterOp, values: Vec<Value>) -> FilterCond {
        FilterCond {
            column: column.into(),
            op,
            values,
            case_insensitive: false,
        }
    }
    fn and(conds: Vec<FilterCond>) -> Filter {
        Filter::Conditions {
            join: FilterJoin::And,
            conds,
        }
    }
    fn page() -> Page {
        Page {
            limit: 100,
            offset: 0,
        }
    }
    /// The WHERE clause a filter gives on the PG `users` table.
    fn pg_where(f: &Filter) -> Result<String, BuildError> {
        count_filtered_sql(
            &pg_table(),
            Some(f),
            &[id_pg(), name_pg()],
            Dialect::Postgres,
        )
        .map(|s| s.replace("SELECT COUNT(*) FROM \"public\".\"users\"", ""))
    }
    fn my_where(f: &Filter, d: Dialect) -> Result<String, BuildError> {
        let cols = [id_my(), name_my("utf8mb4_0900_ai_ci")];
        count_filtered_sql(&my_table(), Some(f), &cols, d)
            .map(|s| s.replace("SELECT COUNT(*) FROM `shop`.`users`", ""))
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
        // Nothing to set is an error, not `UPDATE … SET WHERE`.
        assert!(matches!(
            update_sql(&pg_table(), &k, &[], &RowGuard::KeyOnly, Dialect::Postgres),
            Err(BuildError::Invalid { .. })
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

    #[test]
    fn like_escapes_wildcards() {
        let contains = |v: &str, ci: bool| {
            let mut c = cond("name", FilterOp::Contains, vec![Value::Text(v.into())]);
            c.case_insensitive = ci;
            and(vec![c])
        };
        assert_eq!(
            pg_where(&contains("50%_off", false)).unwrap(),
            " WHERE (\"name\"::text LIKE '%50\\%\\_off%' ESCAPE '\\')"
        );
        assert_eq!(
            pg_where(&contains("a\\b", true)).unwrap(),
            " WHERE (\"name\"::text ILIKE '%a\\\\b%' ESCAPE '\\')"
        );
        // MySQL default mode: the backslashes are doubled again by the string literal,
        // and the escape character itself is written `'\\'`.
        assert_eq!(
            my_where(&contains("50%_off\\", false), Dialect::MYSQL).unwrap(),
            " WHERE (`name` LIKE _utf8mb4'%50\\\\%\\\\_off\\\\\\\\%' ESCAPE '\\\\')"
        );
        // NO_BACKSLASH_ESCAPES: `!` is the escape character.
        assert_eq!(
            my_where(&contains("50%_off!\\", true), NBE).unwrap(),
            " WHERE (`name` LIKE _utf8mb4'%50!%!_off!!\\%' ESCAPE '!')"
        );
        let op = |op: FilterOp| {
            pg_where(&and(vec![cond("name", op, vec![Value::Text("x%".into())])])).unwrap()
        };
        assert_eq!(
            op(FilterOp::StartsWith),
            " WHERE (\"name\"::text LIKE 'x\\%%' ESCAPE '\\')"
        );
        assert_eq!(
            op(FilterOp::EndsWith),
            " WHERE (\"name\"::text LIKE '%x\\%' ESCAPE '\\')"
        );
        // A pattern on a number column matches its text.
        assert_eq!(
            pg_where(&and(vec![cond(
                "id",
                FilterOp::Contains,
                vec![Value::Int(5)]
            )]))
            .unwrap(),
            " WHERE (\"id\"::text LIKE '%5%' ESCAPE '\\')"
        );
    }

    #[test]
    fn filter_operators_render_literals_by_column_type() {
        let f = Filter::Conditions {
            join: FilterJoin::Or,
            conds: vec![
                cond("id", FilterOp::Eq, vec![Value::Text("5".into())]),
                cond("id", FilterOp::Ne, vec![Value::Int(5)]),
                cond("id", FilterOp::Lt, vec![Value::Int(5)]),
                cond("id", FilterOp::Le, vec![Value::Int(5)]),
                cond("id", FilterOp::Gt, vec![Value::Int(5)]),
                cond("id", FilterOp::Ge, vec![Value::Int(5)]),
                cond("id", FilterOp::In, vec![Value::Int(1), Value::Int(2)]),
                cond("id", FilterOp::Between, vec![Value::Int(1), Value::Int(9)]),
                cond("name", FilterOp::IsNull, vec![]),
                cond("name", FilterOp::IsNotNull, vec![]),
                cond("name", FilterOp::Eq, vec![Value::Text("it's".into())]),
            ],
        };
        assert_eq!(
            pg_where(&f).unwrap(),
            " WHERE (\"id\" = '5'::int4) OR (\"id\" <> 5) OR (\"id\" < 5) OR (\"id\" <= 5) \
             OR (\"id\" > 5) OR (\"id\" >= 5) OR (\"id\" IN (1, 2)) OR (\"id\" BETWEEN 1 AND 9) \
             OR (\"name\" IS NULL) OR (\"name\" IS NOT NULL) OR (\"name\" = 'it''s')"
        );
        // PG types without equality compare as text.
        let doc = c("doc", "json", LogicalType::Json, None);
        let sql = count_filtered_sql(
            &pg_table(),
            Some(&and(vec![cond(
                "doc",
                FilterOp::Eq,
                vec![Value::Json("{}".into())],
            )])),
            &[doc],
            Dialect::Postgres,
        )
        .unwrap();
        assert!(sql.ends_with(" WHERE (\"doc\"::text = '{}')"), "{sql}");
        // No conditions: no WHERE.
        assert_eq!(pg_where(&and(vec![])).unwrap(), "");
    }

    #[test]
    fn filter_rejects_bad_conditions() {
        let bad = [
            cond("nope", FilterOp::IsNull, vec![]),
            cond("ID", FilterOp::IsNull, vec![]),
            cond("id", FilterOp::Eq, vec![]),
            cond("id", FilterOp::Eq, vec![Value::Int(1), Value::Int(2)]),
            cond("id", FilterOp::Between, vec![Value::Int(1)]),
            cond("id", FilterOp::IsNull, vec![Value::Int(1)]),
            cond("id", FilterOp::In, vec![]),
            cond("id", FilterOp::In, vec![Value::Int(1); 1001]),
            cond("id", FilterOp::Eq, vec![Value::Null]),
            cond("name", FilterOp::Contains, vec![Value::Bool(true)]),
        ];
        for b in bad {
            let r = pg_where(&and(vec![b.clone()]));
            assert!(matches!(r, Err(BuildError::Invalid { .. })), "{b:?}: {r:?}");
        }
        assert!(pg_where(&and(vec![cond(
            "id",
            FilterOp::In,
            vec![Value::Int(1); 1000]
        )]))
        .is_ok());
        // A cut value is never compared.
        let cut = Value::Bytes {
            len: 100,
            preview: vec![0; 64],
        };
        assert_eq!(
            pg_where(&and(vec![cond("id", FilterOp::Eq, vec![cut])])),
            Err(BuildError::NotFull)
        );
    }

    #[test]
    fn select_puts_extra_cols_first() {
        let f = and(vec![cond("id", FilterOp::Eq, vec![Value::Int(5)])]);
        assert_eq!(
            select_sql(
                &pg_table(),
                &["xmin::text".into()],
                Some(&f),
                &[id_pg()],
                &[("id".into(), true)],
                page(),
                Dialect::Postgres,
            )
            .unwrap(),
            "SELECT xmin::text, * FROM \"public\".\"users\" WHERE (\"id\" = 5) \
             ORDER BY \"id\" ASC LIMIT 100 OFFSET 0"
        );
        // MySQL only accepts `*` next to other items when it is qualified.
        assert_eq!(
            select_sql(
                &my_table(),
                &["`hid`".into()],
                None,
                &[],
                &[("n`x".into(), false)],
                Page {
                    limit: 5,
                    offset: 10
                },
                Dialect::MYSQL,
            )
            .unwrap(),
            "SELECT `hid`, `shop`.`users`.* FROM `shop`.`users` ORDER BY `n``x` DESC \
             LIMIT 5 OFFSET 10"
        );
        assert_eq!(
            select_sql(&my_table(), &[], None, &[], &[], page(), Dialect::MYSQL).unwrap(),
            "SELECT * FROM `shop`.`users` LIMIT 100 OFFSET 0"
        );
    }

    #[test]
    fn raw_where_is_wrapped_so_a_line_comment_cannot_eat_the_rest() {
        let f = Filter::Raw {
            where_sql: "id > 1 -- newest".into(),
        };
        assert_eq!(
            select_sql(&my_table(), &[], Some(&f), &[], &[], page(), Dialect::MYSQL).unwrap(),
            "SELECT * FROM `shop`.`users` WHERE (id > 1 -- newest\n) LIMIT 100 OFFSET 0"
        );
        let bad = Filter::Raw {
            where_sql: "1=1; DROP TABLE t".into(),
        };
        assert!(matches!(
            count_filtered_sql(&my_table(), Some(&bad), &[], Dialect::MYSQL),
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
    fn dialect_must_match_the_table() {
        assert!(matches!(
            select_sql(&pg_table(), &[], None, &[], &[], page(), Dialect::MYSQL),
            Err(BuildError::Invalid { .. })
        ));
    }

    #[test]
    fn filter_serde_shape() {
        let f: Filter = serde_json::from_str(
            r#"{"mode":"conditions","join":"or","conds":[
                {"column":"a","op":"starts_with","values":[{"t":"Text","v":"x"}],"case_insensitive":true}]}"#,
        )
        .unwrap();
        assert!(
            matches!(f, Filter::Conditions { join: FilterJoin::Or, ref conds }
            if conds[0].op == FilterOp::StartsWith && conds[0].case_insensitive)
        );
        let raw: Filter = serde_json::from_str(r#"{"mode":"raw","where_sql":"a=1"}"#).unwrap();
        assert!(matches!(raw, Filter::Raw { ref where_sql } if where_sql == "a=1"));
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
