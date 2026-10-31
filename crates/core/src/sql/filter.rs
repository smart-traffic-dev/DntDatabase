//! The data view's filter and sort: a structured AST from the UI, turned into a `WHERE`
//! clause with bound parameters here. The UI never sends SQL text.
//!
//! # Safety
//!
//! - Every column must be a column of the object, by exact name (checked against the
//!   table detail the caller passes); it is quoted with [`quote_ident`].
//! - Every value is a bound parameter (`?` on MySQL, `$n` on PostgreSQL), never part of
//!   the SQL text; operators and the `ESCAPE` character are fixed strings chosen here.
//!
//! # Parameter types
//!
//! Values are always sent as text ([`crate::driver::ExecOptions::params`]):
//!
//! - MySQL compares a string parameter with the column like a string literal
//!   (`int_col = '5'`, `date_col >= '2024-01-01'`, `uuid_char = '…'`): the usual MySQL
//!   conversions, as if the user had typed the literal.
//! - PostgreSQL infers the placeholder's type from the column (`"col" = $1` gives `$1`
//!   the type of `col`) and parses the text with that type's input function, exactly
//!   like a quoted literal: numbers, dates, timestamps, booleans (`t`, `true`, `1`…),
//!   uuid, enums, arrays (`{1,2}`), jsonb, inet all work, and a malformed value is the
//!   server's own `invalid input syntax` error. Types without an equality / ordering
//!   (`json`, `xml`, the geometric types: [`pg_unordered`]) are compared as text
//!   (`"col"::text = $1`). Pattern operators always compare `"col"::text`.
//!
//! # Pattern operators
//!
//! `LIKE` / `NOT LIKE` take the user's pattern as is (`%` and `_` are wildcards).
//! `contains`, `starts with` and `ends with` match the value literally: `%`, `_` and the
//! escape character `!` are escaped and the clause says `ESCAPE '!'` (a `!` needs no
//! escaping in either engine's string syntax, whatever `sql_mode` says). On PostgreSQL
//! they use `ILIKE` (case-insensitive, like the default MySQL collations).

use crate::error::DbError;
use crate::meta::ColumnDetail;
use crate::server::SqlKind;
use crate::sql::ident::quote_ident;
use serde::{Deserialize, Serialize};
use specta::Type;

/// Most conditions in one filter.
pub const MAX_CONDITIONS: usize = 50;
/// Most values of one `IN` list.
pub const MAX_IN_VALUES: usize = 1000;
/// Largest single value (bytes).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// Most sort keys.
pub const MAX_SORT_KEYS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

/// Server-side `ORDER BY` of one column (which must be a column of the object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SortSpec {
    pub column: String,
    pub dir: SortDir,
}

/// How the conditions of a filter combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    NotLike,
    Contains,
    StartsWith,
    EndsWith,
    /// One or more values.
    In,
    /// Exactly two values: low, high.
    Between,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    /// How many values the operator takes: `(min, max)`.
    pub fn arity(self) -> (usize, usize) {
        match self {
            FilterOp::IsNull | FilterOp::IsNotNull => (0, 0),
            FilterOp::Between => (2, 2),
            FilterOp::In => (1, MAX_IN_VALUES),
            _ => (1, 1),
        }
    }
}

/// One condition: `column op values`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FilterCond {
    pub column: String,
    pub op: FilterOp,
    pub values: Vec<String>,
}

/// The data view's filter: conditions joined with `AND` or `OR`, or (raw mode) the
/// user's own `WHERE` text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DataFilter {
    pub mode: FilterMode,
    pub conditions: Vec<FilterCond>,
    /// Raw mode: a `WHERE` condition typed by the user. `conditions` must then be empty
    /// and `mode` is ignored. It is checked with
    /// [`crate::sql::build::check_raw_where`] and run inside a read-only transaction; it
    /// never goes through [`build_where`]. Absent / null: structured mode.
    #[serde(default)]
    pub raw_where: Option<String>,
}

/// A `WHERE` clause (with its leading space, or empty) and its parameters, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WhereClause {
    pub sql: String,
    pub params: Vec<String>,
}

