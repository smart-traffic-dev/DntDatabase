//! Builder additions of P5 (`insert_sql_with`, upsert, `OVERRIDING SYSTEM VALUE`,
//! `dump_literal`) against the Docker test matrix (`DNT_TEST_DB=1`). Everything lives in
//! the `p5_build` database (MySQL family) / schema of the fixture database (PG), dropped
//! before (if left over) and after the test.

use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::server::DbKind;
use dnt_core::sql::build::{
    dump_literal, insert_sql_with, ColumnTarget, InsertOptions, OnConflict, TableRef,
};
use dnt_core::sql::{classify, Dialect};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB};

const SCOPE: &str = "p5_build";

struct Collect(Vec<Vec<Value>>);

impl RowSink for Collect {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.0.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn connect(t: Target) -> Box<dyn Session> {
    let driver: Box<dyn Driver> = match t.kind() {
        DbKind::MySql => Box::new(MySqlDriver),
        DbKind::Postgres => Box::new(PgDriver),
    };
    driver.connect(&t.resolved(Some(FIXTURE_DB))).await.unwrap()
}

/// Runs `sql` (a script); the rows of every result.
async fn run(s: &dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut sink = Collect(Vec::new());
    for stmt in dnt_core::sql::split(sql, s.dialect()) {
        let kind = classify(&stmt.text, s.dialect());
        s.execute_one(&stmt.text, kind, &ExecOptions::default(), &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:?}", stmt.text));
    }
    sink.0
}

/// A cell as plain text, so results compare the same whatever type the driver decodes.
fn text(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Decimal(s) | Value::Text(s) | Value::Json(s) | Value::Other { text: s, .. } => {
            s.clone()
        }
        other => format!("{other:?}"),
    }
}

async fn rows_text(s: &dyn Session, sql: &str) -> Vec<Vec<String>> {
    run(s, sql)
        .await
        .iter()
        .map(|r| r.iter().map(text).collect())
        .collect()
}

fn col(name: &str, db_type: &str, logical: LogicalType) -> ColumnTarget {
    ColumnTarget {
        name: name.into(),
        db_type: db_type.into(),
        logical,
        collation: None,
    }
}

fn fv(v: Value) -> FullValue {
    FullValue::try_from_value(v).unwrap()
}

fn table(t: Target, name: &str) -> TableRef {
    match t.kind() {
        DbKind::MySql => TableRef {
            kind: DbKind::MySql,
            database: SCOPE.into(),
            schema: None,
            name: name.into(),
        },
        DbKind::Postgres => TableRef {
            kind: DbKind::Postgres,
            database: FIXTURE_DB.into(),
            schema: Some(SCOPE.into()),
            name: name.into(),
        },
    }
}

fn drop_scope(t: Target) -> String {
    match t.kind() {
        DbKind::MySql => format!("DROP DATABASE IF EXISTS {SCOPE}"),
        DbKind::Postgres => format!("DROP SCHEMA IF EXISTS {SCOPE} CASCADE"),
    }
}

async fn exec(s: &dyn Session, sql: &str) {
    run(s, sql).await;
}

async fn upsert_round_trip(t: Target, s: &dyn Session, d: Dialect) {
    let tbl = table(t, "p5_upsert");
    let rel = tbl.relation();
    exec(
        s,
        &format!("CREATE TABLE {rel} (id INT PRIMARY KEY, name VARCHAR(50), qty INT)"),
    )
    .await;
    let cols = [
        col("id", "int4", LogicalType::Integer),
        col("name", "varchar", LogicalType::Text),
        col("qty", "int4", LogicalType::Integer),
    ];
    let row = |id: i64, name: &str, qty: i64| {
        vec![
            fv(Value::Int(id)),
            fv(Value::Text(name.into())),
            fv(Value::Int(qty)),
        ]
    };
    let append = insert_sql_with(
        &tbl,
        &cols,
        &[row(1, "it's", 1)],
        &InsertOptions::default(),
        d,
    )
    .unwrap();
    exec(s, &append).await;
    let upsert = |update: &[&str]| InsertOptions {
        overriding_system_value: false,
        on_conflict: OnConflict::Update {
            key: vec!["id".into()],
            update: update.iter().map(|u| u.to_string()).collect(),
        },
    };
    let sql = insert_sql_with(
        &tbl,
        &cols,
        &[row(1, "Nguyễn", 9)],
        &upsert(&["name", "qty"]),
        d,
    )
    .unwrap();
    exec(s, &sql).await;
    let select = format!("SELECT COUNT(*), MAX(name), MAX(qty) FROM {rel}");
    assert_eq!(
        rows_text(s, &select).await,
        vec![vec!["1".to_string(), "Nguyễn".into(), "9".into()]],
        "{t:?} upsert"
    );
    // Key only: the existing row stays as it is (PG DO NOTHING, MySQL `id` = `id`).
    let sql = insert_sql_with(&tbl, &cols, &[row(1, "x", 0)], &upsert(&[]), d).unwrap();
    exec(s, &sql).await;
    assert_eq!(
        rows_text(s, &select).await,
        vec![vec!["1".to_string(), "Nguyễn".into(), "9".into()]],
        "{t:?} key-only upsert"
    );
    // A new key is inserted.
    let sql = insert_sql_with(&tbl, &cols, &[row(2, "b", 2)], &upsert(&["name"]), d).unwrap();
    exec(s, &sql).await;
    assert_eq!(
        rows_text(s, &format!("SELECT COUNT(*) FROM {rel}")).await,
        vec![vec!["2".to_string()]],
        "{t:?} upsert insert"
    );
}

