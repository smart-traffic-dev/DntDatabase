//! Captures the explain fixtures under `tests/fixtures/explain/` from the Docker matrix.
//!
//! Opt-in: writes only when both `DNT_TEST_DB=1` and `DNT_UPDATE_FIXTURES=1` are set;
//! normal runs read the committed fixtures (`tests/explain.rs`).
//! Needs `docker compose -f testdata/docker-compose.test.yml up -d --wait` plus the
//! `mysql84` service, which is in the `explain` profile:
//! `docker compose -f testdata/docker-compose.test.yml --profile explain up -d --wait mysql84`.

use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_query::explain::explain_sql;
use dnt_testkit::{db_enabled, Target, FIXTURE_DB};
use serde_json::json;
use std::path::PathBuf;

const QUERY_MYSQL: &str = "SELECT c.id, COUNT(*) FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = 'paid' GROUP BY c.id ORDER BY 2 DESC";
const QUERY_PG: &str = "SELECT c.id, COUNT(*) FROM public.orders o JOIN public.customers c ON c.id = o.customer_id WHERE o.status = 'paid' GROUP BY c.id ORDER BY 2 DESC";

#[derive(Default)]
struct Collect {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl RowSink for Collect {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.columns = cols.iter().map(|c| c.name.clone()).collect();
        self.rows.clear();
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/explain")
}

async fn connect(t: Target) -> Box<dyn Session> {
    let cfg = t.resolved(Some(FIXTURE_DB));
    let r = if t == Target::Pg12 || t == Target::Pg17 {
        PgDriver.connect(&cfg).await
    } else {
        MySqlDriver.connect(&cfg).await
    };
    r.unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

async fn capture(t: Target, name: &str) {
    let s = connect(t).await;
    let stmt = if t == Target::Pg12 || t == Target::Pg17 {
        QUERY_PG
    } else {
        QUERY_MYSQL
    };
    for (analyze, suffix) in [(false, "plain"), (true, "analyze")] {
        let Ok(ex) = explain_sql(stmt, s.server_info(), analyze) else {
            assert!(analyze && t == Target::MySql57, "{t:?} analyze={analyze}");
            continue;
        };
        let mut sink = Collect::default();
        s.execute(&ex.sql, ExecOptions::default(), &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{t:?} {}: {e}", ex.sql));
        let doc = json!({ "columns": sink.columns, "rows": sink.rows });
        let text = serde_json::to_string_pretty(&doc).unwrap() + "\n";
        let path = fixture_dir().join(format!("{name}_{suffix}.json"));
        std::fs::write(&path, text).unwrap();
        eprintln!("wrote {}", path.display());
    }
    s.close().await;
}

#[tokio::test]
async fn capture_explain_fixtures() {
    let update = std::env::var("DNT_UPDATE_FIXTURES").is_ok_and(|v| v == "1");
    if !db_enabled() || !update {
        eprintln!("skipped: needs DNT_TEST_DB=1 and DNT_UPDATE_FIXTURES=1");
        return;
    }
    std::fs::create_dir_all(fixture_dir()).unwrap();
    for (t, name) in [
        (Target::Pg12, "pg12"),
        (Target::Pg17, "pg17"),
        (Target::MySql8, "mysql80"),
        (Target::MySql84, "mysql84"),
        (Target::MariaDb106, "mariadb106"),
        (Target::MariaDb114, "mariadb114"),
        (Target::MySql57, "mysql57"),
    ] {
        capture(t, name).await;
    }
}
