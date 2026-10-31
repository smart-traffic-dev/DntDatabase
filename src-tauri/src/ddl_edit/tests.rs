//! DDL edits against an in-process fake driver: the object's DDL, canned answers to
//! the catalog reads (by statement prefix) and the objects `list_objects` returns.
use super::apply::OWNER_NOT_RESTORED;
use super::catalog::{MYSQL_SAME_TIMING_SQL, MYSQL_TRIGGER_SQL, PG_ROUTINE_SQL, PG_TRIGGER_SQL};
use super::*;
use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, Engine, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, SqlSession, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, detect_pg, DbKind, ServerInfo, SqlKind};
use dnt_core::sql::{Dialect, MySqlMode, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::Value;
use dnt_store::db::open_in_memory;
use dnt_store::history::{HistoryEntry, HistoryFilter};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const MYSQL_PROC: &str =
    "CREATE DEFINER=`root`@`%` PROCEDURE `p`(IN x INT)\nBEGIN\n  SELECT x;\nEND";
const MYSQL_VIEW: &str = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1 AS `id`";
const PG_FUNC: &str = "CREATE OR REPLACE FUNCTION public.f(x integer)\n RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT x $function$";

fn mysql8() -> ServerInfo {
    detect_mysql("8.0.36").unwrap()
}
fn mariadb() -> ServerInfo {
    detect_mysql("10.6.16-MariaDB").unwrap()
}
fn pg17() -> ServerInfo {
    detect_pg(170002, "PostgreSQL 17.2")
}
fn pg12() -> ServerInfo {
    detect_pg(120020, "PostgreSQL 12.20")
}

/// What the fake server holds and what it was sent, shared with the test.
struct Fake {
    server: ServerInfo,
    ddl: Mutex<String>,
    ddl_error: Mutex<Option<DbError>>,
    /// Every statement sent through `execute_one`, with its params, in order.
    seen: Mutex<Vec<(String, Vec<String>)>>,
    /// Rows answered to a statement starting with the prefix.
    answers: Mutex<Vec<(String, Vec<Vec<Value>>)>>,
    /// Names `list_objects` returns per kind.
    objects: Mutex<HashMap<ObjectKind, Vec<String>>>,
    /// A statement containing the text fails with the error.
    fail: Mutex<Option<(String, DbError)>>,
    /// A statement containing the text (once it succeeded) sets the DDL to the second.
    sets_ddl: Mutex<Option<(String, String)>>,
    /// Run once by the next `list_objects`.
    on_list: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Sessions opened.
    connects: AtomicUsize,
    /// Connects from this ordinal on (0-based, counting every attempt) fail.
    connect_fail_from: Mutex<Option<usize>>,
    /// `(n, hook)`: the hook runs in the `n`-th `object_ddl` call from now (0 = next).
    on_ddl: Mutex<Option<(usize, Hook)>>,
    /// A statement containing the text is sent after a silent reconnect of its session.
    reconnect_on: Mutex<Option<String>>,
}

type Hook = Box<dyn FnOnce() + Send>;

impl Fake {
    fn new(server: ServerInfo, ddl: &str) -> Arc<Fake> {
        Arc::new(Fake {
            server,
            ddl: Mutex::new(ddl.to_string()),
            ddl_error: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(Vec::new()),
            objects: Mutex::new(HashMap::new()),
            fail: Mutex::new(None),
            sets_ddl: Mutex::new(None),
            on_list: Mutex::new(None),
            connects: AtomicUsize::new(0),
            connect_fail_from: Mutex::new(None),
            on_ddl: Mutex::new(None),
            reconnect_on: Mutex::new(None),
        })
    }

    fn fail_on(&self, text: &str, e: DbError) {
        *self.fail.lock().unwrap() = Some((text.to_string(), e));
    }

    fn answer(&self, prefix: &str, rows: Vec<Vec<&str>>) {
        self.answers.lock().unwrap().push((
            prefix.to_string(),
            rows.into_iter()
                .map(|r| r.into_iter().map(|v| Value::Text(v.into())).collect())
                .collect(),
        ));
    }

    fn sent(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    fn params_of(&self, sql: &str) -> Vec<Vec<String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| s == sql)
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// Nothing but catalog reads was sent.
    fn only_reads(&self) -> bool {
        self.sent().iter().all(|s| s.starts_with("SELECT"))
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
        let n = self.0.connects.fetch_add(1, AtomicOrdering::SeqCst);
        if self
            .0
            .connect_fail_from
            .lock()
            .unwrap()
            .is_some_and(|from| n >= from)
        {
            return Err(DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message: "unreachable".into(),
            });
        }
        Ok(Box::new(FakeSession {
            fake: Arc::clone(&self.0),
            tx: Mutex::new(TxState::Idle),
            reconnects: AtomicUsize::new(0),
        }))
    }
}

struct FakeSession {
    fake: Arc<Fake>,
    tx: Mutex<TxState>,
    reconnects: AtomicUsize,
}

struct NoCancel;

#[async_trait]
impl Canceller for NoCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        Ok(())
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.fake.server
    }
    fn engine(&self) -> Engine<'_> {
        Engine::Sql(self)
    }
    fn reconnects(&self) -> u64 {
        self.reconnects.load(AtomicOrdering::SeqCst) as u64
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
impl SqlSession for FakeSession {
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        self.fake
            .seen
            .lock()
            .unwrap()
            .push((stmt.to_string(), opts.params.clone()));
        let reconnect = self.fake.reconnect_on.lock().unwrap().clone();
        if reconnect.is_some_and(|text| stmt.contains(&text)) {
            self.reconnects.fetch_add(1, AtomicOrdering::SeqCst);
        }
        let fail = self.fake.fail.lock().unwrap().clone();
        if let Some((text, e)) = fail {
            if stmt.contains(&text) {
                return Err(e);
            }
        }
        let sets = self.fake.sets_ddl.lock().unwrap().clone();
        if let Some((text, ddl)) = sets {
            if stmt.contains(&text) {
                *self.fake.ddl.lock().unwrap() = ddl;
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
        let rows = self
            .fake
            .answers
            .lock()
            .unwrap()
            .iter()
            .find(|(prefix, _)| stmt.starts_with(prefix.as_str()))
            .map(|(_, rows)| rows.clone());
        if let Some(rows) = rows {
            sink.rows(rows);
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
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let hook = self.fake.on_list.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        Ok(self
            .fake
            .objects
            .lock()
            .unwrap()
            .get(&kind)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|name| ObjectSummary {
                name,
                kind,
                comment: None,
                row_estimate: None,
            })
            .collect())
    }
    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }
    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Err(DbError::Internal {
            message: "not served by this fake".into(),
        })
    }
    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        let hook = {
            let mut slot = self.fake.on_ddl.lock().unwrap();
            match slot.take() {
                Some((0, hook)) => Some(hook),
                Some((n, hook)) => {
                    *slot = Some((n - 1, hook));
                    None
                }
                None => None,
            }
        };
        if let Some(hook) = hook {
            hook();
        }
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
    let sm = SessionManager::new(
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
    let sm = Arc::new(sm);
    sm.open(conn, None).await.unwrap();
    Fixture {
        sm,
        fake,
        conn,
        _dir: dir,
    }
}

fn scope(f: &Fixture) -> Scope {
    Scope {
        database: "shop".into(),
        schema: (f.fake.server.kind == DbKind::Postgres).then(|| "public".into()),
    }
}

async fn open(f: &Fixture, kind: ObjectKind, name: Option<&str>) -> DdlEditOpen {
    f.sm.ddl_edit_open(f.conn, scope(f), kind, name.map(Into::into), None)
        .await
        .unwrap()
}

/// A PostgreSQL fixture whose function `public.f` has one `pg_proc` row.
async fn pg_function(server: ServerInfo) -> Fixture {
    let fake = Fake::new(server, PG_FUNC);
    fake.answer(
        PG_ROUTINE_SQL,
        vec![vec!["1", "f", "x integer", "false", "true"]],
    );
    fixture(fake, None, false).await
}

#[tokio::test]
async fn open_returns_server_ddl_and_strategies() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    assert_eq!(o.text, MYSQL_PROC);
    assert_eq!(o.strategies, [Strategy::Recreate]);
    assert_eq!(o.strategy, Some(Strategy::Recreate));
    assert!(!o.notes.is_empty());
    assert_eq!(o.blocked_reason, None);
    assert!(!o.read_only);
    assert_eq!(
        o.object,
        Some(ObjectRef {
            database: "shop".into(),
            schema: None,
            name: "p".into(),
            kind: ObjectKind::Procedure,
        })
    );
    assert_eq!(o.target.name.as_deref(), Some("p"));

    for server in [pg17(), pg12()] {
        let f = pg_function(server).await;
        let o = open(&f, ObjectKind::Function, Some("f")).await;
        assert_eq!(o.text, PG_FUNC);
        assert_eq!(o.strategies, [Strategy::Replace, Strategy::Recreate]);
        assert_eq!(o.strategy, Some(Strategy::Replace));
        assert!(!o.notes.is_empty());
        assert_eq!(o.blocked_reason, None);
        // The catalog read is parameterised.
        assert_eq!(f.fake.params_of(PG_ROUTINE_SQL), [vec!["public", "f"]]);
        let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
        assert_eq!(edit.facts.identity_args.as_deref(), Some("x integer"));
        assert!(edit.facts.has_grants && !edit.facts.has_comment);
    }
}

