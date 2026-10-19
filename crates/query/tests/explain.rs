//! Explain SQL per engine and plan normalization against real-server fixtures
//! (captured by `tests/explain_capture.rs`).

use dnt_core::error::DbError;
use dnt_core::server::{detect_mysql, detect_pg, ServerInfo};
use dnt_core::value::Value;
use dnt_query::explain::{explain_sql, parse_plan, self_cost, ExplainFormat, PlanNode, PlanTree};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

fn load(name: &str) -> Fixture {
    let path = format!(
        "{}/tests/fixtures/explain/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn parse(name: &str, format: ExplainFormat) -> PlanTree {
    let f = load(name);
    parse_plan(format, &f.columns, &f.rows).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn root(tree: &PlanTree) -> &PlanNode {
    match tree {
        PlanTree::Tree { root, raw } => {
            assert!(!raw.is_empty());
            root
        }
        PlanTree::Table { .. } => panic!("expected a tree, got {tree:?}"),
    }
}

fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    out.push(n);
    for c in &n.children {
        walk(c, out);
    }
}

fn nodes(n: &PlanNode) -> Vec<&PlanNode> {
    let mut v = vec![];
    walk(n, &mut v);
    v
}

/// Root label is non-empty and some node mentions `orders`.
fn assert_tree(name: &str, format: ExplainFormat) -> PlanTree {
    let tree = parse(name, format);
    let r = root(&tree);
    assert!(!r.label.is_empty(), "{name}: empty root label");
    assert!(
        nodes(r).iter().any(|n| n.label.contains("orders")
            || n.detail.as_deref().is_some_and(|d| d.contains("orders"))),
        "{name}: no node mentions orders: {r:#?}"
    );
    tree
}

fn assert_actuals(name: &str, tree: &PlanTree) {
    assert!(
        nodes(root(tree)).iter().any(|n| n.actual_rows.is_some()),
        "{name}: no actual_rows"
    );
}

fn mysql(v: &str) -> ServerInfo {
    detect_mysql(v).unwrap()
}

#[test]
fn explain_sql_per_engine() {
    let pg = detect_pg(170002, "PostgreSQL 17.2");
    let cases: [(ServerInfo, bool, &str, ExplainFormat); 7] = [
        (
            pg.clone(),
            false,
            "EXPLAIN (FORMAT JSON) SELECT 1",
            ExplainFormat::PgJson,
        ),
        (
            pg,
            true,
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT 1",
            ExplainFormat::PgJson,
        ),
        (
            mysql("8.0.36"),
            false,
            "EXPLAIN FORMAT=JSON SELECT 1",
            ExplainFormat::MySqlJson,
        ),
        (
            mysql("8.0.18"),
            true,
            "EXPLAIN ANALYZE SELECT 1",
            ExplainFormat::MySqlTree,
        ),
        (
            mysql("10.6.16-MariaDB"),
            false,
            "EXPLAIN FORMAT=JSON SELECT 1",
            ExplainFormat::MariaDbJson,
        ),
        (
            mysql("10.6.16-MariaDB"),
            true,
            "ANALYZE FORMAT=JSON SELECT 1",
            ExplainFormat::MariaDbJson,
        ),
        (
            mysql("5.7.44-log"),
            false,
            "EXPLAIN SELECT 1",
            ExplainFormat::Table,
        ),
    ];
    for (info, analyze, sql, format) in cases {
        let got = explain_sql("SELECT 1", &info, analyze).unwrap();
        assert_eq!(
            (got.sql.as_str(), got.format),
            (sql, format),
            "{info:?} {analyze}"
        );
    }
    for v in ["8.0.17", "5.7.44-log"] {
        assert!(
            matches!(
                explain_sql("SELECT 1", &mysql(v), true),
                Err(DbError::Unsupported { .. })
            ),
            "{v}"
        );
    }
}

#[test]
fn pg12_plain_tree() {
    let tree = assert_tree("pg12_plain", ExplainFormat::PgJson);
    let r = root(&tree);
    assert!(r.est_cost.is_some() && r.est_rows.is_some());
    assert!(nodes(r).iter().all(|n| n.actual_rows.is_none()));
    assert!(nodes(r)
        .iter()
        .any(|n| n.label.contains(" on orders") || n.label.contains(" on customers")));
}

#[test]
fn pg17_analyze_has_actuals() {
    let tree = assert_tree("pg17_analyze", ExplainFormat::PgJson);
    assert_actuals("pg17_analyze", &tree);
    let r = root(&tree);
    assert!(r.actual_ms.is_some() && r.loops == Some(1));
    // pg12_analyze parses the same way.
    let t12 = assert_tree("pg12_analyze", ExplainFormat::PgJson);
    assert_actuals("pg12_analyze", &t12);
}

#[test]
fn mysql80_json() {
    let tree = assert_tree("mysql80_plain", ExplainFormat::MySqlJson);
    assert!(nodes(root(&tree)).iter().any(|n| n.est_cost.is_some()));
}

#[test]
fn mysql84_json() {
    let tree = assert_tree("mysql84_plain", ExplainFormat::MySqlJson);
    let all = nodes(root(&tree));
    assert!(all
        .iter()
        .any(|n| n.label.ends_with(" o") && n.est_rows.is_some()));
}

#[test]
fn mysql80_analyze_tree() {
    let tree = assert_tree("mysql80_analyze", ExplainFormat::MySqlTree);
    assert_actuals("mysql80_analyze", &tree);
}

#[test]
fn mysql84_analyze_tree() {
    let tree = assert_tree("mysql84_analyze", ExplainFormat::MySqlTree);
    assert_actuals("mysql84_analyze", &tree);
    let r = root(&tree);
    assert!(r.label.starts_with("Sort"), "{}", r.label);
    let all = nodes(r);
    let join = all
        .iter()
        .find(|n| n.label == "Nested loop inner join")
        .expect("join node");
    assert_eq!(join.est_cost, Some(1.55));
    assert_eq!(join.est_rows, Some(1.0));
    assert_eq!(join.actual_ms, Some(0.04));
    assert_eq!(join.loops, Some(1));
    assert_eq!(join.children.len(), 2);
}

#[test]
fn mariadb106_json() {
    let tree = assert_tree("mariadb106_plain", ExplainFormat::MariaDbJson);
    // MariaDB 10.6 repeats the `"table"` member in one object: both tables are kept.
    let labels: Vec<&str> = nodes(root(&tree))
        .iter()
        .map(|n| n.label.as_str())
        .collect();
    assert!(
        labels.contains(&"index o") && labels.contains(&"eq_ref c"),
        "{labels:?}"
    );
}

#[test]
fn mariadb114_analyze_json() {
    let tree = assert_tree("mariadb114_analyze", ExplainFormat::MariaDbJson);
    assert_actuals("mariadb114_analyze", &tree);
    // mariadb106_analyze parses the same way.
    let t106 = assert_tree("mariadb106_analyze", ExplainFormat::MariaDbJson);
    assert_actuals("mariadb106_analyze", &t106);
}

#[test]
fn mysql57_table() {
    let f = load("mysql57_plain");
    match parse_plan(ExplainFormat::Table, &f.columns, &f.rows).unwrap() {
        PlanTree::Table { columns, rows, raw } => {
            assert!(columns.iter().any(|c| c == "table"));
            assert_eq!(rows.len(), f.rows.len());
            assert!(rows.iter().all(|r| r.len() == columns.len()));
            assert!(raw.contains("idx_orders_customer_status"));
        }
        t => panic!("expected a table, got {t:?}"),
    }
}

fn node(cost: Option<f64>, children: Vec<PlanNode>) -> PlanNode {
    PlanNode {
        label: "n".into(),
        detail: None,
        est_cost: cost,
        est_rows: None,
        actual_rows: None,
        actual_ms: None,
        loops: None,
        children,
    }
}

#[test]
fn self_cost_subtracts_children() {
    let n = node(
        Some(10.0),
        vec![node(Some(3.0), vec![]), node(Some(4.0), vec![])],
    );
    assert_eq!(self_cost(&n), Some(3.0));
    assert_eq!(self_cost(&node(None, vec![])), None);
    assert_eq!(self_cost(&node(Some(2.5), vec![])), Some(2.5));
}

#[test]
fn self_cost_is_clamped_to_zero() {
    // PG `Limit` stops early, so its total cost is below its child's.
    let raw = r#"[{"Plan": {"Node Type": "Limit", "Total Cost": 0.5, "Plan Rows": 1,
        "Plans": [{"Node Type": "Seq Scan", "Relation Name": "orders",
                   "Total Cost": 1000, "Plan Rows": 100000}]}}]"#;
    let tree = parse_plan(
        ExplainFormat::PgJson,
        &["QUERY PLAN".to_string()],
        &[vec![Value::Json(raw.into())]],
    )
    .unwrap();
    let r = root(&tree);
    assert_eq!(r.label, "Limit");
    assert_eq!(self_cost(r), Some(0.0));
    assert_eq!(self_cost(&r.children[0]), Some(1000.0));
}

