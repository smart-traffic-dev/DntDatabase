//! Explain (spec §8.2): the SQL that explains a statement on each engine, and the
//! normalization of every engine's output into one [`PlanTree`].

use dnt_core::error::DbError;
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::value::Value;
use serde::{Deserialize, Serialize};

/// Shape of what an explain statement returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainFormat {
    /// PostgreSQL `FORMAT JSON`: one row, one JSON array.
    PgJson,
    /// MySQL 8 `EXPLAIN FORMAT=JSON`.
    MySqlJson,
    /// MariaDB `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON` (adds `r_*` actuals).
    MariaDbJson,
    /// MySQL 8 `EXPLAIN ANALYZE`: indented `-> ` lines.
    MySqlTree,
    /// MySQL 5.7 tabular `EXPLAIN`.
    Table,
}

/// The statement to send for an explain, and how to read its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainSql {
    pub sql: String,
    pub format: ExplainFormat,
}

fn unsupported(feature: &str, info: &ServerInfo) -> DbError {
    DbError::Unsupported {
        feature: feature.into(),
        version: info.version_string.clone(),
    }
}

/// Builds the explain statement for `stmt` on the server `info` describes.
/// `analyze` makes the server execute the statement. This function does not refuse
/// non-`Read` statements under `analyze`; the caller (`query_explain`, Task 15) must.
pub fn explain_sql(stmt: &str, info: &ServerInfo, analyze: bool) -> Result<ExplainSql, DbError> {
    let (prefix, format) = match (info.kind, info.variant, analyze) {
        (DbKind::Postgres, _, false) => ("EXPLAIN (FORMAT JSON)", ExplainFormat::PgJson),
        (DbKind::Postgres, _, true) => (
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)",
            ExplainFormat::PgJson,
        ),
        (_, ServerVariant::MariaDb, false) => ("EXPLAIN FORMAT=JSON", ExplainFormat::MariaDbJson),
        (_, ServerVariant::MariaDb, true) => ("ANALYZE FORMAT=JSON", ExplainFormat::MariaDbJson),
        (_, ServerVariant::MySql57, false) => ("EXPLAIN", ExplainFormat::Table),
        (_, ServerVariant::MySql57, true) => return Err(unsupported("EXPLAIN ANALYZE", info)),
        (_, _, false) => ("EXPLAIN FORMAT=JSON", ExplainFormat::MySqlJson),
        (_, _, true) if info.version >= (8, 0, 18) => ("EXPLAIN ANALYZE", ExplainFormat::MySqlTree),
        (_, _, true) => return Err(unsupported("EXPLAIN ANALYZE", info)),
    };
    Ok(ExplainSql {
        sql: format!("{prefix} {}", stmt.trim()),
        format,
    })
}

/// One node of a normalized plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct PlanNode {
    pub label: String,
    pub detail: Option<String>,
    pub est_cost: Option<f64>,
    pub est_rows: Option<f64>,
    pub actual_rows: Option<f64>,
    pub actual_ms: Option<f64>,
    #[specta(type = Option<f64>)]
    pub loops: Option<u64>,
    pub children: Vec<PlanNode>,
}

/// A normalized explain result. `raw` is the engine's original output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanTree {
    Tree {
        root: PlanNode,
        raw: String,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        raw: String,
    },
}

