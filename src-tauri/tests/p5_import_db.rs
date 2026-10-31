//! Data import (P5b) against the Docker test matrix (`DNT_TEST_DB=1`), through the
//! command `*_impl` functions and the job manager (no Tauri runtime): append with a
//! mapping, upsert, replace all (delete / truncate), the dry run leaving the table intact,
//! the PostgreSQL sequence untouched by a dry run, JSON / JSONL / XLSX sources, a 10 MiB
//! BLOB exported to CSV and imported back byte-exact, and skip mode.
//!
//! Every test works in its own `p5_import_<tag>` database (MySQL family) / schema of the
//! fixture database (PostgreSQL), dropped first (if left over) and dropped again by a
//! guard when the test ends, also on failure. Run with
//! `$env:DNT_TEST_DB = "1"; cargo test -p dntdatabase --test p5_import_db -- --test-threads=1`.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{CellMode, Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::{DbKind, SqlKind};
use dnt_core::sql::build::{insert_sql_with, ColumnTarget, InsertOptions, TableRef};
use dnt_core::sql::{classify, split};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dnt_transfer::convert::{BinaryEncoding, ConvertOptions, EmptyAs, TargetColumn};
use dnt_transfer::mapping::{auto_map, ErrorPolicy, ImportMode, Mapping};
use dnt_transfer::read::{SourceFormat, SourceOptions};
use dntdatabase::commands::connections::{
    close_connection_impl, create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::export::export_table_impl;
use dntdatabase::commands::import_data::{
    import_dry_run_impl, import_prepare_impl, import_start_impl, import_target_impl,
};
use dntdatabase::export_format::{ExportFormat, ExportOptions};
use dntdatabase::file_picks::FileAccess;
use dntdatabase::import_data::{DryRunRequest, ImportConfirm, ImportPreview, ImportRequest};
use dntdatabase::jobs::{JobDetail, JobLogLevel, JobState};
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const JOB_TIMEOUT: Duration = Duration::from_secs(120);

// ------------------------------------------------------------------ direct sessions

/// Rows of every result, complete values.
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

async fn connect(t: Target) -> Result<Box<dyn Session>, DbError> {
    let driver: Box<dyn Driver> = match t.sql_kind() {
        SqlKind::MySql => Box::new(MySqlDriver),
        SqlKind::Postgres => Box::new(PgDriver),
    };
    driver.connect(&t.resolved(Some(FIXTURE_DB))).await
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

/// Runs `sql` (a script) on a fresh admin session; the rows of every result.
async fn run(t: Target, sql: &str) -> Vec<Vec<FullValue>> {
    let s = connect(t).await.unwrap();
    let mut sink = Collect::default();
    let sql_s = s.sql().unwrap();
    for stmt in split(sql, sql_s.dialect()) {
        let kind = classify(&stmt.text, sql_s.dialect());
        let shown: String = stmt.text.chars().take(200).collect();
        sql_s
            .execute_one(&stmt.text, kind, &full_opts(), &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{t:?} {shown}: {e:?}"));
    }
    s.close().await;
    sink.0
}

/// A cell as plain text, so results compare the same whatever type a driver decodes.
fn text(v: &FullValue) -> String {
    if let Some(b) = v.as_bytes() {
        return format!("<{} bytes>", b.len());
    }
    match v.as_value().unwrap() {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s)
        | Value::Other { text: s, .. } => s.clone(),
        other => format!("{other:?}"),
    }
}

async fn rows_text(t: Target, sql: &str) -> Vec<Vec<String>> {
    run(t, sql)
        .await
        .iter()
        .map(|r| r.iter().map(text).collect())
        .collect()
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

// ------------------------------------------------------------------ scope guard

/// The test's own database (MySQL) / schema (PostgreSQL): created fresh, dropped when the
/// guard goes (also when the test panics).
struct Scope {
    t: Target,
    name: String,
}

fn drop_sql(t: Target, name: &str) -> String {
    match t.sql_kind() {
        SqlKind::MySql => format!("DROP DATABASE IF EXISTS {name}"),
        SqlKind::Postgres => format!("DROP SCHEMA IF EXISTS {name} CASCADE"),
    }
}

impl Scope {
    async fn create(t: Target, tag: &str) -> Scope {
        let name = format!("p5_import_{tag}");
        assert!(name.starts_with("p5_"));
        let create = match t.sql_kind() {
            SqlKind::MySql => format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"),
            SqlKind::Postgres => format!("CREATE SCHEMA {name}"),
        };
        run(t, &format!("{};\n{create}", drop_sql(t, &name))).await;
        Scope { t, name }
    }

    fn table(&self, name: &str) -> TableRef {
        match self.t.sql_kind() {
            SqlKind::MySql => TableRef {
                kind: SqlKind::MySql,
                database: self.name.clone(),
                schema: None,
                name: name.into(),
            },
            SqlKind::Postgres => TableRef {
                kind: SqlKind::Postgres,
                database: FIXTURE_DB.into(),
                schema: Some(self.name.clone()),
                name: name.into(),
            },
        }
    }

    fn rel(&self, name: &str) -> String {
        self.table(name).relation()
    }

    fn obj(&self, name: &str) -> ObjectRef {
        let t = self.table(name);
        ObjectRef {
            database: t.database,
            schema: t.schema,
            name: t.name,
            kind: ObjectKind::Table,
        }
    }
}

/// Runs `sql` on its own thread and runtime (usable from `Drop`, also while panicking).
fn run_detached(t: Target, sql: String) {
    let _ = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            if let Ok(s) = connect(t).await {
                let mut sink = Collect::default();
                let _ = s
                    .sql()
                    .unwrap()
                    .execute(&sql, ExecOptions::default(), &mut sink)
                    .await;
                s.close().await;
            }
        });
    })
    .join();
}

