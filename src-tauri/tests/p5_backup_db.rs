//! Dump and restore (P5c, spec §7) against the Docker test matrix (`DNT_TEST_DB=1`),
//! through the command `*_impl` functions and the job manager (no Tauri runtime).
//!
//! Layout per server (every name prefixed `p5_`, dropped first if left over and dropped
//! again by a guard when the test ends, also on failure):
//!
//! - MySQL family: a source database `p5_src` with copies of the fixture tables
//!   (customers, orders with its FK, `đơn_hàng`, weird_names, bin_blob, all_types) and a
//!   view, filled from `dnt_fixture`; restores go into a fresh database `p5_dst`
//!   (`p5_dst2` for the mysqldump file).
//! - PostgreSQL: a source schema `p5_src` inside `dnt_fixture` (its own enum type, an
//!   identity `BY DEFAULT` and an identity `ALWAYS` table); the integrated dump is
//!   schema-qualified, so it is restored into a fresh database `p5_dst`, where it creates
//!   the schema `p5_src` again. Source and copy are then compared table by table.
//!
//! Run with `$env:DNT_TEST_DB = "1"; cargo test -p dntdatabase --test p5_backup_db --
//! --test-threads=1`.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{CellMode, Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::ObjectKind;
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, split};
use dnt_core::value::{ColumnMeta, FullValue, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dnt_transfer::detect::RestorePath;
use dnt_transfer::tools::{find_tool, MysqlDumpOptions, Tool};
use dntdatabase::backup::{DumpRequest, RestoreRequest, ToolDumpRequest};
use dntdatabase::commands::backup::{
    dump_objects_impl, dump_start_impl, restore_preview_impl, restore_start_impl,
    tool_dump_start_impl,
};
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::file_picks::FileAccess;
use dntdatabase::jobs::{JobDetail, JobState};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const JOB_TIMEOUT: Duration = Duration::from_secs(180);
const SRC: &str = "p5_src";
const DST: &str = "p5_dst";
const DST2: &str = "p5_dst2";
/// The tables copied from the fixture (all keyed by `id`).
const TABLES: [&str; 6] = [
    "customers",
    "orders",
    "đơn_hàng",
    "weird_names",
    "bin_blob",
    "all_types",
];

// ------------------------------------------------------------------ direct sessions

#[derive(Default)]
struct Collect(Vec<Vec<FullValue>>);

impl RowSink for Collect {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.0.extend(batch.into_iter().map(|r| {
            r.into_iter()
                .map(|v| FullValue::try_from_value(v).expect("complete value"))
                .collect()
        }));
    }
    fn full_rows(&mut self, batch: Vec<Vec<FullValue>>) {
        self.0.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

async fn connect(t: Target, db: &str) -> Result<Box<dyn Session>, DbError> {
    let driver: Box<dyn Driver> = match t.kind() {
        DbKind::MySql => Box::new(MySqlDriver),
        DbKind::Postgres => Box::new(PgDriver),
    };
    driver.connect(&t.resolved(Some(db))).await
}

fn full_opts() -> ExecOptions {
    ExecOptions {
        full_bytes: true,
        cells: CellMode::Full {
            max_cell_bytes: 64 * 1024 * 1024,
        },
        ..ExecOptions::default()
    }
}

/// Runs `sql` (a script) on a fresh admin session to `db`; the rows of every result.
async fn run_on(t: Target, db: &str, sql: &str) -> Vec<Vec<FullValue>> {
    let s = connect(t, db).await.unwrap();
    let mut sink = Collect::default();
    for stmt in split(sql, s.dialect()) {
        let kind = classify(&stmt.text, s.dialect());
        let shown: String = stmt.text.chars().take(200).collect();
        s.execute_one(&stmt.text, kind, &full_opts(), &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{t:?} {shown}: {e:?}"));
    }
    s.close().await;
    sink.0
}

async fn run(t: Target, sql: &str) -> Vec<Vec<FullValue>> {
    run_on(t, FIXTURE_DB, sql).await
}

fn int(v: &FullValue) -> i64 {
    match v.as_value().unwrap() {
        Value::Int(n) => *n,
        Value::UInt(n) => *n as i64,
        Value::Decimal(s) | Value::Text(s) => s.parse().unwrap(),
        other => panic!("not an integer: {other:?}"),
    }
}

// ------------------------------------------------------------------ scope guard

/// Every `p5_` database / schema a test creates; dropped when the guard goes (also when
/// the test panics). PostgreSQL databases are dropped after their sessions are ended.
struct Scope {
    t: Target,
}

fn drop_all_sql(t: Target) -> Vec<(String, String)> {
    match t.kind() {
        DbKind::MySql => [SRC, DST, DST2]
            .iter()
            .map(|d| {
                (
                    FIXTURE_DB.to_string(),
                    format!("DROP DATABASE IF EXISTS {d}"),
                )
            })
            .collect(),
        DbKind::Postgres => vec![
            (
                FIXTURE_DB.to_string(),
                format!("DROP SCHEMA IF EXISTS {SRC} CASCADE"),
            ),
            (
                "postgres".to_string(),
                format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{DST}' AND pid <> pg_backend_pid()"
                ),
            ),
            (
                "postgres".to_string(),
                format!("DROP DATABASE IF EXISTS {DST}"),
            ),
        ],
    }
}

/// Runs each `(database, statement)` on its own thread and runtime (usable from `Drop`).
fn run_detached(t: Target, stmts: Vec<(String, String)>) {
    let _ = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for (db, sql) in stmts {
                if let Ok(s) = connect(t, &db).await {
                    let mut sink = Collect::default();
                    let _ = s.execute(&sql, ExecOptions::default(), &mut sink).await;
                    s.close().await;
                }
            }
        });
    })
    .join();
}