#[tokio::test]
async fn open_reads_trigger_table_from_the_catalog() {
    let fake = Fake::new(
        mysql8(),
        "CREATE DEFINER=`root`@`%` TRIGGER `t` BEFORE INSERT ON `orders` FOR EACH ROW SET NEW.a = 1",
    );
    fake.answer(MYSQL_TRIGGER_SQL, vec![vec!["orders", "BEFORE", "INSERT"]]);
    fake.answer(MYSQL_SAME_TIMING_SQL, vec![vec!["2"]]);
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    assert_eq!(o.target.table.as_deref(), Some("orders"));
    assert_eq!(
        f.fake.params_of(MYSQL_SAME_TIMING_SQL),
        [vec!["shop", "orders", "BEFORE", "INSERT", "t"]]
    );
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert_eq!(edit.facts.timing_event.as_deref(), Some("BEFORE INSERT"));
    assert_eq!(edit.facts.same_timing_triggers, 2);
    let p =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), o.strategy)
            .await
            .unwrap();
    assert_eq!(p.statements[0], "DROP TRIGGER `shop`.`t`");
    assert!(p.gap);
    assert!(
        p.warnings.iter().any(|w| w.contains("BEFORE INSERT")),
        "{:?}",
        p.warnings
    );
}

#[tokio::test]
async fn open_new_returns_template() {
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let o = open(&f, ObjectKind::View, None).await;
    let target = EditTarget {
        kind: ObjectKind::View,
        database: "shop".into(),
        schema: None,
        name: None,
        table: None,
    };
    assert_eq!(o.text, template(&target, &mysql8()));
    assert!(o.text.contains("new_view"), "{}", o.text);
    assert_eq!(o.target, target);
    assert_eq!(o.object, None);
    assert!(o.strategies.is_empty());
    assert_eq!(o.strategy, None);
    assert_eq!(o.blocked_reason, None);
    assert_eq!(f.sm.ddl_edits.get(o.edit_id).unwrap().fingerprint, ABSENT);
    // A new trigger keeps the table hint; other kinds drop it.
    let o =
        f.sm.ddl_edit_open(
            f.conn,
            scope(&f),
            ObjectKind::Trigger,
            None,
            Some("orders".into()),
        )
        .await
        .unwrap();
    assert_eq!(o.target.table.as_deref(), Some("orders"));
    assert!(o.text.contains("`orders`"), "{}", o.text);
    // An engine without the kind refuses it.
    let e =
        f.sm.ddl_edit_open(f.conn, scope(&f), ObjectKind::MaterializedView, None, None)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Config { field: Some(x), .. } if x == "kind"),
        "{e:?}"
    );
}

#[tokio::test]
async fn open_validates_scope() {
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let e =
        f.sm.ddl_edit_open(
            f.conn,
            Scope {
                database: "shop".into(),
                schema: Some("x".into()),
            },
            ObjectKind::View,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::Config { .. }), "{e:?}");
    let f = pg_function(pg17()).await;
    let e =
        f.sm.ddl_edit_open(
            f.conn,
            Scope {
                database: "shop".into(),
                schema: None,
            },
            ObjectKind::View,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::Config { .. }), "{e:?}");
}

#[tokio::test]
async fn open_read_only_is_view_only() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, true).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    assert!(o.read_only);
    assert_eq!(o.text, MYSQL_PROC);
    assert_eq!(
        o.blocked_reason.as_deref(),
        Some("This connection is read-only.")
    );
}

#[tokio::test]
async fn open_blocks_ddl_not_shown() {
    let fake = Fake::new(mysql8(), MYSQL_PROC);
    *fake.ddl_error.lock().unwrap() = Some(DbError::Server {
        code: Some("1227".into()),
        sqlstate: Some("42000".into()),
        message: "denied".into(),
        position: None,
    });
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    assert_eq!(o.text, "");
    assert_eq!(
        o.blocked_reason.as_deref(),
        Some("The server does not show this definition to this user.")
    );
    let e =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), None)
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Config { .. }), "{e:?}");
}

#[tokio::test]
async fn open_blocks_overloaded_routine() {
    let fake = Fake::new(pg17(), PG_FUNC);
    fake.answer(
        PG_ROUTINE_SQL,
        vec![
            vec!["2", "f", "x integer", "false", "false"],
            vec!["2", "f", "x text", "false", "false"],
        ],
    );
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    assert_eq!(
        o.blocked_reason.as_deref(),
        Some("2 routines are named f in public; editing overloaded routines is not supported. Use a query tab.")
    );
    let e =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Replace))
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Config { message, .. } if message.contains("overloaded")),
        "{e:?}"
    );
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());

    // A trigger name used on several tables.
    let fake = Fake::new(
        pg17(),
        "CREATE TRIGGER t BEFORE UPDATE ON public.a FOR EACH ROW EXECUTE FUNCTION public.tf()",
    );
    fake.answer(
        PG_TRIGGER_SQL,
        vec![vec!["a", "100", "false"], vec!["b", "101", "false"]],
    );
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    assert_eq!(
        o.blocked_reason.as_deref(),
        Some("Trigger t exists on 2 tables in public; edit it with a query tab.")
    );
}

