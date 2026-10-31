//! Import plans and jobs against an in-process fake driver: every statement the jobs
//! send is recorded, failures are programmed per statement text, and a rule can drop
//! the connection (e.g. on `COMMIT`).
use super::*;
use crate::jobs::{JobDetail, JobState};
use crate::session_manager::SessionManager;
use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    ColumnDetail, DatabaseInfo, IndexDetail, IndexPart, ObjectKind, ObjectRef, ObjectSummary,
    SchemaInfo, Scope, TableColumns, TableDetail, TableOptions,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::sql::{classify, Dialect, MySqlMode, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, LogicalType};
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryFilter, HistoryOrigin, HistoryStatus};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_transfer::convert::ConvertOptions;
use dnt_transfer::mapping::{ErrorPolicy, ImportMode, Mapping};
use dnt_transfer::read::{SourceFormat, SourceOptions};
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------------------------

/// A statement containing `contains` fails with `error` (`times` left: `None` = always);
/// `kill` drops the connection with it.
pub(crate) struct Rule {
    contains: String,
    error: DbError,
    times: Option<usize>,
    kill: bool,
}

/// Rows a statement containing the text returns: (text, columns, rows).
pub(crate) type Programmed = (String, Vec<ColumnMeta>, Vec<Vec<dnt_core::value::Value>>);

pub(crate) struct Fake {
    pub(crate) server: ServerInfo,
    pub(crate) detail: Mutex<TableDetail>,
    pub(crate) seen: Mutex<Vec<String>>,
    pub(crate) rules: Mutex<Vec<Rule>>,
    /// A statement containing the text streams these columns and rows (see `rows_for`).
    pub(crate) rows: Mutex<Vec<Programmed>>,
    /// The connection was dropped: nothing more reaches the server.
    pub(crate) dead: AtomicBool,
    /// A statement containing the text takes this long (after it was recorded).
    pub(crate) delay: Mutex<Option<(String, Duration)>>,
}

impl Fake {
    /// Everything sent so far (handy when a test fails).
    #[allow(dead_code)]
    pub(crate) fn sent(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub(crate) fn mark(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub(crate) fn since(&self, mark: usize) -> Vec<String> {
        self.seen.lock().unwrap()[mark..].to_vec()
    }

    pub(crate) fn fail(&self, contains: &str, error: DbError) {
        self.rules.lock().unwrap().push(Rule {
            contains: contains.into(),
            error,
            times: None,
            kill: false,
        });
    }

    /// A statement containing `contains` returns `rows` with columns `cols`.
    pub(crate) fn rows_for(
        &self,
        contains: &str,
        cols: Vec<ColumnMeta>,
        rows: Vec<Vec<dnt_core::value::Value>>,
    ) {
        self.rows
            .lock()
            .unwrap()
            .push((contains.into(), cols, rows));
    }

    pub(crate) fn kill_on(&self, contains: &str, error: DbError) {
        self.rules.lock().unwrap().push(Rule {
            contains: contains.into(),
            error,
            times: Some(1),
            kill: true,
        });
    }
}

pub(crate) struct FakeDriver(pub(crate) Arc<Fake>);

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DbKind {
        self.0.server.kind
    }
    async fn connect(
        &self,
        _cfg: &dnt_core::config::ResolvedConfig,
    ) -> Result<Box<dyn Session>, DbError> {
        Ok(Box::new(FakeSession {
            fake: Arc::clone(&self.0),
            tx: Mutex::new(TxState::Idle),
        }))
    }
}

pub(crate) struct FakeSession {
    fake: Arc<Fake>,
    tx: Mutex<TxState>,
}

struct NoCancel;

#[async_trait]
impl Canceller for NoCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        Ok(())
    }
}

fn logical_of(db_type: &str) -> LogicalType {
    let t = db_type.to_ascii_lowercase();
    let starts = |p: &[&str]| p.iter().any(|x| t.starts_with(x));
    if starts(&["tinyint(1)", "boolean", "bool"]) {
        LogicalType::Bool
    } else if starts(&["int", "bigint", "smallint", "tinyint", "mediumint"]) {
        LogicalType::Integer
    } else if starts(&["decimal", "numeric"]) {
        LogicalType::Decimal
    } else if starts(&["enum"]) {
        LogicalType::Enum
    } else if starts(&["datetime", "timestamp"]) {
        LogicalType::DateTime
    } else if starts(&["date"]) {
        LogicalType::Date
    } else if starts(&["uuid"]) {
        LogicalType::Uuid
    } else if starts(&["json"]) {
        LogicalType::Json
    } else {
        LogicalType::Text
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.fake.server
    }
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        if opts.stop.is_stopped() {
            return Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false,
            });
        }
        if self.fake.dead.load(Ordering::SeqCst) {
            return Err(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message: "gone".into(),
            });
        }
        self.fake.seen.lock().unwrap().push(stmt.to_string());
        let hit = {
            let mut rules = self.fake.rules.lock().unwrap();
            let mut hit = None;
            for r in rules.iter_mut() {
                if stmt.contains(&r.contains) && r.times != Some(0) {
                    if let Some(t) = r.times.as_mut() {
                        *t -= 1;
                    }
                    hit = Some((r.error.clone(), r.kill));
                    break;
                }
            }
            hit
        };
        let delay = self.fake.delay.lock().unwrap().clone();
        if let Some((_, d)) = delay.filter(|(text, _)| stmt.contains(text.as_str())) {
            tokio::time::sleep(d).await;
        }
        if let Some((e, kill)) = hit {
            if kill {
                self.fake.dead.store(true, Ordering::SeqCst);
                *self.tx.lock().unwrap() = TxState::Idle;
            }
            return Err(e);
        }
        let upper = stmt.to_ascii_uppercase();
        if upper.starts_with("BEGIN") || upper.starts_with("START TRANSACTION") {
            *self.tx.lock().unwrap() = TxState::Active;
        } else if upper == "COMMIT" || upper == "ROLLBACK" {
            *self.tx.lock().unwrap() = TxState::Idle;
        }
        let summary = ResultSummary {
            affected: None,
            row_count: 0,
            truncated: false,
            elapsed_ms: 0,
        };
        let programmed = self
            .fake
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|(text, _, _)| stmt.contains(text.as_str()))
            .map(|(_, cols, rows)| (cols.clone(), rows.clone()));
        if let Some((cols, rows)) = programmed {
            sink.result_start(&cols);
            sink.rows(rows);
        } else if stmt.ends_with("WHERE 1 = 0") {
            let cols: Vec<ColumnMeta> = self
                .fake
                .detail
                .lock()
                .unwrap()
                .columns
                .iter()
                .map(|c| ColumnMeta {
                    name: c.name.clone(),
                    db_type: c.db_type.clone(),
                    logical: logical_of(&c.db_type),
                    nullable: Some(c.nullable),
                    origin_table: None,
                    origin_column: None,
                })
                .collect();
            sink.result_start(&cols);
        } else {
            sink.result_start(&[]);
        }
        sink.result_end(&summary);
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: false,
        })
    }
    fn tx_state(&self) -> TxState {
        *self.tx.lock().unwrap()
    }
    fn dialect(&self) -> Dialect {
        match self.fake.server.kind {
            DbKind::MySql => Dialect::MySql(MySqlMode::default()),
            DbKind::Postgres => Dialect::Postgres,
        }
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoCancel)
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }
    async fn close(self: Box<Self>) {}
}

