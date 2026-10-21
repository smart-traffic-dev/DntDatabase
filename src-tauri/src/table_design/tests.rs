//! The designer's commands against an in-process fake driver serving the `orders`
//! fixtures of `dnt-schema` (real server output), so `from_detail` + `fidelity` lock
//! nothing and drafts render as they would on the server.
use super::*;
use crate::ddl_exec::ActionOutcome;
use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::ResultSummary;
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, Introspector, RowSink, Session, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::sql::{Dialect, MySqlMode, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::Value;
use dnt_schema::TableDef;
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryFilter, HistoryOrigin};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const MYSQL_ORDERS: &str = include_str!("../../../crates/schema/tests/fixtures/MySql8_orders.json");
const MYSQL_ORDERS_DDL: &str =
    include_str!("../../../crates/schema/tests/fixtures/MySql8_orders.sql");
const PG_ORDERS: &str = include_str!("../../../crates/schema/tests/fixtures/Pg17_orders.json");

/// What the fake server holds and what it was sent, shared with the test.
struct Fake {
    server: ServerInfo,
    detail: Mutex<TableDetail>,
    ddl: Mutex<String>,
    /// Every statement sent through `execute_one`, in order.
    seen: Mutex<Vec<String>>,
    /// A statement containing the text fails with the error.
    fail: Mutex<Option<(String, DbError)>>,
    /// `SELECT COUNT` statements take this long (the fake ignores statement timeouts).
    count_delay: Mutex<Duration>,
    /// `ExecOptions::timeout` of every `SELECT COUNT`.
    count_timeouts: Mutex<Vec<Option<Duration>>>,
    /// Cancel requests received.
    cancels: AtomicUsize,
    /// MySQL session `sql_mode`; `ANSI_QUOTES` in it sets the dialect flag.
    sql_mode: Mutex<String>,
    /// Whether `ANSI_QUOTES` was on at each `object_ddl` call.
    ddl_ansi: Mutex<Vec<bool>>,
    /// `SELECT @@SESSION.sql_mode` answers with a binary value.
    mode_as_bytes: AtomicBool,
    /// `object_ddl` fails with this error.
    ddl_error: Mutex<Option<DbError>>,
}

impl Fake {
    fn new(fixture: &str, ddl: &str) -> Arc<Fake> {
        let v: serde_json::Value = serde_json::from_str(fixture).unwrap();
        Arc::new(Fake {
            server: serde_json::from_value(v["server"].clone()).unwrap(),
            detail: Mutex::new(serde_json::from_value(v["detail"].clone()).unwrap()),
            ddl: Mutex::new(ddl.trim_end().to_string()),
            seen: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
            count_delay: Mutex::new(Duration::ZERO),
            count_timeouts: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            sql_mode: Mutex::new("STRICT_TRANS_TABLES".into()),
            ddl_ansi: Mutex::new(Vec::new()),
            mode_as_bytes: AtomicBool::new(false),
            ddl_error: Mutex::new(None),
        })
    }

    fn ansi(&self) -> bool {
        self.sql_mode
            .lock()
            .unwrap()
            .split(',')
            .any(|m| m == "ANSI_QUOTES" || m == "ANSI")
    }

    /// The statements sent, without the background SchemaCache load's.
    fn sent(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| !s.starts_with("SELECT unnest"))
            .cloned()
            .collect()
    }
}

struct FakeDriver(Arc<Fake>);

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

struct FakeSession {
    fake: Arc<Fake>,
    tx: Mutex<TxState>,
}

struct CountingCancel(Arc<Fake>);