/// Whether PostgreSQL type `db_type` has no default ordering / equality: `json`, `xml`,
/// the geometric types, and arrays of them. MySQL compares every type.
pub fn pg_unordered(db_type: &str) -> bool {
    let t = db_type.trim().to_ascii_lowercase();
    let mut base = t.as_str();
    while let Some(b) = base.strip_suffix("[]") {
        base = b.trim_end();
    }
    // `pg_catalog.json`, `json(…)` and similar spellings.
    let base = base.rsplit('.').next().unwrap_or(base);
    let base = base.split('(').next().unwrap_or(base).trim();
    matches!(
        base,
        "json" | "xml" | "point" | "line" | "lseg" | "box" | "path" | "polygon" | "circle"
    )
}

/// `v` escaped for a `LIKE … ESCAPE '!'` pattern that matches it literally.
pub fn escape_like(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if matches!(c, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(c);
    }
    out
}

fn filter_error(message: String) -> DbError {
    DbError::Config {
        field: Some("filter".into()),
        message,
    }
}

struct Builder {
    kind: SqlKind,
    params: Vec<String>,
}

impl Builder {
    /// The placeholder of a new parameter `value`.
    fn param(&mut self, value: String) -> String {
        self.params.push(value);
        match self.kind {
            SqlKind::MySql => "?".into(),
            SqlKind::Postgres => format!("${}", self.params.len()),
        }
    }

    fn cond(&mut self, column: &ColumnDetail, c: &FilterCond) -> String {
        let q = quote_ident(self.kind, &column.name);
        let pg = match self.kind {
            SqlKind::MySql => false,
            SqlKind::Postgres => true,
        };
        // Comparisons: the column itself (PG infers the parameter's type from it), or
        // its text where PG has no operator for the type.
        let cmp = if pg && pg_unordered(&column.db_type) {
            format!("{q}::text")
        } else {
            q.clone()
        };
        // Pattern matching works on text.
        let text = if pg { format!("{q}::text") } else { q.clone() };
        let one = |b: &mut Self| b.param(c.values[0].clone());
        match c.op {
            FilterOp::Eq => format!("{cmp} = {}", one(self)),
            FilterOp::Ne => format!("{cmp} <> {}", one(self)),
            FilterOp::Lt => format!("{cmp} < {}", one(self)),
            FilterOp::Le => format!("{cmp} <= {}", one(self)),
            FilterOp::Gt => format!("{cmp} > {}", one(self)),
            FilterOp::Ge => format!("{cmp} >= {}", one(self)),
            FilterOp::Like => format!("{text} LIKE {}", one(self)),
            FilterOp::NotLike => format!("{text} NOT LIKE {}", one(self)),
            FilterOp::Contains | FilterOp::StartsWith | FilterOp::EndsWith => {
                let v = escape_like(&c.values[0]);
                let pattern = match c.op {
                    FilterOp::Contains => format!("%{v}%"),
                    FilterOp::StartsWith => format!("{v}%"),
                    _ => format!("%{v}"),
                };
                let like = if pg { "ILIKE" } else { "LIKE" };
                format!("{text} {like} {} ESCAPE '!'", self.param(pattern))
            }
            FilterOp::In => {
                let list: Vec<String> = c.values.iter().map(|v| self.param(v.clone())).collect();
                format!("{cmp} IN ({})", list.join(", "))
            }
            FilterOp::Between => {
                let lo = self.param(c.values[0].clone());
                let hi = self.param(c.values[1].clone());
                format!("{cmp} BETWEEN {lo} AND {hi}")
            }
            FilterOp::IsNull => format!("{q} IS NULL"),
            FilterOp::IsNotNull => format!("{q} IS NOT NULL"),
        }
    }
}