#[async_trait]
impl Introspector for FakeSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![])
    }
    async fn list_objects(
        &self,
        _scope: &Scope,
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Ok(vec![])
    }
    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }
    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Ok(self.fake.detail.lock().unwrap().clone())
    }
    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Ok(String::new())
    }
}

// ---------------------------------------------------------------------------------------
// Fixture: table `items (id int PK, name varchar(20) NOT NULL UNIQUE, note text NULL)`
// ---------------------------------------------------------------------------------------

pub(crate) fn column(name: &str, ordinal: u32, db_type: &str, nullable: bool) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        ordinal,
        db_type: db_type.into(),
        nullable,
        default: None,
        auto_increment: false,
        identity: None,
        generated: None,
        invisible: false,
        collation: None,
        comment: None,
        charset: None,
        on_update: None,
        identity_kind: None,
        unsigned: false,
        enum_values: vec![],
    }
}

pub(crate) fn index(name: &str, col: &str, primary: bool) -> IndexDetail {
    IndexDetail {
        name: name.into(),
        unique: true,
        primary,
        columns: vec![col.into()],
        parts: vec![IndexPart {
            column: Some(col.into()),
            ..IndexPart::default()
        }],
        ..IndexDetail::default()
    }
}

pub(crate) fn items_detail(kind: DbKind, engine: Option<&str>) -> TableDetail {
    let pg = kind == DbKind::Postgres;
    let mut id = column("id", 1, if pg { "integer" } else { "int" }, false);
    if pg {
        id.default = Some("nextval('items_id_seq'::regclass)".into());
    } else {
        id.auto_increment = true;
    }
    let pk = index(if pg { "items_pkey" } else { "PRIMARY" }, "id", true);
    TableDetail {
        object: ObjectRef {
            database: "shop".into(),
            schema: pg.then(|| "public".into()),
            name: "items".into(),
            kind: ObjectKind::Table,
        },
        columns: vec![
            id,
            column(
                "name",
                2,
                if pg {
                    "character varying(20)"
                } else {
                    "varchar(20)"
                },
                false,
            ),
            column("note", 3, "text", true),
        ],
        primary_key: Some(pk.clone()),
        indexes: vec![pk, index("items_name_key", "name", false)],
        foreign_keys: vec![],
        checks: vec![],
        triggers: vec![],
        options: TableOptions {
            engine: engine.map(str::to_string),
            ..TableOptions::default()
        },
        row_estimate: None,
        size_bytes: None,
        comment: None,
    }
}

pub(crate) struct Fx {
    pub(crate) sm: Arc<SessionManager>,
    pub(crate) fake: Arc<Fake>,
    pub(crate) repo: Arc<ConnectionRepo>,
    pub(crate) conn: Uuid,
    pub(crate) csv: PathBuf,
    pub(crate) _dir: tempfile::TempDir,
}

const CSV: &str = "id,name,note\n1,a,x\n2,b,\n3,c,z\n";

async fn fixture(kind: DbKind, engine: Option<&str>, env: Option<EnvTag>, read_only: bool) -> Fx {
    fixture_with(kind, engine, env, read_only, None).await
}

pub(crate) async fn fixture_with(
    kind: DbKind,
    engine: Option<&str>,
    env: Option<EnvTag>,
    read_only: bool,
    plan_ttl: Option<Duration>,
) -> Fx {
    fixture_detail(kind, items_detail(kind, engine), env, read_only, plan_ttl).await
}

