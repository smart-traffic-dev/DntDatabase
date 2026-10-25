//! The integrated dump against the Docker test matrix (`DNT_TEST_DB=1`): a fixture with
//! the types the driver cannot decode (ranges, geometric, tsquery, arrays of ranges;
//! MySQL spatial), domains, a descending identity, a collation and views whose aliases
//! name other views is dumped through the app's dump job, restored statement by
//! statement (as `split` with the default dialect gives them) and compared with the
//! source. Everything lives in `p5_dump_db*` (dropped before and after).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::meta::ObjectKind;
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, Dialect};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::backup::{DumpObject, DumpRequest};
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const SRC: &str = "p5_dump_db";
const DST: &str = "p5_dump_db_r";

#[derive(Default)]
struct Collect(Vec<Vec<Value>>);
impl RowSink for Collect {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, rows: Vec<Vec<Value>>) {
        self.0.extend(rows);
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}

async fn connect(t: Target, db: &str) -> Box<dyn Session> {
    let driver: Box<dyn Driver> = match t.kind() {
        DbKind::MySql => Box::new(MySqlDriver),
        DbKind::Postgres => Box::new(PgDriver),
    };
    driver.connect(&t.resolved(Some(db))).await.unwrap()
}

/// Runs every statement of `sql` (split by the session's dialect); the rows of all.
async fn run(s: &dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut sink = Collect::default();
    for stmt in dnt_core::sql::split(sql, s.dialect()) {
        let kind = classify(&stmt.text, s.dialect());
        s.execute_one(&stmt.text, kind, &ExecOptions::default(), &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:?}", stmt.text));
    }
    sink.0
}

/// Restores a dump the way the app runner reads it: `split` with the engine's default
/// dialect, one statement at a time.
async fn restore(s: &dyn Session, dump: &str, d: Dialect) {
    for stmt in dnt_core::sql::split(dump, d) {
        let kind = classify(&stmt.text, s.dialect());
        s.execute_one(
            &stmt.text,
            kind,
            &ExecOptions::default(),
            &mut Collect::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("restoring {}: {e:?}", stmt.text));
    }
}

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap()
}

/// A read-only connection (a dump is allowed there).
async fn open(state: &AppState, t: Target, db: &str) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: Some(db.into()),
        env_tag: None,
        read_only: true,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    };
    let secrets: SecretsInput =
        serde_json::from_value(json!({ "db_password": TEST_PASSWORD })).unwrap();
    let id = create_connection_impl(state, cfg, secrets)
        .await
        .unwrap()
        .id;
    open_connection_impl(state, id, None).await.unwrap();
    id
}

/// Runs a dump job; the dump's text and the job log.
async fn dump(state: &AppState, req: DumpRequest) -> (String, String) {
    let path = std::path::PathBuf::from(&req.path);
    let compress = req.compress;
    let id = state.sessions.dump_start(req).await.unwrap();
    assert!(state.sessions.jobs.wait(id, Duration::from_secs(120)).await);
    let d = state.sessions.jobs.detail(id).unwrap();
    let log: Vec<String> = d.log.iter().map(|l| l.message.clone()).collect();
    assert!(d.job.error.is_none(), "{:?}\n{log:#?}", d.job.error);
    let mut text = String::new();
    let file = std::fs::File::open(&path).unwrap();
    if compress {
        flate2::read::MultiGzDecoder::new(file)
            .read_to_string(&mut text)
            .unwrap();
    } else {
        std::io::BufReader::new(file)
            .read_to_string(&mut text)
            .unwrap();
    }
    (text, log.join("\n"))
}

fn text(v: &Value) -> String {
    format!("{v:?}")
}

async fn snapshot(s: &dyn Session, queries: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for q in queries {
        let rows = run(s, q).await;
        out.push(format!(
            "{q}: {:?}",
            rows.iter()
                .map(|r| r.iter().map(text).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        ));
    }
    out
}

const MYSQL_FIXTURE: &str = "
SET SESSION sql_mode = CONCAT(@@sql_mode, ',NO_AUTO_VALUE_ON_ZERO');
CREATE TABLE parent (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) COMMENT 'a; b', b VARBINARY(10), pt POINT NULL, g GEOMETRY NULL) ENGINE=InnoDB;
CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, qty INT, dbl INT AS (qty * 2), CONSTRAINT child_parent FOREIGN KEY (parent_id) REFERENCES parent (id)) ENGINE=InnoDB;
INSERT INTO parent (id, name, b, pt, g) VALUES (0, 'zero', NULL, NULL, NULL), (1, 'O''Brien; -- x \\\\ y', X'00FF27', ST_GeomFromText('POINT(1.5 -2)'), ST_GeomFromText('LINESTRING(0 0, 1 1, 2 0)')), (2, NULL, NULL, ST_GeomFromText('POINT(0 0)'), NULL);
INSERT INTO child (id, parent_id, qty) VALUES (1, 1, 5), (2, 2, NULL);
CREATE VIEW v_b AS SELECT id AS v_a FROM parent;
CREATE VIEW v_a AS SELECT v_a AS x FROM v_b;
";