#[tokio::test]
async fn preview_blocks_read_only() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, true).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let before = f.fake.sent().len();
    let e =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Blocked { .. }), "{e:?}");
    assert_eq!(f.fake.sent().len(), before);
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());
}

#[tokio::test]
async fn preview_refuses_other_object_text() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let other = MYSQL_PROC.replace("`p`", "`q`");
    let e =
        f.sm.ddl_edit_preview(o.edit_id, other, o.strategy)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Config { field: Some(x), .. } if x == "text"),
        "{e:?}"
    );
    // A second statement after the body is refused too.
    let two = format!("{MYSQL_PROC};\nDROP TABLE orders");
    let e =
        f.sm.ddl_edit_preview(o.edit_id, two, o.strategy)
            .await
            .unwrap_err();
    assert!(matches!(&e, DbError::Config { .. }), "{e:?}");
    assert!(f.fake.only_reads(), "{:?}", f.fake.sent());
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());
}

#[tokio::test]
async fn preview_detects_changed_object() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    *f.fake.ddl.lock().unwrap() = MYSQL_PROC.replace("SELECT x", "SELECT x + 1");
    let e =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::PlanStale { reason } if reason == "The object changed since the editor opened it."),
        "{e:?}"
    );
    assert!(f.fake.only_reads(), "{:?}", f.fake.sent());
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());

    // Catalog facts count too: a grant added elsewhere.
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    f.fake.answers.lock().unwrap().clear();
    f.fake.answer(
        PG_ROUTINE_SQL,
        vec![vec!["1", "f", "x integer", "false", "false"]],
    );
    let e =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), o.strategy)
            .await
            .unwrap_err();
    assert!(matches!(&e, DbError::PlanStale { .. }), "{e:?}");
}

#[tokio::test]
async fn preview_new_refuses_existing_name() {
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let o = open(&f, ObjectKind::View, None).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Table, vec!["new_view".into()]);
    let e =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Config { field: Some(x), message } if x == "text" && message == "An object named new_view already exists."),
        "{e:?}"
    );
    f.fake.objects.lock().unwrap().clear();
    let p =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap();
    assert_eq!(p.statements, std::slice::from_ref(&o.text));
    let plan_target = lock(&f.sm.ddl_edits.plans)
        .get(&p.plan_id)
        .map(|p| (p.new, p.target.name.clone()))
        .unwrap();
    assert_eq!(plan_target, (true, Some("new_view".into())));

    // PostgreSQL: functions and procedures share a namespace.
    let f = fixture(Fake::new(pg17(), ""), None, false).await;
    let o = open(&f, ObjectKind::Function, None).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Procedure, vec!["new_function".into()]);
    let e =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap_err();
    assert!(matches!(&e, DbError::Config { .. }), "{e:?}");
    // MySQL functions and procedures do not.
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let o = open(&f, ObjectKind::Function, None).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Procedure, vec!["new_function".into()]);
    f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn preview_mysql_procedure_has_rehearsal_and_gap() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, format!("{MYSQL_PROC};"), None)
            .await
            .unwrap();
    assert_eq!(p.statements.len(), 4, "{:?}", p.statements);
    assert!(
        p.statements[0].contains("PROCEDURE `_dnt_p4c_check_"),
        "{:?}",
        p.statements
    );
    let rehearsal = p.statements[1].strip_prefix("DROP PROCEDURE ").unwrap();
    assert!(rehearsal.starts_with("`_dnt_p4c_check_"), "{rehearsal}");
    assert_eq!(rehearsal.len(), "`_dnt_p4c_check_".len() + 8 + 1);
    assert!(p.statements[0].contains(rehearsal));
    assert_eq!(p.statements[2], "DROP PROCEDURE `shop`.`p`");
    assert_eq!(p.statements[3], MYSQL_PROC);
    assert!(p.gap);
    assert!(!p.transactional);
    assert!(p.checks.is_empty());
    assert_eq!(p.confirm_name, None);
    assert!(!p.warnings.is_empty());
    // Previewing sends nothing but reads; the plan holds exactly the previewed statements.
    assert!(f.fake.only_reads(), "{:?}", f.fake.sent());
    let plans = lock(&f.sm.ddl_edits.plans);
    let plan = plans.get(&p.plan_id).unwrap();
    assert_eq!(plan.statements, p.statements);
    assert_eq!(plan.edit_id, o.edit_id);
    assert_eq!(plan.base_text, MYSQL_PROC);
    assert!(!plan.new);
}

#[tokio::test]
async fn preview_pg_function_replace_has_check() {
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), o.strategy)
            .await
            .unwrap();
    assert_eq!(p.statements, [PG_FUNC]);
    assert!(p.transactional && !p.gap);
    assert_eq!(p.checks.len(), 1, "{:?}", p.checks);
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Recreate))
            .await
            .unwrap();
    assert_eq!(
        p.statements[0],
        "DROP FUNCTION \"public\".\"f\"(x integer) RESTRICT"
    );
    assert!(
        p.warnings.iter().any(|w| w.contains("grants")),
        "{:?}",
        p.warnings
    );
}

#[tokio::test]
async fn production_and_staging_gap_confirm_names() {
    let cases = [
        (
            mysql8(),
            Some(EnvTag::Production),
            ObjectKind::Procedure,
            Some("p"),
        ),
        (
            mysql8(),
            Some(EnvTag::Staging),
            ObjectKind::Procedure,
            Some("p"),
        ),
        (
            mariadb(),
            Some(EnvTag::Staging),
            ObjectKind::Procedure,
            None,
        ),
        (
            mariadb(),
            Some(EnvTag::Production),
            ObjectKind::Procedure,
            Some("p"),
        ),
        (mysql8(), Some(EnvTag::Staging), ObjectKind::View, None),
        (
            mysql8(),
            Some(EnvTag::Production),
            ObjectKind::View,
            Some("v"),
        ),
        (mysql8(), Some(EnvTag::Local), ObjectKind::Procedure, None),
        (mysql8(), None, ObjectKind::Procedure, None),
    ];
    for (server, env, kind, expected) in cases {
        let (ddl, name) = if kind == ObjectKind::View {
            (MYSQL_VIEW, "v")
        } else {
            (MYSQL_PROC, "p")
        };
        let variant = server.variant;
        let f = fixture(Fake::new(server, ddl), env, false).await;
        let o = open(&f, kind, Some(name)).await;
        let p =
            f.sm.ddl_edit_preview(o.edit_id, ddl.into(), o.strategy)
                .await
                .unwrap();
        assert_eq!(
            p.confirm_name.as_deref(),
            expected,
            "{variant:?} {env:?} {kind:?}"
        );
    }
    // A new object confirms with the name from the text.
    let f = fixture(Fake::new(mysql8(), ""), Some(EnvTag::Production), false).await;
    let o = open(&f, ObjectKind::View, None).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap();
    assert_eq!(p.confirm_name.as_deref(), Some("new_view"));
}