impl Drop for Scope {
    fn drop(&mut self) {
        run_detached(self.t, drop_sql(self.t, &self.name));
    }
}

/// MySQL family: `max_allowed_packet` raised to 64 MiB while a test sends or reads a
/// 10 MiB value (MySQL 5.7 defaults to 4 MiB, MariaDB to 16 MiB; a 10 MiB value is a
/// 20 MiB hex literal). The server's own value is restored when the guard goes. Only new
/// sessions see the change, so the guard is taken before any session of the test opens.
struct PacketGuard {
    t: Target,
    original: Option<u64>,
}

const BIG_PACKET: u64 = 64 * 1024 * 1024;

impl PacketGuard {
    async fn raise(t: Target) -> PacketGuard {
        if t.kind() != DbKind::MySql {
            return PacketGuard { t, original: None };
        }
        let now: u64 = rows_text(t, "SELECT @@GLOBAL.max_allowed_packet").await[0][0]
            .parse()
            .unwrap();
        if now >= BIG_PACKET {
            return PacketGuard { t, original: None };
        }
        run(t, &format!("SET GLOBAL max_allowed_packet = {BIG_PACKET}")).await;
        PacketGuard {
            t,
            original: Some(now),
        }
    }
}

impl Drop for PacketGuard {
    fn drop(&mut self) {
        if let Some(n) = self.original {
            run_detached(self.t, format!("SET GLOBAL max_allowed_packet = {n}"));
        }
    }
}

// ------------------------------------------------------------------ app state