#[async_trait]
impl Canceller for CountingCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
        self.fake.seen.lock().unwrap().push(stmt.to_string());
        let fail = self.fake.fail.lock().unwrap().clone();
        if let Some((text, e)) = fail {
            if stmt.contains(&text) {
                return Err(e);
            }
        }
        let upper = stmt.to_ascii_uppercase();
        if upper.starts_with("BEGIN") || upper.starts_with("START TRANSACTION") {
            *self.tx.lock().unwrap() = TxState::Active;
        } else if upper.starts_with("COMMIT") || upper.starts_with("ROLLBACK") {
            *self.tx.lock().unwrap() = TxState::Idle;
        }
        let summary = ResultSummary {
            affected: None,
            row_count: 0,
            truncated: false,
            elapsed_ms: 0,
        };
        sink.result_start(&[]);
        if let Some(mode) = stmt.strip_prefix("SET SESSION sql_mode = '") {
            *self.fake.sql_mode.lock().unwrap() = mode.trim_end_matches('\'').to_string();
        }
        if stmt == "SELECT @@SESSION.sql_mode" {
            let mode = self.fake.sql_mode.lock().unwrap().clone();
            sink.rows(vec![vec![
                if self.fake.mode_as_bytes.load(Ordering::SeqCst) {
                    Value::Bytes {
                        len: mode.len() as u64,
                        preview: mode.into_bytes(),
                    }
                } else {
                    Value::Text(mode)
                },
            ]]);
        }
        if upper.starts_with("SELECT COUNT") {
            self.fake.count_timeouts.lock().unwrap().push(opts.timeout);
            let delay = *self.fake.count_delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            sink.rows(vec![vec![Value::Int(7)]]);
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
            DbKind::MySql => Dialect::MySql(MySqlMode {
                ansi_quotes: self.fake.ansi(),
                ..MySqlMode::default()
            }),
            DbKind::Postgres => Dialect::Postgres,
        }
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(CountingCancel(Arc::clone(&self.fake)))
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
        self.fake.ddl_ansi.lock().unwrap().push(self.fake.ansi());
        if let Some(e) = self.fake.ddl_error.lock().unwrap().clone() {
            return Err(e);
        }
        Ok(self.fake.ddl.lock().unwrap().clone())
    }
}

struct Fixture {
    sm: Arc<SessionManager>,
    fake: Arc<Fake>,
    conn: Uuid,
    _dir: tempfile::TempDir,
}

async fn fixture(fake: Arc<Fake>, env: Option<EnvTag>, read_only: bool) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let kind = fake.server.kind;
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver(Arc::clone(&fake)));
    let dir = tempfile::tempdir().unwrap();
    let mut sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(kind, driver)]),
    );
    sm.table_designs.count_timeout = Duration::from_millis(100);
    sm.table_designs.count_grace = Duration::from_millis(50);
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
    let sm = Arc::new(sm);
    sm.open(conn, None).await.unwrap();
    Fixture {
        sm,
        fake,
        conn,
        _dir: dir,
    }
}

async fn mysql(env: Option<EnvTag>, read_only: bool) -> Fixture {
    fixture(Fake::new(MYSQL_ORDERS, MYSQL_ORDERS_DDL), env, read_only).await
}

async fn pg() -> Fixture {
    fixture(Fake::new(PG_ORDERS, ""), None, false).await
}

fn scope(f: &Fixture) -> Scope {
    let d = f.fake.detail.lock().unwrap();
    Scope {
        database: d.object.database.clone(),
        schema: d.object.schema.clone(),
    }
}

async fn open(f: &Fixture) -> DesignOpen {
    f.sm.table_design_open(f.conn, scope(f), Some("orders".into()))
        .await
        .unwrap()
}

/// The stored table plus a new nullable column `note` (a copy of `created_at`).
fn add_note(def: &TableDef) -> TableDef {
    let mut d = def.clone();
    let mut note = d
        .columns
        .iter()
        .find(|c| c.name == "created_at")
        .unwrap()
        .clone();
    note.original = None;
    note.name = "note".into();
    d.columns.push(note);
    d
}

fn no_progress(_: &str) {}

#[tokio::test]
async fn opens_without_locks_and_renders() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    assert!(o.locked.is_empty(), "{:?}", o.locked);
    assert!(!o.read_only);
    let r =
        f.sm.table_design_render(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    assert!(r.errors.is_empty() && r.error.is_none(), "{r:?}");
    assert_eq!(r.statements.len(), 1);
    assert!(r.statements[0].starts_with("ALTER TABLE"), "{r:?}");
}