#[tokio::test]
async fn plan_ttl_and_capacity() {
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let o = open(&f, ObjectKind::View, Some("v")).await;
    let preview = || {
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
    };
    let p = preview().await.unwrap();
    assert_eq!(u64::from(p.expires_in_seconds), f.sm.plan_ttl.as_secs());
    {
        let plans = lock(&f.sm.ddl_edits.plans);
        let left = plans[&p.plan_id].expires - Instant::now();
        assert!(left <= f.sm.plan_ttl && left > f.sm.plan_ttl - Duration::from_secs(5));
    }
    for _ in 1..MAX_PLANS {
        preview().await.unwrap();
    }
    assert!(matches!(preview().await, Err(DbError::Busy)));
    // Expired plans make room.
    lock(&f.sm.ddl_edits.plans)
        .get_mut(&p.plan_id)
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    preview().await.unwrap();
    // Closing the edit forgets its plans.
    f.sm.ddl_edit_close(o.edit_id).await.unwrap();
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());
    assert!(matches!(preview().await, Err(DbError::Config { .. })));

    // Open edits are bounded too.
    for _ in 0..MAX_EDITS {
        open(&f, ObjectKind::View, None).await;
    }
    let e =
        f.sm.ddl_edit_open(f.conn, scope(&f), ObjectKind::View, None, None)
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Busy), "{e:?}");
    // Closing the connection drops its edits.
    f.sm.close(f.conn).await;
    assert!(lock(&f.sm.ddl_edits.edits).is_empty());
}

#[tokio::test]
async fn cancel_before_apply_is_remembered() {
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let id = Uuid::new_v4();
    f.sm.ddl_edit_cancel(id).await.unwrap();
    let (_guard, stop) = f.sm.ddl_edits.operations.register(id, f.conn).unwrap();
    assert!(stop.is_stopped());
}

// ---- Task 4 review fixes ----

#[tokio::test]
async fn preview_new_name_check_ignores_case_on_mysql() {
    for (kind, listed) in [
        (ObjectKind::View, (ObjectKind::Table, "NEW_VIEW")),
        (
            ObjectKind::Procedure,
            (ObjectKind::Procedure, "New_Procedure"),
        ),
        (ObjectKind::Trigger, (ObjectKind::Trigger, "NEW_TRIGGER")),
    ] {
        let f = fixture(Fake::new(mysql8(), ""), None, false).await;
        let o = open(&f, kind, None).await;
        f.fake
            .objects
            .lock()
            .unwrap()
            .insert(listed.0, vec![listed.1.into()]);
        let e =
            f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
                .await
                .unwrap_err();
        assert!(
            matches!(&e, DbError::Config { message, .. } if message.contains("already exists")),
            "{kind:?}: {e:?}"
        );
    }
    // PostgreSQL names compare exactly.
    let f = fixture(Fake::new(pg17(), ""), None, false).await;
    let o = open(&f, ObjectKind::Function, None).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Function, vec!["New_Function".into()]);
    f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn preview_stores_no_plan_for_an_edit_closed_meanwhile() {
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let o = open(&f, ObjectKind::View, None).await;
    let sm = Arc::clone(&f.sm);
    let id = o.edit_id;
    // The edit is closed while the preview reads the catalog.
    *f.fake.on_list.lock().unwrap() = Some(Box::new(move || {
        lock(&sm.ddl_edits.edits).remove(&id);
    }));
    let e =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Config { field: Some(x), .. } if x == "edit_id"),
        "{e:?}"
    );
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());
}

#[tokio::test]
async fn open_checks_capacity_before_opening_a_session() {
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    for _ in 0..MAX_EDITS {
        open(&f, ObjectKind::View, None).await;
    }
    let before = f.fake.connects.load(AtomicOrdering::SeqCst);
    let e =
        f.sm.ddl_edit_open(f.conn, scope(&f), ObjectKind::View, None, None)
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Busy), "{e:?}");
    assert_eq!(f.fake.connects.load(AtomicOrdering::SeqCst), before);
}

#[tokio::test]
async fn open_blocks_routine_of_another_kind() {
    let fake = Fake::new(pg17(), PG_FUNC);
    fake.answer(
        PG_ROUTINE_SQL,
        vec![vec!["1", "p", "x integer", "false", "false"]],
    );
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    assert_eq!(
        o.blocked_reason.as_deref(),
        Some("f in public is a procedure, not a function; edit it with a query tab.")
    );
    let fake = Fake::new(pg17(), PG_FUNC);
    fake.answer(
        PG_ROUTINE_SQL,
        vec![vec!["1", "a", "x integer", "false", "false"]],
    );
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    assert!(
        o.blocked_reason
            .as_deref()
            .is_some_and(|r| r.contains("aggregate function")),
        "{:?}",
        o.blocked_reason
    );
}

// ---- Apply (spec §6) ----

const MYSQL_TRIGGER: &str =
    "CREATE DEFINER=`root`@`%` TRIGGER `t` BEFORE INSERT ON `orders` FOR EACH ROW SET NEW.a = 1";
const PG_FUNC_AFTER: &str = "CREATE OR REPLACE FUNCTION public.f(x integer)\n RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT x + 1 $function$";

fn no_progress(_: &str) {}

async fn apply(f: &Fixture, plan: Uuid, name: Option<&str>) -> Result<DdlEditReport, DbError> {
    f.sm.ddl_edit_apply(
        plan,
        true,
        name.map(Into::into),
        Uuid::new_v4(),
        &no_progress,
    )
    .await
}

fn history(f: &Fixture) -> Vec<HistoryEntry> {
    f.sm.repo
        .history_list(&HistoryFilter {
            connection_id: Some(f.conn),
            search: None,
            origin: None,
            limit: 10,
        })
        .unwrap()
}

fn server_error(code: &str) -> DbError {
    DbError::Server {
        code: Some(code.into()),
        sqlstate: Some("42000".into()),
        message: "refused".into(),
        position: None,
    }
}

/// The plan's post-checks (PostgreSQL).
fn checks_of(f: &Fixture, plan: Uuid) -> Vec<PostCheck> {
    lock(&f.sm.ddl_edits.plans)[&plan].checks.clone()
}

/// What was sent from `mark` on, without reads other than `keep` (catalog reads,
/// the SchemaCache refresh after the DDL).
fn sent_since(f: &Fixture, mark: usize, keep: &[String]) -> Vec<String> {
    f.fake.sent()[mark..]
        .iter()
        .filter(|s| !s.starts_with("SELECT") || keep.contains(s))
        .cloned()
        .collect()
}

#[tokio::test]
async fn apply_sends_exact_plan_statements() {
    // PostgreSQL: lock bound, BEGIN, statements, post-check read, COMMIT.
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Replace))
            .await
            .unwrap();
    let checks = checks_of(&f, p.plan_id);
    assert_eq!(checks.len(), 1);
    f.fake.answer(&checks[0].sql, vec![vec!["1:f"]]);
    let mark = f.fake.sent().len();
    let stages = Mutex::new(Vec::new());
    let r =
        f.sm.ddl_edit_apply(p.plan_id, true, None, Uuid::new_v4(), &|s| {
            stages.lock().unwrap().push(s.to_string())
        })
        .await
        .unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Changed);
    assert_eq!(r.statements_sent, 1);
    // PostgreSQL reports no per-statement progress: the transaction is all or nothing.
    assert!(r.executed.is_empty());
    assert_eq!(r.failed, None);
    assert!(r.error.is_none());
    let mut expected = vec![
        "SET lock_timeout = '5s'".to_string(),
        "BEGIN".into(),
        "SET LOCAL standard_conforming_strings = on".into(),
    ];
    expected.extend(p.statements.iter().cloned());
    expected.push(checks[0].sql.clone());
    expected.push("COMMIT".into());
    assert_eq!(sent_since(&f, mark, &[checks[0].sql.clone()]), expected);
    assert_eq!(f.fake.params_of(&checks[0].sql), [checks[0].params.clone()]);
    assert!(stages
        .lock()
        .unwrap()
        .contains(&"executing 1/1".to_string()));

    // MySQL: lock bound, then the statements one by one.
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    let mark = f.fake.sent().len();
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    assert_eq!(r.statements_sent, 4);
    let mut expected = vec!["SET SESSION lock_wait_timeout = 5".to_string()];
    expected.extend(p.statements.iter().cloned());
    assert_eq!(sent_since(&f, mark, &[]), expected);
}

