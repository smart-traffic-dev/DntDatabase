//! Captures the fidelity fixtures (`crates/schema/tests/fixtures/`) from the Docker
//! matrix: each table's `TableDetail` plus the server (`<Target>_<table>.json`) and, on
//! MySQL / MariaDB, its `SHOW CREATE TABLE` (`<Target>_<table>.sql`). The fidelity unit
//! tests (`src/fidelity.rs`) read them.
//!
//! Run with `DNT_TEST_DB=1 DNT_UPDATE_FIXTURES=1 cargo test -p dnt-schema --test fidelity
//! capture_fixtures -- --ignored`. Scratch tables for the lock cases are created, captured
//! and dropped here.

use dnt_core::driver::{Introspector, Session};
use dnt_core::meta::{ObjectKind, ObjectRef, TableDetail};
use dnt_core::server::ServerInfo;
use dnt_driver_mysql::{MySqlDriver, MySqlSession};
use dnt_driver_pg::{PgDriver, PgSession};
use dnt_testkit::{require_db, Target, FIXTURE_DB};
use std::path::PathBuf;

const PG_SCRATCH: &str = "fid_scratch";
const PG_TABLESPACE: &str = "dnt_fid_ts";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn obj(schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        database: FIXTURE_DB.into(),
        schema: schema.map(str::to_string),
        name: name.into(),
        kind: ObjectKind::Table,
    }
}

/// Writes `<Target>_<table>.json` (server + detail, statistics cleared so the file is
/// stable) and, when given, `<Target>_<table>.sql`.
fn write(t: Target, table: &str, server: &ServerInfo, mut detail: TableDetail, ddl: Option<&str>) {
    detail.row_estimate = None;
    detail.size_bytes = None;
    detail.options.auto_increment = None;
    let json = serde_json::json!({ "server": server, "detail": detail });
    let base = fixtures().join(format!("{t:?}_{table}"));
    std::fs::write(
        base.with_extension("json"),
        serde_json::to_string_pretty(&json).unwrap() + "\n",
    )
    .unwrap();
    if let Some(ddl) = ddl {
        std::fs::write(base.with_extension("sql"), format!("{ddl}\n")).unwrap();
    }
}