#[tokio::test]
async fn render_rejects_unknown_original_and_restores_locked() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    let mut d = o.def.clone();
    d.columns[1].original = Some("ghost".into());
    let r = f.sm.table_design_render(o.design_id, d).await.unwrap();
    assert!(r.statements.is_empty());
    assert!(!r.errors.is_empty(), "{r:?}");
    // A client-sent `locked` flag is never trusted.
    let mut d = add_note(&o.def);
    d.columns[0].locked = true;
    d.columns[0].comment = Some("x".into());
    let r = f.sm.table_design_render(o.design_id, d).await.unwrap();
    assert!(r.error.is_none() && r.errors.is_empty(), "{r:?}");
    assert!(r.statements[0].contains("COMMENT"), "{r:?}");
}

#[tokio::test]
async fn preview_blocks_read_only() {
    let f = mysql(None, true).await;
    let o = open(&f).await;
    assert!(o.read_only);
    let e =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Blocked { .. }), "{e:?}");
}

#[tokio::test]
async fn preview_detects_changed_table() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    // Dropping a column renders a count: none may run once the table changed.
    let draft = drop_created_at(&o.def);
    f.fake.detail.lock().unwrap().comment = Some("changed elsewhere".into());
    let e =
        f.sm.table_design_preview(o.design_id, draft)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::PlanStale { reason } if reason == "Table changed"),
        "{e:?}"
    );
    let sent = f.fake.sent();
    assert!(
        !sent
            .iter()
            .any(|s| s.starts_with("SELECT COUNT") || s.contains("READ ONLY")),
        "{sent:?}"
    );
}

/// The stored table without `created_at` (renders a data-loss impact with a count).
fn drop_created_at(def: &TableDef) -> TableDef {
    let mut d = def.clone();
    d.columns.retain(|c| c.name != "created_at");
    d
}

#[tokio::test]
async fn edited_locked_element_is_restored_and_never_rendered() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    lock(&f.sm.table_designs.designs)
        .get_mut(&o.design_id)
        .unwrap()
        .base
        .columns
        .iter_mut()
        .find(|c| c.name == "total")
        .unwrap()
        .locked = true;
    // The client clears the flag and edits the locked column.
    let mut d = o.def.clone();
    let total = d.columns.iter_mut().find(|c| c.name == "total").unwrap();
    total.locked = false;
    total.comment = Some("hacked".into());
    total.nullable = true;
    let r =
        f.sm.table_design_render(o.design_id, d.clone())
            .await
            .unwrap();
    assert!(r.statements.is_empty(), "{r:?}");
    let e =
        f.sm.table_design_preview(o.design_id, d.clone())
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Config { .. }), "{e:?}");
    // Next to a real change, the locked column still never reaches the SQL.
    let mut with_note = add_note(&d);
    with_note
        .columns
        .iter_mut()
        .find(|c| c.name == "total")
        .unwrap()
        .comment = Some("hacked".into());
    let r =
        f.sm.table_design_render(o.design_id, with_note)
            .await
            .unwrap();
    assert_eq!(r.statements.len(), 1, "{r:?}");
    assert!(!r.statements[0].contains("hacked"), "{r:?}");
    assert!(!r.statements[0].contains("`total`"), "{r:?}");
}

#[tokio::test]
async fn apply_detects_changed_table_and_sends_nothing() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    f.fake.detail.lock().unwrap().comment = Some("changed elsewhere".into());
    let e =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::PlanStale { reason } if reason == "Table changed"),
        "{e:?}"
    );
    let sent = f.fake.sent();
    assert!(!sent.iter().any(|s| s.starts_with("ALTER")), "{sent:?}");
    let history =
        f.sm.repo
            .history_list(&HistoryFilter {
                connection_id: Some(f.conn),
                search: None,
                origin: None,
                limit: 10,
            })
            .unwrap();
    assert!(history.is_empty());
}