/// An open connection to the fake server whose table is `detail`.
pub(crate) async fn fixture_detail(
    kind: DbKind,
    detail: TableDetail,
    env: Option<EnvTag>,
    read_only: bool,
    plan_ttl: Option<Duration>,
) -> Fx {
    let server = match kind {
        DbKind::MySql => ServerInfo {
            kind,
            variant: ServerVariant::MySql8,
            version: (8, 0, 36),
            version_string: "8.0.36".into(),
        },
        DbKind::Postgres => ServerInfo {
            kind,
            variant: ServerVariant::Postgres,
            version: (17, 0, 0),
            version_string: "17.0".into(),
        },
    };
    let fake = Arc::new(Fake {
        server,
        detail: Mutex::new(detail),
        seen: Mutex::new(Vec::new()),
        rules: Mutex::new(Vec::new()),
        dead: AtomicBool::new(false),
        delay: Mutex::new(None),
        rows: Mutex::new(Vec::new()),
    });
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver(Arc::clone(&fake)));
    let dir = tempfile::tempdir().unwrap();
    let mut sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(kind, driver)]),
    );
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind,
        default_db: None,
        env_tag: env,
        read_only,
        ..sample_config()
    };
    let conn = repo
        .create(
            cfg,
            NewSecrets {
                db_password: None,
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    if let Some(ttl) = plan_ttl {
        sm.plan_ttl = ttl;
    }
    let sm = Arc::new(sm);
    sm.open(conn, None).await.unwrap();
    let csv = dir.path().join("items.csv");
    std::fs::write(&csv, CSV).unwrap();
    Fx {
        sm,
        fake,
        repo,
        conn,
        csv,
        _dir: dir,
    }
}

async fn pg() -> Fx {
    fixture(DbKind::Postgres, None, None, false).await
}

async fn mysql(engine: &str) -> Fx {
    fixture(DbKind::MySql, Some(engine), None, false).await
}

impl Fx {
    fn table(&self) -> ObjectRef {
        self.fake.detail.lock().unwrap().object.clone()
    }

    fn request(&self, mode: ImportMode, errors: ErrorPolicy) -> ImportRequest {
        ImportRequest {
            connection_id: self.conn,
            table: self.table(),
            path: self.csv.to_string_lossy().into_owned(),
            source: SourceOptions {
                format: SourceFormat::Csv,
                delimiter: None,
                header: true,
                sheet: None,
            },
            mapping: ["id", "name", "note"]
                .iter()
                .enumerate()
                .map(|(i, t)| Mapping {
                    target: (*t).into(),
                    source: Some(i as u32),
                })
                .collect(),
            mode,
            errors,
            convert: ConvertOptions::default(),
            batch_rows: 1000,
            batch_bytes: 1024 * 1024,
            override_identity: false,
        }
    }

    fn append(&self) -> ImportRequest {
        self.request(ImportMode::Append, ErrorPolicy::Stop)
    }

    fn write_csv(&self, text: &str) {
        std::fs::write(&self.csv, text).unwrap();
    }

    async fn wait(&self, id: Uuid) -> JobDetail {
        assert!(self.sm.jobs.wait(id, Duration::from_secs(10)).await);
        self.sm.jobs.detail(id).unwrap()
    }

    /// Prepare + start with every confirmation given; the finished job.
    async fn import(&self, req: ImportRequest) -> (ImportPreview, JobDetail) {
        let preview = self.sm.import_prepare(req).await.unwrap();
        let id = self
            .sm
            .import_start(preview.plan_id, ok_confirm(Some("items")))
            .await
            .unwrap();
        (preview, self.wait(id).await)
    }

    async fn dry_run(&self, req: ImportRequest, server_check: bool) -> JobDetail {
        let id = self
            .sm
            .import_dry_run(DryRunRequest {
                request: req,
                server_check,
                confirmed: true,
            })
            .await
            .unwrap();
        self.wait(id).await
    }

    fn history(&self) -> Vec<dnt_store::history::HistoryEntry> {
        self.repo
            .history_list(&HistoryFilter {
                connection_id: Some(self.conn),
                search: None,
                origin: None,
                limit: 100,
            })
            .unwrap()
    }
}

/// Rows programmed with `rows_for` reach the caller's sink.
#[tokio::test]
async fn fake_streams_programmed_rows() {
    use dnt_core::value::Value;
    struct Collect(Vec<Vec<Value>>);
    impl RowSink for Collect {
        fn result_start(&mut self, _: &[ColumnMeta]) {}
        fn rows(&mut self, rows: Vec<Vec<Value>>) {
            self.0.extend(rows);
        }
        fn result_end(&mut self, _: &ResultSummary) {}
    }
    let fx = pg().await;
    let meta = ColumnMeta {
        name: "id".into(),
        db_type: "integer".into(),
        logical: LogicalType::Integer,
        nullable: Some(false),
        origin_table: None,
        origin_column: None,
    };
    fx.fake.rows_for(
        "SELECT DISTINCT",
        vec![meta],
        vec![vec![Value::Int(1)], vec![Value::Int(2)]],
    );
    let session = FakeDriver(Arc::clone(&fx.fake))
        .connect(&dnt_core::test_util::sample_resolved(None))
        .await
        .unwrap();
    let sql = "SELECT DISTINCT \"id\" FROM \"public\".\"t\"";
    let mut sink = Collect(Vec::new());
    session
        .execute_one(
            sql,
            classify(sql, session.dialect()),
            &ExecOptions::default(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.0, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

pub(crate) fn ok_confirm(name: Option<&str>) -> ImportConfirm {
    ImportConfirm {
        confirmed: true,
        name: name.map(str::to_string),
        irreversible_ack: true,
    }
}

/// The control statements and the start of every write, for sequence checks. The
/// metadata reads (the preview's target read, the SchemaCache load) are left out.
pub(crate) fn shape(stmts: &[String]) -> Vec<String> {
    stmts
        .iter()
        .filter(|s| !s.starts_with("SELECT unnest") && !s.ends_with("WHERE 1 = 0"))
        .map(|s| {
            if s.starts_with("INSERT") {
                let rows = s.matches("), (").count() + 1;
                format!("INSERT x{rows}")
            } else {
                s.clone()
            }
        })
        .collect()
}

pub(crate) fn log_text(d: &JobDetail) -> String {
    d.log
        .iter()
        .map(|l| l.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A server error as the drivers report it: a 5-character PostgreSQL SQLSTATE (no
/// code), or a MySQL error number (with the generic SQLSTATE many of them carry).
pub(crate) fn server_error(code: &str) -> DbError {
    let pg = code.len() == 5;
    DbError::Server {
        code: (!pg).then(|| code.into()),
        sqlstate: Some(if pg { code.into() } else { "HY000".into() }),
        message: "rejected".into(),
        position: None,
    }
}

// ---------------------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------------------

/// No statement touches the target table except the temporary table's `LIKE` and the
/// metadata read; every insert goes to the temporary table.
fn assert_target_untouched(sent: &[String], target: &str) {
    for s in sent {
        let upper = s.to_ascii_uppercase();
        assert!(
            !upper.starts_with("DELETE") && !upper.starts_with("TRUNCATE"),
            "{s}"
        );
        if upper.starts_with("INSERT") {
            assert!(s.contains("dnt_import_check_"), "{s}");
            assert!(!s.contains(target), "{s}");
        }
        if s.contains(target) {
            assert!(
                upper.starts_with("CREATE TEMP") || s.ends_with("WHERE 1 = 0"),
                "{s}"
            );
        }
    }
}

#[tokio::test]
async fn dry_run_never_writes_target() {
    for kind in [DbKind::Postgres, DbKind::MySql] {
        let fx = fixture(kind, Some("InnoDB"), None, false).await;
        let target = match kind {
            DbKind::Postgres => "\"public\".\"items\"",
            DbKind::MySql => "`shop`.`items`",
        };
        for mode in [
            ImportMode::Append,
            ImportMode::Replace { truncate: true },
            ImportMode::Replace { truncate: false },
            ImportMode::Upsert {
                key: vec!["id".into()],
            },
        ] {
            for server_check in [false, true] {
                let mark = fx.fake.mark();
                let req = fx.request(mode.clone(), ErrorPolicy::Stop);
                let d = fx.dry_run(req, server_check).await;
                assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
                let sent = fx.fake.since(mark);
                assert_target_untouched(&sent, target);
                assert!(sent.iter().any(|s| s.ends_with("WHERE 1 = 0")));
                let inserts = sent.iter().filter(|s| s.starts_with("INSERT")).count();
                if server_check {
                    assert_eq!(inserts, 1, "{sent:?}");
                    let rollback = sent.iter().position(|s| s == "ROLLBACK").unwrap();
                    let last_insert = sent.iter().rposition(|s| s.starts_with("INSERT"));
                    assert!(last_insert.unwrap() < rollback);
                    if kind == DbKind::MySql {
                        let last = sent.last().unwrap();
                        assert!(last.starts_with("DROP TEMPORARY TABLE"), "{last}");
                    } else {
                        assert_eq!(sent.last().unwrap(), "ROLLBACK");
                    }
                } else {
                    assert_eq!(inserts, 0);
                    assert!(!sent.iter().any(|s| s.starts_with("CREATE")));
                }
                assert!(
                    d.job
                        .summary
                        .as_deref()
                        .unwrap()
                        .starts_with("3 rows valid, 0 rows rejected"),
                    "{:?}",
                    d.job.summary
                );
            }
        }
        // No History for a dry run.
        assert!(fx.history().is_empty());
    }
}

#[tokio::test]
async fn dry_run_collects_conversion_and_server_errors() {
    let fx = pg().await;
    fx.write_csv("id,name,note\n1,a,x\nnope,b,\n3,bad,z\n4,d,\n");
    fx.fake.fail("'bad'", server_error("23514"));
    let d = fx.dry_run(fx.append(), true).await;
    assert_eq!(d.job.state, JobState::Done);
    assert!(
        d.job
            .summary
            .as_deref()
            .unwrap()
            .starts_with("2 rows valid, 2 rows rejected"),
        "{:?}",
        d.job.summary
    );
    let log = log_text(&d);
    assert!(log.contains("Row 3, column id:"), "{log}");
    assert!(log.contains("Row 4"), "{log}");
}

#[tokio::test]
async fn pg_temp_table_drops_nextval_defaults() {
    let fx = pg().await;
    let mark = fx.fake.mark();
    let d = fx.dry_run(fx.append(), true).await;
    assert_eq!(d.job.state, JobState::Done);
    let sent = fx.fake.since(mark);
    let create = sent
        .iter()
        .position(|s| s.starts_with("CREATE TEMP TABLE"))
        .unwrap();
    let tmp = sent[create].split_whitespace().nth(3).unwrap().to_string();
    assert!(tmp.starts_with("\"pg_temp\".\"dnt_import_check_"), "{tmp}");
    assert!(sent[create].ends_with(
        "(LIKE \"public\".\"items\" INCLUDING DEFAULTS INCLUDING CONSTRAINTS \
         INCLUDING IDENTITY INCLUDING GENERATED)"
    ));
    assert_eq!(
        sent[create + 1],
        format!(
            "ALTER TABLE {tmp} ALTER COLUMN \"id\" DROP DEFAULT, ALTER COLUMN \"id\" DROP NOT NULL"
        )
    );
    assert_eq!(sent[create - 1], "BEGIN");
}

#[tokio::test]
async fn read_only_blocks_real_run_and_server_check() {
    let fx = fixture(DbKind::Postgres, None, None, true).await;
    let mark = fx.fake.mark();
    assert!(matches!(
        fx.sm.import_prepare(fx.append()).await,
        Err(DbError::Blocked { .. })
    ));
    assert!(matches!(
        fx.sm
            .import_dry_run(DryRunRequest {
                request: fx.append(),
                server_check: true,
                confirmed: true,
            })
            .await,
        Err(DbError::Blocked { .. })
    ));
    // A file-only dry run reads and is allowed.
    let d = fx.dry_run(fx.append(), false).await;
    assert_eq!(d.job.state, JobState::Done);
    for s in fx.fake.since(mark) {
        assert_eq!(classify(&s, Dialect::Postgres), StatementKind::Read, "{s}");
    }
}

#[tokio::test]
async fn server_check_on_production_needs_confirmation() {
    let fx = fixture(DbKind::Postgres, None, Some(EnvTag::Production), false).await;
    let r = fx
        .sm
        .import_dry_run(DryRunRequest {
            request: fx.append(),
            server_check: true,
            confirmed: false,
        })
        .await;
    assert!(
        matches!(r, Err(DbError::ConfirmationRequired { .. })),
        "{r:?}"
    );
    let d = fx.dry_run(fx.append(), true).await;
    assert_eq!(d.job.state, JobState::Done);
}

// ---------------------------------------------------------------------------------------
// Real runs: statement sequences
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn append_pg_is_one_transaction() {
    let fx = pg().await;
    let mark = fx.fake.mark();
    let (preview, d) = fx.import(fx.append()).await;
    assert!(preview.transactional);
    assert!(preview.pre_step.is_none());
    assert_eq!(
        preview.insert_head,
        "INSERT INTO \"public\".\"items\" (\"id\", \"name\", \"note\") VALUES "
    );
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "INSERT x3",
            "COMMIT"
        ]
    );
    assert_eq!(
        d.job.summary.as_deref(),
        Some("Imported 3 rows (0 skipped) into items in 1 batches")
    );
}

#[tokio::test]
async fn batches_split_by_rows() {
    let fx = pg().await;
    let mut req = fx.append();
    req.batch_rows = 2;
    let mark = fx.fake.mark();
    let (_, d) = fx.import(req).await;
    assert_eq!(d.job.state, JobState::Done);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "INSERT x2",
            "INSERT x1",
            "COMMIT"
        ]
    );
}

#[tokio::test]
async fn replace_pg_delete_in_tx() {
    let fx = pg().await;
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(ImportMode::Replace { truncate: false }, ErrorPolicy::Stop))
        .await;
    let pre = preview.pre_step.unwrap();
    assert!(!pre.irreversible);
    assert_eq!(pre.sql, "DELETE FROM \"public\".\"items\"");
    assert_eq!(d.job.state, JobState::Done);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "DELETE FROM \"public\".\"items\"",
            "INSERT x3",
            "COMMIT"
        ]
    );
}