#[tokio::test]
async fn mysql_dump_round_trip_with_spatial_values() {
    require_db!();
    for t in Target::mysql_family() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let admin = connect(t, FIXTURE_DB).await;
        run(
            admin.as_ref(),
            &format!(
                "DROP DATABASE IF EXISTS {SRC}; DROP DATABASE IF EXISTS {DST}; \
                 CREATE DATABASE {SRC}; CREATE DATABASE {DST}"
            ),
        )
        .await;
        let src = connect(t, SRC).await;
        run(src.as_ref(), MYSQL_FIXTURE).await;
        let conn = open(&state, t, SRC).await;
        let listed = state
            .sessions
            .dump_objects(conn, SRC.into(), vec![])
            .await
            .unwrap();
        let path = dir.path().join("d.sql");
        let (text, _log) = dump(
            &state,
            DumpRequest {
                connection_id: conn,
                database: SRC.into(),
                schemas: vec![],
                objects: listed.objects,
                include_drop: true,
                compress: false,
                path: path.to_string_lossy().into_owned(),
            },
        )
        .await;
        // Spatial values as their stored bytes.
        assert!(text.contains(", X'"), "{t:?}\n{text}");
        // `v_b` names a column `v_a`: it still comes first.
        let vb = text.find("VIEW `v_b`").unwrap();
        let va = text.find("VIEW `v_a`").unwrap();
        assert!(vb < va, "{t:?}\n{text}");
        let dst = connect(t, DST).await;
        restore(dst.as_ref(), &text, Dialect::MYSQL).await;
        let queries = |db: &str| {
            vec![
                format!(
                    "SELECT id, name, HEX(b), ST_AsText(pt), ST_SRID(pt), ST_AsText(g) \
                     FROM `{db}`.parent ORDER BY id"
                ),
                format!("SELECT * FROM `{db}`.child ORDER BY id"),
                format!("SELECT * FROM `{db}`.v_a ORDER BY x"),
                format!(
                    "SELECT AUTO_INCREMENT FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = '{db}' AND TABLE_NAME = 'parent'"
                ),
            ]
        };
        let a = snapshot(src.as_ref(), &queries(SRC)).await;
        let b = snapshot(dst.as_ref(), &queries(DST)).await;
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.replace(SRC, "X"), y.replace(DST, "X"), "{t:?}");
        }
        run(
            admin.as_ref(),
            &format!("DROP DATABASE IF EXISTS {SRC}; DROP DATABASE IF EXISTS {DST}"),
        )
        .await;
    }
}

const PG_FIXTURE: &str = "
CREATE SCHEMA p5_dump_db;
CREATE TYPE p5_dump_db.mood AS ENUM ('ok', 'sad');
CREATE DOMAIN p5_dump_db.mood_d AS p5_dump_db.mood NOT NULL DEFAULT 'ok';
CREATE DOMAIN p5_dump_db.pos_int AS integer DEFAULT 1 CONSTRAINT pos CHECK (VALUE > 0);
CREATE TYPE p5_dump_db.pair AS (a p5_dump_db.pos_int, b text);
CREATE SEQUENCE p5_dump_db.counter START 1;
SELECT nextval('p5_dump_db.counter');
SELECT nextval('p5_dump_db.counter');
CREATE TABLE p5_dump_db.parent (
    id INT GENERATED ALWAYS AS IDENTITY (INCREMENT BY -1 START WITH -1 MAXVALUE -1 MINVALUE -1000) PRIMARY KEY,
    name TEXT COLLATE \"C\",
    m p5_dump_db.mood_d,
    pi p5_dump_db.pos_int,
    pr p5_dump_db.pair,
    tags TEXT[],
    b BYTEA,
    j JSONB,
    at TIMESTAMPTZ,
    r int4range,
    tr tstzrange,
    pt point,
    q tsquery,
    rs int4range[],
    iv interval,
    net inet
);
COMMENT ON TABLE p5_dump_db.parent IS 'it''s';
CREATE TABLE p5_dump_db.child (id SERIAL PRIMARY KEY, parent_id INT REFERENCES p5_dump_db.parent (id), qty INT, dbl INT GENERATED ALWAYS AS (qty * 2) STORED);
CREATE INDEX child_qty ON p5_dump_db.child (qty);
INSERT INTO p5_dump_db.parent (name, m, pi, pr, tags, b, j, at, r, tr, pt, q, rs, iv, net) VALUES
  ('O''Brien; -- $$ x', 'sad', 5, ROW(2, 'x y'), '{\"a b\",NULL,c}', '\\x00ff27', '{\"a\": [1]}', '2026-10-25 01:02:03+02',
   '[1,5)', '[2026-01-01 00:00:00+00,2026-02-01 00:00:00+00)', '(1.5,-2)', 'fat & (rat | cat)', '{\"[1,5)\",\"[7,9)\",empty}',
   '1 year 2 mons 3 days 04:05:06.7', '10.1.2.0/24'),
  (NULL, 'ok', NULL, NULL, NULL, NULL, NULL, NULL, 'empty', NULL, NULL, NULL, NULL, NULL, NULL);