/// Normalizes the rows an explain statement of `format` returned.
/// Output that does not have the engine's shape is `DbError::Internal`.
pub fn parse_plan(
    format: ExplainFormat,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<PlanTree, DbError> {
    if format == ExplainFormat::Table {
        return Ok(parse_table(columns, rows));
    }
    let raw = single_text(rows)?;
    let root = match format {
        ExplainFormat::PgJson => pg::parse(&raw)?,
        ExplainFormat::MySqlJson | ExplainFormat::MariaDbJson => mysql_json::parse(&raw)?,
        ExplainFormat::MySqlTree => mysql_tree::parse(&raw)?,
        ExplainFormat::Table => unreachable!("handled above"),
    };
    Ok(PlanTree::Tree { root, raw })
}

/// The node's own cost: its `est_cost` minus the sum of its children's `est_cost`
/// (a child without a cost counts as 0), clamped to 0. `None` when the node has no cost.
///
/// The clamp covers nodes that stop early: a PG `Limit` costs less than the scan below it.
///
/// MySQL JSON `prefix_cost` is cumulative: each table's cost includes the tables joined
/// before it, and the query block's `query_cost` includes everything. The subtraction is
/// therefore only approximate there, and the query-block node usually carries the
/// highest self cost.
pub fn self_cost(node: &PlanNode) -> Option<f64> {
    let own = node.est_cost?;
    let children: f64 = node.children.iter().filter_map(|c| c.est_cost).sum();
    Some((own - children).max(0.0))
}

fn internal(message: &str) -> DbError {
    DbError::Internal {
        message: format!("explain: {message}"),
    }
}

/// The text of the first column of the first row (every tree format is one cell).
fn single_text(rows: &[Vec<Value>]) -> Result<String, DbError> {
    match rows.first().and_then(|r| r.first()) {
        Some(Value::Text(s) | Value::Json(s)) => Ok(s.clone()),
        Some(Value::Other { text, .. }) => Ok(text.clone()),
        Some(_) => Err(internal("the plan is not text")),
        None => Err(internal("the server returned no plan")),
    }
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s) => s.clone(),
        Value::Bytes { preview, .. } => String::from_utf8_lossy(preview).into_owned(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(cell_text).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Other { text, .. } => text.clone(),
    }
}

/// MySQL 5.7 tabular `EXPLAIN`; `raw` is tab-separated, one line per row.
fn parse_table(columns: &[String], rows: &[Vec<Value>]) -> PlanTree {
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(cell_text).collect())
        .collect();
    let mut raw = columns.join("\t");
    for r in &rows {
        raw.push('\n');
        raw.push_str(&r.join("\t"));
    }
    PlanTree::Table {
        columns: columns.to_vec(),
        rows,
        raw,
    }
}

fn leaf(label: String) -> PlanNode {
    PlanNode {
        label,
        detail: None,
        est_cost: None,
        est_rows: None,
        actual_rows: None,
        actual_ms: None,
        loops: None,
        children: vec![],
    }
}

/// A JSON document that keeps every object member in order, duplicates included
/// (MariaDB 10.x writes one `"table"` member per table of a join into one object).
mod json {
    use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    #[derive(Debug, Clone, PartialEq)]
    pub enum J {
        Null,
        Bool(bool),
        Num(f64),
        Str(String),
        Arr(Vec<J>),
        Obj(Vec<(String, J)>),
    }