/// The `WHERE` clause of `filter` over an object with `columns`, for `kind`; empty
/// without a filter or conditions. PostgreSQL placeholders are numbered from `$1`.
///
/// Errors (`Config`, field `filter`): an unknown column (exact name), a wrong number of
/// values for the operator, too many conditions or values, a value over
/// [`MAX_VALUE_BYTES`].
pub fn build_where(
    kind: SqlKind,
    columns: &[ColumnDetail],
    filter: Option<&DataFilter>,
) -> Result<WhereClause, DbError> {
    let Some(filter) = filter else {
        return Ok(WhereClause::default());
    };
    if filter.raw_where.is_some() {
        // A raw filter is never silently dropped (or mixed with conditions).
        return Err(filter_error(
            "a raw WHERE filter is not a structured filter".into(),
        ));
    }
    if filter.conditions.is_empty() {
        return Ok(WhereClause::default());
    }
    if filter.conditions.len() > MAX_CONDITIONS {
        return Err(filter_error(format!(
            "a filter has at most {MAX_CONDITIONS} conditions"
        )));
    }
    let mut b = Builder {
        kind,
        params: Vec::new(),
    };
    let mut parts = Vec::with_capacity(filter.conditions.len());
    for c in &filter.conditions {
        let Some(column) = columns.iter().find(|d| d.name == c.column) else {
            return Err(filter_error(format!(
                "\"{}\" is not a column of this object",
                c.column
            )));
        };
        let (min, max) = c.op.arity();
        let n = c.values.len();
        if n < min || n > max {
            let want = match (min, max) {
                (0, 0) => "no value".to_string(),
                (a, b) if a == b => format!("{a} value{}", if a == 1 { "" } else { "s" }),
                (a, b) => format!("{a} to {b} values"),
            };
            return Err(filter_error(format!(
                "the condition on \"{}\" takes {want}, not {n}",
                c.column
            )));
        }
        if let Some(v) = c.values.iter().find(|v| v.len() > MAX_VALUE_BYTES) {
            return Err(filter_error(format!(
                "a filter value is too long ({} bytes; at most {MAX_VALUE_BYTES})",
                v.len()
            )));
        }
        parts.push(format!("({})", b.cond(column, c)));
    }
    let joiner = match filter.mode {
        FilterMode::And => " AND ",
        FilterMode::Or => " OR ",
    };
    Ok(WhereClause {
        sql: format!(" WHERE {}", parts.join(joiner)),
        params: b.params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, db_type: &str) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 1,
            db_type: db_type.into(),
            nullable: true,
            default: None,
            auto_increment: false,
            identity: None,
            generated: None,
            invisible: false,
            collation: None,
            comment: None,
            ..Default::default()
        }
    }

    fn cols() -> Vec<ColumnDetail> {
        vec![
            col("id", "integer"),
            col("name", "text"),
            col("doc", "json"),
            col("we`ird\"col", "varchar(10)"),
            col("born", "date"),
        ]
    }

    fn cond(column: &str, op: FilterOp, values: &[&str]) -> FilterCond {
        FilterCond {
            column: column.into(),
            op,
            values: values.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn filter(mode: FilterMode, conditions: Vec<FilterCond>) -> DataFilter {
        DataFilter {
            mode,
            conditions,
            raw_where: None,
        }
    }

    #[test]
    fn a_raw_filter_is_never_built_as_structured() {
        let mut raw = filter(FilterMode::And, vec![]);
        raw.raw_where = Some("id > 0".into());
        let (field, _) = field_of(build(SqlKind::MySql, &raw).unwrap_err());
        assert_eq!(field.as_deref(), Some("filter"));
        // Absent from the JSON: structured mode.
        let f: DataFilter = serde_json::from_str(r#"{"mode":"and","conditions":[]}"#).unwrap();
        assert_eq!(f.raw_where, None);
    }

    fn build(kind: SqlKind, f: &DataFilter) -> Result<WhereClause, DbError> {
        build_where(kind, &cols(), Some(f))
    }

    fn field_of(e: DbError) -> (Option<String>, String) {
        match e {
            DbError::Config { field, message } => (field, message),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_filter_or_no_conditions_is_no_where() {
        assert_eq!(
            build_where(SqlKind::MySql, &cols(), None).unwrap(),
            WhereClause::default()
        );
        let empty = filter(FilterMode::And, vec![]);
        assert_eq!(
            build(SqlKind::Postgres, &empty).unwrap(),
            WhereClause::default()
        );
    }

    #[test]
    fn every_operator_on_mysql() {
        use FilterOp::*;
        let cases: &[(FilterOp, &[&str], &str)] = &[
            (Eq, &["5"], "(`id` = ?)"),
            (Ne, &["5"], "(`id` <> ?)"),
            (Lt, &["5"], "(`id` < ?)"),
            (Le, &["5"], "(`id` <= ?)"),
            (Gt, &["5"], "(`id` > ?)"),
            (Ge, &["5"], "(`id` >= ?)"),
            (Like, &["a%"], "(`id` LIKE ?)"),
            (NotLike, &["a%"], "(`id` NOT LIKE ?)"),
            (Contains, &["x"], "(`id` LIKE ? ESCAPE '!')"),
            (StartsWith, &["x"], "(`id` LIKE ? ESCAPE '!')"),
            (EndsWith, &["x"], "(`id` LIKE ? ESCAPE '!')"),
            (In, &["1", "2", "3"], "(`id` IN (?, ?, ?))"),
            (Between, &["1", "9"], "(`id` BETWEEN ? AND ?)"),
            (IsNull, &[], "(`id` IS NULL)"),
            (IsNotNull, &[], "(`id` IS NOT NULL)"),
        ];
        for (op, values, want) in cases {
            let w = build(
                SqlKind::MySql,
                &filter(FilterMode::And, vec![cond("id", *op, values)]),
            )
            .unwrap();
            assert_eq!(w.sql, format!(" WHERE {want}"), "{op:?}");
            assert_eq!(w.params.len(), values.len(), "{op:?}");
        }
    }

    #[test]
    fn postgres_numbers_placeholders_and_casts_where_needed() {
        let f = filter(
            FilterMode::Or,
            vec![
                cond("id", FilterOp::Eq, &["5"]),
                cond("doc", FilterOp::Eq, &["{}"]),
                cond("name", FilterOp::Contains, &["50%_off!"]),
                cond("born", FilterOp::Between, &["2024-01-01", "2024-12-31"]),
                cond("id", FilterOp::In, &["1", "2"]),
                cond("name", FilterOp::Like, &["a_%"]),
                cond("doc", FilterOp::IsNull, &[]),
            ],
        );
        let w = build(SqlKind::Postgres, &f).unwrap();
        assert_eq!(
            w.sql,
            " WHERE (\"id\" = $1) OR (\"doc\"::text = $2) OR (\"name\"::text ILIKE $3 ESCAPE '!') \
             OR (\"born\" BETWEEN $4 AND $5) OR (\"id\" IN ($6, $7)) OR (\"name\"::text LIKE $8) \
             OR (\"doc\" IS NULL)"
        );
        assert_eq!(
            w.params,
            [
                "5",
                "{}",
                "%50!%!_off!!%",
                "2024-01-01",
                "2024-12-31",
                "1",
                "2",
                "a_%"
            ]
        );
    }

    #[test]
    fn like_escaping_for_contains_starts_and_ends() {
        assert_eq!(escape_like("a%b_c!d"), "a!%b!_c!!d");
        assert_eq!(escape_like("Tiếng Việt"), "Tiếng Việt");
        assert_eq!(escape_like(r"back\slash"), r"back\slash");
        let w = |op| {
            build(
                SqlKind::MySql,
                &filter(FilterMode::And, vec![cond("name", op, &["5%"])]),
            )
            .unwrap()
            .params
        };
        assert_eq!(w(FilterOp::Contains), ["%5!%%"]);
        assert_eq!(w(FilterOp::StartsWith), ["5!%%"]);
        assert_eq!(w(FilterOp::EndsWith), ["%5!%"]);
        // LIKE keeps the user's own wildcards.
        assert_eq!(w(FilterOp::Like), ["5%"]);
    }

    #[test]
    fn injection_in_values_stays_in_the_parameters() {
        let evil = "'; DROP TABLE orders; -- \" ` $1 ?";
        for kind in [SqlKind::MySql, SqlKind::Postgres] {
            let w = build(
                kind,
                &filter(
                    FilterMode::And,
                    vec![
                        cond("name", FilterOp::Eq, &[evil]),
                        cond("name", FilterOp::Contains, &[evil]),
                    ],
                ),
            )
            .unwrap();
            assert!(!w.sql.contains("DROP"), "{}", w.sql);
            assert_eq!(w.params[0], evil);
        }
    }

    #[test]
    fn identifiers_are_quoted_and_must_exist_exactly() {
        let w = build(
            SqlKind::MySql,
            &filter(
                FilterMode::And,
                vec![cond("we`ird\"col", FilterOp::Eq, &["x"])],
            ),
        )
        .unwrap();
        assert_eq!(w.sql, " WHERE (`we``ird\"col` = ?)");
        let w = build(
            SqlKind::Postgres,
            &filter(
                FilterMode::And,
                vec![cond("we`ird\"col", FilterOp::Eq, &["x"])],
            ),
        )
        .unwrap();
        assert_eq!(w.sql, " WHERE (\"we`ird\"\"col\" = $1)");
        for bad in [
            "ID",
            "id; DROP TABLE orders",
            "id` = 1 OR `id",
            "\"id\"",
            "",
            "id ",
        ] {
            let e = build(
                SqlKind::Postgres,
                &filter(FilterMode::And, vec![cond(bad, FilterOp::IsNull, &[])]),
            )
            .unwrap_err();
            let (field, message) = field_of(e);
            assert_eq!(field.as_deref(), Some("filter"), "{bad}");
            assert!(message.contains("is not a column"), "{message}");
        }
    }

    #[test]
    fn operator_arity_is_checked() {
        let bad: &[(FilterOp, &[&str])] = &[
            (FilterOp::Eq, &[]),
            (FilterOp::Eq, &["1", "2"]),
            (FilterOp::Between, &["1"]),
            (FilterOp::Between, &["1", "2", "3"]),
            (FilterOp::In, &[]),
            (FilterOp::IsNull, &["x"]),
            (FilterOp::Contains, &[]),
        ];
        for (op, values) in bad {
            let e = build(
                SqlKind::MySql,
                &filter(FilterMode::And, vec![cond("id", *op, values)]),
            )
            .unwrap_err();
            let (field, message) = field_of(e);
            assert_eq!(field.as_deref(), Some("filter"));
            assert!(message.contains("takes"), "{op:?}: {message}");
        }
        let many: Vec<String> = (0..=MAX_IN_VALUES).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(build(
            SqlKind::MySql,
            &filter(FilterMode::And, vec![cond("id", FilterOp::In, &refs)]),
        )
        .is_err());
        assert!(build(
            SqlKind::MySql,
            &filter(
                FilterMode::And,
                vec![cond("id", FilterOp::In, &refs[..MAX_IN_VALUES])]
            ),
        )
        .is_ok());
    }

    #[test]
    fn limits_on_conditions_and_value_size() {
        let conds = vec![cond("id", FilterOp::IsNull, &[]); MAX_CONDITIONS + 1];
        assert!(build(SqlKind::MySql, &filter(FilterMode::And, conds)).is_err());
        let long = "x".repeat(MAX_VALUE_BYTES + 1);
        assert!(build(
            SqlKind::MySql,
            &filter(FilterMode::And, vec![cond("name", FilterOp::Eq, &[&long])]),
        )
        .is_err());
    }

    #[test]
    fn pg_unordered_types() {
        for t in [
            "json",
            "JSON",
            "xml",
            "point",
            "polygon",
            "json[]",
            "json[][]",
            "pg_catalog.json",
        ] {
            assert!(pg_unordered(t), "{t}");
        }
        for t in ["jsonb", "text", "integer", "uuid", "int[]", "boolean"] {
            assert!(!pg_unordered(t), "{t}");
        }
    }

    #[test]
    fn ast_serde_shapes() {
        let f: DataFilter = serde_json::from_str(
            r#"{"mode":"or","conditions":[{"column":"a","op":"starts_with","values":["x"]},
                {"column":"b","op":"is_not_null","values":[]}]}"#,
        )
        .unwrap();
        assert_eq!(f.mode, FilterMode::Or);
        assert_eq!(f.conditions[0].op, FilterOp::StartsWith);
        assert_eq!(f.conditions[1].op, FilterOp::IsNotNull);
        assert!(serde_json::from_str::<FilterOp>("\"raw_sql\"").is_err());
    }
}