#[tokio::test]
async fn history_records_every_statement_as_table_design() {
    let f = pg().await;
    let o = open(&f).await;
    let mut d = add_note(&o.def);
    d.columns
        .iter_mut()
        .find(|c| c.name == "total")
        .unwrap()
        .name = "amount".into();
    let p = f.sm.table_design_preview(o.design_id, d).await.unwrap();
    assert_eq!(p.statements.len(), 2);
    let report =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
            .await
            .unwrap();
    assert_eq!(report.outcome, ActionOutcome::Applied, "{report:?}");
    let history =
        f.sm.repo
            .history_list(&HistoryFilter {
                connection_id: Some(f.conn),
                search: None,
                origin: None,
                limit: 10,
            })
            .unwrap();
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0].origin, HistoryOrigin::TableDesign);
    assert_eq!(history[0].sql_text, p.statements.join(";\n"));
    assert_eq!(history[0].kind, "ddl");
}

#[test]
fn mysql_needs_exactly_one_statement() {
    let one = vec!["ALTER TABLE t ADD c int".to_string()];
    let two = vec![one[0].clone(), one[0].clone()];
    assert!(apply::check_statement_count(DbKind::MySql, &one).is_ok());
    assert!(matches!(
        apply::check_statement_count(DbKind::MySql, &two),
        Err(DbError::Config { .. })
    ));
    assert!(matches!(
        apply::check_statement_count(DbKind::MySql, &[]),
        Err(DbError::Config { .. })
    ));
    assert!(apply::check_statement_count(DbKind::Postgres, &two).is_ok());
}

#[test]
fn ansi_quote_modes_are_dropped() {
    assert_eq!(
        without_ansi_quotes("STRICT_TRANS_TABLES,ANSI_QUOTES,NO_ZERO_DATE").as_deref(),
        Some("STRICT_TRANS_TABLES,NO_ZERO_DATE")
    );
    assert_eq!(
        without_ansi_quotes(
            "REAL_AS_FLOAT,PIPES_AS_CONCAT,ANSI_QUOTES,IGNORE_SPACE,ONLY_FULL_GROUP_BY,ANSI"
        )
        .as_deref(),
        Some("REAL_AS_FLOAT,PIPES_AS_CONCAT,IGNORE_SPACE,ONLY_FULL_GROUP_BY")
    );
    assert_eq!(
        without_ansi_quotes("PIPES_AS_CONCAT,ansi_quotes,ORACLE,MSSQL,DB2,MAXDB,POSTGRESQL")
            .as_deref(),
        Some("PIPES_AS_CONCAT")
    );
    assert_eq!(without_ansi_quotes("").as_deref(), Some(""));
    // Anything that is not a plain word is never spliced.
    assert_eq!(
        without_ansi_quotes("ANSI_QUOTES,X'; DROP TABLE t; --"),
        None
    );
}

#[test]
fn auto_increment_counter_is_ignored() {
    let ddl = "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB AUTO_INCREMENT=42 DEFAULT CHARSET=utf8mb4";
    assert_eq!(
        strip_auto_increment(ddl),
        "CREATE TABLE `t` (\n  `id` int NOT NULL AUTO_INCREMENT,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB  DEFAULT CHARSET=utf8mb4"
    );
    assert_eq!(
        strip_auto_increment(ddl),
        strip_auto_increment(&ddl.replace("=42", "=43"))
    );
    assert_eq!(
        strip_auto_increment("x AUTO_INCREMENT=y"),
        "x AUTO_INCREMENT=y"
    );
}