#[test]
fn mysql_tree_depth_is_capped() {
    let deep: String = (0..5000)
        .map(|i| format!("{}-> Filter: x{i}\n", " ".repeat(4 * i)))
        .collect();
    match parse_plan(
        ExplainFormat::MySqlTree,
        &["EXPLAIN".to_string()],
        &[vec![Value::Text(deep)]],
    ) {
        Err(DbError::Internal { .. }) => {}
        other => panic!("expected Internal, got {:?}", other.map(|_| "a tree")),
    }
    // 1000 levels still parse.
    let ok: String = (0..1000)
        .map(|i| format!("{}-> Filter: x{i}\n", " ".repeat(4 * i)))
        .collect();
    assert!(parse_plan(
        ExplainFormat::MySqlTree,
        &["EXPLAIN".to_string()],
        &[vec![Value::Text(ok)]],
    )
    .is_ok());
}

#[test]
fn malformed_json_is_internal_error() {
    let cols = vec!["QUERY PLAN".to_string()];
    let text = |s: &str| vec![vec![Value::Text(s.into())]];
    let cases: [(ExplainFormat, Vec<Vec<Value>>); 8] = [
        (ExplainFormat::PgJson, text("[{\"Plan\": ")),
        (ExplainFormat::PgJson, text("[]")),
        (ExplainFormat::PgJson, text("{\"Plan\": 3}")),
        (ExplainFormat::MySqlJson, text("not json")),
        (ExplainFormat::MariaDbJson, text("{\"query_block\": 1}")),
        (ExplainFormat::MySqlTree, text("no arrows here")),
        (ExplainFormat::MySqlJson, vec![]),
        (ExplainFormat::PgJson, vec![vec![Value::Int(1)]]),
    ];
    for (format, rows) in cases {
        match parse_plan(format, &cols, &rows) {
            Err(DbError::Internal { .. }) => {}
            other => panic!("{format:?} {rows:?}: expected Internal, got {other:?}"),
        }
    }
}