fn app_state(dir: &Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open_conn(state: &AppState, t: Target) -> Uuid {
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
        read_only: false,
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

/// Writes `content` to `dir/name` and admits it for the import commands.
fn file(state: &AppState, dir: &Path, name: &str, content: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    state
        .file_picks
        .admit(FileAccess::ImportRead, path)
        .unwrap()
}

fn csv_source() -> SourceOptions {
    SourceOptions {
        format: SourceFormat::Csv,
        delimiter: None,
        header: true,
        sheet: None,
    }
}

fn source(format: SourceFormat) -> SourceOptions {
    SourceOptions {
        format,
        ..csv_source()
    }
}

fn convert_opts(binary: BinaryEncoding) -> ConvertOptions {
    ConvertOptions {
        empty: EmptyAs::Null,
        null_text: Some("\\N".into()),
        trim: false,
        binary,
    }
}

/// Every target column, mapped to the source column index of `pairs` or unmapped.
fn mapping(targets: &[TargetColumn], pairs: &[(&str, u32)]) -> Vec<Mapping> {
    targets
        .iter()
        .map(|t| Mapping {
            target: t.name.clone(),
            source: pairs.iter().find(|p| p.0 == t.name).map(|p| p.1),
        })
        .collect()
}

struct Ctx<'a> {
    state: &'a AppState,
    conn: Uuid,
}

impl Ctx<'_> {
    async fn targets(&self, obj: &ObjectRef) -> Vec<TargetColumn> {
        import_target_impl(self.state, self.conn, obj.clone())
            .await
            .unwrap()
            .columns
    }

    fn request(
        &self,
        obj: &ObjectRef,
        path: &str,
        source: SourceOptions,
        mapping: Vec<Mapping>,
        mode: ImportMode,
    ) -> ImportRequest {
        ImportRequest {
            connection_id: self.conn,
            table: obj.clone(),
            path: path.into(),
            source,
            mapping,
            mode,
            errors: ErrorPolicy::Stop,
            convert: convert_opts(BinaryEncoding::Hex),
            batch_rows: 0,
            batch_bytes: 0,
            override_identity: false,
        }
    }

    async fn finish(&self, id: Uuid) -> JobDetail {
        let jobs = &self.state.sessions.jobs;
        assert!(jobs.wait(id, JOB_TIMEOUT).await, "job {id} did not finish");
        jobs.detail(id).unwrap()
    }

    async fn prepare(&self, req: ImportRequest) -> ImportPreview {
        import_prepare_impl(self.state, req)
            .await
            .unwrap_or_else(|e| panic!("prepare: {e:?}"))
    }

    async fn start(&self, preview: &ImportPreview, irreversible_ack: bool) -> JobDetail {
        let id = import_start_impl(
            self.state,
            preview.plan_id,
            ImportConfirm {
                confirmed: true,
                name: None,
                irreversible_ack,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("start: {e:?}"));
        self.finish(id).await
    }

    /// Prepare + start (no irreversible pre-step expected) + wait; the job must succeed.
    async fn import(&self, req: ImportRequest) -> JobDetail {
        let p = self.prepare(req).await;
        let d = self.start(&p, false).await;
        assert_done(&d);
        d
    }

    async fn dry_run(&self, req: ImportRequest, server_check: bool) -> JobDetail {
        let id = import_dry_run_impl(
            self.state,
            DryRunRequest {
                request: req,
                server_check,
                confirmed: false,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("dry run: {e:?}"));
        let d = self.finish(id).await;
        assert_done(&d);
        d
    }
}

fn assert_done(d: &JobDetail) {
    assert_eq!(
        d.job.state,
        JobState::Done,
        "{}: {:?}\nlog: {:#?}",
        d.job.title,
        d.job.error,
        d.log
    );
}

fn summary(d: &JobDetail) -> &str {
    d.job.summary.as_deref().unwrap_or("")
}

/// The app state, an open connection and the scope of one test on one server. Fields
/// drop in order: the connection's sessions go before the scope is dropped.
struct Env {
    state: AppState,
    conn: Uuid,
    dir: tempfile::TempDir,
    scope: Scope,
}

impl Env {
    async fn new(t: Target, tag: &str) -> Env {
        let scope = Scope::create(t, tag).await;
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let conn = open_conn(&state, t).await;
        Env {
            state,
            conn,
            dir,
            scope,
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            state: &self.state,
            conn: self.conn,
        }
    }

    fn file(&self, name: &str, content: &[u8]) -> String {
        file(&self.state, self.dir.path(), name, content)
    }

    async fn close(self) {
        close_connection_impl(&self.state, self.conn).await.unwrap();
    }
}

const PEOPLE: &str = "p5_people";

/// `p5_people`: a key, a NOT NULL text, two nullable columns and a NOT NULL column with a
/// default (left unmapped by the imports).
async fn create_people(t: Target, scope: &Scope) -> String {
    let rel = scope.rel(PEOPLE);
    run(
        t,
        &format!(
            "CREATE TABLE {rel} (id INT PRIMARY KEY, full_name VARCHAR(100) NOT NULL, \
             city VARCHAR(50), score INT, note VARCHAR(20) NOT NULL DEFAULT 'none')"
        ),
    )
    .await;
    rel
}

async fn seed_people(t: Target, rel: &str) {
    run(
        t,
        &format!(
            "INSERT INTO {rel} (id, full_name, city, score, note) VALUES \
             (1, 'Old One', 'Huế', 1, 'seed'), (2, 'Old Two', NULL, 2, 'seed'), \
             (3, 'Old Three', 'Vinh', NULL, 'seed')"
        ),
    )
    .await;
}

async fn people(t: Target, rel: &str) -> Vec<Vec<String>> {
    rows_text(
        t,
        &format!("SELECT id, full_name, city, score, note FROM {rel} ORDER BY id"),
    )
    .await
}

/// Every row by key, complete values (the before/after checksum).
async fn snapshot(t: Target, rel: &str) -> Vec<Vec<FullValue>> {
    run(t, &format!("SELECT * FROM {rel} ORDER BY id")).await
}

/// `id,full_name,city,score` rows for the people table.
const PEOPLE_CSV: &str = "id,full_name,city,score\n2,New Two,Đà Lạt,20\n4,New Four,\\N,40\n";

fn people_map(targets: &[TargetColumn]) -> Vec<Mapping> {
    mapping(
        targets,
        &[("id", 0), ("full_name", 1), ("city", 2), ("score", 3)],
    )
}

// ------------------------------------------------------------------ tests

/// CSV columns reordered and renamed, Unicode text, `\N` as NULL, an empty number as NULL,
/// an unmapped NOT NULL column filled by its default.
#[tokio::test]
async fn csv_append_with_mapping() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "append").await;
        let rel = create_people(t, &env.scope).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        let path = env.file(
            "people.csv",
            "score,Name,ID,town\n10,Nguyễn Văn A,1,Hà Nội\n,Trần Thị B,2,\\N\n\
             7,\"Lê, \"\"C\"\"\",3,Đà Nẵng\n"
                .as_bytes(),
        );
        let targets = ctx.targets(&obj).await;
        let m = mapping(
            &targets,
            &[("id", 2), ("full_name", 1), ("city", 3), ("score", 0)],
        );
        let d = ctx
            .import(ctx.request(&obj, &path, csv_source(), m, ImportMode::Append))
            .await;
        assert!(
            summary(&d).starts_with("Imported 3 rows (0 skipped)"),
            "{t:?}: {}",
            summary(&d)
        );
        assert_eq!(
            people(t, &rel).await,
            strs(&[
                &["1", "Nguyễn Văn A", "Hà Nội", "10", "none"],
                &["2", "Trần Thị B", "NULL", "NULL", "none"],
                &["3", "Lê, \"C\"", "Đà Nẵng", "7", "none"],
            ]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("csv_append_with_mapping {t:?}: ok");
    }
}

/// Upsert on the primary key: an existing key is updated, a new one inserted, other
/// rows unchanged.
#[tokio::test]
async fn csv_upsert_updates_and_inserts() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "upsert").await;
        let rel = create_people(t, &env.scope).await;
        seed_people(t, &rel).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        let path = env.file("people.csv", PEOPLE_CSV.as_bytes());
        let targets = ctx.targets(&obj).await;
        let mode = ImportMode::Upsert {
            key: vec!["id".into()],
        };
        let d = ctx
            .import(ctx.request(&obj, &path, csv_source(), people_map(&targets), mode))
            .await;
        assert!(
            summary(&d).starts_with("Imported 2 rows"),
            "{t:?}: {}",
            summary(&d)
        );
        assert_eq!(
            people(t, &rel).await,
            strs(&[
                &["1", "Old One", "Huế", "1", "seed"],
                &["2", "New Two", "Đà Lạt", "20", "seed"],
                &["3", "Old Three", "Vinh", "NULL", "seed"],
                &["4", "New Four", "NULL", "40", "none"],
            ]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("csv_upsert_updates_and_inserts {t:?}: ok");
    }
}