#[tokio::test]
async fn ddl_is_read_without_ansi_quotes_and_the_mode_restored() {
    for as_bytes in [false, true] {
        let fake = Fake::new(MYSQL_ORDERS, MYSQL_ORDERS_DDL);
        fake.mode_as_bytes.store(as_bytes, Ordering::SeqCst);
        *fake.sql_mode.lock().unwrap() = "STRICT_TRANS_TABLES,ANSI_QUOTES".into();
        let s = FakeSession {
            fake: Arc::clone(&fake),
            tx: Mutex::new(TxState::Idle),
        };
        let obj = fake.detail.lock().unwrap().object.clone();
        let ddl = read_ddl(&s, &obj).await.unwrap();
        assert_eq!(ddl.as_deref(), Some(MYSQL_ORDERS_DDL.trim_end()));
        assert_eq!(*fake.ddl_ansi.lock().unwrap(), [false]);
        assert_eq!(
            fake.sent(),
            [
                "SELECT @@SESSION.sql_mode",
                "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'",
                "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,ANSI_QUOTES'",
            ]
        );
        assert!(matches!(s.dialect(), Dialect::MySql(m) if m.ansi_quotes));
    }
    // Without ANSI_QUOTES nothing is switched.
    let fake = Fake::new(MYSQL_ORDERS, MYSQL_ORDERS_DDL);
    let s = FakeSession {
        fake: Arc::clone(&fake),
        tx: Mutex::new(TxState::Idle),
    };
    let obj = fake.detail.lock().unwrap().object.clone();
    read_ddl(&s, &obj).await.unwrap();
    assert!(fake.sent().is_empty());
}

/// The preview carries the row estimate of the table read with it (statistics move
/// without making the table "changed"); a new table has none.
#[tokio::test]
async fn preview_carries_the_row_estimate() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    f.fake.detail.lock().unwrap().row_estimate = Some(1_234_567);
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    assert_eq!(p.row_estimate, Some(1_234_567));
    let n =
        f.sm.table_design_open(f.conn, scope(&f), None)
            .await
            .unwrap();
    let mut d = n.def.clone();
    d.name = "fresh".into();
    d.columns = o.def.columns.clone();
    for c in &mut d.columns {
        c.original = None;
    }
    let p = f.sm.table_design_preview(n.design_id, d).await.unwrap();
    assert_eq!(p.row_estimate, None);
}

fn server_error(code: Option<&str>, sqlstate: Option<&str>) -> DbError {
    DbError::Server {
        code: code.map(str::to_string),
        sqlstate: sqlstate.map(str::to_string),
        message: "refused".into(),
        position: None,
    }
}

/// Only a DDL the server will not show (permission, not supported) is "no DDL"; a lock
/// wait or another transient error is passed on, so a busy table neither looks changed
/// at apply nor opens read-only.
#[tokio::test]
async fn only_refused_ddl_reads_as_no_ddl() {
    let refused = [
        server_error(None, None),
        server_error(Some("1142"), Some("42000")),
        server_error(Some("1044"), Some("42000")),
        server_error(Some("1227"), Some("42000")),
        server_error(Some("1235"), Some("42000")),
        server_error(None, Some("42501")),
        server_error(None, Some("0A000")),
    ];
    let transient = [
        server_error(Some("1205"), Some("HY000")),
        server_error(Some("1213"), Some("40001")),
        server_error(None, Some("55P03")),
        DbError::timeout(dnt_core::error::ConnectStage::Execute),
    ];
    for (e, shown_as_none) in refused
        .into_iter()
        .map(|e| (e, true))
        .chain(transient.into_iter().map(|e| (e, false)))
    {
        let fake = Fake::new(MYSQL_ORDERS, MYSQL_ORDERS_DDL);
        *fake.ddl_error.lock().unwrap() = Some(e.clone());
        let s = FakeSession {
            fake: Arc::clone(&fake),
            tx: Mutex::new(TxState::Idle),
        };
        let obj = fake.detail.lock().unwrap().object.clone();
        let r = read_table(&s, &obj).await;
        if shown_as_none {
            assert!(matches!(&r, Ok((_, None))), "{e:?}: {r:?}");
        } else {
            assert!(r.is_err(), "{e:?}: {r:?}");
        }
    }
}

/// A lock wait while the apply re-reads the table is an error (nothing sent), not
/// "Table changed".
#[tokio::test]
async fn lock_wait_at_the_recheck_is_not_table_changed() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    *f.fake.ddl_error.lock().unwrap() = Some(server_error(Some("1205"), Some("HY000")));
    let e =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Server { code: Some(c), .. } if c == "1205"),
        "{e:?}"
    );
    assert!(!f.fake.sent().iter().any(|s| s.starts_with("ALTER")));
}