#[tokio::test]
async fn replace_pg_truncate_in_tx() {
    let fx = pg().await;
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(ImportMode::Replace { truncate: true }, ErrorPolicy::Stop))
        .await;
    assert!(!preview.pre_step.unwrap().irreversible);
    assert_eq!(d.job.state, JobState::Done);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "TRUNCATE \"public\".\"items\"",
            "INSERT x3",
            "COMMIT"
        ]
    );
}

#[tokio::test]
async fn replace_mysql_transactional_delete_in_tx() {
    let fx = mysql("InnoDB").await;
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(ImportMode::Replace { truncate: false }, ErrorPolicy::Stop))
        .await;
    assert!(preview.transactional);
    assert!(!preview.pre_step.unwrap().irreversible);
    assert_eq!(d.job.state, JobState::Done);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "START TRANSACTION",
            "DELETE FROM `shop`.`items`",
            "INSERT x3",
            "COMMIT"
        ]
    );
}

#[tokio::test]
async fn irreversible_pre_step_needs_ack() {
    let fx = mysql("InnoDB").await;
    let req = fx.request(ImportMode::Replace { truncate: true }, ErrorPolicy::Stop);
    let preview = fx.sm.import_prepare(req.clone()).await.unwrap();
    let pre = preview.pre_step.clone().unwrap();
    assert!(pre.irreversible);
    assert_eq!(pre.sql, "TRUNCATE `shop`.`items`");
    assert!(pre.warning.is_some());
    let mark = fx.fake.mark();
    let r = fx
        .sm
        .import_start(
            preview.plan_id,
            ImportConfirm {
                confirmed: true,
                name: None,
                irreversible_ack: false,
            },
        )
        .await;
    assert!(
        matches!(r, Err(DbError::ConfirmationRequired { .. })),
        "{r:?}"
    );
    assert!(fx.fake.since(mark).is_empty());
    assert_eq!(fx.sm.jobs.active_count(), 0);

    // Acknowledged: the TRUNCATE is sent and committed first, then the transaction.
    let mark = fx.fake.mark();
    let (_, d) = fx.import(req).await;
    assert_eq!(d.job.state, JobState::Done);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "TRUNCATE `shop`.`items`",
            "START TRANSACTION",
            "INSERT x3",
            "COMMIT"
        ]
    );

    // A non-transactional table: DELETE is the irreversible pre-step too.
    let fx = mysql("MyISAM").await;
    let preview = fx
        .sm
        .import_prepare(fx.request(ImportMode::Replace { truncate: false }, ErrorPolicy::Stop))
        .await
        .unwrap();
    assert!(!preview.transactional);
    assert!(preview.pre_step.unwrap().irreversible);
    assert!(preview
        .warnings
        .iter()
        .any(|w| w.starts_with("Not transactional")));
}