INSERT INTO p5_dump_db.child (parent_id, qty) VALUES (-1, 5), (-2, NULL), (-1, 7);
CREATE TABLE p5_dump_db.t (x INT);
CREATE VIEW p5_dump_db.v_b AS SELECT x AS v_a FROM p5_dump_db.t;
CREATE VIEW p5_dump_db.v_a AS SELECT v_a FROM p5_dump_db.v_b;
";

#[tokio::test]
async fn pg_dump_round_trip_with_undecoded_types() {
    require_db!();
    for t in Target::pg() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let admin = connect(t, FIXTURE_DB).await;
        run(
            admin.as_ref(),
            &format!("DROP SCHEMA IF EXISTS {SRC} CASCADE"),
        )
        .await;
        run(admin.as_ref(), PG_FIXTURE).await;
        let conn = open(&state, t, FIXTURE_DB).await;
        let listed = state
            .sessions
            .dump_objects(conn, FIXTURE_DB.into(), vec![SRC.into()])
            .await
            .unwrap();
        // Domains are offered; the types are left out of the request on purpose: the
        // tables bring them along.
        assert!(listed
            .objects
            .iter()
            .any(|o| o.kind == ObjectKind::Type && o.name == "pos_int"));
        let objects: Vec<DumpObject> = listed
            .objects
            .into_iter()
            .filter(|o| o.kind != ObjectKind::Type)
            .collect();
        let path = dir.path().join("d.sql.gz");
        let (text, log) = dump(
            &state,
            DumpRequest {
                connection_id: conn,
                database: FIXTURE_DB.into(),
                schemas: vec![SRC.into()],
                objects,
                include_drop: false,
                compress: true,
                path: path.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(log.contains("Type p5_dump_db.mood is dumped too"), "{log}");
        assert!(log.contains("Type p5_dump_db.pair is dumped too"), "{log}");
        for part in [
            "'[1,5)'::int4range",
            "'(1.5,-2)'::point",
            "'''fat'' & ( ''rat'' | ''cat'' )'::tsquery",
            "::int4range[]",
            "INCREMENT BY -1 MINVALUE -1000 MAXVALUE -1",
            "name text COLLATE \"pg_catalog\".\"C\"",
            "CREATE DOMAIN \"p5_dump_db\".\"pos_int\" AS integer DEFAULT 1",
        ] {
            assert!(text.contains(part), "{t:?}: {part}\n{text}");
        }
        let vb = text.find("VIEW p5_dump_db.v_b").unwrap();
        let va = text.find("VIEW p5_dump_db.v_a").unwrap();
        assert!(vb < va, "{t:?}\n{text}");
        let queries: Vec<String> = vec![
            format!("SELECT row_to_json(p)::text FROM {SRC}.parent p ORDER BY id"),
            format!("SELECT row_to_json(c)::text FROM {SRC}.child c ORDER BY id"),
            format!("SELECT * FROM {SRC}.v_a"),
            format!("SELECT last_value, is_called FROM {SRC}.counter"),
            format!(
                "SELECT column_name::text, collation_name::text FROM information_schema.columns \
                 WHERE table_schema = '{SRC}' AND collation_name IS NOT NULL \
                 ORDER BY table_name, column_name"
            ),
            format!(
                "SELECT seqstart, seqincrement, seqmin, seqmax FROM pg_sequence \
                 WHERE seqrelid = pg_get_serial_sequence('{SRC}.parent', 'id')::regclass"
            ),
            format!(
                "SELECT pg_get_serial_sequence('{SRC}.parent', 'id'), \
                 pg_get_serial_sequence('{SRC}.child', 'id')"
            ),
            format!(
                "SELECT t.typname::text, pg_catalog.format_type(t.typbasetype, t.typtypmod), \
                 t.typnotnull, t.typdefault FROM pg_type t JOIN pg_namespace n \
                 ON n.oid = t.typnamespace WHERE n.nspname = '{SRC}' AND t.typtype = 'd' ORDER BY 1"
            ),
            format!("SELECT nextval(pg_get_serial_sequence('{SRC}.child', 'id'))"),
            format!("INSERT INTO {SRC}.parent (name) VALUES ('next') RETURNING id"),
        ];
        let before = snapshot(admin.as_ref(), &queries).await;
        run(admin.as_ref(), &format!("DROP SCHEMA {SRC} CASCADE")).await;
        let dst = connect(t, FIXTURE_DB).await;
        restore(dst.as_ref(), &text, Dialect::Postgres).await;
        let after = snapshot(dst.as_ref(), &queries).await;
        for (x, y) in before.iter().zip(&after) {
            assert_eq!(x, y, "{t:?}\n{text}");
        }
        run(
            admin.as_ref(),
            &format!("DROP SCHEMA IF EXISTS {SRC} CASCADE"),
        )
        .await;
    }
}