/// Replace all with `DELETE`: inside the transaction (reversible) on every server here
/// (InnoDB / PostgreSQL).
#[tokio::test]
async fn replace_all_delete_mode() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "replace").await;
        let rel = create_people(t, &env.scope).await;
        seed_people(t, &rel).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        let path = env.file("people.csv", PEOPLE_CSV.as_bytes());
        let targets = ctx.targets(&obj).await;
        let req = ctx.request(
            &obj,
            &path,
            csv_source(),
            people_map(&targets),
            ImportMode::Replace { truncate: false },
        );
        let p = ctx.prepare(req).await;
        assert!(p.transactional, "{t:?}");
        let pre = p.pre_step.clone().expect("a pre-step");
        assert!(pre.sql.starts_with("DELETE FROM"), "{t:?}: {}", pre.sql);
        assert!(!pre.irreversible, "{t:?}");
        let d = ctx.start(&p, false).await;
        assert_done(&d);
        assert_eq!(
            people(t, &rel).await,
            strs(&[
                &["2", "New Two", "Đà Lạt", "20", "none"],
                &["4", "New Four", "NULL", "40", "none"],
            ]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("replace_all_delete_mode {t:?}: ok");
    }
}

/// MySQL `TRUNCATE` commits at once: the preview flags an irreversible pre-step, a start
/// without the acknowledgement is refused with nothing sent, with it the import runs.
/// PostgreSQL truncates inside the transaction (not irreversible).
#[tokio::test]
async fn replace_all_truncate_mysql_is_irreversible_step() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "truncate").await;
        let rel = create_people(t, &env.scope).await;
        seed_people(t, &rel).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        let path = env.file("people.csv", PEOPLE_CSV.as_bytes());
        let targets = ctx.targets(&obj).await;
        let req = ctx.request(
            &obj,
            &path,
            csv_source(),
            people_map(&targets),
            ImportMode::Replace { truncate: true },
        );
        let mysql = t.kind() == DbKind::MySql;
        let p = ctx.prepare(req.clone()).await;
        let pre = p.pre_step.clone().expect("a pre-step");
        assert!(pre.sql.starts_with("TRUNCATE"), "{t:?}: {}", pre.sql);
        assert_eq!(pre.irreversible, mysql, "{t:?}");
        let p = if mysql {
            assert!(pre.warning.is_some(), "{t:?}");
            let before = snapshot(t, &rel).await;
            let err = import_start_impl(
                &env.state,
                p.plan_id,
                ImportConfirm {
                    confirmed: true,
                    name: None,
                    irreversible_ack: false,
                },
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, DbError::ConfirmationRequired { .. }),
                "{t:?}: {err:?}"
            );
            assert_eq!(snapshot(t, &rel).await, before, "{t:?}: nothing sent");
            ctx.prepare(req).await
        } else {
            p
        };
        let d = ctx.start(&p, mysql).await;
        assert_done(&d);
        if mysql {
            assert!(
                d.log
                    .iter()
                    .any(|l| l.message.contains("irreversible pre-step")),
                "{t:?}: {:#?}",
                d.log
            );
        }
        assert_eq!(
            people(t, &rel).await,
            strs(&[
                &["2", "New Two", "Đà Lạt", "20", "none"],
                &["4", "New Four", "NULL", "40", "none"],
            ]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("replace_all_truncate_mysql_is_irreversible_step {t:?}: ok");
    }
}