async fn my_exec(s: &MySqlSession, sql: &str) {
    s.query_row_text(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

async fn capture_mysql(t: Target, s: &MySqlSession, table: &str) {
    let o = obj(None, table);
    let detail = s
        .table_detail(&o)
        .await
        .unwrap_or_else(|e| panic!("{t:?} {table}: {e}"));
    let ddl = s
        .object_ddl(&o)
        .await
        .unwrap_or_else(|e| panic!("{t:?} {table}: {e}"));
    write(t, table, s.server_info(), detail, Some(&ddl));
}

async fn pg_exec(s: &PgSession, sql: &str) {
    s.query_row_text(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

async fn capture_pg(t: Target, s: &PgSession, schema: &str, table: &str) {
    let detail = s
        .table_detail(&obj(Some(schema), table))
        .await
        .unwrap_or_else(|e| panic!("{t:?} {schema}.{table}: {e}"));
    write(t, table, s.server_info(), detail, None);
}

#[tokio::test]
#[ignore = "writes fixtures: DNT_TEST_DB=1 DNT_UPDATE_FIXTURES=1"]
async fn capture_fixtures() {
    require_db!();
    assert_eq!(
        std::env::var("DNT_UPDATE_FIXTURES").as_deref(),
        Ok("1"),
        "set DNT_UPDATE_FIXTURES=1 to rewrite the fixtures"
    );
    std::fs::create_dir_all(fixtures()).unwrap();

    for t in Target::mysql_family() {
        let s = MySqlDriver
            .connect_session(&t.resolved(None))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        for table in ["orders", "all_types", "weird_names"] {
            capture_mysql(t, &s, table).await;
        }
        // Lock cases: a FULLTEXT index (always locked) next to a plain one, and on
        // MySQL 8 a NOT ENFORCED check next to an enforced one.
        let checks = if t == Target::MySql8 {
            ", CONSTRAINT ck_enforced CHECK (a > 0), \
             CONSTRAINT ck_not_enforced CHECK (a < 100) NOT ENFORCED"
        } else {
            ""
        };
        let table = format!("`{FIXTURE_DB}`.`fid_mysql_locks`");
        my_exec(&s, &format!("DROP TABLE IF EXISTS {table}")).await;
        my_exec(
            &s,
            &format!(
                "CREATE TABLE {table} (id int NOT NULL, a int NULL, body text NULL, \
                 PRIMARY KEY (id), KEY k_a (a), FULLTEXT KEY ft_body (body){checks}) \
                 ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
            ),
        )
        .await;
        capture_mysql(t, &s, "fid_mysql_locks").await;
        my_exec(&s, &format!("DROP TABLE {table}")).await;
    }

    for t in Target::pg() {
        let s = PgDriver
            .connect_session(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        for (schema, table) in [
            ("public", "orders"),
            ("public", "all_types"),
            ("public", "weird_names"),
            ("sales", "invoices"),
            ("sales", "measurements"),
        ] {
            capture_pg(t, &s, schema, table).await;
        }
        // Lock cases (T5-6, T9-1): index opclass, per-part COLLATE, NULLS FIRST, WITH (…)
        // storage parameters, an EXCLUDE constraint and a plain and a constraint index
        // in a non-default tablespace, next to a plain index. The tablespace directory
        // is made through the server (superuser `COPY … TO PROGRAM`), and CREATE /
        // DROP TABLESPACE run on their own (not inside a transaction block).
        pg_exec(&s, &format!("DROP SCHEMA IF EXISTS {PG_SCRATCH} CASCADE")).await;
        pg_exec(&s, &format!("DROP TABLESPACE IF EXISTS {PG_TABLESPACE}")).await;
        pg_exec(
            &s,
            &format!("COPY (SELECT 1) TO PROGRAM 'mkdir -p /tmp/{PG_TABLESPACE}'"),
        )
        .await;
        pg_exec(
            &s,
            &format!("CREATE TABLESPACE {PG_TABLESPACE} LOCATION '/tmp/{PG_TABLESPACE}'"),
        )
        .await;
        pg_exec(&s, &format!("CREATE SCHEMA {PG_SCRATCH}")).await;
        pg_exec(
            &s,
            &format!(
                "CREATE TABLE {PG_SCRATCH}.fid_pg_locks (id integer PRIMARY KEY, \
                 name text NULL, span int4range NULL, \
                 CONSTRAINT fid_excl EXCLUDE USING gist (span WITH &&), \
                 CONSTRAINT fid_uq_ts UNIQUE (id, name) USING INDEX TABLESPACE {PG_TABLESPACE}); \
                 CREATE INDEX idx_ts ON {PG_SCRATCH}.fid_pg_locks (name) TABLESPACE {PG_TABLESPACE}; \
                 CREATE INDEX idx_plain ON {PG_SCRATCH}.fid_pg_locks (name); \
                 CREATE INDEX idx_opclass ON {PG_SCRATCH}.fid_pg_locks (name text_pattern_ops); \
                 CREATE INDEX idx_collate ON {PG_SCRATCH}.fid_pg_locks (name COLLATE \"C\"); \
                 CREATE INDEX idx_nulls ON {PG_SCRATCH}.fid_pg_locks (name NULLS FIRST); \
                 CREATE INDEX idx_with ON {PG_SCRATCH}.fid_pg_locks (name) WITH (fillfactor = 70)"
            ),
        )
        .await;
        capture_pg(t, &s, PG_SCRATCH, "fid_pg_locks").await;
        pg_exec(&s, &format!("DROP SCHEMA {PG_SCRATCH} CASCADE")).await;
        pg_exec(&s, &format!("DROP TABLESPACE {PG_TABLESPACE}")).await;
    }
}