#[tokio::test]
async fn plan_is_single_use() {
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let o = open(&f, ObjectKind::View, Some("v")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
            .await
            .unwrap();
    apply(&f, p.plan_id, None).await.unwrap();
    let mark = f.fake.sent().len();
    assert!(matches!(
        apply(&f, p.plan_id, None).await,
        Err(DbError::PlanExpired)
    ));
    // An expired plan sends nothing either.
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
            .await
            .unwrap();
    lock(&f.sm.ddl_edits.plans)
        .get_mut(&p.plan_id)
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    assert!(matches!(
        apply(&f, p.plan_id, None).await,
        Err(DbError::PlanExpired)
    ));
    assert_eq!(f.fake.sent().len(), mark);
}

#[tokio::test]
async fn production_requires_exact_name() {
    let f = fixture(
        Fake::new(mysql8(), MYSQL_VIEW),
        Some(EnvTag::Production),
        false,
    )
    .await;
    let o = open(&f, ObjectKind::View, Some("v")).await;
    let preview = || async {
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
            .await
            .unwrap()
    };
    let mark = f.fake.sent().len();
    for name in [None, Some("V"), Some(" v")] {
        let p = preview().await;
        assert_eq!(p.confirm_name.as_deref(), Some("v"));
        let e = apply(&f, p.plan_id, name).await.unwrap_err();
        assert!(
            matches!(e, DbError::ConfirmationRequired { .. }),
            "{name:?}: {e:?}"
        );
        // The plan was consumed.
        assert!(matches!(
            apply(&f, p.plan_id, Some("v")).await,
            Err(DbError::PlanExpired)
        ));
    }
    // Not confirmed at all.
    let p = preview().await;
    let e =
        f.sm.ddl_edit_apply(
            p.plan_id,
            false,
            Some("v".into()),
            Uuid::new_v4(),
            &no_progress,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, DbError::ConfirmationRequired { .. }), "{e:?}");
    assert!(f.fake.sent()[mark..]
        .iter()
        .all(|s| !s.contains("VIEW") && !s.starts_with("SET")));
    let p = preview().await;
    let r = apply(&f, p.plan_id, Some("v")).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
}

#[tokio::test]
async fn stale_object_sends_nothing() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    *f.fake.ddl.lock().unwrap() = MYSQL_PROC.replace("SELECT x", "SELECT x + 1");
    let mark = f.fake.sent().len();
    let e = apply(&f, p.plan_id, None).await.unwrap_err();
    assert!(
        matches!(&e, DbError::PlanStale { reason } if reason == "The object changed since the editor opened it."),
        "{e:?}"
    );
    let sent = f.fake.sent()[mark..].to_vec();
    assert!(sent.iter().all(|s| !p.statements.contains(s)), "{sent:?}");
    assert!(history(&f).is_empty());

    // A new object whose name was taken meanwhile.
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let o = open(&f, ObjectKind::View, None).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap();
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::View, vec!["new_view".into()]);
    let e = apply(&f, p.plan_id, None).await.unwrap_err();
    assert!(matches!(&e, DbError::PlanStale { .. }), "{e:?}");
    assert!(!f.fake.sent().contains(&p.statements[0]));
}

#[tokio::test]
async fn pg_recreate_owner_failure_is_explained() {
    let fake = Fake::new(pg17(), PG_FUNC);
    fake.answer(
        PG_ROUTINE_SQL,
        vec![vec!["1", "f", "x integer", "false", "false", "app_owner"]],
    );
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    assert_eq!(
        f.sm.ddl_edits
            .get(o.edit_id)
            .unwrap()
            .facts
            .owner
            .as_deref(),
        Some("app_owner")
    );
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Recreate))
            .await
            .unwrap();
    assert_eq!(
        p.statements.last().map(String::as_str),
        Some("ALTER FUNCTION \"public\".\"f\" OWNER TO \"app_owner\"")
    );
    f.fake.fail_on("OWNER TO", server_error("42501"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::RolledBack, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unchanged);
    assert_eq!(r.warnings, [OWNER_NOT_RESTORED], "{r:?}");
}

#[tokio::test]
async fn pg_failure_rolls_back() {
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Recreate))
            .await
            .unwrap();
    assert_eq!(p.statements.len(), 2);
    f.fake
        .fail_on("CREATE OR REPLACE FUNCTION", server_error("42P13"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::RolledBack, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unchanged);
    assert_eq!(r.statements_sent, 2);
    // The DROP was rolled back with the rest: nothing is reported as executed.
    assert!(r.executed.is_empty(), "{:?}", r.executed);
    assert_eq!(r.failed, None);
    assert!(!r.error.as_ref().unwrap().outcome_unknown);
    assert_eq!(r.restore_text, None);
    let sent = f.fake.sent();
    assert!(sent.contains(&"ROLLBACK".to_string()), "{sent:?}");
    assert!(!sent.contains(&"COMMIT".to_string()), "{sent:?}");
    let h = history(&f);
    assert_eq!(h.len(), 1);
    assert!(!h[0].outcome_unknown);
    // The edit keeps its base.
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert_eq!(edit.base_text, PG_FUNC);
    assert!(edit.object.is_some());
}

#[tokio::test]
async fn pg_post_check_failure_rolls_back() {
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Replace))
            .await
            .unwrap();
    let check = checks_of(&f, p.plan_id).remove(0);
    f.fake.answer(&check.sql, vec![vec!["2:f"]]);
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::RolledBack, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unchanged);
    let error = r.error.unwrap();
    assert_eq!(error.field.as_deref(), Some("check"));
    assert_eq!(error.message, check.failure);
    let sent = f.fake.sent();
    assert!(sent.contains(&"ROLLBACK".to_string()), "{sent:?}");
    assert!(!sent.contains(&"COMMIT".to_string()), "{sent:?}");
}

#[tokio::test]
async fn pg_commit_lost_is_unknown() {
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Replace))
            .await
            .unwrap();
    let check = checks_of(&f, p.plan_id).remove(0);
    f.fake.answer(&check.sql, vec![vec!["1:f"]]);
    f.fake.fail_on(
        "COMMIT",
        DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: true,
            message: "lost".into(),
        },
    );
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Unknown, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unknown);
    assert!(r.error.as_ref().unwrap().outcome_unknown);
    let h = history(&f);
    assert_eq!(h.len(), 1);
    assert!(h[0].outcome_unknown);
}