#[tokio::test]
async fn production_requires_exact_table_name() {
    let fx = fixture(DbKind::Postgres, None, Some(EnvTag::Production), false).await;
    for name in [None, Some("Items"), Some("item")] {
        let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
        assert_eq!(
            preview.confirm,
            ConfirmLevel::TypeName {
                name: "items".into()
            }
        );
        let mark = fx.fake.mark();
        let r = fx.sm.import_start(preview.plan_id, ok_confirm(name)).await;
        assert!(
            matches!(r, Err(DbError::ConfirmationRequired { .. })),
            "{r:?}"
        );
        assert!(fx.fake.since(mark).is_empty());
    }
    let (_, d) = fx.import(fx.append()).await;
    assert_eq!(d.job.state, JobState::Done);

    // Staging: a click.
    let fx = fixture(DbKind::Postgres, None, Some(EnvTag::Staging), false).await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    assert_eq!(preview.confirm, ConfirmLevel::Click);
    let r = fx
        .sm
        .import_start(
            preview.plan_id,
            ImportConfirm {
                confirmed: false,
                name: None,
                irreversible_ack: false,
            },
        )
        .await;
    assert!(matches!(r, Err(DbError::ConfirmationRequired { .. })));
}

#[tokio::test]
async fn upsert_pg_uses_on_conflict() {
    let fx = pg().await;
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(
            ImportMode::Upsert {
                key: vec!["id".into()],
            },
            ErrorPolicy::Stop,
        ))
        .await;
    let tail = " ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\", \"note\" = EXCLUDED.\"note\"";
    assert_eq!(preview.insert_tail, tail);
    assert_eq!(d.job.state, JobState::Done);
    let insert = fx
        .fake
        .since(mark)
        .into_iter()
        .find(|s| s.starts_with("INSERT"))
        .unwrap();
    assert!(insert.ends_with(tail), "{insert}");
}

#[tokio::test]
async fn upsert_mysql_uses_on_duplicate_key() {
    let fx = mysql("InnoDB").await;
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(
            ImportMode::Upsert {
                key: vec!["id".into()],
            },
            ErrorPolicy::Stop,
        ))
        .await;
    let tail = " ON DUPLICATE KEY UPDATE `name` = VALUES(`name`), `note` = VALUES(`note`)";
    assert_eq!(preview.insert_tail, tail);
    // Another unique key (name) and VALUES() deprecation on 8.0.20+.
    assert!(
        preview
            .warnings
            .iter()
            .any(|w| w.contains("any unique key")),
        "{:?}",
        preview.warnings
    );
    assert!(preview.warnings.iter().any(|w| w.contains("VALUES()")));
    assert_eq!(d.job.state, JobState::Done);
    let insert = fx
        .fake
        .since(mark)
        .into_iter()
        .find(|s| s.starts_with("INSERT"))
        .unwrap();
    assert!(insert.ends_with(tail), "{insert}");
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_mode_rolls_back_on_server_error() {
    let fx = pg().await;
    fx.fake.fail("INSERT", server_error("23505"));
    let mark = fx.fake.mark();
    let (_, d) = fx.import(fx.append()).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(!d.job.outcome_unknown);
    assert!(
        d.job.summary.as_deref().unwrap().contains("rolled back"),
        "{:?}",
        d.job.summary
    );
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "INSERT x3",
            "ROLLBACK"
        ]
    );
    let h = fx.history();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].origin, HistoryOrigin::Import);
    assert_eq!(h[0].status, HistoryStatus::Error);
    assert!(!h[0].outcome_unknown);
    // Nothing was committed.
    assert_eq!(h[0].affected, Some(0));
    assert!(
        h[0].sql_text.contains(": append, 0 rows"),
        "{}",
        h[0].sql_text
    );
}

#[tokio::test]
async fn stop_mode_conversion_error_stops_before_any_insert() {
    let fx = pg().await;
    fx.write_csv("id,name,note\n1,a,x\nnope,b,\n");
    let mark = fx.fake.mark();
    let (_, d) = fx.import(fx.append()).await;
    assert_eq!(d.job.state, JobState::Failed);
    let e = d.job.error.unwrap();
    assert!(e.message.contains("Row 3, column id"), "{}", e.message);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        ["SET standard_conforming_strings = on", "BEGIN", "ROLLBACK"]
    );
}

#[tokio::test]
async fn skip_mode_isolates_rows_with_savepoints() {
    let fx = pg().await;
    fx.write_csv("id,name,note\n1,a,x\n2,bad,\n3,c,z\n");
    fx.fake.fail("'bad'", server_error("23514"));
    let mark = fx.fake.mark();
    let (_, d) = fx
        .import(fx.request(ImportMode::Append, ErrorPolicy::Skip { max_errors: 10 }))
        .await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "SAVEPOINT dnt_import",
            "INSERT x3",
            "ROLLBACK TO SAVEPOINT dnt_import",
            "SAVEPOINT dnt_import",
            "INSERT x1",
            "RELEASE SAVEPOINT dnt_import",
            "SAVEPOINT dnt_import",
            "INSERT x1",
            "ROLLBACK TO SAVEPOINT dnt_import",
            "SAVEPOINT dnt_import",
            "INSERT x1",
            "RELEASE SAVEPOINT dnt_import",
            "COMMIT"
        ]
    );
    assert!(log_text(&d).contains("Row 3"), "{}", log_text(&d));
    assert_eq!(
        d.job.summary.as_deref(),
        Some("Imported 2 rows (1 skipped) into items in 2 batches")
    );
}

#[tokio::test]
async fn skip_mode_non_transactional_sends_single_rows() {
    let fx = mysql("MyISAM").await;
    fx.write_csv("id,name,note\n1,a,x\n2,bad,\n3,c,z\n");
    fx.fake.fail("'bad'", server_error("1366"));
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(ImportMode::Append, ErrorPolicy::Skip { max_errors: 10 }))
        .await;
    assert!(!preview.transactional);
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
    assert_eq!(
        shape(&fx.fake.since(mark)),
        ["INSERT x1", "INSERT x1", "INSERT x1"]
    );
    assert_eq!(
        d.job.summary.as_deref(),
        Some("Imported 2 rows (1 skipped) into items in 2 batches")
    );
}

/// The preview's sample statement holds as many rows as the job's statements will.
#[tokio::test]
async fn preview_sample_follows_rows_per_statement() {
    let fx = mysql("MyISAM").await;
    for (errors, rows) in [
        (ErrorPolicy::Stop, 3),
        (ErrorPolicy::Skip { max_errors: 10 }, 1),
    ] {
        let preview = fx
            .sm
            .import_prepare(fx.request(ImportMode::Append, errors))
            .await
            .unwrap();
        let sample = preview.sample.unwrap();
        assert_eq!(sample.matches("), (").count() + 1, rows, "{sample}");
    }
}