/// A dry run with the server check (append, upsert, replace delete, MySQL also replace
/// truncate) leaves every row of the target as it was; bad rows are counted.
#[tokio::test]
async fn dry_run_keeps_table_intact() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "dryrun").await;
        let rel = create_people(t, &env.scope).await;
        seed_people(t, &rel).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        // Row 2 is a key already in the table, row 3 a bad number, row 4 new.
        let path = env.file(
            "people.csv",
            "id,full_name,city,score\n2,New Two,Đà Lạt,20\n5,Bad,X,notanumber\n\
             9,New Nine,\\N,90\n"
                .as_bytes(),
        );
        let targets = ctx.targets(&obj).await;
        let before = snapshot(t, &rel).await;
        assert_eq!(before.len(), 3);
        let mut modes = vec![
            ImportMode::Append,
            ImportMode::Upsert {
                key: vec!["id".into()],
            },
            ImportMode::Replace { truncate: false },
        ];
        if t.kind() == DbKind::MySql {
            modes.push(ImportMode::Replace { truncate: true });
        }
        for mode in modes {
            for server_check in [true, false] {
                let req = ctx.request(
                    &obj,
                    &path,
                    csv_source(),
                    people_map(&targets),
                    mode.clone(),
                );
                let d = ctx.dry_run(req, server_check).await;
                assert!(
                    summary(&d).starts_with("2 rows valid, 1 row rejected"),
                    "{t:?} {mode:?} check={server_check}: {}\n{:#?}",
                    summary(&d),
                    d.log
                );
                assert_eq!(
                    snapshot(t, &rel).await,
                    before,
                    "{t:?} {mode:?} check={server_check}: the table changed"
                );
            }
        }
        env.close().await;
        eprintln!("dry_run_keeps_table_intact {t:?}: ok");
    }
}