#[tokio::test]
async fn plan_is_single_use() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    assert_eq!(p.confirm_name, None);
    let report =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
            .await
            .unwrap();
    assert_eq!(report.outcome, ActionOutcome::Applied, "{report:?}");
    assert_eq!(report.statements_sent, 1);
    let sent = f.fake.sent();
    let alter: Vec<_> = sent.iter().filter(|s| s.starts_with("ALTER")).collect();
    assert_eq!(alter, [&p.statements[0]]);
    assert!(sent
        .iter()
        .any(|s| s == "SET SESSION lock_wait_timeout = 5"));
    assert!(!sent.iter().any(|s| s == "BEGIN"));
    let again =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
            .await
            .unwrap_err();
    assert!(matches!(again, DbError::PlanExpired), "{again:?}");
}

#[tokio::test]
async fn production_requires_exact_name() {
    let f = mysql(Some(EnvTag::Production), false).await;
    let o = open(&f).await;
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    assert_eq!(p.confirm_name.as_deref(), Some("orders"));
    let e =
        f.sm.table_design_apply(
            p.plan_id,
            true,
            Some("Orders".into()),
            Uuid::new_v4(),
            &no_progress,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::ConfirmationRequired { .. }), "{e:?}");
    assert!(!f.fake.sent().iter().any(|s| s.starts_with("ALTER")));
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    let report =
        f.sm.table_design_apply(
            p.plan_id,
            true,
            Some("orders".into()),
            Uuid::new_v4(),
            &no_progress,
        )
        .await
        .unwrap();
    assert_eq!(report.outcome, ActionOutcome::Applied, "{report:?}");
}

#[tokio::test]
async fn pg_failure_mid_way_rolls_back() {
    let f = pg().await;
    let o = open(&f).await;
    assert!(o.locked.is_empty(), "{:?}", o.locked);
    let mut d = add_note(&o.def);
    d.columns
        .iter_mut()
        .find(|c| c.name == "total")
        .unwrap()
        .name = "amount".into();
    let p = f.sm.table_design_preview(o.design_id, d).await.unwrap();
    assert_eq!(p.statements.len(), 2, "{:?}", p.statements);
    assert!(p.statements[1].contains("note"), "{:?}", p.statements);
    *f.fake.fail.lock().unwrap() = Some((
        "\"note\"".into(),
        DbError::Server {
            code: None,
            sqlstate: Some("42701".into()),
            message: "column exists".into(),
            position: None,
        },
    ));
    let stages = Mutex::new(Vec::new());
    let report =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &|s: &str| {
            stages.lock().unwrap().push(s.to_string())
        })
        .await
        .unwrap();
    assert_eq!(report.outcome, ActionOutcome::RolledBack, "{report:?}");
    assert_eq!(report.statements_sent, 2);
    assert!(!report.error.as_ref().unwrap().outcome_unknown);
    let sent = f.fake.sent();
    let begin = sent.iter().position(|s| s == "BEGIN").unwrap();
    assert_eq!(
        sent[begin + 1],
        "SET LOCAL standard_conforming_strings = on"
    );
    assert!(sent.iter().any(|s| s == "ROLLBACK"));
    assert!(!sent.iter().any(|s| s == "COMMIT"));
    let stages = stages.into_inner().unwrap();
    assert!(stages.contains(&"executing 1/2".to_string()), "{stages:?}");
    assert!(stages.contains(&"executing 2/2".to_string()), "{stages:?}");
}