#[tokio::test]
async fn mysql_failure_after_drop_is_partial_with_restore() {
    let fake = Fake::new(mysql8(), MYSQL_TRIGGER);
    fake.answer(MYSQL_TRIGGER_SQL, vec![vec!["orders", "BEFORE", "INSERT"]]);
    fake.answer(MYSQL_SAME_TIMING_SQL, vec![vec!["0"]]);
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap();
    assert_eq!(p.statements, ["DROP TRIGGER `shop`.`t`", MYSQL_TRIGGER]);
    // The CREATE fails; `list_objects` then lacks the trigger.
    f.fake.fail_on("CREATE DEFINER", server_error("1064"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Partial, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Missing);
    assert_eq!(r.executed, ["DROP TRIGGER `shop`.`t`"]);
    assert_eq!(r.failed.as_deref(), Some(MYSQL_TRIGGER));
    assert_eq!(r.restore_text.as_deref(), Some(MYSQL_TRIGGER));
    assert_eq!(r.statements_sent, 2);
    assert!(!r.error.as_ref().unwrap().outcome_unknown);
    // History: a failure, not unknown.
    let h = history(&f);
    assert_eq!(h.len(), 1);
    assert!(!h[0].outcome_unknown);
    assert_eq!(h[0].status, dnt_store::history::HistoryStatus::Error);
    // The edit is now a new one of the same name.
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert_eq!(edit.object, None);
    assert_eq!(edit.target.name, None);
    assert_eq!(edit.fingerprint, ABSENT);
    // The restore is an ordinary create.
    *f.fake.fail.lock().unwrap() = None;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, r.restore_text.unwrap(), None)
            .await
            .unwrap();
    assert_eq!(p.statements, [MYSQL_TRIGGER]);
    assert!(lock(&f.sm.ddl_edits.plans)[&p.plan_id].new);
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
}

#[tokio::test]
async fn mysql_rehearsal_failure_is_not_applied() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Procedure, vec!["p".into()]);
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    f.fake.fail_on(&p.statements[0], server_error("1064"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::NotApplied, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unchanged);
    assert!(r.executed.is_empty());
    assert_eq!(r.failed.as_deref(), Some(p.statements[0].as_str()));
    assert_eq!(r.statements_sent, 1);
    assert!(
        !r.warnings.iter().any(|w| w.contains("left behind")),
        "{:?}",
        r.warnings
    );
    let sent = f.fake.sent();
    assert!(!sent.iter().any(|s| s.starts_with("DROP")), "{sent:?}");
    assert_eq!(f.sm.ddl_edits.get(o.edit_id).unwrap().base_text, MYSQL_PROC);

    // The rehearsal routine was created, but its DROP failed: it is named with its DROP.
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    f.fake.fail_on(&p.statements[1], server_error("1370"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::NotApplied, "{r:?}");
    let rehearsal = p.statements[1]
        .strip_prefix("DROP PROCEDURE `")
        .unwrap()
        .trim_end_matches('`');
    assert!(
        r.warnings.contains(&format!(
            "A check routine `{rehearsal}` was left behind; drop it with: DROP PROCEDURE `{rehearsal}`"
        )),
        "{:?}",
        r.warnings
    );
}

#[tokio::test]
async fn mysql_rehearsal_create_unknown_warns() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Procedure, vec!["p".into()]);
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    // The rehearsal CREATE was sent; the connection was lost before its result.
    f.fake.fail_on(
        &p.statements[0],
        DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: true,
            message: "lost".into(),
        },
    );
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.statements_sent, 1);
    assert!(r.executed.is_empty());
    let rehearsal = p.statements[1]
        .strip_prefix("DROP PROCEDURE `")
        .unwrap()
        .trim_end_matches('`');
    assert!(
        r.warnings.contains(&format!(
            "A check routine `{rehearsal}` may have been left behind; drop it with: DROP PROCEDURE IF EXISTS `{rehearsal}`"
        )),
        "{:?}",
        r.warnings
    );
}

#[tokio::test]
async fn mysql_reconnect_before_last_statement_is_applied() {
    let fake = Fake::new(mysql8(), MYSQL_TRIGGER);
    fake.answer(MYSQL_TRIGGER_SQL, vec![vec!["orders", "BEFORE", "INSERT"]]);
    fake.answer(MYSQL_SAME_TIMING_SQL, vec![vec!["0"]]);
    let f = fixture(fake, None, false).await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap();
    assert_eq!(p.statements.len(), 2);
    // The last statement runs after a silent reconnect, and completes.
    *f.fake.reconnect_on.lock().unwrap() = Some(p.statements[1].clone());
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    assert!(r.error.is_none(), "{:?}", r.error);
    assert_eq!(r.executed, p.statements);
    assert_eq!(
        r.warnings,
        ["The connection was re-established during the apply; every statement completed."]
    );
    assert_eq!(r.text.as_deref(), Some(MYSQL_TRIGGER));

    // A reconnect before an earlier statement still stops the apply.
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap();
    *f.fake.reconnect_on.lock().unwrap() = Some(p.statements[0].clone());
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_ne!(r.outcome, EditOutcome::Applied, "{r:?}");
    assert_eq!(r.executed, p.statements[..1]);
}

#[tokio::test]
async fn success_updates_base_and_turns_new_into_edit() {
    // A new view becomes an edit of the created view.
    const CREATED: &str = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `new_view` AS select 1 AS `id`";
    let f = fixture(Fake::new(mysql8(), CREATED), None, false).await;
    let o = open(&f, ObjectKind::View, None).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap();
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    let object = ObjectRef {
        database: "shop".into(),
        schema: None,
        name: "new_view".into(),
        kind: ObjectKind::View,
    };
    assert_eq!(r.object.as_ref(), Some(&object));
    assert_eq!(r.text.as_deref(), Some(CREATED));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert_eq!(edit.object, Some(object));
    assert_eq!(edit.target.name.as_deref(), Some("new_view"));
    assert_eq!(edit.base_text, CREATED);
    assert_ne!(edit.fingerprint, ABSENT);
    let p =
        f.sm.ddl_edit_preview(o.edit_id, CREATED.into(), Some(Strategy::Replace))
            .await
            .unwrap();
    assert!(!lock(&f.sm.ddl_edits.plans)[&p.plan_id].new);
    assert!(p.statements[0].starts_with("CREATE OR REPLACE"));

    // An edit takes the server's DDL after the change as its new base.
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC_AFTER.into(), Some(Strategy::Replace))
            .await
            .unwrap();
    let check = checks_of(&f, p.plan_id).remove(0);
    f.fake.answer(&check.sql, vec![vec!["1:f"]]);
    *f.fake.sets_ddl.lock().unwrap() = Some(("COMMIT".into(), PG_FUNC_AFTER.into()));
    let before = f.sm.ddl_edits.get(o.edit_id).unwrap().fingerprint;
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    assert_eq!(r.text.as_deref(), Some(PG_FUNC_AFTER));
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert_eq!(edit.base_text, PG_FUNC_AFTER);
    assert_ne!(edit.fingerprint, before);

    // A failed re-read keeps Applied and asks to reopen.
    let f = fixture(Fake::new(mysql8(), ""), None, false).await;
    let o = open(&f, ObjectKind::View, None).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, o.text.clone(), None)
            .await
            .unwrap();
    *f.fake.ddl_error.lock().unwrap() = Some(DbError::Internal {
        message: "gone".into(),
    });
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    assert_eq!(
        r.warnings,
        ["The definition could not be reloaded; reopen the editor."]
    );
    assert_eq!(r.text, None);
    // The created object is still named, for the editor to reopen it.
    assert_eq!(r.object.as_ref().map(|o| o.name.as_str()), Some("new_view"));
}