async fn pg_overriding_and_dump_literals(t: Target, s: &dyn Session) {
    let d = Dialect::Postgres;
    // OVERRIDING SYSTEM VALUE writes an identity ALWAYS column.
    let tbl = table(t, "p5_ident");
    let rel = tbl.relation();
    exec(
        s,
        &format!("CREATE TABLE {rel} (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text)"),
    )
    .await;
    let cols = [
        col("id", "int4", LogicalType::Integer),
        col("v", "text", LogicalType::Text),
    ];
    let opts = InsertOptions {
        overriding_system_value: true,
        on_conflict: OnConflict::Error,
    };
    let sql = insert_sql_with(
        &tbl,
        &cols,
        &[vec![fv(Value::Int(42)), fv(Value::Text("a".into()))]],
        &opts,
        d,
    )
    .unwrap();
    exec(s, &sql).await;
    assert_eq!(
        rows_text(s, &format!("SELECT id, v FROM {rel}")).await,
        vec![vec!["42".to_string(), "a".into()]],
        "{t:?} overriding"
    );

    // Dump literals of values `literal` refuses (arrays, `Other`) and a few it writes.
    exec(
        s,
        &format!("CREATE TYPE {SCOPE}.p5_mood AS ENUM ('ok', 'sad')"),
    )
    .await;
    let tbl = table(t, "p5_dump");
    let rel = tbl.relation();
    exec(
        s,
        &format!(
            "CREATE TABLE {rel} (ia integer[], im integer[], ta text[], ba bytea[], \
             iv interval, e {SCOPE}.p5_mood, j jsonb)"
        ),
    )
    .await;
    let cols = [
        col("ia", "integer[]", LogicalType::Array),
        col("im", "integer[]", LogicalType::Array),
        col("ta", "text[]", LogicalType::Array),
        col("ba", "bytea[]", LogicalType::Array),
        col("iv", "interval", LogicalType::Other),
        col("e", &format!("{SCOPE}.p5_mood"), LogicalType::Enum),
        col("j", "jsonb", LogicalType::Json),
    ];
    let txt = |s: &str| Value::Text(s.into());
    let values = [
        Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]),
        Value::Array(vec![
            Value::Array(vec![Value::Int(1), Value::Int(2)]),
            Value::Array(vec![Value::Int(3), Value::Int(4)]),
        ]),
        Value::Array(vec![
            txt("a b"),
            Value::Null,
            txt("it's"),
            txt("q\"x"),
            txt("back\\slash"),
            txt("NULL"),
            txt(""),
            txt("{x,y}"),
        ]),
        Value::Array(vec![Value::bytes(&[0, 0xff]), Value::Null]),
        Value::Other {
            type_name: "interval".into(),
            text: "1 day 02:00:00".into(),
        },
        txt("sad"),
        Value::Json(r#"{"a": [1, "it's"]}"#.into()),
    ];
    let lits: Vec<String> = cols
        .iter()
        .zip(values)
        .map(|(c, v)| dump_literal(&fv(v), c, d).unwrap_or_else(|e| panic!("{}: {e}", c.name)))
        .collect();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    exec(
        s,
        &format!(
            "INSERT INTO {rel} ({}) VALUES ({})",
            names.join(", "),
            lits.join(", ")
        ),
    )
    .await;
    let check = format!(
        "SELECT ia = ARRAY[1, NULL, 3], \
                im = ARRAY[[1, 2], [3, 4]], \
                ta = ARRAY['a b', NULL, 'it''s', 'q\"x', 'back\\slash', 'NULL', '', '{{x,y}}']::text[], \
                ta[2] IS NULL, ta[6] IS NOT NULL, \
                ba = ARRAY['\\x00ff'::bytea, NULL], \
                iv = interval '1 day 2 hours', \
                e::text, \
                j = '{{\"a\": [1, \"it''s\"]}}'::jsonb \
         FROM {rel}"
    );
    let t_ = "true".to_string();
    assert_eq!(
        rows_text(s, &check).await,
        vec![vec![
            t_.clone(),
            t_.clone(),
            t_.clone(),
            t_.clone(),
            t_.clone(),
            t_.clone(),
            t_.clone(),
            "sad".into(),
            t_
        ]],
        "{t:?} dump literals"
    );
}

#[tokio::test]
async fn upsert_and_dump_literal_round_trip() {
    require_db!();
    for t in Target::all() {
        let s = connect(t).await;
        let d = s.dialect();
        exec(&*s, &drop_scope(t)).await;
        match t.kind() {
            DbKind::MySql => {
                exec(
                    &*s,
                    &format!("CREATE DATABASE {SCOPE} CHARACTER SET utf8mb4"),
                )
                .await
            }
            DbKind::Postgres => exec(&*s, &format!("CREATE SCHEMA {SCOPE}")).await,
        }
        upsert_round_trip(t, &*s, d).await;
        if t.kind() == DbKind::Postgres {
            pg_overriding_and_dump_literals(t, &*s).await;
        }
        exec(&*s, &drop_scope(t)).await;
        s.close().await;
        eprintln!("{t:?}: ok");
    }
}