/// The 5 s lock wait bounds the whole apply: it is set before the table is read again
/// (on PostgreSQL that read waits for a lock another session holds on the table), so a
/// stale table is found with the bound already in place and nothing else sent.
#[tokio::test]
async fn lock_wait_bound_covers_the_recheck() {
    for (f, set) in [
        (
            mysql(None, false).await,
            "SET SESSION lock_wait_timeout = 5",
        ),
        (pg().await, "SET lock_timeout = '5s'"),
    ] {
        let o = open(&f).await;
        let p =
            f.sm.table_design_preview(o.design_id, add_note(&o.def))
                .await
                .unwrap();
        f.fake.detail.lock().unwrap().comment = Some("changed elsewhere".into());
        let before = f.fake.sent().len();
        let e =
            f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
                .await
                .unwrap_err();
        assert!(matches!(&e, DbError::PlanStale { .. }), "{e:?}");
        let sent = f.fake.sent()[before..].to_vec();
        assert_eq!(sent, [set], "{sent:?}");
    }
}

/// MySQL / MariaDB refuse a statement that could not get its lock within
/// `lock_wait_timeout` with error 1205 before changing anything: not applied. Any other
/// server error of a sent MySQL DDL stays unknown (DDL is not transactional there).
#[tokio::test]
async fn mysql_lock_wait_timeout_is_not_applied() {
    for (code, expected) in [
        ("1205", ActionOutcome::NotApplied),
        ("1062", ActionOutcome::Unknown),
    ] {
        let f = mysql(None, false).await;
        let o = open(&f).await;
        let p =
            f.sm.table_design_preview(o.design_id, add_note(&o.def))
                .await
                .unwrap();
        *f.fake.fail.lock().unwrap() = Some((
            "ALTER TABLE".into(),
            DbError::Server {
                code: Some(code.into()),
                sqlstate: Some("HY000".into()),
                message: "refused".into(),
                position: None,
            },
        ));
        let report =
            f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
                .await
                .unwrap();
        assert_eq!(report.outcome, expected, "{code}: {report:?}");
        assert_eq!(
            report.error.as_ref().unwrap().outcome_unknown,
            expected == ActionOutcome::Unknown,
            "{code}: {report:?}"
        );
        assert_eq!(report.statements_sent, 1);
    }
}

#[tokio::test]
async fn commit_lost_is_unknown() {
    let f = pg().await;
    let o = open(&f).await;
    let p =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap();
    *f.fake.fail.lock().unwrap() = Some((
        "COMMIT".into(),
        DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: true,
            message: "lost".into(),
        },
    ));
    let report =
        f.sm.table_design_apply(p.plan_id, true, None, Uuid::new_v4(), &no_progress)
            .await
            .unwrap();
    assert_eq!(report.outcome, ActionOutcome::Unknown, "{report:?}");
    assert!(report.error.as_ref().unwrap().outcome_unknown);
    assert_eq!(report.statements_sent, 1);
}

#[tokio::test]
async fn count_timeout_is_not_counted() {
    let f = mysql(None, false).await;
    let o = open(&f).await;
    let mut d = o.def.clone();
    d.columns.retain(|c| c.name != "created_at");
    let p =
        f.sm.table_design_preview(o.design_id, d.clone())
            .await
            .unwrap();
    let counted: Vec<_> = p.impacts.iter().filter(|i| i.counted).collect();
    assert_eq!(counted.len(), 1, "{:?}", p.impacts);
    assert_eq!(counted[0].rows, Some(7.0));
    // The driver gets the bound as its statement timeout (it stops the statement on
    // the server when it expires).
    assert_eq!(
        *f.fake.count_timeouts.lock().unwrap(),
        [Some(Duration::from_millis(100))]
    );
    // The server bounds it too, inside the read-only transaction.
    let sent = f.fake.sent();
    let start = sent
        .iter()
        .position(|s| s == "START TRANSACTION READ ONLY")
        .unwrap();
    assert_eq!(sent[start + 1], "SET SESSION max_execution_time = 100");
    assert!(sent[start + 2].starts_with("SELECT COUNT"), "{sent:?}");
    // A count that does not return in time (the fake ignores timeouts) is not counted
    // and does not hold the preview up.
    *f.fake.count_delay.lock().unwrap() = Duration::from_secs(5);
    let started = std::time::Instant::now();
    let p = f.sm.table_design_preview(o.design_id, d).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    let with_sql: Vec<_> = p
        .impacts
        .iter()
        .filter(|i| i.impact.count_sql.is_some())
        .collect();
    assert_eq!(with_sql.len(), 1, "{:?}", p.impacts);
    assert!(!with_sql[0].counted);
    assert_eq!(with_sql[0].rows, None);
}