/// PostgreSQL: a server-check dry run into a table with a `serial` key does not advance
/// the sequence (the real import afterwards does, so the check can see a change).
#[tokio::test]
async fn pg_sequence_unchanged() {
    require_db!();
    for t in Target::pg() {
        let env = Env::new(t, "seq").await;
        let rel = env.scope.rel("p5_serial");
        let seq = format!("{}.p5_serial_id_seq", env.scope.name);
        run(
            t,
            &format!(
                "CREATE TABLE {rel} (id serial PRIMARY KEY, v text NOT NULL);\n\
                 INSERT INTO {rel} (v) VALUES ('a'), ('b')"
            ),
        )
        .await;
        let seq_state = |seq: String| async move {
            rows_text(t, &format!("SELECT last_value, is_called FROM {seq}")).await
        };
        let before = seq_state(seq.clone()).await;
        assert_eq!(before, strs(&[&["2", "true"]]), "{t:?}");
        let obj = env.scope.obj("p5_serial");
        let ctx = env.ctx();
        let path = env.file("v.csv", b"v\nx\ny\nz\n");
        let targets = ctx.targets(&obj).await;
        let m = mapping(&targets, &[("v", 0)]);
        let req = ctx.request(&obj, &path, csv_source(), m, ImportMode::Append);
        let d = ctx.dry_run(req.clone(), true).await;
        assert!(
            summary(&d).starts_with("3 rows valid"),
            "{t:?}: {}",
            summary(&d)
        );
        assert_eq!(
            seq_state(seq.clone()).await,
            before,
            "{t:?}: sequence moved"
        );
        assert_eq!(
            rows_text(t, &format!("SELECT count(*) FROM {rel}")).await,
            strs(&[&["2"]])
        );
        ctx.import(req).await;
        assert_eq!(
            seq_state(seq.clone()).await,
            strs(&[&["5", "true"]]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("pg_sequence_unchanged {t:?}: ok");
    }
}

fn write_xlsx(path: &Path) {
    use rust_xlsxwriter::Workbook;
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet().set_name("People").unwrap();
    ws.write_string(0, 0, "full_name").unwrap();
    ws.write_string(0, 1, "id").unwrap();
    ws.write_string(0, 2, "score").unwrap();
    ws.write_string(1, 0, "Xlsx Một").unwrap();
    ws.write_number(1, 1, 31).unwrap();
    ws.write_number(1, 2, 3).unwrap();
    ws.write_string(2, 0, "Xlsx Hai").unwrap();
    ws.write_number(2, 1, 32).unwrap();
    // C3 empty: NULL.
    wb.save(path).unwrap();
}

/// JSON (array of objects), JSONL and XLSX sources, mapped automatically by name.
#[tokio::test]
async fn json_and_xlsx_import() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "json").await;
        let rel = create_people(t, &env.scope).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        let targets = ctx.targets(&obj).await;
        let json_path = env.file(
            "people.json",
            r#"[{"id": 11, "full_name": "Json Một", "score": 5, "city": null},
                {"full_name": "Json Hai", "id": 12, "city": "Sài Gòn"}]"#
                .as_bytes(),
        );
        let jsonl_path = env.file(
            "people.jsonl",
            "{\"id\": 21, \"full_name\": \"Line A\", \"score\": -1}\n\n\
             {\"id\": 22, \"full_name\": \"Line B\", \"city\": \"Cần Thơ\"}\n"
                .as_bytes(),
        );
        let xlsx = env.dir.path().join("people.xlsx");
        write_xlsx(&xlsx);
        let xlsx_path = env
            .state
            .file_picks
            .admit(FileAccess::ImportRead, xlsx.clone())
            .unwrap();
        for (path, format, columns) in [
            (
                &json_path,
                SourceFormat::Json,
                vec!["id", "full_name", "score", "city"],
            ),
            (
                &jsonl_path,
                SourceFormat::Jsonl,
                vec!["id", "full_name", "score", "city"],
            ),
            (
                &xlsx_path,
                SourceFormat::Xlsx,
                vec!["full_name", "id", "score"],
            ),
        ] {
            let source = source(format);
            let cols: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
            let m = auto_map(&cols, &targets);
            let req = ctx.request(&obj, path, source, m, ImportMode::Append);
            ctx.import(req).await;
        }
        assert_eq!(
            people(t, &rel).await,
            strs(&[
                &["11", "Json Một", "NULL", "5", "none"],
                &["12", "Json Hai", "Sài Gòn", "NULL", "none"],
                &["21", "Line A", "NULL", "-1", "none"],
                &["22", "Line B", "Cần Thơ", "NULL", "none"],
                &["31", "Xlsx Một", "NULL", "3", "none"],
                &["32", "Xlsx Hai", "NULL", "NULL", "none"],
            ]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("json_and_xlsx_import {t:?}: ok");
    }
}

