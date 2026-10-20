//! Content risks beyond unbounded writes (spec §4.3): `DROP` statements, the name a
//! Production confirmation must be typed with, and the best-effort row estimate of a
//! flagged `UPDATE` / `DELETE`.
//!
//! [`drop_targets`] reads a statement with the shared lexer, like
//! [`crate::unbounded`]: every [`readings`] of a MySQL executable comment is judged
//! (so `/*!40000 DROP TABLE t */` and `DROP TABLE a /*!, b */` count), and a statement
//! the server runs as several (a `DELIMITER` left on) is read piece by piece.
//! Comments, strings and dollar-quoted bodies never count.
//!
//! What the guard makes of them ([`crate::guard`]), by the connection's env tag:
//!
//! | Risk                                  | Local / untagged | Staging | Production          |
//! |---------------------------------------|------------------|---------|---------------------|
//! | unbounded `UPDATE` / `DELETE`         | setting          | setting | Confirm + table     |
//! | `TRUNCATE`                            | setting          | Confirm | Confirm + table     |
//! | `DROP` of an object                   | Allow            | Confirm | Confirm + object    |
//! | `DROP DATABASE` / `DROP SCHEMA`       | Confirm          | Confirm | Confirm + its name  |
//!
//! ("setting": Confirm while "Confirm UPDATE/DELETE without WHERE" is on.) A read-only
//! connection blocks all of them whatever the tag.
//!
//! Known limits (as in [`crate::unbounded`]): dynamic and compound SQL is not inspected
//! for `DROP`s: PostgreSQL `DO $$ … $$` blocks and function bodies, PL/pgSQL
//! `EXECUTE '…'`, MySQL `PREPARE … FROM '…'` / `EXECUTE`, routine, trigger and event
//! bodies, MariaDB `BEGIN NOT ATOMIC … END`, and whatever a `CALL` runs. Such
//! statements keep only their ordinary guard verdict.

use dnt_core::sql::{readings, resplit_reading, resplits, tokenize, Dialect, TokKind, Token};
use dnt_core::value::Value;
use serde::{Deserialize, Serialize};

use crate::unbounded::{UnboundedWrite, WriteOp};

/// What a `DROP` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DropKind {
    /// `DROP DATABASE`.
    Database,
    /// `DROP SCHEMA` (on MySQL a synonym of `DROP DATABASE`).
    Schema,
    /// Any other `DROP` (table, view, index, routine, user, …).
    Object,
}

/// One name a `DROP` removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DropTarget {
    pub kind: DropKind,
    /// The name as written, quotes removed (`` `shop`.`t` `` → `shop.t`); empty when
    /// the statement names nothing readable (`DROP OPERATOR + (int, int)`).
    pub name: String,
}

/// The estimated number of rows a flagged `UPDATE` / `DELETE` changes, from the
/// server's `EXPLAIN` (no `ANALYZE`: nothing runs).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowEstimate {
    Rows {
        n: f64,
    },
    /// The explain failed, timed out or said nothing usable.
    Unavailable,
}

/// The names the `DROP`s of `stmt` (one planned statement) remove, in order, without
/// repeats.
pub fn drop_targets(stmt: &str, dialect: Dialect) -> Vec<DropTarget> {
    let mut out = Vec::new();
    for r in readings(stmt, dialect) {
        let toks = tokenize(&r, dialect);
        if toks.is_empty() {
            continue;
        }
        match resplit_reading(&r, &toks, dialect) {
            Some(pieces) => {
                for p in pieces {
                    let ptoks = tokenize(&p, dialect);
                    scan(&p, &ptoks, dialect, &mut out);
                }
            }
            None => scan(&r, &toks, dialect, &mut out),
        }
    }
    let mut seen: Vec<DropTarget> = Vec::with_capacity(out.len());
    for d in out {
        if !seen.contains(&d) {
            seen.push(d);
        }
    }
    seen
}