/// A MySQL fake that reports itself as `variant` `version`.
fn mysql_fake_as(variant: &str, version: [u32; 3]) -> Arc<Fake> {
    let mut v: serde_json::Value = serde_json::from_str(MYSQL_ORDERS).unwrap();
    v["server"]["variant"] = serde_json::json!(variant);
    v["server"]["version"] = serde_json::json!(version);
    Fake::new(&v.to_string(), MYSQL_ORDERS_DDL)
}

/// MySQL counts are bounded on the server: `max_execution_time` (ms) on MySQL 5.7.8+,
/// `max_statement_time` (s) on MariaDB, set inside the read-only transaction.
#[tokio::test]
async fn mysql_family_counts_are_bounded_on_the_server() {
    for (variant, version, expected) in [
        (
            "MySql8",
            [8, 0, 36],
            Some("SET SESSION max_execution_time = 250"),
        ),
        (
            "MySql57",
            [5, 7, 44],
            Some("SET SESSION max_execution_time = 250"),
        ),
        (
            "MariaDb",
            [10, 6, 16],
            Some("SET SESSION max_statement_time = 0.250"),
        ),
        ("MySql57", [5, 7, 7], None),
    ] {
        let fake = mysql_fake_as(variant, version);
        let s = FakeSession {
            fake: Arc::clone(&fake),
            tx: Mutex::new(TxState::Idle),
        };
        let impact = Impact {
            kind: dnt_schema::ImpactKind::DataLoss,
            element: ElementRef::Column("x".into()),
            message: "m".into(),
            count_sql: Some("SELECT COUNT(*) FROM t".into()),
        };
        let out = apply::count_impacts(
            &s,
            vec![impact],
            Duration::from_millis(250),
            Duration::from_millis(50),
        )
        .await;
        assert!(out[0].counted, "{variant}");
        let sent = fake.sent();
        let mut want = vec!["START TRANSACTION READ ONLY".to_string()];
        want.extend(expected.map(str::to_string));
        want.push("SELECT COUNT(*) FROM t".into());
        want.push("ROLLBACK".into());
        assert_eq!(sent, want, "{variant} {version:?}");
    }
}

#[tokio::test]
async fn pg_counts_run_read_only_with_a_server_timeout() {
    let f = pg().await;
    let o = open(&f).await;
    let p =
        f.sm.table_design_preview(o.design_id, drop_created_at(&o.def))
            .await
            .unwrap();
    assert!(p.impacts.iter().any(|i| i.counted), "{:?}", p.impacts);
    let sent = f.fake.sent();
    let begin = sent.iter().position(|s| s == "BEGIN READ ONLY").unwrap();
    assert_eq!(
        sent[begin + 1..begin + 3],
        [
            "SET LOCAL standard_conforming_strings = on",
            "SET LOCAL statement_timeout = '100ms'"
        ]
    );
    assert!(sent[begin + 3].starts_with("SELECT COUNT"), "{sent:?}");
    assert_eq!(sent[begin + 4], "ROLLBACK");
}

#[tokio::test]
async fn non_read_counts_never_run() {
    let fake = Fake::new(PG_ORDERS, "");
    let s = FakeSession {
        fake: Arc::clone(&fake),
        tx: Mutex::new(TxState::Idle),
    };
    let impact = Impact {
        kind: dnt_schema::ImpactKind::DataLoss,
        element: ElementRef::Column("x".into()),
        message: "m".into(),
        count_sql: Some("DELETE FROM t".into()),
    };
    let out = apply::count_impacts(
        &s,
        vec![impact],
        Duration::from_millis(100),
        Duration::from_millis(50),
    )
    .await;
    assert!(!out[0].counted);
    assert!(!fake.sent().iter().any(|s| s.starts_with("DELETE")));
}