    impl J {
        /// First member named `key`.
        pub fn get(&self, key: &str) -> Option<&J> {
            match self {
                J::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        pub fn str(&self) -> Option<&str> {
            match self {
                J::Str(s) => Some(s),
                _ => None,
            }
        }

        /// A number, or a string holding one (MySQL quotes its costs).
        pub fn num(&self) -> Option<f64> {
            match self {
                J::Num(n) => Some(*n),
                J::Str(s) => s.trim().parse().ok(),
                _ => None,
            }
        }

        /// Text for a detail line: strings as-is, arrays joined by `, `.
        pub fn text(&self) -> Option<String> {
            match self {
                J::Str(s) => Some(s.clone()),
                J::Num(n) => Some(n.to_string()),
                J::Bool(b) => Some(b.to_string()),
                J::Arr(items) => {
                    let parts: Vec<String> = items.iter().filter_map(J::text).collect();
                    (!parts.is_empty()).then(|| parts.join(", "))
                }
                J::Null | J::Obj(_) => None,
            }
        }
    }

    pub fn parse(text: &str) -> Result<J, serde_json::Error> {
        serde_json::from_str(text)
    }

    struct JVisitor;

    impl<'de> Visitor<'de> for JVisitor {
        type Value = J;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("any JSON value")
        }
        fn visit_unit<E: de::Error>(self) -> Result<J, E> {
            Ok(J::Null)
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<J, E> {
            Ok(J::Bool(v))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<J, E> {
            Ok(J::Num(v as f64))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<J, E> {
            Ok(J::Num(v as f64))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<J, E> {
            Ok(J::Num(v))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<J, E> {
            Ok(J::Str(v.to_string()))
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<J, E> {
            Ok(J::Str(v))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<J, A::Error> {
            let mut items = vec![];
            while let Some(v) = seq.next_element()? {
                items.push(v);
            }
            Ok(J::Arr(items))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<J, A::Error> {
            let mut members = vec![];
            while let Some((k, v)) = map.next_entry::<String, J>()? {
                members.push((k, v));
            }
            Ok(J::Obj(members))
        }
    }

    impl<'de> Deserialize<'de> for J {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<J, D::Error> {
            d.deserialize_any(JVisitor)
        }
    }
}

/// PostgreSQL `EXPLAIN (FORMAT JSON)`: `[{"Plan": {...}, ...}]`.
mod pg {
    use super::json::{self, J};
    use super::{internal, leaf, PlanNode};
    use dnt_core::error::DbError;

    pub fn parse(raw: &str) -> Result<PlanNode, DbError> {
        let doc = json::parse(raw).map_err(|e| internal(&format!("invalid JSON: {e}")))?;
        let plan = match &doc {
            J::Arr(items) => items.first().and_then(|q| q.get("Plan")),
            _ => None,
        }
        .ok_or_else(|| internal("no \"Plan\" in the PostgreSQL output"))?;
        node(plan)
    }

    fn node(p: &J) -> Result<PlanNode, DbError> {
        let J::Obj(_) = p else {
            return Err(internal("a PostgreSQL plan node is not an object"));
        };
        let kind = p
            .get("Node Type")
            .and_then(J::str)
            .ok_or_else(|| internal("a PostgreSQL plan node has no \"Node Type\""))?;
        let mut n = leaf(match p.get("Relation Name").and_then(J::str) {
            Some(rel) => format!("{kind} on {rel}"),
            None => kind.to_string(),
        });
        let detail: Vec<String> = [
            ("Index Name", "Index"),
            ("Filter", "Filter"),
            ("Join Filter", "Join Filter"),
            ("Hash Cond", "Hash Cond"),
        ]
        .iter()
        .filter_map(|(key, name)| p.get(key).and_then(J::text).map(|v| format!("{name}: {v}")))
        .collect();
        n.detail = (!detail.is_empty()).then(|| detail.join("; "));
        n.est_cost = p.get("Total Cost").and_then(J::num);
        n.est_rows = p.get("Plan Rows").and_then(J::num);
        n.actual_rows = p.get("Actual Rows").and_then(J::num);
        n.actual_ms = p.get("Actual Total Time").and_then(J::num);
        n.loops = p
            .get("Actual Loops")
            .and_then(J::num)
            .map(|l| l.max(0.0) as u64);
        if let Some(children) = p.get("Plans") {
            let J::Arr(items) = children else {
                return Err(internal("PostgreSQL \"Plans\" is not an array"));
            };
            n.children = items.iter().map(node).collect::<Result<_, _>>()?;
        }
        Ok(n)
    }
}

/// MySQL 8 / MariaDB `FORMAT=JSON` (and MariaDB `ANALYZE FORMAT=JSON`).
mod mysql_json {
    use super::json::{self, J};
    use super::{internal, leaf, PlanNode};
    use dnt_core::error::DbError;

    pub fn parse(raw: &str) -> Result<PlanNode, DbError> {
        let doc = json::parse(raw).map_err(|e| internal(&format!("invalid JSON: {e}")))?;
        if !matches!(doc.get("query_block"), Some(J::Obj(_))) {
            return Err(internal("no \"query_block\" object in the output"));
        }
        let mut nodes = walk(&doc);
        Ok(if nodes.len() == 1 {
            nodes.remove(0)
        } else {
            PlanNode {
                children: nodes,
                ..leaf("Query".into())
            }
        })
    }

    /// Display name of the operation members that become a node; `None` for members
    /// that are walked through (their operations become siblings).
    fn op_label(key: &str) -> Option<&'static str> {
        Some(match key {
            "query_block" => "Query block",
            "nested_loop" => "Nested loop",
            "ordering_operation" => "Ordering",
            "grouping_operation" => "Grouping",
            "duplicates_removal" => "Duplicates removal",
            "materialized_from_subquery" => "Materialize subquery",
            "filesort" => "Filesort",
            "temporary_table" => "Temporary table",
            "union_result" => "Union result",
            "windowing" => "Window",
            "read_sorted_file" => "Read sorted file",
            _ => return None,
        })
    }

    /// The nodes for the operations found in `v`, in document order.
    fn walk(v: &J) -> Vec<PlanNode> {
        match v {
            J::Arr(items) => items.iter().flat_map(walk).collect(),
            J::Obj(members) => members.iter().flat_map(|(k, v)| member(k, v)).collect(),
            _ => vec![],
        }
    }

    fn member(key: &str, v: &J) -> Vec<PlanNode> {
        if key == "table" {
            return match v {
                J::Obj(_) => vec![table(v)],
                _ => vec![],
            };
        }
        match op_label(key) {
            Some(label) if matches!(v, J::Obj(_) | J::Arr(_)) => {
                let mut n = leaf(label.into());
                if key == "query_block" {
                    if let Some(id) = v.get("select_id").and_then(J::text) {
                        n.label = format!("{label} #{id}");
                    }
                }
                n.detail = op_detail(v);
                fill_measures(&mut n, v);
                n.children = walk(v);
                vec![n]
            }
            _ => walk(v),
        }
    }

    fn op_detail(v: &J) -> Option<String> {
        let mut parts = vec![];
        if let Some(k) = v.get("sort_key").and_then(J::text) {
            parts.push(format!("sort key: {k}"));
        }
        for (flag, text) in [
            ("using_filesort", "using filesort"),
            ("using_temporary_table", "using temporary"),
        ] {
            if matches!(v.get(flag), Some(J::Bool(true))) {
                parts.push(text.to_string());
            }
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// Cost, rows and (MariaDB) actuals of an operation or table object.
    fn fill_measures(n: &mut PlanNode, v: &J) {
        let ci = v.get("cost_info");
        let ci_num = |k: &str| ci.and_then(|c| c.get(k)).and_then(J::num);
        n.est_cost = ci_num("prefix_cost")
            .or_else(|| ci_num("query_cost"))
            .or_else(|| Some(ci_num("read_cost")? + ci_num("eval_cost")?))
            .or_else(|| v.get("cost").and_then(J::num));
        n.est_rows = v
            .get("rows_examined_per_scan")
            .or_else(|| v.get("rows"))
            .and_then(J::num);
        n.actual_rows = v
            .get("r_rows")
            .or_else(|| v.get("r_output_rows"))
            .and_then(J::num);
        n.actual_ms = v
            .get("r_total_time_ms")
            .or_else(|| v.get("r_table_time_ms"))
            .and_then(J::num);
        n.loops = v.get("r_loops").and_then(J::num).map(|l| l.max(0.0) as u64);
    }

    fn table(t: &J) -> PlanNode {
        let name = t.get("table_name").and_then(J::text).unwrap_or_default();
        let access = t.get("access_type").and_then(J::text);
        let mut n = leaf(match access {
            Some(a) => format!("{a} {name}"),
            None => name,
        });
        let detail: Vec<String> = [
            ("key", "key"),
            ("ref", "ref"),
            ("attached_condition", "condition"),
            ("message", "message"),
        ]
        .iter()
        .filter_map(|(k, name)| t.get(k).and_then(J::text).map(|v| format!("{name}: {v}")))
        .collect();
        n.detail = (!detail.is_empty()).then(|| detail.join("; "));
        fill_measures(&mut n, t);
        n.children = walk(t);
        n
    }
}

/// MySQL 8 `EXPLAIN ANALYZE`: one node per `-> ` line, nested by 4-space indentation.
mod mysql_tree {
    use super::{internal, leaf, PlanNode};
    use dnt_core::error::DbError;

    /// Deepest nesting accepted. Real plans are far shallower; the cap keeps the
    /// recursive walks over `PlanNode` (drop, serialize, the UI) off the stack limit.
    const MAX_DEPTH: usize = 1000;

    pub fn parse(raw: &str) -> Result<PlanNode, DbError> {
        // (depth, node) of the open path from the root to the last node.
        let mut stack: Vec<(usize, PlanNode)> = vec![];
        let mut roots: Vec<PlanNode> = vec![];
        for line in raw.lines() {
            let body = line.trim_start_matches(' ');
            let Some(text) = body.strip_prefix("-> ") else {
                if body.trim().is_empty() {
                    continue;
                }
                // A continuation of the previous node's text.
                let (_, last) = stack
                    .last_mut()
                    .ok_or_else(|| internal("the plan does not start with \"-> \""))?;
                last.label.push(' ');
                last.label.push_str(body.trim());
                continue;
            };
            let depth = (line.len() - body.len()) / 4;
            close_to(&mut stack, &mut roots, depth);
            if stack.len() >= MAX_DEPTH {
                return Err(internal(&format!(
                    "the plan nests deeper than {MAX_DEPTH} levels"
                )));
            }
            stack.push((depth, node(text)));
        }
        close_to(&mut stack, &mut roots, 0);
        match roots.len() {
            0 => Err(internal("no \"-> \" line in the plan")),
            1 => Ok(roots.remove(0)),
            _ => Ok(PlanNode {
                children: roots,
                ..leaf("Query".into())
            }),
        }
    }

    /// Pops every open node at `depth` or deeper into its parent (or the roots).
    fn close_to(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, depth: usize) {
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            let (_, n) = stack.pop().expect("checked non-empty");
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(n),
                None => roots.push(n),
            }
        }
    }

    fn node(text: &str) -> PlanNode {
        let cut = [
            "  (cost=",
            " (cost=",
            "  (actual time=",
            " (actual time=",
            "  (never executed)",
            " (never executed)",
        ]
        .iter()
        .filter_map(|m| text.find(m))
        .min()
        .unwrap_or(text.len());
        let mut n = leaf(text[..cut].trim().to_string());
        if let Some(est) = group(text, "(cost=") {
            n.est_cost = field(&est, "cost").and_then(range_end);
            n.est_rows = field(&est, "rows").and_then(|v| v.parse().ok());
        }
        if let Some(act) = group(text, "(actual time=") {
            n.actual_ms = field(&act, "time").and_then(range_end);
            n.actual_rows = field(&act, "rows").and_then(|v| v.parse().ok());
            n.loops = field(&act, "loops").and_then(|v| v.parse().ok());
        }
        n
    }

    /// The `key=value ...` text of the parenthesized group starting with `open`,
    /// e.g. `cost=1.55 rows=1` for `(cost=1.55 rows=1)`.
    fn group(text: &str, open: &str) -> Option<String> {
        let start = text.find(open)? + 1;
        let end = start + text[start..].find(')')?;
        Some(text[start..end].to_string())
    }

    fn field<'a>(group: &'a str, key: &str) -> Option<&'a str> {
        group
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
    }

    /// `a..b` gives `b`; a single number gives itself.
    fn range_end(v: &str) -> Option<f64> {
        v.rsplit("..").next()?.parse().ok()
    }
}