#[tokio::test]
async fn history_uses_object_action_and_delimiter_form() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    apply(&f, p.plan_id, None).await.unwrap();
    let h = history(&f);
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(h[0].origin, HISTORY_ORIGIN);
    assert_eq!(h[0].origin, HistoryOrigin::ObjectAction);
    assert_eq!(h[0].kind, "ddl");
    assert_eq!(h[0].database.as_deref(), Some("shop"));
    assert_eq!(
        h[0].sql_text,
        format!(
            "DELIMITER $$\n{}$$\n{}$$\n{}$$\n{}$$\nDELIMITER ;",
            p.statements[0], p.statements[1], p.statements[2], p.statements[3]
        )
    );

    // A MySQL view and every PostgreSQL plan are joined with `;`.
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let o = open(&f, ObjectKind::View, Some("v")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
            .await
            .unwrap();
    apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(history(&f)[0].sql_text, p.statements.join(";\n"));
    let statements = [
        "DROP VIEW x".to_string(),
        "CREATE VIEW x AS SELECT 1".into(),
    ];
    assert_eq!(
        super::apply::history_sql(SqlKind::Postgres, ObjectKind::Function, &statements),
        "DROP VIEW x;\nCREATE VIEW x AS SELECT 1"
    );

    // The delimiter is one that no statement contains.
    let body = |s: &str| format!("CREATE PROCEDURE p() BEGIN SELECT '{s}'; END");
    let history = |statements: &[String]| {
        super::apply::history_sql(SqlKind::MySql, ObjectKind::Procedure, statements)
    };
    let s = [body("$$")];
    assert_eq!(
        history(&s),
        format!("DELIMITER //\n{}//\nDELIMITER ;", s[0])
    );
    let s = [body("$$ //"), body(";;")];
    assert_eq!(
        history(&s),
        format!("DELIMITER $$1\n{}$$1\n{}$$1\nDELIMITER ;", s[0], s[1])
    );
    let s = [body("$$ // ;; $$1")];
    assert_eq!(
        history(&s),
        format!("DELIMITER $$2\n{}$$2\nDELIMITER ;", s[0])
    );
}

#[test]
fn namespaces_and_name_matching_differ_per_engine() {
    use ObjectKind::*;
    let kinds = super::namespace_kinds;
    assert_eq!(kinds(View, SqlKind::MySql), [Table, View]);
    assert_eq!(kinds(Procedure, SqlKind::MySql), [Procedure]);
    assert_eq!(
        kinds(View, SqlKind::Postgres),
        [Table, View, MaterializedView]
    );
    assert_eq!(
        kinds(MaterializedView, SqlKind::Postgres),
        [Table, View, MaterializedView]
    );
    assert_eq!(kinds(Procedure, SqlKind::Postgres), [Function, Procedure]);
    assert_eq!(kinds(Trigger, SqlKind::Postgres), [Trigger]);
    assert!(super::same_catalog_name(SqlKind::MySql, "Orders", "orders"));
    assert!(!super::same_catalog_name(
        SqlKind::Postgres,
        "Orders",
        "orders"
    ));
    assert!(super::same_catalog_name(
        SqlKind::Postgres,
        "orders",
        "orders"
    ));
}

#[tokio::test]
async fn open_checks_the_scope_per_engine() {
    let message = |e: DbError| match e {
        DbError::Config { field, message } => (field, message),
        other => panic!("{other:?}"),
    };
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let bad = Scope {
        database: "shop".into(),
        schema: Some("public".into()),
    };
    let e =
        f.sm.ddl_edit_open(f.conn, bad, ObjectKind::View, Some("v".into()), None)
            .await
            .unwrap_err();
    assert_eq!(
        message(e),
        (
            Some("scope".into()),
            "MySQL has no schemas inside a database.".into()
        )
    );
    let f = fixture(Fake::new(pg17(), PG_FUNC), None, false).await;
    let bad = Scope {
        database: "shop".into(),
        schema: None,
    };
    let e =
        f.sm.ddl_edit_open(f.conn, bad, ObjectKind::Function, Some("f".into()), None)
            .await
            .unwrap_err();
    assert_eq!(
        message(e),
        (
            Some("scope".into()),
            "A schema is required on PostgreSQL.".into()
        )
    );
}

#[tokio::test]
async fn cancel_before_send_sends_nothing() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    let op = Uuid::new_v4();
    f.sm.ddl_edit_cancel(op).await.unwrap();
    let mark = f.fake.sent().len();
    let e =
        f.sm.ddl_edit_apply(p.plan_id, true, None, op, &no_progress)
            .await
            .unwrap_err();
    assert!(matches!(e, DbError::Cancelled { .. }), "{e:?}");
    assert_eq!(f.fake.sent().len(), mark);
    assert!(history(&f).is_empty());
    assert!(matches!(
        apply(&f, p.plan_id, None).await,
        Err(DbError::PlanExpired)
    ));
}

#[tokio::test]
async fn apply_blocks_read_only() {
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let o = open(&f, ObjectKind::View, Some("v")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
            .await
            .unwrap();
    let mut cfg = f.sm.repo.get(f.conn).unwrap();
    cfg.read_only = true;
    f.sm.repo
        .update(
            cfg,
            dnt_store::repo::SecretUpdate {
                db_password: dnt_store::repo::SecretChange::Keep,
                ssh_password: dnt_store::repo::SecretChange::Keep,
                ssh_key_passphrase: dnt_store::repo::SecretChange::Keep,
            },
        )
        .unwrap();
    let mark = f.fake.sent().len();
    let e = apply(&f, p.plan_id, None).await.unwrap_err();
    assert!(matches!(e, DbError::Blocked { .. }), "{e:?}");
    assert_eq!(f.fake.sent().len(), mark);
}

// ---- Task 5 review fixes ----

fn is_object_changed(e: &DbError) -> bool {
    matches!(e, DbError::PlanStale { reason } if reason == "The object changed since the editor opened it.")
}

/// A MySQL trigger `t` on `orders`, listed, with its catalog facts.
async fn mysql_trigger() -> Fixture {
    let fake = Fake::new(mysql8(), MYSQL_TRIGGER);
    fake.answer(MYSQL_TRIGGER_SQL, vec![vec!["orders", "BEFORE", "INSERT"]]);
    fake.answer(MYSQL_SAME_TIMING_SQL, vec![vec!["0"]]);
    fake.objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Trigger, vec!["t".into()]);
    fixture(fake, None, false).await
}