/// Only data errors are row errors in skip mode: a missing privilege refuses every row
/// alike, so it stops the job instead of skipping the whole file row by row.
#[tokio::test]
async fn skip_mode_stops_on_a_non_data_server_error() {
    for (fx, code) in [(pg().await, "42501"), (mysql("MyISAM").await, "1142")] {
        fx.write_csv("id,name,note\n1,a,x\n2,b,\n3,c,z\n");
        fx.fake.fail("INSERT", server_error(code));
        let mark = fx.fake.mark();
        let (_, d) = fx
            .import(fx.request(ImportMode::Append, ErrorPolicy::Skip { max_errors: 10 }))
            .await;
        assert_eq!(d.job.state, JobState::Failed, "{code}: {:?}", d.job);
        let inserts = fx
            .fake
            .since(mark)
            .iter()
            .filter(|s| s.starts_with("INSERT"))
            .count();
        assert_eq!(inserts, 1, "{code}: stopped at the first refusal");
        assert!(
            !log_text(&d).contains("Row 2"),
            "{code}: no row was skipped"
        );
    }
}

#[tokio::test]
async fn max_errors_stops() {
    let fx = pg().await;
    fx.write_csv("id,name,note\n1,a,x\nx,b,\ny,c,\n4,d,\n");
    let mark = fx.fake.mark();
    let (_, d) = fx
        .import(fx.request(ImportMode::Append, ErrorPolicy::Skip { max_errors: 1 }))
        .await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(d
        .job
        .error
        .as_ref()
        .unwrap()
        .message
        .contains("More than 1"));
    let sent = fx.fake.since(mark);
    assert_eq!(sent.last().unwrap(), "ROLLBACK");
    assert!(!sent.iter().any(|s| s == "COMMIT"));
}

#[tokio::test]
async fn commit_lost_is_unknown() {
    let fx = pg().await;
    fx.fake.kill_on(
        "COMMIT",
        DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: true,
            message: "gone".into(),
        },
    );
    let (_, d) = fx.import(fx.append()).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(d.job.outcome_unknown);
    assert!(d.job.error.as_ref().unwrap().outcome_unknown);
    let h = fx.history();
    assert_eq!(h.len(), 1);
    assert!(h[0].outcome_unknown);
}

/// The irreversible pre-step's answer is lost: it may have run, and the summary says
/// the existing rows may already be gone.
#[tokio::test]
async fn lost_pre_step_answer_says_rows_may_be_deleted() {
    let fx = mysql("InnoDB").await;
    fx.fake.kill_on(
        "TRUNCATE",
        DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: true,
            message: "gone".into(),
        },
    );
    let (_, d) = fx
        .import(fx.request(ImportMode::Replace { truncate: true }, ErrorPolicy::Stop))
        .await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(d.job.outcome_unknown);
    let s = d.job.summary.as_deref().unwrap();
    assert!(
        s.contains("the existing rows may already be deleted"),
        "{s}"
    );
    let h = fx.history();
    assert_eq!(h.len(), 1);
    assert!(h[0].outcome_unknown);
}

#[tokio::test]
async fn lost_connection_in_tx_is_not_committed() {
    let fx = pg().await;
    fx.fake.kill_on(
        "INSERT",
        DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: true,
            message: "gone".into(),
        },
    );
    let (_, d) = fx.import(fx.append()).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(!d.job.outcome_unknown);
    assert!(
        d.job.summary.as_deref().unwrap().contains("Not committed"),
        "{:?}",
        d.job.summary
    );
}

#[tokio::test]
async fn lost_connection_mid_batch_non_tx_is_unknown() {
    let fx = mysql("MyISAM").await;
    fx.fake.kill_on(
        "'b'",
        DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: true,
            message: "gone".into(),
        },
    );
    let mut req = fx.append();
    req.batch_rows = 1;
    let mark = fx.fake.mark();
    let (_, d) = fx.import(req).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(d.job.outcome_unknown);
    assert!(
        d.job
            .summary
            .as_deref()
            .unwrap()
            .contains("1 batch was written before the error"),
        "{:?}",
        d.job.summary
    );
    assert_eq!(shape(&fx.fake.since(mark)), ["INSERT x1", "INSERT x1"]);
    assert!(fx.history()[0].outcome_unknown);
}

/// Starts an import of one row per batch whose second batch takes 300 ms on the server.
async fn start_slow(fx: &Fx) -> (Uuid, usize) {
    *fx.fake.delay.lock().unwrap() = Some(("'b'".into(), Duration::from_millis(300)));
    let mut req = fx.append();
    req.batch_rows = 1;
    let preview = fx.sm.import_prepare(req).await.unwrap();
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .import_start(preview.plan_id, ok_confirm(None))
        .await
        .unwrap();
    // Wait until the slow batch is on the server.
    for _ in 0..100 {
        if fx.fake.since(mark).iter().any(|s| s.contains("'b'")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (id, mark)
}

#[tokio::test]
async fn stop_between_batches_sends_nothing_more() {
    let fx = pg().await;
    let (id, mark) = start_slow(&fx).await;
    assert!(fx.sm.jobs.cancel(id).await);
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Cancelled, "{:?}", d.job);
    assert!(!d.job.outcome_unknown);
    // The batch on the server finishes; nothing but the ROLLBACK follows it.
    assert_eq!(
        shape(&fx.fake.since(mark)),
        [
            "SET standard_conforming_strings = on",
            "BEGIN",
            "INSERT x1",
            "INSERT x1",
            "ROLLBACK"
        ]
    );
}

#[tokio::test]
async fn read_only_switch_mid_run_blocks_the_next_write() {
    let fx = mysql("MyISAM").await;
    let (id, mark) = start_slow(&fx).await;
    let mut cfg = fx.repo.get(fx.conn).unwrap();
    cfg.read_only = true;
    fx.repo
        .update(
            cfg,
            dnt_store::repo::SecretUpdate {
                db_password: dnt_store::repo::SecretChange::Keep,
                ssh_password: dnt_store::repo::SecretChange::Keep,
                ssh_key_passphrase: dnt_store::repo::SecretChange::Keep,
            },
        )
        .unwrap();
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Failed, "{:?}", d.job);
    assert_eq!(d.job.error.as_ref().unwrap().kind, "blocked");
    assert_eq!(shape(&fx.fake.since(mark)), ["INSERT x1", "INSERT x1"]);
    assert!(d
        .job
        .summary
        .as_deref()
        .unwrap()
        .contains("2 batches were written before the error"));
}