/// The name a Production confirmation of a statement must be typed with: the first
/// database / schema it drops, else the first table it changes every row of, else the
/// first object it drops. `None` when it has no such name.
pub fn confirm_name(unbounded: &[UnboundedWrite], drops: &[DropTarget]) -> Option<String> {
    let named = |kinds: &[DropKind]| {
        drops
            .iter()
            .find(|d| kinds.contains(&d.kind) && !d.name.is_empty())
            .map(|d| d.name.clone())
    };
    named(&[DropKind::Database, DropKind::Schema])
        .or_else(|| unbounded.iter().find_map(|w| first_name(&w.table)))
        .or_else(|| named(&[DropKind::Object]))
}

/// Whether the names the user typed (`given`) are exactly the names the plan asks for
/// (`required`), compared case-sensitively: every required name typed, and nothing
/// typed that is not required. Repeats do not matter.
pub fn confirm_names_match(required: &[String], given: &[String]) -> bool {
    required.iter().all(|r| given.contains(r)) && given.iter().all(|g| required.contains(g))
}

/// The statement that estimates the rows `stmt` changes, or `None` when it may not be
/// explained safely: it must be ONE statement for the server (no `DELIMITER` pieces)
/// starting with `UPDATE` or `DELETE` in every reading. PostgreSQL gets
/// `EXPLAIN (FORMAT JSON) …`, MySQL / MariaDB a plain (tabular) `EXPLAIN …`.
pub fn estimate_sql(stmt: &str, dialect: Dialect) -> Option<String> {
    if resplits(stmt, dialect) {
        return None;
    }
    let mut any = false;
    for r in readings(stmt, dialect) {
        let toks = tokenize(&r, dialect);
        if toks.is_empty() {
            continue;
        }
        let first = word(&toks, 0)?;
        if !(first.eq_ignore_ascii_case("UPDATE") || first.eq_ignore_ascii_case("DELETE")) {
            return None;
        }
        any = true;
    }
    if !any {
        return None;
    }
    let prefix = if dialect.is_mysql() {
        "EXPLAIN"
    } else {
        "EXPLAIN (FORMAT JSON)"
    };
    Some(format!("{prefix} {}", stmt.trim()))
}

/// The table name to look for in a MySQL `EXPLAIN` of a statement with these unbounded
/// writes: the last part of the first target of the first `UPDATE` / `DELETE`.
pub fn estimate_table(unbounded: &[UnboundedWrite]) -> Option<String> {
    let w = unbounded
        .iter()
        .find(|w| matches!(w.op, WriteOp::Update | WriteOp::Delete))?;
    let name = first_name(&w.table)?;
    Some(name.rsplit('.').next().unwrap_or(&name).to_string())
}

/// Reads what [`estimate_sql`] returned.
///
/// - PostgreSQL: the `Plan Rows` of the root node of the JSON plan; a `ModifyTable`
///   root (which returns no rows itself) gives the rows of its first child, the rows it
///   changes.
/// - MySQL / MariaDB: the `rows` column of the row whose `table` is `table` (ASCII
///   case-insensitive), else of the first row that has a number there.
///
/// Anything else is `Unavailable`.
pub fn parse_estimate(
    dialect: Dialect,
    columns: &[String],
    rows: &[Vec<Value>],
    table: Option<&str>,
) -> RowEstimate {
    let n = if dialect.is_mysql() {
        mysql_rows(columns, rows, table)
    } else {
        pg_rows(rows)
    };
    match n {
        Some(n) if n.is_finite() && n >= 0.0 => RowEstimate::Rows { n },
        _ => RowEstimate::Unavailable,
    }
}

fn pg_rows(rows: &[Vec<Value>]) -> Option<f64> {
    let text = match rows.first()?.first()? {
        Value::Text(s) | Value::Json(s) => s.as_str(),
        Value::Other { text, .. } => text.as_str(),
        _ => return None,
    };
    let doc: serde_json::Value = serde_json::from_str(text).ok()?;
    let root = doc.get(0)?.get("Plan")?;
    let node = if root.get("Node Type").and_then(|t| t.as_str()) == Some("ModifyTable") {
        root.get("Plans").and_then(|p| p.get(0)).unwrap_or(root)
    } else {
        root
    };
    node.get("Plan Rows")?.as_f64()
}