#[tokio::test]
async fn object_dropped_elsewhere_is_stale() {
    let f = fixture(Fake::new(mysql8(), MYSQL_PROC), None, false).await;
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Procedure, vec!["p".into()]);
    let o = open(&f, ObjectKind::Procedure, Some("p")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap();
    // Another session drops the procedure: SHOW CREATE answers 1305 and it is no
    // longer listed.
    *f.fake.ddl_error.lock().unwrap() = Some(server_error("1305"));
    f.fake.objects.lock().unwrap().clear();
    let mark = f.fake.sent().len();
    let e = apply(&f, p.plan_id, None).await.unwrap_err();
    assert!(is_object_changed(&e), "{e:?}");
    let sent = f.fake.sent()[mark..].to_vec();
    assert!(sent.iter().all(|s| !p.statements.contains(s)), "{sent:?}");
    assert!(history(&f).is_empty());
    let e =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap_err();
    assert!(is_object_changed(&e), "{e:?}");
    // A failed read of an object that is still listed stays the server's error.
    f.fake
        .objects
        .lock()
        .unwrap()
        .insert(ObjectKind::Procedure, vec!["p".into()]);
    *f.fake.ddl_error.lock().unwrap() = Some(server_error("1205"));
    let e =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_PROC.into(), o.strategy)
            .await
            .unwrap_err();
    assert!(
        matches!(&e, DbError::Server { code: Some(c), .. } if c == "1205"),
        "{e:?}"
    );
}

#[tokio::test]
async fn pg_failure_before_first_statement_is_not_applied() {
    let f = pg_function(pg17()).await;
    let o = open(&f, ObjectKind::Function, Some("f")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, PG_FUNC.into(), Some(Strategy::Replace))
            .await
            .unwrap();
    f.fake.fail_on("SET LOCAL", server_error("25P02"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::NotApplied, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unchanged);
    assert_eq!(r.statements_sent, 0);
    assert!(r.executed.is_empty() && r.failed.is_none());
    assert!(!r.error.as_ref().unwrap().outcome_unknown);
    let sent = f.fake.sent();
    assert!(!sent.contains(&p.statements[0]), "{sent:?}");
    // The open transaction is still closed.
    assert!(sent.contains(&"ROLLBACK".to_string()), "{sent:?}");
    assert!(history(&f).is_empty());
}

#[tokio::test]
async fn apply_updates_edit_before_dropping_its_plans() {
    const AFTER: &str = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 2 AS `id`";
    let f = fixture(Fake::new(mysql8(), MYSQL_VIEW), None, false).await;
    let o = open(&f, ObjectKind::View, Some("v")).await;
    let before = f.sm.ddl_edits.get(o.edit_id).unwrap().fingerprint;
    let applied =
        f.sm.ddl_edit_preview(o.edit_id, AFTER.into(), o.strategy)
            .await
            .unwrap();
    let other =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_VIEW.into(), o.strategy)
            .await
            .unwrap();
    *f.fake.sets_ddl.lock().unwrap() = Some(("CREATE OR REPLACE".into(), AFTER.into()));
    // During the re-read after the change, a watcher waits for the other plan to be
    // dropped and then reads the edit: by then it must already be the new one.
    let (tx, rx) = std::sync::mpsc::channel();
    let sm = Arc::clone(&f.sm);
    let (edit_id, other_id) = (o.edit_id, other.plan_id);
    *f.fake.on_ddl.lock().unwrap() = Some((
        1,
        Box::new(move || {
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while lock(&sm.ddl_edits.plans).contains_key(&other_id) {
                    if Instant::now() > deadline {
                        return;
                    }
                    std::thread::yield_now();
                }
                let _ = tx.send(sm.ddl_edits.get(edit_id).unwrap().fingerprint);
            });
        }),
    ));
    let r = apply(&f, applied.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Applied, "{r:?}");
    let seen = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let now = f.sm.ddl_edits.get(o.edit_id).unwrap().fingerprint;
    assert_ne!(now, before);
    assert_eq!(seen, now);
    assert!(lock(&f.sm.ddl_edits.plans).is_empty());
}

#[tokio::test]
async fn mysql_failure_with_object_changed_is_partial() {
    let f = mysql_trigger().await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap();
    // The DROP completes but leaves a different trigger of that name behind (another
    // session re-created it); the CREATE then fails.
    *f.fake.sets_ddl.lock().unwrap() = Some((
        "DROP TRIGGER".into(),
        MYSQL_TRIGGER.replace("NEW.a = 1", "NEW.a = 2"),
    ));
    f.fake.fail_on("CREATE DEFINER", server_error("1359"));
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Partial, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Changed);
    assert_eq!(r.executed, ["DROP TRIGGER `shop`.`t`"]);
    assert_eq!(r.failed.as_deref(), Some(MYSQL_TRIGGER));
    assert_eq!(r.restore_text, None);
    assert!(!r.error.as_ref().unwrap().outcome_unknown);
    let h = history(&f);
    assert_eq!(h.len(), 1);
    assert!(!h[0].outcome_unknown);
    // The edit keeps the object; its next preview finds it changed.
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert!(edit.object.is_some());
    *f.fake.fail.lock().unwrap() = None;
    let e =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap_err();
    assert!(is_object_changed(&e), "{e:?}");
}

fn transport_lost() -> DbError {
    DbError::ConnectionLost {
        was_in_tx: false,
        outcome_unknown: true,
        message: "lost".into(),
    }
}

#[tokio::test]
async fn mysql_transport_loss_rereads_on_fresh_session() {
    let f = mysql_trigger().await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap();
    // The DROP ran; the link drops during the CREATE; the trigger is gone.
    f.fake.objects.lock().unwrap().clear();
    f.fake.fail_on("CREATE DEFINER", transport_lost());
    let before = f.fake.connects.load(AtomicOrdering::SeqCst);
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert!(f.fake.connects.load(AtomicOrdering::SeqCst) >= before + 2);
    assert_eq!(r.outcome, EditOutcome::Partial, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Missing);
    assert_eq!(r.restore_text.as_deref(), Some(MYSQL_TRIGGER));
    assert_eq!(r.executed, ["DROP TRIGGER `shop`.`t`"]);
    assert_eq!(r.failed.as_deref(), Some(MYSQL_TRIGGER));
}

#[tokio::test]
async fn mysql_fresh_reread_failure_is_unknown() {
    let f = mysql_trigger().await;
    let o = open(&f, ObjectKind::Trigger, Some("t")).await;
    let p =
        f.sm.ddl_edit_preview(o.edit_id, MYSQL_TRIGGER.into(), o.strategy)
            .await
            .unwrap();
    f.fake.fail_on("CREATE DEFINER", transport_lost());
    // The apply's own session connects; the fresh one for the re-read cannot. (The
    // apply's session would still answer: the state must not come from it.)
    let n = f.fake.connects.load(AtomicOrdering::SeqCst);
    *f.fake.connect_fail_from.lock().unwrap() = Some(n + 1);
    let r = apply(&f, p.plan_id, None).await.unwrap();
    assert_eq!(r.outcome, EditOutcome::Unknown, "{r:?}");
    assert_eq!(r.object_state, ObjectState::Unknown);
    assert_eq!(r.restore_text, None);
    assert!(r.error.as_ref().unwrap().outcome_unknown);
    assert_eq!(r.executed, ["DROP TRIGGER `shop`.`t`"]);
    let h = history(&f);
    assert_eq!(h.len(), 1);
    assert!(h[0].outcome_unknown);
    // The edit is left as it was opened.
    let edit = f.sm.ddl_edits.get(o.edit_id).unwrap();
    assert!(edit.object.is_some());
    assert_eq!(edit.base_text, MYSQL_TRIGGER);
}