// ---------------------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn changed_file_is_stale() {
    let fx = pg().await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    assert_eq!(preview.file.name, "items.csv");
    assert_eq!(preview.file.size, CSV.len() as u64);
    fx.write_csv("id,name,note\n9,z,z\n");
    let mark = fx.fake.mark();
    let r = fx.sm.import_start(preview.plan_id, ok_confirm(None)).await;
    assert!(matches!(r, Err(DbError::PlanStale { .. })), "{r:?}");
    assert!(fx.fake.since(mark).is_empty());
    assert_eq!(fx.sm.jobs.active_count(), 0);
}

#[tokio::test]
async fn changed_table_is_stale() {
    let fx = pg().await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    fx.fake
        .detail
        .lock()
        .unwrap()
        .columns
        .push(column("extra", 4, "text", true));
    let mark = fx.fake.mark();
    let r = fx.sm.import_start(preview.plan_id, ok_confirm(None)).await;
    assert!(matches!(r, Err(DbError::PlanStale { .. })), "{r:?}");
    assert!(fx
        .fake
        .since(mark)
        .iter()
        .all(|s| classify(s, Dialect::Postgres) == StatementKind::Read));
}

#[tokio::test]
async fn plan_is_single_use() {
    let fx = pg().await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    let id = fx
        .sm
        .import_start(preview.plan_id, ok_confirm(None))
        .await
        .unwrap();
    fx.wait(id).await;
    let r = fx.sm.import_start(preview.plan_id, ok_confirm(None)).await;
    assert!(matches!(r, Err(DbError::PlanExpired)), "{r:?}");
}

#[tokio::test]
async fn mapping_errors_refuse_the_plan() {
    let fx = pg().await;
    let mut req = fx.append();
    req.mapping.retain(|m| m.target != "name");
    let r = fx.sm.import_prepare(req).await;
    assert!(
        matches!(&r, Err(DbError::Config { field: Some(f), .. }) if f == "mapping"),
        "{r:?}"
    );
}

#[tokio::test]
async fn history_entry_is_written_once_per_job() {
    let fx = pg().await;
    let mut req = fx.append();
    req.batch_rows = 1;
    let (_, d) = fx.import(req).await;
    assert_eq!(d.job.state, JobState::Done);
    let h = fx.history();
    assert_eq!(h.len(), 1, "{h:?}");
    let e = &h[0];
    assert_eq!(e.origin, HistoryOrigin::Import);
    assert_eq!(e.kind, "write");
    assert_eq!(e.status, HistoryStatus::Ok);
    assert_eq!(e.affected, Some(3));
    assert!(
        e.sql_text
            .starts_with("-- Import items.csv into items: append, 3 rows, 3 batches\n"),
        "{}",
        e.sql_text
    );
    assert!(e
        .sql_text
        .contains("INSERT INTO \"public\".\"items\" (\"id\", \"name\", \"note\") VALUES …"));
    // No value of the file reaches History.
    assert!(!e.sql_text.contains("'a'"));
}

#[tokio::test]
async fn generated_batches_classify_like_their_template() {
    let fx = mysql("InnoDB").await;
    let mark = fx.fake.mark();
    let (preview, d) = fx
        .import(fx.request(
            ImportMode::Upsert {
                key: vec!["id".into()],
            },
            ErrorPolicy::Stop,
        ))
        .await;
    assert_eq!(d.job.state, JobState::Done);
    let dialect = Dialect::MySql(MySqlMode::default());
    let template = format!(
        "{}(NULL, NULL, NULL){}",
        preview.insert_head, preview.insert_tail
    );
    let kind = classify(&template, dialect);
    assert_eq!(kind, StatementKind::Write);
    assert_eq!(classify(preview.sample.as_deref().unwrap(), dialect), kind);
    for s in fx
        .fake
        .since(mark)
        .iter()
        .filter(|s| s.starts_with("INSERT"))
    {
        assert_eq!(classify(s, dialect), kind);
    }
}

#[tokio::test]
async fn closing_the_connection_drops_its_plans() {
    let fx = pg().await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    fx.sm.close(fx.conn).await;
    let r = fx.sm.import_start(preview.plan_id, ok_confirm(None)).await;
    assert!(matches!(r, Err(DbError::PlanExpired)), "{r:?}");
}

#[tokio::test]
async fn target_reads_types_keys_and_policy() {
    let fx = mysql("InnoDB").await;
    let t = fx.sm.import_target(fx.conn, fx.table()).await.unwrap();
    assert_eq!(
        t.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "name", "note"]
    );
    assert_eq!(t.columns[0].logical, LogicalType::Integer);
    assert_eq!(t.columns[1].max_chars, Some(20));
    assert_eq!(t.keys.len(), 2);
    assert_eq!(t.atomicity, dnt_query::Atomicity::Yes);
    assert!(!t.read_only);
    assert_eq!(t.server.kind, DbKind::MySql);
}

#[test]
fn shape_counts_rows() {
    assert_eq!(
        shape(&["INSERT INTO t (a) VALUES (1), (2)".to_string()]),
        ["INSERT x2"]
    );
}

// ---------------------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn queued_job_rechecks_the_file_before_sending() {
    let fx = pg().await;
    fx.repo.set_app_setting("jobs_per_connection", "1").unwrap();
    // A second file for the queued job: the running one keeps its own.
    let other = fx.csv.with_file_name("other.csv");
    std::fs::write(&other, "id,name,note\n7,q,\n").unwrap();
    let mut queued = fx.append();
    queued.path = other.to_string_lossy().into_owned();
    let queued_plan = fx.sm.import_prepare(queued).await.unwrap();
    let (first, _) = start_slow(&fx).await;
    let second = fx
        .sm
        .import_start(queued_plan.plan_id, ok_confirm(None))
        .await
        .unwrap();
    assert_eq!(
        fx.sm.jobs.detail(second).unwrap().job.state,
        JobState::Queued
    );
    std::fs::write(&other, "id,name,note\n8,r,\n").unwrap();
    assert_eq!(fx.wait(first).await.job.state, JobState::Done);
    let d = fx.wait(second).await;
    assert_eq!(d.job.state, JobState::Failed, "{:?}", d.job);
    assert_eq!(d.job.error.as_ref().unwrap().kind, "plan_stale");
    // The summary says why (M7), not the generic stale text.
    assert!(
        d.job
            .summary
            .as_deref()
            .unwrap()
            .starts_with("The file changed since the preview"),
        "{:?}",
        d.job.summary
    );
    let sent = fx.fake.sent();
    assert!(!sent.iter().any(|s| s.contains("'q'") || s.contains("'r'")));
    assert_eq!(sent.iter().filter(|s| *s == "BEGIN").count(), 1, "{sent:?}");
    // Nothing was sent: no History entry for the queued job.
    assert_eq!(fx.history().len(), 1);
}

#[tokio::test]
async fn commit_refused_by_the_server_is_not_committed() {
    let fx = pg().await;
    fx.fake.fail("COMMIT", server_error("40001"));
    let (_, d) = fx.import(fx.append()).await;
    assert_eq!(d.job.state, JobState::Failed);
    assert!(!d.job.outcome_unknown);
    assert!(!d.job.error.as_ref().unwrap().outcome_unknown);
    assert!(
        d.job
            .summary
            .as_deref()
            .unwrap()
            .contains("refused the COMMIT"),
        "{:?}",
        d.job.summary
    );
    let h = fx.history();
    assert!(!h[0].outcome_unknown);
    assert_eq!(h[0].affected, Some(0));
}