fn mysql_rows(columns: &[String], rows: &[Vec<Value>], table: Option<&str>) -> Option<f64> {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let rows_at = col("rows")?;
    let table_at = col("table");
    let number = |r: &Vec<Value>| r.get(rows_at).and_then(value_number);
    if let (Some(t), Some(at)) = (table, table_at) {
        let hit = rows.iter().find(|r| {
            r.get(at)
                .and_then(value_text)
                .is_some_and(|v| v.eq_ignore_ascii_case(t))
        });
        if let Some(n) = hit.and_then(number) {
            return Some(n);
        }
    }
    rows.iter().find_map(number)
}

fn value_text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(s) | Value::Decimal(s) => Some(s),
        Value::Other { text, .. } => Some(text),
        _ => None,
    }
}

fn value_number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        _ => value_text(v)?.trim().parse().ok(),
    }
}

fn word<'a>(toks: &[Token<'a>], i: usize) -> Option<&'a str> {
    match toks.get(i)?.kind {
        TokKind::Word(w) => Some(w),
        _ => None,
    }
}

fn is_word(toks: &[Token], i: usize, kw: &str) -> bool {
    word(toks, i).is_some_and(|w| w.eq_ignore_ascii_case(kw))
}

fn is_punct(toks: &[Token], i: usize, c: u8) -> bool {
    toks.get(i).is_some_and(|t| t.kind == TokKind::Punct(c))
}

/// One part of a name: a word as written, or a quoted identifier unquoted.
fn part(src: &str, t: &Token) -> Option<String> {
    match t.kind {
        TokKind::Word(w) => Some(w.to_string()),
        TokKind::Quoted => crate::guard::unquote(&src[t.start..t.end]),
        _ => None,
    }
}

/// The qualified name at `i` (`a`, `a.b`, `` `a`.`b` ``, `"s"."t"`), its parts unquoted
/// and joined with `.`, and the index past it.
fn name_at(src: &str, toks: &[Token], i: usize) -> Option<(String, usize)> {
    let mut parts = vec![part(src, toks.get(i)?)?];
    let mut end = i;
    while is_punct(toks, end + 1, b'.') {
        match toks.get(end + 2).and_then(|t| part(src, t)) {
            Some(p) => {
                parts.push(p);
                end += 2;
            }
            None => break,
        }
    }
    Some((parts.join("."), end + 1))
}

/// The first name of an [`UnboundedWrite::table`] list (as written, comma-separated),
/// unquoted.
fn first_name(list: &str) -> Option<String> {
    // Every target list is read the same way whatever the dialect: backticks and
    // double quotes both quote a name there.
    let dialect = Dialect::MYSQL;
    let toks = tokenize(list, dialect);
    name_at(list, &toks, 0)
        .map(|(n, _)| n)
        .filter(|n| !n.is_empty())
}

/// The kind words after `DROP`, longest first (`MATERIALIZED VIEW` before `VIEW`).
const OBJECT_KINDS: &[&[&str]] = &[
    &["SPATIAL", "REFERENCE", "SYSTEM"],
    &["FOREIGN", "DATA", "WRAPPER"],
    &["TEXT", "SEARCH", "CONFIGURATION"],
    &["TEXT", "SEARCH", "DICTIONARY"],
    &["TEXT", "SEARCH", "PARSER"],
    &["TEXT", "SEARCH", "TEMPLATE"],
    &["TEMPORARY", "TABLE"],
    &["MATERIALIZED", "VIEW"],
    &["FOREIGN", "TABLE"],
    &["EVENT", "TRIGGER"],
    &["OPERATOR", "CLASS"],
    &["OPERATOR", "FAMILY"],
    &["PROCEDURAL", "LANGUAGE"],
    &["UNDO", "TABLESPACE"],
    &["ACCESS", "METHOD"],
    &["USER", "MAPPING"],
    &["OWNED", "BY"],
    &["LOGFILE", "GROUP"],
    &["RESOURCE", "GROUP"],
    &["PACKAGE", "BODY"],
];

