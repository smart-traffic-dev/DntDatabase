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
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::Value;
use dnt_schema::TableDef;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
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
    /// `SELECT COUNT` statements take this long.
    count_delay: Mutex<Duration>,
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
        })
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

struct NoopCancel;

#[async_trait]
impl Canceller for NoopCancel {
    async fn cancel(&self) -> Result<(), DbError> {
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
        _opts: &ExecOptions,
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
        if upper.starts_with("SELECT COUNT") {
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
        Dialect::for_kind(self.fake.server.kind)
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoopCancel)
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
    f.fake.detail.lock().unwrap().comment = Some("changed elsewhere".into());
    let e =
        f.sm.table_design_preview(o.design_id, add_note(&o.def))
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::PlanStale { reason } if reason == "Table changed"),
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
    *f.fake.count_delay.lock().unwrap() = Duration::from_millis(500);
    let p = f.sm.table_design_preview(o.design_id, d).await.unwrap();
    let with_sql: Vec<_> = p
        .impacts
        .iter()
        .filter(|i| i.impact.count_sql.is_some())
        .collect();
    assert_eq!(with_sql.len(), 1, "{:?}", p.impacts);
    assert!(!with_sql[0].counted);
    assert_eq!(with_sql[0].rows, None);
}
