//! DDL edits against an in-process fake driver: the object's DDL, canned answers to
//! the catalog reads (by statement prefix) and the objects `list_objects` returns.
use super::catalog::{MYSQL_SAME_TIMING_SQL, MYSQL_TRIGGER_SQL, PG_ROUTINE_SQL, PG_TRIGGER_SQL};
use super::*;
use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, detect_pg, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, MySqlMode, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::Value;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
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
}

impl Fake {
    fn new(server: ServerInfo, ddl: &str) -> Arc<Fake> {
        Arc::new(Fake {
            server,
            ddl: Mutex::new(ddl.to_string()),
            ddl_error: Mutex::new(None),
            seen: Mutex::new(Vec::new()),
            answers: Mutex::new(Vec::new()),
            objects: Mutex::new(HashMap::new()),
        })
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
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
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