#[tokio::test]
async fn lost_commit_has_no_row_count() {
    let fx = pg().await;
    fx.fake.kill_on(
        "COMMIT",
        DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: true,
            message: "gone".into(),
        },
    );
    fx.import(fx.append()).await;
    let h = fx.history();
    assert_eq!(h[0].affected, None);
    assert!(
        h[0].sql_text.contains(": append, unknown rows"),
        "{}",
        h[0].sql_text
    );
}

#[tokio::test]
async fn mapping_to_a_missing_source_column_is_refused() {
    let fx = pg().await;
    let mut req = fx.append();
    req.mapping[2].source = Some(3);
    let r = fx.sm.import_prepare(req.clone()).await;
    assert!(
        matches!(&r, Err(DbError::Config { field: Some(f), message }) if f == "mapping" && message.contains("3 columns")),
        "{r:?}"
    );
    let r = fx
        .sm
        .import_dry_run(DryRunRequest {
            request: req,
            server_check: false,
            confirmed: false,
        })
        .await;
    assert!(
        matches!(&r, Err(DbError::Config { field: Some(f), .. }) if f == "mapping"),
        "{r:?}"
    );
}

#[tokio::test]
async fn expired_plan_is_refused() {
    let fx = fixture_with(
        DbKind::Postgres,
        None,
        None,
        false,
        Some(Duration::from_millis(50)),
    )
    .await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mark = fx.fake.mark();
    let r = fx.sm.import_start(preview.plan_id, ok_confirm(None)).await;
    assert!(matches!(r, Err(DbError::PlanExpired)), "{r:?}");
    assert!(fx.fake.since(mark).is_empty());
}

#[tokio::test]
async fn same_size_file_change_is_stale() {
    let fx = pg().await;
    let preview = fx.sm.import_prepare(fx.append()).await.unwrap();
    let changed = CSV.replace("1,a,x", "1,q,x");
    assert_eq!(changed.len(), CSV.len());
    fx.write_csv(&changed);
    let mark = fx.fake.mark();
    let r = fx.sm.import_start(preview.plan_id, ok_confirm(None)).await;
    assert!(matches!(r, Err(DbError::PlanStale { .. })), "{r:?}");
    assert!(fx.fake.since(mark).is_empty());
}

#[tokio::test]
async fn production_irreversible_pre_step_needs_name_and_ack() {
    let fx = fixture(
        DbKind::MySql,
        Some("InnoDB"),
        Some(EnvTag::Production),
        false,
    )
    .await;
    let req = fx.request(ImportMode::Replace { truncate: true }, ErrorPolicy::Stop);
    for (name, ack) in [(Some("items"), false), (None, true), (Some("Items"), true)] {
        let preview = fx.sm.import_prepare(req.clone()).await.unwrap();
        assert_eq!(preview.irreversible_name.as_deref(), Some("items"));
        let mark = fx.fake.mark();
        let r = fx
            .sm
            .import_start(
                preview.plan_id,
                ImportConfirm {
                    confirmed: true,
                    name: name.map(str::to_string),
                    irreversible_ack: ack,
                },
            )
            .await;
        assert!(
            matches!(r, Err(DbError::ConfirmationRequired { .. })),
            "{name:?} {ack}: {r:?}"
        );
        assert!(fx.fake.since(mark).is_empty());
    }
    let (_, d) = fx.import(req).await;
    assert_eq!(d.job.state, JobState::Done, "{:?}", d.job);
}

#[tokio::test]
async fn mysql_dry_run_creates_the_temp_table_before_the_transaction() {
    let fx = mysql("InnoDB").await;
    let mark = fx.fake.mark();
    let d = fx.dry_run(fx.append(), true).await;
    assert_eq!(d.job.state, JobState::Done);
    let sent = shape(&fx.fake.since(mark));
    assert!(sent[0].starts_with("CREATE TEMPORARY TABLE"), "{sent:?}");
    assert_eq!(sent[1], "START TRANSACTION");
    assert_eq!(sent[sent.len() - 2], "ROLLBACK");
    assert!(sent[sent.len() - 1].starts_with("DROP TEMPORARY TABLE IF EXISTS"));
}

#[tokio::test]
async fn dry_run_cleans_up_after_an_error() {
    for (kind, fail) in [
        (DbKind::MySql, "INSERT"),
        (DbKind::MySql, "CREATE TEMPORARY"),
        (DbKind::Postgres, "INSERT"),
    ] {
        let fx = fixture(kind, Some("InnoDB"), None, false).await;
        // A deadlock is never a row error: it ends the dry run.
        fx.fake.fail(
            fail,
            server_error(if kind == DbKind::MySql {
                "1213"
            } else {
                "40P01"
            }),
        );
        let mark = fx.fake.mark();
        let d = fx.dry_run(fx.append(), true).await;
        assert_eq!(d.job.state, JobState::Failed, "{kind:?} {fail}");
        assert!(!d.job.outcome_unknown);
        let sent = shape(&fx.fake.since(mark));
        if kind == DbKind::MySql {
            // The temporary table is dropped even when its CREATE failed.
            assert!(
                sent.last()
                    .unwrap()
                    .starts_with("DROP TEMPORARY TABLE IF EXISTS"),
                "{sent:?}"
            );
            if fail == "INSERT" {
                assert_eq!(sent[sent.len() - 2], "ROLLBACK", "{sent:?}");
            }
        } else {
            assert_eq!(sent.last().unwrap(), "ROLLBACK", "{sent:?}");
        }
    }
}

#[tokio::test]
async fn dry_run_cleans_up_after_stop() {
    let fx = mysql("InnoDB").await;
    *fx.fake.delay.lock().unwrap() = Some(("'b'".into(), Duration::from_millis(300)));
    let mut req = fx.append();
    req.batch_rows = 1;
    let mark = fx.fake.mark();
    let id = fx
        .sm
        .import_dry_run(DryRunRequest {
            request: req,
            server_check: true,
            confirmed: true,
        })
        .await
        .unwrap();
    for _ in 0..100 {
        if fx.fake.since(mark).iter().any(|s| s.contains("'b'")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(fx.sm.jobs.cancel(id).await);
    let d = fx.wait(id).await;
    assert_eq!(d.job.state, JobState::Cancelled, "{:?}", d.job);
    let sent = shape(&fx.fake.since(mark));
    // Nothing more is inserted after the Stop; the cleanup still runs.
    assert!(!fx.fake.since(mark).iter().any(|s| s.contains("'c'")));
    assert_eq!(sent[sent.len() - 2], "ROLLBACK", "{sent:?}");
    assert!(sent[sent.len() - 1].starts_with("DROP TEMPORARY TABLE IF EXISTS"));
}