/// Deterministic pseudo-random bytes (xorshift64*).
fn random_bytes(n: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        out.extend_from_slice(&seed.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(n);
    out
}

/// A 10 MiB binary value: inserted with `insert_sql_with`, exported to CSV (hex) with
/// `export_table_impl`, imported into a second table with `BinaryEncoding::Hex`, equal
/// byte for byte.
#[tokio::test]
async fn blob_10mib_round_trip() {
    require_db!();
    for t in Target::all() {
        let _packet = PacketGuard::raise(t).await;
        let env = Env::new(t, "blob").await;
        let blob_type = match t.sql_kind() {
            SqlKind::MySql => "LONGBLOB",
            SqlKind::Postgres => "bytea",
        };
        let (src, dst) = (env.scope.rel("p5_blob"), env.scope.rel("p5_blob_copy"));
        run(
            t,
            &format!(
                "CREATE TABLE {src} (id INT PRIMARY KEY, data {blob_type});\n\
                 CREATE TABLE {dst} (id INT PRIMARY KEY, data {blob_type})"
            ),
        )
        .await;
        let bytes = random_bytes(10 * 1024 * 1024, 0x5eed_0000 + t.host_port() as u64);
        let cols = [
            ColumnTarget {
                name: "id".into(),
                db_type: "int".into(),
                logical: LogicalType::Integer,
                collation: None,
            },
            ColumnTarget {
                name: "data".into(),
                db_type: blob_type.to_lowercase(),
                logical: LogicalType::Binary,
                collation: None,
            },
        ];
        let rows = vec![
            vec![
                FullValue::try_from_value(Value::Int(1)).unwrap(),
                FullValue::bytes(bytes.clone()),
            ],
            vec![
                FullValue::try_from_value(Value::Int(2)).unwrap(),
                FullValue::bytes(vec![0u8, 1, 0xff]),
            ],
            vec![
                FullValue::try_from_value(Value::Int(3)).unwrap(),
                FullValue::try_from_value(Value::Null).unwrap(),
            ],
        ];
        let s = connect(t).await.unwrap();
        let insert = insert_sql_with(
            &env.scope.table("p5_blob"),
            &cols,
            &rows,
            &InsertOptions::default(),
            s.sql().unwrap().dialect(),
        )
        .unwrap();
        s.close().await;
        run(t, &insert).await;
        drop(insert);

        let csv = env.dir.path().join("blob.csv");
        let exported = export_table_impl(
            &env.state,
            env.conn,
            env.scope.obj("p5_blob"),
            vec![],
            None,
            ExportOptions {
                format: ExportFormat::Csv,
                delimiter: ",".into(),
                header: true,
                quote_all: false,
                bom: false,
                table_name: "t".into(),
                formula_guard: false,
            },
            csv.to_string_lossy().into(),
            "export:blob".into(),
            Box::new(|_| {}),
        )
        .await
        .unwrap_or_else(|e| panic!("{t:?} export: {e:?}"));
        assert_eq!(exported.rows, 3, "{t:?}");
        assert_eq!(exported.partial_binaries, 0, "{t:?}");

        let path = env
            .state
            .file_picks
            .admit(FileAccess::ImportRead, csv)
            .unwrap();
        let ctx = env.ctx();
        let obj = env.scope.obj("p5_blob_copy");
        let targets = ctx.targets(&obj).await;
        let mut req = ctx.request(
            &obj,
            &path,
            csv_source(),
            mapping(&targets, &[("id", 0), ("data", 1)]),
            ImportMode::Append,
        );
        req.convert = ConvertOptions {
            empty: EmptyAs::Null,
            null_text: None,
            trim: false,
            binary: BinaryEncoding::Hex,
        };
        let d = ctx.import(req).await;
        assert!(
            summary(&d).starts_with("Imported 3 rows"),
            "{t:?}: {}",
            summary(&d)
        );

        let read = |rel: String| async move {
            run(t, &format!("SELECT id, data FROM {rel} ORDER BY id")).await
        };
        let copy = read(dst.clone()).await;
        assert_eq!(copy.len(), 3, "{t:?}");
        assert_eq!(
            copy[0][1].as_bytes().map(<[u8]>::len),
            Some(bytes.len()),
            "{t:?}"
        );
        assert!(
            copy[0][1].as_bytes() == Some(&bytes[..]),
            "{t:?}: bytes differ"
        );
        assert_eq!(copy[1][1].as_bytes(), Some(&[0u8, 1, 0xff][..]), "{t:?}");
        assert!(copy[2][1].is_null(), "{t:?}");
        assert!(read(src).await == copy, "{t:?}: the tables differ");
        env.close().await;
        eprintln!("blob_10mib_round_trip {t:?}: ok");
    }
}

/// Skip mode: rows with a key already in the table are refused by the server, skipped
/// and listed in the job log; the other rows are imported.
#[tokio::test]
async fn skip_mode_logs_bad_rows() {
    require_db!();
    for t in Target::all() {
        let env = Env::new(t, "skip").await;
        let rel = create_people(t, &env.scope).await;
        seed_people(t, &rel).await;
        let obj = env.scope.obj(PEOPLE);
        let ctx = env.ctx();
        let path = env.file(
            "people.csv",
            "id,full_name,city,score\n2,Dup Two,x,1\n7,Seven,y,7\n1,Dup One,z,1\n8,Eight,\\N,8\n"
                .as_bytes(),
        );
        let targets = ctx.targets(&obj).await;
        let mut req = ctx.request(
            &obj,
            &path,
            csv_source(),
            people_map(&targets),
            ImportMode::Append,
        );
        req.errors = ErrorPolicy::Skip { max_errors: 10 };
        let d = ctx.import(req).await;
        assert!(
            summary(&d).starts_with("Imported 2 rows (2 skipped)"),
            "{t:?}: {}\n{:#?}",
            summary(&d),
            d.log
        );
        let skipped: Vec<&str> = d
            .log
            .iter()
            .filter(|l| l.level == JobLogLevel::Warn && l.message.starts_with("Row "))
            .map(|l| l.message.as_str())
            .collect();
        assert_eq!(skipped.len(), 2, "{t:?}: {:#?}", d.log);
        // CSV records are numbered by the file line they start on (the header is line 1).
        assert!(skipped[0].starts_with("Row 2:"), "{t:?}: {skipped:?}");
        assert!(skipped[1].starts_with("Row 4:"), "{t:?}: {skipped:?}");
        // The value of the refused row never reaches the log.
        assert!(
            !skipped
                .iter()
                .any(|m| m.contains("Dup Two") || m.contains("Dup One")),
            "{t:?}: {skipped:?}"
        );
        assert_eq!(
            people(t, &rel).await,
            strs(&[
                &["1", "Old One", "Huế", "1", "seed"],
                &["2", "Old Two", "NULL", "2", "seed"],
                &["3", "Old Three", "Vinh", "NULL", "seed"],
                &["7", "Seven", "y", "7", "none"],
                &["8", "Eight", "NULL", "8", "none"],
            ]),
            "{t:?}"
        );
        env.close().await;
        eprintln!("skip_mode_logs_bad_rows {t:?}: ok");
    }
}