impl Scope {
    fn new(t: Target) -> Scope {
        run_detached(t, drop_all_sql(t));
        Scope { t }
    }

    /// The target database `name` created fresh.
    async fn fresh_target(&self, name: &str) {
        assert!(name.starts_with("p5_"));
        match self.t.kind() {
            DbKind::MySql => {
                run(
                    self.t,
                    &format!(
                        "DROP DATABASE IF EXISTS {name};\n\
                         CREATE DATABASE {name} CHARACTER SET utf8mb4"
                    ),
                )
                .await;
            }
            DbKind::Postgres => {
                run_detached(
                    self.t,
                    vec![
                        (
                            "postgres".into(),
                            format!(
                                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                                 WHERE datname = '{name}' AND pid <> pg_backend_pid()"
                            ),
                        ),
                        ("postgres".into(), format!("DROP DATABASE IF EXISTS {name}")),
                    ],
                );
                run_on(self.t, "postgres", &format!("CREATE DATABASE {name}")).await;
            }
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        run_detached(self.t, drop_all_sql(self.t));
    }
}

const MYSQL_SOURCE: &str = "
CREATE DATABASE p5_src CHARACTER SET utf8mb4;
CREATE TABLE p5_src.customers LIKE dnt_fixture.customers;
INSERT INTO p5_src.customers SELECT * FROM dnt_fixture.customers;
CREATE TABLE p5_src.orders LIKE dnt_fixture.orders;
INSERT INTO p5_src.orders SELECT * FROM dnt_fixture.orders;
ALTER TABLE p5_src.orders ADD CONSTRAINT fk_p5_orders_customer
  FOREIGN KEY (customer_id) REFERENCES p5_src.customers (id);
CREATE TABLE p5_src.`đơn_hàng` LIKE dnt_fixture.`đơn_hàng`;
INSERT INTO p5_src.`đơn_hàng` SELECT * FROM dnt_fixture.`đơn_hàng`;
CREATE TABLE p5_src.weird_names LIKE dnt_fixture.weird_names;
INSERT INTO p5_src.weird_names SELECT * FROM dnt_fixture.weird_names;
CREATE TABLE p5_src.bin_blob LIKE dnt_fixture.bin_blob;
INSERT INTO p5_src.bin_blob SELECT * FROM dnt_fixture.bin_blob;
CREATE TABLE p5_src.all_types LIKE dnt_fixture.all_types;
INSERT INTO p5_src.all_types SELECT * FROM dnt_fixture.all_types;
CREATE VIEW p5_src.v_customer_orders AS
  SELECT c.id AS customer_id, c.name AS customer_name, o.id AS order_id, o.status, o.total
  FROM p5_src.customers c JOIN p5_src.orders o ON o.customer_id = c.id;
";

const PG_SOURCE: &str = "
CREATE SCHEMA p5_src;
CREATE TYPE p5_src.p5_status AS ENUM ('new', 'paid', 'shipped', 'cancelled');
CREATE TABLE p5_src.customers (
  id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
  name varchar(100) NOT NULL,
  email varchar(255) NOT NULL UNIQUE,
  created_at timestamp NULL
);
INSERT INTO p5_src.customers SELECT * FROM public.customers;
SELECT setval(pg_get_serial_sequence('p5_src.customers', 'id'), 3);
CREATE TABLE p5_src.orders (
  id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  customer_id integer NOT NULL REFERENCES p5_src.customers (id),
  status p5_src.p5_status NOT NULL DEFAULT 'new',
  total numeric(12,2) NOT NULL DEFAULT 0 CHECK (total >= 0),
  created_at timestamp NULL
);
INSERT INTO p5_src.orders OVERRIDING SYSTEM VALUE
  SELECT id, customer_id, status::text::p5_src.p5_status, total, created_at FROM public.orders;
SELECT setval(pg_get_serial_sequence('p5_src.orders', 'id'), 3);
CREATE TABLE p5_src.\"đơn_hàng\" (LIKE public.\"đơn_hàng\" INCLUDING ALL);
INSERT INTO p5_src.\"đơn_hàng\" SELECT * FROM public.\"đơn_hàng\";
SELECT setval(pg_get_serial_sequence('p5_src.\"đơn_hàng\"', 'id'), 2);
CREATE TABLE p5_src.weird_names (LIKE public.weird_names INCLUDING ALL);
INSERT INTO p5_src.weird_names SELECT * FROM public.weird_names;
CREATE TABLE p5_src.bin_blob (LIKE public.bin_blob INCLUDING ALL);
INSERT INTO p5_src.bin_blob SELECT * FROM public.bin_blob;
CREATE TABLE p5_src.all_types (LIKE public.all_types INCLUDING ALL);
INSERT INTO p5_src.all_types SELECT * FROM public.all_types;
ALTER TABLE p5_src.all_types ALTER COLUMN c_enum TYPE p5_src.p5_status
  USING c_enum::text::p5_src.p5_status;
CREATE VIEW p5_src.v_customer_orders AS
  SELECT c.id AS customer_id, c.name AS customer_name, o.id AS order_id, o.status, o.total
  FROM p5_src.customers c JOIN p5_src.orders o ON o.customer_id = c.id;
";

async fn create_source(t: Target) {
    match t.kind() {
        DbKind::MySql => run(t, MYSQL_SOURCE).await,
        DbKind::Postgres => run(t, PG_SOURCE).await,
    };
}

// ------------------------------------------------------------------ app state

fn app_state(dir: &Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open_conn(state: &AppState, t: Target, read_only: bool) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: Some(FIXTURE_DB.into()),
        env_tag: None,
        read_only,
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

async fn finish(state: &AppState, id: Uuid) -> JobDetail {
    let jobs = &state.sessions.jobs;
    assert!(jobs.wait(id, JOB_TIMEOUT).await, "job {id} did not finish");
    jobs.detail(id).unwrap()
}

fn assert_done(t: Target, d: &JobDetail) {
    assert_eq!(
        d.job.state,
        JobState::Done,
        "{t:?} {}: {:?}\nlog: {:#?}",
        d.job.title,
        d.job.error,
        d.log
    );
}

/// The integrated dump of the source (every object it offers, tables with data).
async fn dump_source(state: &AppState, conn: Uuid, t: Target, path: &Path, compress: bool) {
    let (database, schemas) = match t.kind() {
        DbKind::MySql => (SRC.to_string(), vec![]),
        DbKind::Postgres => (FIXTURE_DB.to_string(), vec![SRC.to_string()]),
    };
    let listed = dump_objects_impl(state, conn, database.clone(), schemas.clone())
        .await
        .unwrap();
    let mut objects = listed.objects;
    for o in &mut objects {
        o.data = o.kind == ObjectKind::Table;
    }
    let tables = objects
        .iter()
        .filter(|o| o.kind == ObjectKind::Table)
        .count();
    assert_eq!(tables, TABLES.len(), "{t:?}: {objects:?}");
    assert!(
        objects.iter().any(|o| o.kind == ObjectKind::View),
        "{t:?}: {objects:?}"
    );
    let admitted = state
        .file_picks
        .admit(FileAccess::DumpWrite, path.to_path_buf())
        .unwrap();
    let id = dump_start_impl(
        state,
        DumpRequest {
            connection_id: conn,
            database,
            schemas,
            objects,
            include_drop: false,
            compress,
            path: admitted,
        },
    )
    .await
    .unwrap();
    assert_done(t, &finish(state, id).await);
}

/// Preview + start + wait of a restore of `path` into `database`.
async fn restore(
    state: &AppState,
    conn: Uuid,
    t: Target,
    path: &Path,
    database: &str,
) -> (dntdatabase::backup::RestorePreview, JobDetail) {
    let admitted = state
        .file_picks
        .admit(FileAccess::RestoreRead, path.to_path_buf())
        .unwrap();
    let preview = restore_preview_impl(
        state,
        RestoreRequest {
            connection_id: conn,
            database: database.into(),
            path: admitted,
            single_transaction: true,
            clean: false,
            no_owner: true,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("{t:?} preview {path:?}: {e:?}"));
    let id = restore_start_impl(state, preview.plan_id, true, None)
        .await
        .unwrap_or_else(|e| panic!("{t:?} start: {e:?}"));
    let d = finish(state, id).await;
    (preview, d)
}

/// `(database to connect to, relation)` of table `name` in the source / a target.
fn relation(t: Target, db: &str, name: &str) -> (String, String) {
    match t.kind() {
        DbKind::MySql => (FIXTURE_DB.into(), format!("`{db}`.`{name}`")),
        DbKind::Postgres => (db.into(), format!("\"{SRC}\".\"{name}\"")),
    }
}

async fn rows(t: Target, db: &str, name: &str, order: &str) -> Vec<Vec<FullValue>> {
    let (connect_db, rel) = relation(t, db, name);
    run_on(
        t,
        &connect_db,
        &format!("SELECT * FROM {rel} ORDER BY {order}"),
    )
    .await
}

/// Every table of `tables` and (`with_view`) the view of the copy in `dst` equal the
/// source, row by row.
async fn assert_same(t: Target, dst: &str, label: &str, tables: &[&str], with_view: bool) {
    let src_db = match t.kind() {
        DbKind::MySql => SRC,
        DbKind::Postgres => FIXTURE_DB,
    };
    for &name in tables {
        let a = rows(t, src_db, name, "id").await;
        let b = rows(t, dst, name, "id").await;
        assert!(!a.is_empty(), "{t:?} {name}: the source has rows");
        assert_eq!(a, b, "{t:?} {label}: table {name} differs");
    }
    if with_view {
        let a = rows(t, src_db, "v_customer_orders", "order_id").await;
        let b = rows(t, dst, "v_customer_orders", "order_id").await;
        assert_eq!(a.len(), 3, "{t:?}");
        assert_eq!(a, b, "{t:?} {label}: the view differs");
    }
}

/// The copy's generated keys continue after the restored rows.
async fn assert_keys_continue(t: Target, dst: &str) {
    match t.kind() {
        DbKind::MySql => {
            let r = run(
                t,
                &format!(
                    "INSERT INTO `{dst}`.customers (name, email) VALUES ('n', 'n@x');\n\
                     SELECT LAST_INSERT_ID()"
                ),
            )
            .await;
            assert_eq!(int(&r[0][0]), 4, "{t:?}: AUTO_INCREMENT continues");
        }
        DbKind::Postgres => {
            let r = run_on(
                t,
                dst,
                "INSERT INTO p5_src.customers (name, email) VALUES ('n', 'n@x') RETURNING id;\n\
                 INSERT INTO p5_src.orders (customer_id) VALUES (1) RETURNING id;\n\
                 INSERT INTO p5_src.\"đơn_hàng\" (ten) VALUES ('x') RETURNING id",
            )
            .await;
            assert_eq!(int(&r[0][0]), 4, "{t:?}: identity BY DEFAULT continues");
            assert_eq!(int(&r[1][0]), 4, "{t:?}: identity ALWAYS continues");
            assert_eq!(int(&r[2][0]), 3, "{t:?}: đơn_hàng identity continues");
        }
    }
}

// ------------------------------------------------------------------ tests

async fn round_trip_on(t: Target) {
    let scope = Scope::new(t);
    create_source(t).await;
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let conn = open_conn(&state, t, false).await;
    for compress in [false, true] {
        let name = if compress { "dump.sql.gz" } else { "dump.sql" };
        let path = dir.path().join(name);
        dump_source(&state, conn, t, &path, compress).await;
        scope.fresh_target(DST).await;
        let (preview, d) = restore(&state, conn, t, &path, DST).await;
        assert_done(t, &d);
        assert_eq!(preview.path, RestorePath::AppRunner, "{t:?}");
        assert_eq!(preview.compressed, compress);
        let stats = preview.stats.as_ref().unwrap();
        assert!(stats.app_dump, "{t:?}: {stats:?}");
        assert_eq!(preview.transactional, t.kind() == DbKind::Postgres, "{t:?}");
        assert_same(t, DST, name, &TABLES, true).await;
        assert_keys_continue(t, DST).await;
        eprintln!(
            "{t:?} {name}: {} statements restored ({})",
            stats.statements,
            d.job.summary.unwrap_or_default()
        );
    }
    state.sessions.close(conn).await;
    drop(state);
    drop(scope);
}

#[tokio::test(flavor = "multi_thread")]
async fn dump_restore_round_trip_identical() {
    require_db!();
    for t in Target::all() {
        round_trip_on(t).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mysqldump_file_restores_via_app_runner() {
    require_db!();
    let path_env = std::env::var_os("PATH");
    if find_tool(Tool::MysqlDump, None, path_env.as_deref()).is_none() {
        eprintln!("skipped: mysqldump not found on PATH");
        return;
    }
    let t = Target::MySql8;
    let scope = Scope::new(t);
    create_source(t).await;
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let conn = open_conn(&state, t, false).await;
    let path = dir.path().join("mysqldump.sql");
    // TODO(N7): `đơn_hàng` is left out: on Windows mysqldump reads its arguments in the
    // ANSI code page, so a non-ASCII table name arrives mangled ("don_h?ng") and the
    // dump fails. Reported to the controller; the view is left out because mysqldump
    // keeps its `p5_src.` qualifiers.
    let tables: Vec<&str> = TABLES.iter().copied().filter(|n| n.is_ascii()).collect();
    let admitted = state
        .file_picks
        .admit(FileAccess::DumpWrite, path.clone())
        .unwrap();
    let id = tool_dump_start_impl(
        &state,
        ToolDumpRequest {
            connection_id: conn,
            database: SRC.into(),
            tool: Tool::MysqlDump,
            mysql: Some(MysqlDumpOptions {
                tables: tables.iter().map(|s| s.to_string()).collect(),
                structure: true,
                data: true,
                routines: false,
                events: false,
                triggers: false,
                single_transaction: true,
            }),
            pg: None,
            compress: false,
            path: admitted,
        },
    )
    .await
    .unwrap();
    assert_done(t, &finish(&state, id).await);
    scope.fresh_target(DST2).await;
    let (preview, d) = restore(&state, conn, t, &path, DST2).await;
    assert_done(t, &d);
    assert_eq!(preview.path, RestorePath::AppRunner);
    assert!(!preview.stats.as_ref().unwrap().app_dump);
    assert!(!preview.transactional);
    assert_same(t, DST2, "mysqldump", &tables, false).await;
    eprintln!("{t:?} mysqldump: {}", d.job.summary.unwrap_or_default());
    state.sessions.close(conn).await;
    drop(state);
    drop(scope);
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_plain_copy_file_is_refused_without_psql() {
    require_db!();
    let t = Target::Pg17;
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let configured = dntdatabase::commands::settings::saved_external_tools_dir(&state.repo);
    let path_env = std::env::var_os("PATH");
    if find_tool(Tool::Psql, configured.as_deref(), path_env.as_deref()).is_some() {
        eprintln!("skipped: psql is installed (the refusal needs it missing)");
        return;
    }
    let scope = Scope::new(t);
    scope.fresh_target(DST).await;
    let conn = open_conn(&state, t, false).await;
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/p5/pg_dump_plain_copy.sql");
    let admitted = state
        .file_picks
        .admit(FileAccess::RestoreRead, fixture)
        .unwrap();
    let e = restore_preview_impl(
        &state,
        RestoreRequest {
            connection_id: conn,
            database: DST.into(),
            path: admitted,
            single_transaction: true,
            clean: false,
            no_owner: true,
        },
    )
    .await
    .unwrap_err();
    match &e {
        DbError::Unsupported { feature, .. } => {
            assert!(feature.contains("needs psql"), "{feature}")
        }
        e => panic!("{e:?}"),
    }
    for db in [DST, FIXTURE_DB] {
        let r = run_on(
            t,
            db,
            "SELECT to_regclass('public.p5_restore_copy') IS NULL",
        )
        .await;
        assert_eq!(
            r[0][0].as_value(),
            Some(&Value::Bool(true)),
            "{db}: nothing was executed"
        );
    }
    assert!(state.sessions.jobs.list(100).unwrap().is_empty(), "no job");
    state.sessions.close(conn).await;
    drop(state);
    drop(scope);
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_connection_blocks_restore() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/p5");
    for t in Target::all() {
        let conn = open_conn(&state, t, true).await;
        let files: Vec<std::path::PathBuf> = match t.kind() {
            DbKind::MySql => vec![fixture.join("mysqldump_sample.sql")],
            DbKind::Postgres => vec![
                fixture.join("pg_dump_plain_copy.sql"),
                dir.path().join("plain.sql"),
            ],
        };
        std::fs::write(
            dir.path().join("plain.sql"),
            "CREATE TABLE public.p5_never (id int);\n",
        )
        .unwrap();
        for f in files {
            let admitted = state
                .file_picks
                .admit(FileAccess::RestoreRead, f.clone())
                .unwrap();
            let e = restore_preview_impl(
                &state,
                RestoreRequest {
                    connection_id: conn,
                    database: FIXTURE_DB.into(),
                    path: admitted,
                    single_transaction: true,
                    clean: false,
                    no_owner: true,
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(e, DbError::Blocked { .. }), "{t:?} {f:?}: {e:?}");
        }
        state.sessions.close(conn).await;
    }
    let r = run(
        Target::Pg17,
        "SELECT to_regclass('public.p5_never') IS NULL",
    )
    .await;
    assert_eq!(r[0][0].as_value(), Some(&Value::Bool(true)));
}