/// Judges the statement made of `toks` (tokens of `src`).
fn scan(src: &str, toks: &[Token], dialect: Dialect, out: &mut Vec<DropTarget>) {
    let mut toks = toks;
    // MariaDB `SET STATEMENT … FOR DROP …`.
    if dialect.is_mysql() && is_word(toks, 0, "SET") && is_word(toks, 1, "STATEMENT") {
        match (2..toks.len()).find(|&k| is_word(toks, k, "FOR")) {
            Some(at) => toks = &toks[at + 1..],
            None => return,
        }
    }
    if !is_word(toks, 0, "DROP") {
        return;
    }
    let mut i = 1;
    let kind = if is_word(toks, i, "DATABASE") {
        i += 1;
        DropKind::Database
    } else if is_word(toks, i, "SCHEMA") {
        i += 1;
        DropKind::Schema
    } else if dialect.is_mysql() && is_word(toks, i, "PREPARE") {
        // `DROP PREPARE p`: deallocates a prepared statement.
        return;
    } else {
        let phrase = OBJECT_KINDS
            .iter()
            .find(|p| p.iter().enumerate().all(|(k, w)| is_word(toks, i + k, w)));
        i += phrase.map_or(1, |p| p.len());
        DropKind::Object
    };
    if is_word(toks, i, "CONCURRENTLY") {
        i += 1;
    }
    if is_word(toks, i, "IF") && is_word(toks, i + 1, "EXISTS") {
        i += 2;
    }
    let before = out.len();
    while let Some((name, next)) = name_at(src, toks, i) {
        out.push(DropTarget { kind, name });
        i = next;
        // A routine's argument types: `DROP FUNCTION f(int), g(text)`.
        if toks.get(i).is_some_and(|t| t.kind == TokKind::Open) {
            let mut depth = 0usize;
            while let Some(t) = toks.get(i) {
                i += 1;
                match t.kind {
                    TokKind::Open => depth += 1,
                    TokKind::Close => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !is_punct(toks, i, b',') {
            break;
        }
        i += 1;
    }
    if out.len() == before {
        out.push(DropTarget {
            kind,
            name: String::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unbounded::UnboundedReason;
    use DropKind::{Database, Object, Schema};

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;
    const BOTH: [Dialect; 2] = [MY, PG];

    fn drops(s: &str, d: Dialect) -> Vec<(DropKind, String)> {
        drop_targets(s, d)
            .into_iter()
            .map(|t| (t.kind, t.name))
            .collect()
    }

    fn of(items: &[(DropKind, &str)]) -> Vec<(DropKind, String)> {
        items.iter().map(|(k, n)| (*k, n.to_string())).collect()
    }

    #[test]
    fn drop_targets_cover_kinds() {
        assert_eq!(
            drops("DROP TABLE IF EXISTS a, `b`", MY),
            of(&[(Object, "a"), (Object, "b")])
        );
        assert_eq!(
            drops(r#"DROP TABLE IF EXISTS a, "B""#, PG),
            of(&[(Object, "a"), (Object, "B")])
        );
        for d in BOTH {
            assert_eq!(
                drops("DROP SCHEMA s CASCADE", d),
                of(&[(Schema, "s")]),
                "{d:?}"
            );
            assert_eq!(drops("DROP DATABASE d", d), of(&[(Database, "d")]));
            assert_eq!(
                drops("drop database if exists Shop", d),
                of(&[(Database, "Shop")])
            );
            assert_eq!(
                drops("DROP VIEW v1, v2", d),
                of(&[(Object, "v1"), (Object, "v2")])
            );
            assert_eq!(
                drops("DROP TABLE shop.orders", d),
                of(&[(Object, "shop.orders")])
            );
            assert_eq!(drops("DROP INDEX i ON t", d), of(&[(Object, "i")]));
            assert_eq!(drops("SELECT 'DROP TABLE t'", d), []);
            assert_eq!(drops("-- DROP TABLE t", d), []);
            assert_eq!(drops("CREATE TABLE t (a int)", d), []);
            assert_eq!(drops("DELETE FROM t", d), []);
        }
        // Inside a MySQL executable comment, and a name only one reading has.
        assert_eq!(drops("/*!40000 DROP TABLE t */", MY), of(&[(Object, "t")]));
        assert_eq!(
            drops("/*!32312 DROP DATABASE `shop` */", MY),
            of(&[(Database, "shop")])
        );
        assert_eq!(
            drops("DROP TABLE a /*!, b */", MY),
            of(&[(Object, "a"), (Object, "b")])
        );
        // In PG `/*!` is a plain comment.
        assert_eq!(drops("/*! DROP TABLE t */ SELECT 1", PG), []);
        // MySQL forms.
        assert_eq!(
            drops("DROP TEMPORARY TABLE IF EXISTS `shop`.`t`", MY),
            of(&[(Object, "shop.t")])
        );
        assert_eq!(drops("DROP PREPARE p", MY), []);
        assert_eq!(
            drops("SET STATEMENT max_statement_time = 1 FOR DROP TABLE t", MY),
            of(&[(Object, "t")])
        );
        assert_eq!(drops("DROP USER 'bob'@'%'", MY), of(&[(Object, "bob")]));
        // PostgreSQL forms.
        assert_eq!(
            drops("DROP MATERIALIZED VIEW IF EXISTS mv CASCADE", PG),
            of(&[(Object, "mv")])
        );
        assert_eq!(
            drops("DROP INDEX CONCURRENTLY IF EXISTS public.ix", PG),
            of(&[(Object, "public.ix")])
        );
        assert_eq!(
            drops("DROP FUNCTION f(int, text), \"G\"(int)", PG),
            of(&[(Object, "f"), (Object, "G")])
        );
        assert_eq!(drops("DROP OWNED BY bob", PG), of(&[(Object, "bob")]));
        assert_eq!(drops("DROP OPERATOR + (int, int)", PG), of(&[(Object, "")]));
        assert_eq!(drops("DO $$ BEGIN DROP TABLE t; END $$", PG), []);
    }

    #[test]
    fn drops_in_delimiter_pieces_are_found() {
        assert_eq!(
            drops("SELECT 1; DROP TABLE t; DROP DATABASE d", MY),
            of(&[(Object, "t"), (Database, "d")])
        );
        // Routine bodies are not judged.
        assert_eq!(
            drops("CREATE PROCEDURE p() BEGIN DROP TABLE t; END", MY),
            []
        );
    }

    fn write(op: WriteOp, table: &str) -> UnboundedWrite {
        UnboundedWrite {
            op,
            reason: UnboundedReason::NoWhere,
            table: table.into(),
        }
    }

    #[test]
    fn confirm_name_prefers_databases_then_tables() {
        let t = |k, n: &str| DropTarget {
            kind: k,
            name: n.into(),
        };
        assert_eq!(
            confirm_name(&[write(WriteOp::Delete, "`shop`.`orders`, b")], &[]),
            Some("shop.orders".into())
        );
        assert_eq!(
            confirm_name(&[write(WriteOp::Update, r#""Orders""#)], &[]),
            Some("Orders".into())
        );
        assert_eq!(
            confirm_name(&[], &[t(Object, "t"), t(Schema, "s")]),
            Some("s".into())
        );
        assert_eq!(
            confirm_name(&[write(WriteOp::Truncate, "x")], &[t(Object, "t")]),
            Some("x".into())
        );
        assert_eq!(confirm_name(&[], &[t(Object, "t")]), Some("t".into()));
        assert_eq!(confirm_name(&[write(WriteOp::Truncate, "")], &[]), None);
        assert_eq!(confirm_name(&[], &[t(Object, "")]), None);
        assert_eq!(confirm_name(&[], &[]), None);
    }

    #[test]
    fn confirm_names_compare_exactly() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(confirm_names_match(&v(&[]), &v(&[])));
        assert!(confirm_names_match(&v(&["orders"]), &v(&["orders"])));
        assert!(confirm_names_match(&v(&["a", "b", "a"]), &v(&["b", "a"])));
        assert!(!confirm_names_match(&v(&["orders"]), &v(&[])));
        assert!(!confirm_names_match(&v(&["orders"]), &v(&["Orders"])));
        assert!(!confirm_names_match(&v(&["orders"]), &v(&["orders", "x"])));
        assert!(!confirm_names_match(&v(&[]), &v(&["x"])));
    }

    #[test]
    fn estimate_sql_only_for_one_update_or_delete() {
        assert_eq!(
            estimate_sql("DELETE FROM t", MY).as_deref(),
            Some("EXPLAIN DELETE FROM t")
        );
        assert_eq!(
            estimate_sql("  update t set a = 1 ", PG).as_deref(),
            Some("EXPLAIN (FORMAT JSON) update t set a = 1")
        );
        assert_eq!(
            estimate_sql("DELETE FROM t /*!50000 WHERE id = 1 */", MY).as_deref(),
            Some("EXPLAIN DELETE FROM t /*!50000 WHERE id = 1 */")
        );
        // The whole statement in an executable comment (an older server ignores it,
        // and its EXPLAIN alone is a syntax error).
        assert_eq!(
            estimate_sql("/*!40000 DELETE FROM t */", MY).as_deref(),
            Some("EXPLAIN /*!40000 DELETE FROM t */")
        );
        for (s, d) in [
            ("SELECT 1; DELETE FROM t", MY),
            ("DELETE FROM t; DROP TABLE u", MY),
            ("TRUNCATE t", MY),
            ("WITH x AS (SELECT 1) DELETE FROM t", PG),
            ("EXPLAIN ANALYZE DELETE FROM t", PG),
            ("/*!40000 SELECT 1 */ DELETE FROM t", MY),
            ("", PG),
        ] {
            assert_eq!(estimate_sql(s, d), None, "{s}");
        }
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn estimates_are_read_per_engine() {
        // PG: a ModifyTable root gives its child's rows.
        let plan = r#"[{"Plan": {"Node Type": "ModifyTable", "Operation": "Delete", "Plan Rows": 0,
            "Plans": [{"Node Type": "Seq Scan", "Plan Rows": 1234}]}}]"#;
        assert_eq!(
            parse_estimate(PG, &["QUERY PLAN".into()], &[vec![text(plan)]], None),
            RowEstimate::Rows { n: 1234.0 }
        );
        let plan = r#"[{"Plan": {"Node Type": "Seq Scan", "Plan Rows": 7}}]"#;
        assert_eq!(
            parse_estimate(PG, &[], &[vec![Value::Json(plan.into())]], None),
            RowEstimate::Rows { n: 7.0 }
        );
        assert_eq!(
            parse_estimate(PG, &[], &[vec![text("not json")]], None),
            RowEstimate::Unavailable
        );
        assert_eq!(parse_estimate(PG, &[], &[], None), RowEstimate::Unavailable);

        // MySQL: the target table's `rows`.
        let cols: Vec<String> = ["id", "select_type", "table", "type", "rows", "Extra"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let row = |t: &str, n: Value| {
            vec![
                Value::Int(1),
                text("DELETE"),
                text(t),
                text("ALL"),
                n,
                Value::Null,
            ]
        };
        let rows = vec![row("u", Value::Int(5)), row("orders", Value::UInt(42))];
        assert_eq!(
            parse_estimate(MY, &cols, &rows, Some("ORDERS")),
            RowEstimate::Rows { n: 42.0 }
        );
        assert_eq!(
            parse_estimate(MY, &cols, &rows, Some("zzz")),
            RowEstimate::Rows { n: 5.0 }
        );
        assert_eq!(
            parse_estimate(MY, &cols, &[row("t", text("17"))], None),
            RowEstimate::Rows { n: 17.0 }
        );
        assert_eq!(
            parse_estimate(MY, &cols, &[row("t", Value::Null)], Some("t")),
            RowEstimate::Unavailable
        );
        // A JSON explain (`explain_format = JSON`): no `rows` column.
        assert_eq!(
            parse_estimate(MY, &["EXPLAIN".into()], &[vec![text("{}")]], None),
            RowEstimate::Unavailable
        );
    }

    #[test]
    fn estimate_table_is_the_first_target() {
        assert_eq!(
            estimate_table(&[
                write(WriteOp::Truncate, "x"),
                write(WriteOp::Delete, "`shop`.`orders`, u")
            ]),
            Some("orders".into())
        );
        assert_eq!(estimate_table(&[write(WriteOp::Truncate, "x")]), None);
    }

    #[test]
    fn serde_shapes() {
        assert_eq!(
            serde_json::to_string(&DropTarget {
                kind: Database,
                name: "d".into()
            })
            .unwrap(),
            r#"{"kind":"database","name":"d"}"#
        );
        assert_eq!(
            serde_json::to_string(&RowEstimate::Rows { n: 3.0 }).unwrap(),
            r#"{"kind":"rows","n":3.0}"#
        );
        assert_eq!(
            serde_json::to_string(&RowEstimate::Unavailable).unwrap(),
            r#"{"kind":"unavailable"}"#
        );
    }
}
