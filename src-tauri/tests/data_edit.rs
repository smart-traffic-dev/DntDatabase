//! Data view edits (`data_preview` / `data_apply`) against an in-process fake driver:
//! plans are consumed once and re-checked before anything is sent, atomic plans run in
//! one transaction, and every failure is reported with what is known about it.

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, CellMode, Driver, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, StmtOutcome,
};
use dnt_core::error::DbError;
use dnt_core::meta::{
    ColumnDetail, DatabaseInfo, IndexDetail, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo,
    Scope, TableColumns, TableDetail, TableOptions,
};
use dnt_core::secret::{MemorySecretStore, SecretStore};
use dnt_core::server::{detect_mysql, DbKind, ServerInfo};
use dnt_core::sql::{Dialect, MySqlMode, StatementKind};
use dnt_core::test_util::sample_config;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_query::edit::{Atomicity, CellInput, EditSet, RowEdit, RowRef};
use dnt_query::Verdict;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets, SecretChange, SecretUpdate};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::data::{data_cell_load_file_impl, data_cell_save_file_impl};
use dntdatabase::edit_plans::{ApplyOutcome, ApplyReport, CellPayload};
use dntdatabase::file_picks::FileAccess;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use secrecy::SecretString;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

/// What the fake server holds and answers, and what it saw.
struct Server {
    detail: Mutex<TableDetail>,
    metas: Mutex<Vec<ColumnMeta>>,
    dialect: Mutex<Dialect>,
    charset: Mutex<String>,
    /// Sessions opened so far (the metadata session is number 0).
    sessions: AtomicUsize,
    closes: AtomicUsize,
    /// `(session number, statement)` of every `execute_one`, in order.
    executed: Mutex<Vec<(usize, String)>>,
    /// The outcome of each write in turn (affected rows); `Ok(1)` once they run out.
    writes: Mutex<VecDeque<Result<u64, DbError>>>,
    /// The next `COMMIT` fails with this.
    commit: Mutex<Option<DbError>>,
    /// A recheck (`SELECT 1 FROM …`) finds its row.
    recheck_found: AtomicBool,
    /// Silent reconnects of the sessions so far (`Session::reconnects`).
    reconnects: AtomicU64,
    /// The next statement starting with this replaces its connection first (a silent
    /// reconnect before it is sent), counted in `reconnects`.
    reconnect_before: Mutex<Option<&'static str>>,
    /// The `character_set_client` check flips the session's `sql_mode` once it is read.
    flip_dialect_after_check: AtomicBool,
    /// While set, reading the table detail waits for it (`detail_entered` says so).
    detail_gate: Mutex<Option<Arc<Notify>>>,
    detail_entered: Notify,
    /// A statement starting with this hangs (ignoring cancels) until the `Notify`, if
    /// any, is notified; `hang_entered` says it started.
    hang: Mutex<Option<(String, Option<Arc<Notify>>)>>,
    hang_entered: Notify,
    /// Session numbers in the order they were closed.
    closed: Mutex<Vec<usize>>,
}

/// A write that reports no affected count.
const NO_COUNT: u64 = u64::MAX;

impl Server {
    fn new(engine: &str) -> Arc<Server> {
        Arc::new(Server {
            reconnects: AtomicU64::new(0),
            reconnect_before: Mutex::default(),
            flip_dialect_after_check: AtomicBool::new(false),
            detail_gate: Mutex::default(),
            detail_entered: Notify::new(),
            detail: Mutex::new(items(engine)),
            metas: Mutex::new(vec![meta("id"), meta("qty")]),
            hang: Mutex::default(),
            hang_entered: Notify::new(),
            closed: Mutex::default(),
            dialect: Mutex::new(Dialect::MYSQL),
            charset: Mutex::new("utf8mb4".into()),
            sessions: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            executed: Mutex::default(),
            writes: Mutex::default(),
            commit: Mutex::default(),
            recheck_found: AtomicBool::new(false),
        })
    }

    fn script(&self, writes: Vec<Result<u64, DbError>>) {
        *self.writes.lock().unwrap() = writes.into();
    }

    fn executed(&self) -> Vec<String> {
        self.executed
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Statements sent from position `from` on.
    fn since(&self, from: usize) -> Vec<String> {
        self.executed()[from..].to_vec()
    }
}

fn meta(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        db_type: "INT".into(),
        logical: LogicalType::Integer,
        nullable: None,
        origin_table: None,
        origin_column: None,
    }
}

fn column(name: &str, nullable: bool) -> ColumnDetail {
    ColumnDetail {
        name: name.into(),
        ordinal: 0,
        db_type: "int".into(),
        nullable,
        default: None,
        auto_increment: false,
        identity: None,
        generated: None,
        invisible: false,
        collation: None,
        comment: None,
    }
}

fn object() -> ObjectRef {
    ObjectRef {
        database: "shop".into(),
        schema: None,
        name: "items".into(),
        kind: ObjectKind::Table,
    }
}

/// `shop.items (id int PK, qty int NULL)` on `engine`.
fn items(engine: &str) -> TableDetail {
    TableDetail {
        object: object(),
        columns: vec![column("id", false), column("qty", true)],
        primary_key: Some(IndexDetail {
            name: "PRIMARY".into(),
            unique: true,
            primary: true,
            partial: false,
            columns: vec!["id".into()],
            expression: None,
            method: None,
            definition: None,
        }),
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        triggers: vec![],
        options: TableOptions {
            engine: Some(engine.into()),
            collation: None,
            tablespace: None,
            partitioned_by: None,
            system_versioned: false,
        },
        row_estimate: None,
        size_bytes: None,
        comment: None,
    }
}

struct FakeDriver {
    server: Arc<Server>,
}

#[async_trait]
impl Driver for FakeDriver {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }
    async fn connect(&self, _cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let number = self.server.sessions.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeSession {
            number,
            info: detect_mysql("8.0.36").unwrap(),
            server: Arc::clone(&self.server),
            tx: Mutex::new(TxState::Idle),
        }))
    }
}

struct FakeSession {
    number: usize,
    info: ServerInfo,
    server: Arc<Server>,
    tx: Mutex<TxState>,
}

struct NoCancel;

#[async_trait]
impl Canceller for NoCancel {
    async fn cancel(&self) -> Result<(), DbError> {
        Ok(())
    }
}

/// One result of `rows`: display values, or complete ones in `CellMode::Full` (as the
/// real drivers do).
fn result(
    sink: &mut dyn RowSink,
    opts: &ExecOptions,
    cols: &[ColumnMeta],
    rows: Vec<Vec<Value>>,
) -> StmtOutcome {
    sink.result_start(cols);
    let n = rows.len() as u64;
    if !rows.is_empty() {
        match opts.cells {
            CellMode::Preview => sink.rows(rows),
            CellMode::Full { .. } => sink.full_rows(
                rows.into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|v| FullValue::try_from_value(v).unwrap())
                            .collect()
                    })
                    .collect(),
            ),
        }
    }
    let summary = ResultSummary {
        affected: None,
        row_count: n,
        truncated: false,
        elapsed_ms: 1,
    };
    sink.result_end(&summary);
    StmtOutcome {
        results: vec![summary],
        late_completion: false,
    }
}

fn done(affected: Option<u64>) -> StmtOutcome {
    StmtOutcome {
        results: vec![ResultSummary {
            affected,
            row_count: 0,
            truncated: false,
            elapsed_ms: 1,
        }],
        late_completion: false,
    }
}

impl FakeSession {
    fn set_tx(&self, tx: TxState) {
        *self.tx.lock().unwrap() = tx;
    }
}

#[async_trait]
impl Session for FakeSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    async fn execute_one(
        &self,
        stmt: &str,
        _kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let s = &self.server;
        s.executed
            .lock()
            .unwrap()
            .push((self.number, stmt.to_string()));
        {
            let mut before = s.reconnect_before.lock().unwrap();
            if before.is_some_and(|p| stmt.starts_with(p)) {
                *before = None;
                s.reconnects.fetch_add(1, Ordering::SeqCst);
            }
        }
        let hang = {
            let mut h = s.hang.lock().unwrap();
            match h.as_ref() {
                Some((prefix, _)) if stmt.starts_with(prefix.as_str()) => h.take(),
                _ => None,
            }
        };
        if let Some((_, release)) = hang {
            s.hang_entered.notify_one();
            match release {
                Some(release) => release.notified().await,
                None => std::future::pending::<()>().await,
            }
        }
        let word = stmt.split_whitespace().next().unwrap_or("").to_uppercase();
        match word.as_str() {
            "START" | "BEGIN" => {
                self.set_tx(TxState::Active);
                Ok(done(None))
            }
            "ROLLBACK" => {
                self.set_tx(TxState::Idle);
                Ok(done(None))
            }
            "COMMIT" => {
                self.set_tx(TxState::Idle);
                match s.commit.lock().unwrap().take() {
                    Some(e) => Err(e),
                    None => Ok(done(None)),
                }
            }
            "UPDATE" | "DELETE" | "INSERT" => {
                let next = s.writes.lock().unwrap().pop_front().unwrap_or(Ok(1));
                match next {
                    Ok(NO_COUNT) => Ok(done(None)),
                    Ok(n) => Ok(done(Some(n))),
                    Err(e) => {
                        if matches!(e, DbError::ConnectionLost { .. }) {
                            self.set_tx(TxState::Idle);
                        }
                        Err(e)
                    }
                }
            }
            "SELECT" if stmt.contains("@@character_set_client") => {
                let cs = s.charset.lock().unwrap().clone();
                if s.flip_dialect_after_check.swap(false, Ordering::SeqCst) {
                    *s.dialect.lock().unwrap() = Dialect::MySql(MySqlMode {
                        no_backslash_escapes: true,
                        ..MySqlMode::default()
                    });
                }
                Ok(result(
                    sink,
                    opts,
                    &[meta("cs")],
                    vec![vec![Value::Text(cs)]],
                ))
            }
            "SELECT" if stmt.starts_with("SELECT 1 FROM") => {
                let rows = if s.recheck_found.load(Ordering::SeqCst) {
                    vec![vec![Value::Int(1)]]
                } else {
                    vec![]
                };
                Ok(result(sink, opts, &[meta("1")], rows))
            }
            "SELECT" if stmt.contains("LIMIT 0") => {
                Ok(result(sink, opts, &s.metas.lock().unwrap(), vec![]))
            }
            // A cell's complete value (`data_cell_fetch`): row 404 is gone.
            "SELECT" if stmt.starts_with("SELECT `qty` FROM") => {
                let rows = if stmt.ends_with("= 404") {
                    vec![]
                } else {
                    vec![vec![Value::Int(7)]]
                };
                Ok(result(sink, opts, &[meta("qty")], rows))
            }
            "SELECT" => Ok(result(
                sink,
                opts,
                &s.metas.lock().unwrap(),
                vec![vec![Value::Int(41), Value::Int(5)]],
            )),
            _ => Ok(done(None)),
        }
    }
    fn tx_state(&self) -> TxState {
        *self.tx.lock().unwrap()
    }
    fn dialect(&self) -> Dialect {
        *self.server.dialect.lock().unwrap()
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(NoCancel)
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    fn reconnects(&self) -> u64 {
        self.server.reconnects.load(Ordering::SeqCst)
    }
    async fn ping(&self) -> Result<(), DbError> {
        self.server
            .executed
            .lock()
            .unwrap()
            .push((self.number, "PING".into()));
        Ok(())
    }
    async fn close(self: Box<Self>) {
        self.server.closed.lock().unwrap().push(self.number);
        self.server.closes.fetch_add(1, Ordering::SeqCst);
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
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Ok(vec![])
    }
    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }
    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        let gate = self.server.detail_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.server.detail_entered.notify_one();
            gate.notified().await;
        }
        Ok(self.server.detail.lock().unwrap().clone())
    }
    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Ok(String::new())
    }
}

struct Fixture {
    sm: SessionManager,
    repo: Arc<ConnectionRepo>,
    server: Arc<Server>,
    id: Uuid,
}

async fn fixture_with(engine: &str, env_tag: Option<EnvTag>) -> Fixture {
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::default());
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&secrets),
    ));
    let server = Server::new(engine);
    let driver: Arc<dyn Driver> = Arc::new(FakeDriver {
        server: Arc::clone(&server),
    });
    let dir = std::env::temp_dir();
    let sm = SessionManager::new(
        Arc::clone(&repo),
        secrets,
        KnownHosts::new(dir.join(format!("dnt_edit_kh_{}", Uuid::new_v4()))),
        HashMap::from([(DbKind::MySql, driver)]),
    );
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::MySql,
        default_db: Some("shop".into()),
        env_tag,
        secret_mode: SecretMode::Saved,
        ..sample_config()
    };
    let id = repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(SecretString::from("pw")),
                ssh_password: None,
                ssh_key_passphrase: None,
            },
        )
        .unwrap()
        .id;
    sm.open(id, None).await.unwrap();
    Fixture {
        sm,
        repo,
        server,
        id,
    }
}

async fn fixture() -> Fixture {
    fixture_with("InnoDB", None).await
}

/// `UPDATE items SET qty = <qty> WHERE id = <id>` (row loaded with qty 0).
fn update(id: i64, qty: &str) -> RowEdit {
    RowEdit::Update {
        row: RowRef {
            key: vec![("id".into(), Value::Int(id))],
            original: vec![("id".into(), Value::Int(id)), ("qty".into(), Value::Int(0))],
            xmin: None,
            loaded: vec![],
        },
        set: vec![("qty".into(), CellInput::Text { text: qty.into() })],
    }
}

fn edits(edits: Vec<RowEdit>) -> EditSet {
    EditSet {
        object: object(),
        edits,
        overwrite: false,
    }
}

fn updates(n: i64) -> EditSet {
    edits((1..=n).map(|i| update(i, "5")).collect())
}

fn is_write_or_tx(s: &str) -> bool {
    let w = s.split_whitespace().next().unwrap_or("").to_uppercase();
    matches!(
        w.as_str(),
        "UPDATE" | "DELETE" | "INSERT" | "START" | "BEGIN" | "COMMIT"
    )
}

fn lost(outcome_unknown: bool) -> DbError {
    DbError::ConnectionLost {
        was_in_tx: true,
        outcome_unknown,
        message: "server gone".into(),
    }
}

fn kind(e: &DbError) -> String {
    dnt_core::error::AppError::from(e.clone()).kind
}

#[tokio::test]
async fn preview_returns_the_statements_that_apply_runs() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    assert_eq!(p.atomic, Atomicity::Yes);
    assert_eq!(p.verdict, Verdict::Allow);
    assert_eq!(p.reason, None);
    assert_eq!(p.protections.len(), 2);
    assert_eq!(
        p.statements,
        [
            "UPDATE `shop`.`items` SET `qty` = 5 WHERE `id` = 1 AND `qty` <=> 0",
            "UPDATE `shop`.`items` SET `qty` = 5 WHERE `id` = 2 AND `qty` <=> 0",
        ]
    );
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
    assert_eq!(r.statements.len(), 2);
    assert!(r
        .statements
        .iter()
        .all(|s| s.affected == Some(1.0) && !s.conflict && s.error.is_none()));
    // Exactly the previewed strings, inside one transaction.
    let sent: Vec<String> = f
        .server
        .since(before)
        .into_iter()
        .filter(|s| is_write_or_tx(s))
        .collect();
    assert_eq!(
        sent,
        [
            "START TRANSACTION".to_string(),
            p.statements[0].clone(),
            p.statements[1].clone(),
            "COMMIT".to_string()
        ]
    );
}

#[tokio::test]
async fn apply_consumes_plan_once() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
    let before = f.server.executed().len();
    let again = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(again, DbError::PlanExpired), "{again:?}");
    assert!(f.server.since(before).is_empty(), "nothing sent");
    // An unknown id, too.
    assert!(matches!(
        f.sm.edit_apply(Uuid::new_v4(), false).await,
        Err(DbError::PlanExpired)
    ));
    // A newer preview of the same table replaces the older plan.
    let old = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let new = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    assert!(matches!(
        f.sm.edit_apply(old.plan_id, false).await,
        Err(DbError::PlanExpired)
    ));
    assert_eq!(
        f.sm.edit_apply(new.plan_id, false).await.unwrap().outcome,
        ApplyOutcome::Committed
    );
}

#[tokio::test]
async fn expired_plan_is_refused() {
    let f = fixture().await;
    let sm = f.sm.with_plan_ttl(std::time::Duration::from_millis(1));
    let p = sm.edit_preview(f.id, updates(1)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(matches!(
        sm.edit_apply(p.plan_id, false).await,
        Err(DbError::PlanExpired)
    ));
}

/// Apply checks the table again: a column added since the preview makes the plan stale,
/// and nothing is sent.
#[tokio::test]
async fn fingerprint_change_makes_plan_stale() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    f.server
        .detail
        .lock()
        .unwrap()
        .columns
        .push(column("note", true));
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    assert_eq!(kind(&e), "plan_stale");
    assert!(
        !f.server.since(before).iter().any(|s| is_write_or_tx(s)),
        "{:?}",
        f.server.since(before)
    );
    // The plan is gone too.
    assert!(matches!(
        f.sm.edit_apply(p.plan_id, false).await,
        Err(DbError::PlanExpired)
    ));
}

/// The session that would run the plan reads literals differently (a `sql_mode`
/// change): stale, nothing sent.
#[tokio::test]
async fn dialect_change_makes_plan_stale() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    *f.server.dialect.lock().unwrap() = Dialect::MySql(MySqlMode {
        no_backslash_escapes: true,
        ..MySqlMode::default()
    });
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    assert!(
        f.server.since(before).is_empty(),
        "{:?}",
        f.server.since(before)
    );
}

/// A replaced SSH tunnel (or a closed connection) makes the plan stale.
#[tokio::test]
async fn tunnel_or_connection_change_makes_plan_stale() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    f.sm.simulate_tunnel_replaced(f.id);
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");

    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    f.sm.close(f.id).await;
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(
        matches!(e, DbError::PlanStale { .. } | DbError::PlanExpired),
        "{e:?}"
    );
}

/// The apply session must speak utf8mb4 (R12): otherwise nothing of the plan is sent.
#[tokio::test]
async fn client_charset_other_than_utf8mb4_is_refused() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    *f.server.charset.lock().unwrap() = "latin1".into();
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert_eq!(kind(&e), "config", "{e:?}");
    let sent = f.server.since(before);
    assert!(
        !sent.iter().any(|s| s.starts_with("UPDATE")) && !sent.iter().any(|s| s == "COMMIT"),
        "{sent:?}"
    );
}

/// Statement 2 of 3 finds no row (changed meanwhile): the batch is rolled back, the
/// third statement is never sent, and statement 2 is the conflict.
#[tokio::test]
async fn affected_zero_rolls_back_atomic_batch() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(3)).await.unwrap();
    f.server.script(vec![Ok(1), Ok(0), Ok(1)]);
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::RolledBack);
    assert_eq!(r.statements.len(), 2);
    assert!(!r.statements[0].conflict);
    assert!(r.statements[1].conflict);
    assert_eq!(r.statements[1].index, 1);
    assert!(r.inserted.is_empty());
    let sent = f.server.since(before);
    assert!(!sent.contains(&p.statements[2]), "{sent:?}");
    assert!(sent.iter().any(|s| s == "ROLLBACK"), "{sent:?}");
    assert!(!sent.iter().any(|s| s == "COMMIT"), "{sent:?}");
    // MySQL reported 0 rows: the guarded row was looked for before calling it a
    // conflict, and was not there.
    assert!(
        sent.iter().any(|s| s.starts_with("SELECT 1 FROM")),
        "{sent:?}"
    );
}

/// MySQL counts changed rows: an UPDATE writing the values a row has reports 0, which
/// is not a conflict when the guarded row is there.
#[tokio::test]
async fn mysql_update_writing_same_values_is_not_a_conflict() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    f.server.script(vec![Ok(0), Ok(1)]);
    f.server.recheck_found.store(true, Ordering::SeqCst);
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
    assert!(!r.statements[0].conflict);
    assert_eq!(r.statements[0].affected, Some(1.0));
}

/// The connection drops before COMMIT was sent: the server rolls the transaction back.
#[tokio::test]
async fn connection_lost_before_commit_is_rolled_back() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(3)).await.unwrap();
    f.server.script(vec![Ok(1), Err(lost(false))]);
    let closes = f.server.closes.load(Ordering::SeqCst);
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::RolledBack);
    assert_eq!(r.statements.len(), 2);
    let err = r.statements[1].error.as_ref().expect("statement 2 failed");
    assert_eq!(err.kind, "connection_lost");
    assert!(!r.statements[1].conflict);
    let sent = f.server.since(before);
    assert!(!sent.contains(&p.statements[2]), "{sent:?}");
    assert!(!sent.iter().any(|s| s == "COMMIT"), "{sent:?}");
    // Never retried, and the session is not pooled.
    assert_eq!(
        sent.iter().filter(|s| **s == p.statements[1]).count(),
        1,
        "{sent:?}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(f.server.closes.load(Ordering::SeqCst) > closes);
}

/// The connection drops while COMMIT's answer is awaited: it may have committed.
#[tokio::test]
async fn connection_lost_during_commit_is_unknown() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    *f.server.commit.lock().unwrap() = Some(lost(true));
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 2 });
    assert!(r.statements.iter().all(|s| s.error.is_none()));
    assert_eq!(
        r.error.as_ref().map(|e| e.kind.as_str()),
        Some("connection_lost")
    );
    // COMMIT was sent once; nothing after it.
    let sent = f.server.since(before);
    assert_eq!(sent.last().map(String::as_str), Some("COMMIT"), "{sent:?}");
    let json = serde_json::to_value(r.outcome).unwrap();
    assert_eq!(json, serde_json::json!({"outcome": "unknown", "at": 2}));
}

/// A table that is not atomic takes one edit at a time, run without a transaction.
#[tokio::test]
async fn non_atomic_plan_has_one_statement() {
    let f = fixture_with("MyISAM", None).await;
    let e = f.sm.edit_preview(f.id, updates(2)).await.unwrap_err();
    match &e {
        DbError::Config { field, .. } => assert_eq!(field.as_deref(), Some("one_at_a_time")),
        other => panic!("{other:?}"),
    }
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    assert_eq!(p.atomic, Atomicity::No);
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
    let sent = f.server.since(before);
    assert!(
        !sent
            .iter()
            .any(|s| s.starts_with("START") || s.starts_with("BEGIN") || s == "COMMIT"),
        "{sent:?}"
    );
    assert!(sent.contains(&p.statements[0]));

    // Lost while it ran: whether it was applied is unknown.
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    f.server.script(vec![Err(DbError::ConnectionLost {
        was_in_tx: false,
        outcome_unknown: true,
        message: "gone".into(),
    })]);
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 0 });

    // No row matched (and the recheck finds none): nothing was applied.
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    f.server.script(vec![Ok(0)]);
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::PartiallyApplied { applied: 0 });
    assert!(r.statements[0].conflict);
}

#[tokio::test]
async fn production_needs_confirmation() {
    let f = fixture_with("InnoDB", Some(EnvTag::Production)).await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    assert_eq!(p.verdict, Verdict::Confirm);
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(
        matches!(e, DbError::ConfirmationRequired { count: 1 }),
        "{e:?}"
    );
    assert!(f.server.since(before).is_empty());
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let r = f.sm.edit_apply(p.plan_id, true).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
}

/// The connection was made read-only after the preview: the stored plan is judged
/// again with the config as it is now.
#[tokio::test]
async fn read_only_connection_blocked_even_if_ui_sends_plan() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let mut cfg = f.repo.get(f.id).unwrap();
    cfg.read_only = true;
    f.repo
        .update(
            cfg,
            SecretUpdate {
                db_password: SecretChange::Keep,
                ssh_password: SecretChange::Keep,
                ssh_key_passphrase: SecretChange::Keep,
            },
        )
        .unwrap();
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, true).await.unwrap_err();
    assert!(matches!(e, DbError::Blocked { count: 1 }), "{e:?}");
    assert!(f.server.since(before).is_empty());
    // And no new plan is made.
    let e = f.sm.edit_preview(f.id, updates(1)).await.unwrap_err();
    assert_eq!(kind(&e), "config", "{e:?}");
}

/// A report for the UI: outcome tagged, numbers as JS numbers.
#[tokio::test]
async fn apply_report_wire_format() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let r: ApplyReport = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["outcome"], serde_json::json!({"outcome": "committed"}));
    assert_eq!(json["statements"][0]["index"], 0);
    assert_eq!(json["statements"][0]["affected"], 1.0);
    assert_eq!(json["inserted"], serde_json::json!([]));
    let preview = serde_json::to_value(&p).unwrap();
    assert_eq!(preview["atomic"], "yes");
    assert_eq!(preview["verdict"], "allow");
}

/// An insert is read back (MySQL: by `LAST_INSERT_ID()` / its key) into `inserted`.
#[tokio::test]
async fn inserted_row_is_read_back() {
    let f = fixture().await;
    f.server.detail.lock().unwrap().columns[0].auto_increment = true;
    let p =
        f.sm.edit_preview(
            f.id,
            edits(vec![RowEdit::Insert {
                values: vec![("qty".into(), CellInput::Text { text: "5".into() })],
            }]),
        )
        .await
        .unwrap();
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
    assert_eq!(r.inserted, [vec![Value::Int(41), Value::Int(5)]]);
    let sent = f.server.since(before);
    assert!(
        sent.iter()
            .any(|s| s == "SELECT `id`, `qty` FROM `shop`.`items` WHERE `id` = LAST_INSERT_ID()"),
        "{sent:?}"
    );
}

// ---- review fixes (R22, R24, pooling) ------------------------------------------------

/// Without a transaction the driver may replace a dead connection silently: right
/// before its statement the session is pinged and checked again, and a `sql_mode` that
/// changed meanwhile stops the plan before anything is written.
#[tokio::test]
async fn non_atomic_dialect_flip_between_check_and_write_is_stale() {
    let f = fixture_with("MyISAM", None).await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    f.server
        .flip_dialect_after_check
        .store(true, Ordering::SeqCst);
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    let sent = f.server.since(before);
    assert_eq!(sent.first().map(String::as_str), Some("PING"), "{sent:?}");
    assert!(!sent.iter().any(|s| s.starts_with("UPDATE")), "{sent:?}");
}

/// A silent reconnect during the checks: the checks no longer describe the connection
/// the write would use, so nothing is written.
#[tokio::test]
async fn non_atomic_reconnect_during_checks_is_stale() {
    let f = fixture_with("MyISAM", None).await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    *f.server.reconnect_before.lock().unwrap() = Some("SELECT @@character_set_client");
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    let sent = f.server.since(before);
    assert!(!sent.iter().any(|s| s.starts_with("UPDATE")), "{sent:?}");
}

/// A silent reconnect seen only after the write: it ran on a connection the checks did
/// not cover, so its outcome is not vouched for.
#[tokio::test]
async fn non_atomic_reconnect_before_the_write_is_unknown() {
    let f = fixture_with("MyISAM", None).await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    *f.server.reconnect_before.lock().unwrap() = Some("UPDATE");
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 0 }, "{r:?}");
}

/// A row count other than 0 or the one expected (or none at all) cannot be read as
/// "applied" or "not applied".
#[tokio::test]
async fn non_atomic_unexpected_count_is_unknown() {
    let f = fixture_with("MyISAM", None).await;
    for count in [NO_COUNT, 2] {
        let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
        f.server.script(vec![Ok(count)]);
        let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 0 }, "{count}: {r:?}");
        assert!(!r.statements[0].conflict, "{count}");
    }
}

/// Whether the apply session went back to the pool (`true`) or was closed.
async fn pooled(f: &Fixture, closes_before: usize) -> bool {
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.server.closes.load(Ordering::SeqCst) == closes_before
}

/// A session is pooled only after a commit or a conflict's clean rollback. After a
/// statement that failed (a timeout or cancel sent KILL QUERY / a cancel request that
/// may still land) or a lost COMMIT, it is closed.
#[tokio::test]
async fn atomic_session_is_pooled_only_after_success_or_conflict() {
    let f = fixture().await;

    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let closes = f.server.closes.load(Ordering::SeqCst);
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed);
    assert!(pooled(&f, closes).await, "commit: pooled");

    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    f.server.script(vec![Ok(1), Ok(0)]);
    let closes = f.server.closes.load(Ordering::SeqCst);
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::RolledBack);
    assert!(pooled(&f, closes).await, "conflict: pooled");

    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    f.server.script(vec![
        Ok(1),
        Err(DbError::Timeout {
            stage: dnt_core::error::ConnectStage::Execute,
            outcome_unknown: false,
            was_in_tx: true,
        }),
    ]);
    let closes = f.server.closes.load(Ordering::SeqCst);
    let before = f.server.executed().len();
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::RolledBack);
    assert_eq!(
        r.statements[1].error.as_ref().map(|e| e.kind.as_str()),
        Some("timeout")
    );
    // Still rolled back explicitly, then closed rather than pooled.
    assert!(f.server.since(before).iter().any(|s| s == "ROLLBACK"));
    assert!(!pooled(&f, closes).await, "timeout: closed");

    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    *f.server.commit.lock().unwrap() = Some(lost(true));
    let closes = f.server.closes.load(Ordering::SeqCst);
    let r = f.sm.edit_apply(p.plan_id, false).await.unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 1 });
    assert!(!pooled(&f, closes).await, "lost COMMIT: closed");
}

/// The SSH tunnel is replaced while Apply reads the table: the session it then takes
/// (opened through the new tunnel) is not the connection the plan was made for.
#[tokio::test]
async fn tunnel_replaced_during_apply_checks_is_stale() {
    let f = fixture().await;
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let gate = Arc::new(Notify::new());
    *f.server.detail_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let before = f.server.executed().len();
    let (r, ()) = tokio::join!(f.sm.edit_apply(p.plan_id, false), async {
        f.server.detail_entered.notified().await;
        f.sm.simulate_tunnel_replaced(f.id);
        gate.notify_one();
    });
    let e = r.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    assert!(
        !f.server.since(before).iter().any(|s| is_write_or_tx(s)),
        "{:?}",
        f.server.since(before)
    );
}

// ---- closing the connection mid-apply, stale dialect (Task 9) ------------------------

/// `f` with closing waiting at most `wait` for a running apply.
fn closing_after(f: Fixture, wait: Duration) -> Fixture {
    Fixture {
        sm: f.sm.with_apply_close_wait(wait),
        ..f
    }
}

/// Runs `edit_apply(plan)` and, once a statement starting with `hang_at` hangs, closes
/// the connection; the apply's report, and the number of the session it ran on.
async fn close_during_apply(f: &Fixture, plan: Uuid, hang_at: &str) -> (ApplyReport, usize) {
    *f.server.hang.lock().unwrap() = Some((hang_at.to_string(), None));
    let (r, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(f.sm.edit_apply(plan, false), async {
            f.server.hang_entered.notified().await;
            f.sm.close(f.id).await;
        })
    })
    .await
    .expect("closing ends the apply");
    let session = f
        .server
        .executed
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(_, s)| s.starts_with(hang_at))
        .map(|(n, _)| *n)
        .unwrap();
    (r.expect("a report, not an error"), session)
}

/// Whether session `n` was closed (not pooled).
async fn was_closed(f: &Fixture, n: usize) -> bool {
    tokio::time::sleep(Duration::from_millis(30)).await;
    f.server.closed.lock().unwrap().contains(&n)
}

/// Closing the connection while an apply runs ends it with what is known (spec §4.2d):
/// stuck before `COMMIT` was sent, nothing is committed (the server rolls back when the
/// session goes); stuck in `COMMIT`, it may have committed. The session is closed.
#[tokio::test]
async fn close_connection_during_apply_reports_outcome() {
    // Stuck in statement 2 of 3.
    let f = closing_after(fixture().await, Duration::from_millis(100));
    let p = f.sm.edit_preview(f.id, updates(3)).await.unwrap();
    let before = f.server.executed().len();
    let (r, session) = close_during_apply(&f, p.plan_id, &p.statements[1]).await;
    assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{r:?}");
    assert_eq!(r.statements.len(), 2, "{r:?}");
    assert!(r.statements[0].error.is_none());
    assert_eq!(r.statements[1].index, 1);
    let err = r.statements[1]
        .error
        .as_ref()
        .expect("statement 2 did not finish");
    assert_eq!(err.kind, "connection_lost");
    assert!(!err.outcome_unknown);
    assert!(r.inserted.is_empty());
    let sent = f.server.since(before);
    assert!(!sent.contains(&p.statements[2]), "{sent:?}");
    assert!(!sent.iter().any(|s| s == "COMMIT"), "{sent:?}");
    assert!(was_closed(&f, session).await, "the apply session is closed");

    // Stuck in COMMIT.
    let f = closing_after(fixture().await, Duration::from_millis(100));
    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    let (r, session) = close_during_apply(&f, p.plan_id, "COMMIT").await;
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 2 }, "{r:?}");
    assert_eq!(r.statements.len(), 2);
    assert!(r.statements.iter().all(|s| s.error.is_none()));
    let err = r.error.as_ref().expect("the COMMIT's error");
    assert_eq!(err.kind, "connection_lost");
    assert!(err.outcome_unknown);
    assert!(was_closed(&f, session).await, "the apply session is closed");
}

/// Without a transaction, a statement cut off by closing may or may not have run.
#[tokio::test]
async fn close_connection_during_non_atomic_apply_is_unknown() {
    let f = closing_after(
        fixture_with("MyISAM", None).await,
        Duration::from_millis(100),
    );
    let p = f.sm.edit_preview(f.id, updates(1)).await.unwrap();
    let (r, session) = close_during_apply(&f, p.plan_id, "UPDATE").await;
    assert_eq!(r.outcome, ApplyOutcome::Unknown { at: 0 }, "{r:?}");
    assert_eq!(r.statements.len(), 1);
    let err = r.statements[0]
        .error
        .as_ref()
        .expect("the statement's error");
    assert_eq!(err.kind, "connection_lost");
    assert!(err.outcome_unknown);
    assert!(was_closed(&f, session).await, "the apply session is closed");
}

/// Closing waits (boundedly) for a running apply: one that ends within the wait is
/// reported as it ended, and closing returns only afterwards.
#[tokio::test]
async fn close_connection_waits_for_a_running_apply() {
    let f = closing_after(fixture().await, Duration::from_secs(5));
    let p = f.sm.edit_preview(f.id, updates(2)).await.unwrap();
    let release = Arc::new(Notify::new());
    *f.server.hang.lock().unwrap() = Some((p.statements[1].clone(), Some(Arc::clone(&release))));
    let applied = AtomicBool::new(false);
    let (r, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            async {
                let r = f.sm.edit_apply(p.plan_id, false).await;
                applied.store(true, Ordering::SeqCst);
                r
            },
            async {
                f.server.hang_entered.notified().await;
                let release = Arc::clone(&release);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    release.notify_one();
                });
                f.sm.close(f.id).await;
                assert!(applied.load(Ordering::SeqCst), "close waited for the apply");
            }
        )
    })
    .await
    .expect("no hang");
    assert_eq!(r.unwrap().outcome, ApplyOutcome::Committed);
}

/// An atomic plan whose session's `sql_mode` turns on `NO_BACKSLASH_ESCAPES` after the
/// session was taken (seen by the check inside the transaction): a literal with `\`
/// would be read differently, so the plan is stale, nothing is written and the
/// transaction is rolled back. (The change seen before the session is used is
/// `dialect_change_makes_plan_stale`; without a transaction,
/// `non_atomic_dialect_flip_between_check_and_write_is_stale`.)
#[tokio::test]
async fn sql_mode_change_makes_plan_stale() {
    let f = fixture().await;
    {
        let mut d = f.server.detail.lock().unwrap();
        let mut note = column("note", true);
        note.db_type = "varchar(20)".into();
        d.columns.push(note);
    }
    f.server.metas.lock().unwrap().push(ColumnMeta {
        name: "note".into(),
        db_type: "VARCHAR".into(),
        logical: LogicalType::Text,
        nullable: None,
        origin_table: None,
        origin_column: None,
    });
    let set = edits(vec![RowEdit::Update {
        row: RowRef {
            key: vec![("id".into(), Value::Int(1))],
            original: vec![
                ("id".into(), Value::Int(1)),
                ("qty".into(), Value::Int(0)),
                ("note".into(), Value::Text("x".into())),
            ],
            xmin: None,
            loaded: vec![],
        },
        set: vec![(
            "note".into(),
            CellInput::Text {
                text: r"a\b".into(),
            },
        )],
    }]);
    let p = f.sm.edit_preview(f.id, set).await.unwrap();
    assert_eq!(p.atomic, Atomicity::Yes);
    assert!(p.statements[0].contains(r"'a\\b'"), "{:?}", p.statements);
    f.server
        .flip_dialect_after_check
        .store(true, Ordering::SeqCst);
    let before = f.server.executed().len();
    let e = f.sm.edit_apply(p.plan_id, false).await.unwrap_err();
    assert!(matches!(e, DbError::PlanStale { .. }), "{e:?}");
    let sent = f.server.since(before);
    assert!(!sent.iter().any(|s| s.starts_with("UPDATE")), "{sent:?}");
    assert!(!sent.iter().any(|s| s == "COMMIT"), "{sent:?}");
    assert!(sent.iter().any(|s| s == "ROLLBACK"), "{sent:?}");
}

// ---- complete cell values (`data_cell_fetch`) -----------------------------------------

fn key(id: i64) -> Vec<(String, Value)> {
    vec![("id".into(), Value::Int(id))]
}

fn field(e: &DbError) -> String {
    match e {
        DbError::Config { field, .. } => field.clone().unwrap_or_default(),
        other => panic!("not a config error: {other:?}"),
    }
}

fn message(e: &DbError) -> String {
    match e {
        DbError::Config { message, .. } => message.clone(),
        other => panic!("not a config error: {other:?}"),
    }
}

fn set_cell(column: &str, input: CellInput) -> EditSet {
    edits(vec![RowEdit::Update {
        row: RowRef {
            key: key(1),
            original: vec![("id".into(), Value::Int(1)), ("qty".into(), Value::Int(0))],
            xmin: None,
            loaded: vec![],
        },
        set: vec![(column.into(), input)],
    }])
}

/// The cell is read by its key on a checked session, in full; its token stands for the
/// value in a later edit of the same cell only.
#[tokio::test]
async fn cell_fetch_reads_by_key_and_token_binds_to_its_cell() {
    let f = fixture().await;
    let before = f.server.executed().len();
    let cell =
        f.sm.cell_fetch(f.id, object(), key(1), "qty".into())
            .await
            .unwrap();
    assert_eq!(cell.payload, CellPayload::Text { text: "7".into() });
    let sent = f.server.since(before);
    let select = sent
        .iter()
        .position(|s| s == "SELECT `qty` FROM `shop`.`items` WHERE `id` = 1")
        .unwrap_or_else(|| panic!("{sent:?}"));
    // The session reads app-built literals as they were built (R12), checked first.
    let check = sent
        .iter()
        .position(|s| s.contains("@@character_set_client"))
        .unwrap_or_else(|| panic!("{sent:?}"));
    assert!(check < select, "{sent:?}");

    let p =
        f.sm.edit_preview(
            f.id,
            set_cell("qty", CellInput::Loaded { token: cell.token }),
        )
        .await
        .unwrap();
    assert!(
        p.statements[0].starts_with("UPDATE `shop`.`items` SET `qty` = 7 WHERE"),
        "{:?}",
        p.statements
    );
    // Another column of the same table: refused.
    let e =
        f.sm.edit_preview(
            f.id,
            set_cell("id", CellInput::Loaded { token: cell.token }),
        )
        .await
        .unwrap_err();
    assert_eq!(field(&e), "column:id", "{e:?}");
    assert!(message(&e).contains("another"), "{e:?}");
    // An unknown token.
    let e =
        f.sm.edit_preview(
            f.id,
            set_cell(
                "qty",
                CellInput::Loaded {
                    token: Uuid::new_v4(),
                },
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(field(&e), "column:qty", "{e:?}");
}

#[tokio::test]
async fn cell_fetch_of_a_row_that_is_gone() {
    let f = fixture().await;
    let e =
        f.sm.cell_fetch(f.id, object(), key(404), "qty".into())
            .await
            .unwrap_err();
    assert_eq!(field(&e), "row", "{e:?}");
    // Not a column of the table; a key that is not the table's key.
    let e =
        f.sm.cell_fetch(f.id, object(), key(1), "nope".into())
            .await
            .unwrap_err();
    assert_eq!(field(&e), "column:nope", "{e:?}");
    let e =
        f.sm.cell_fetch(
            f.id,
            object(),
            vec![("qty".into(), Value::Int(1))],
            "qty".into(),
        )
        .await
        .unwrap_err();
    assert_eq!(field(&e), "column:id", "{e:?}");
}

/// A session whose client character set is not utf8mb4 would misread the key's
/// literals: nothing is read.
#[tokio::test]
async fn cell_fetch_refuses_a_session_without_utf8mb4() {
    let f = fixture().await;
    *f.server.charset.lock().unwrap() = "latin1".into();
    let before = f.server.executed().len();
    let e =
        f.sm.cell_fetch(f.id, object(), key(1), "qty".into())
            .await
            .unwrap_err();
    assert_eq!(kind(&e), "config", "{e:?}");
    let sent = f.server.since(before);
    assert!(
        !sent.iter().any(|s| s.starts_with("SELECT `qty`")),
        "{sent:?}"
    );
}

/// A token lives `LOADED_TTL` after its last use: an expired one is refused with a clear
/// reason.
#[tokio::test]
async fn expired_token_is_refused() {
    let f = fixture().await;
    let sm = f.sm.with_loaded_ttl(Duration::from_millis(1));
    let cell = sm
        .cell_fetch(f.id, object(), key(1), "qty".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let e = sm
        .edit_preview(
            f.id,
            set_cell("qty", CellInput::Loaded { token: cell.token }),
        )
        .await
        .unwrap_err();
    assert_eq!(field(&e), "column:qty", "{e:?}");
    assert!(message(&e).contains("load it again"), "{e:?}");
}

/// A value loaded from a file gets a token like a fetched one (binary columns keep the
/// bytes, others need UTF-8 text).
#[tokio::test]
async fn cell_load_bytes_gives_a_token() {
    let f = fixture().await;
    let cell =
        f.sm.cell_load(f.id, object(), "qty".into(), b"12".to_vec())
            .await
            .unwrap();
    assert_eq!(cell.payload, CellPayload::Text { text: "12".into() });
    let p =
        f.sm.edit_preview(
            f.id,
            set_cell("qty", CellInput::Loaded { token: cell.token }),
        )
        .await
        .unwrap();
    assert!(
        p.statements[0].contains("SET `qty` = 12"),
        "{:?}",
        p.statements
    );
    let e =
        f.sm.cell_load(f.id, object(), "qty".into(), vec![0xff, 0xfe])
            .await
            .unwrap_err();
    assert_eq!(field(&e), "path", "{e:?}");
    // The saved value of a token, for "Save to file".
    assert_eq!(f.sm.loaded_bytes(cell.token).unwrap(), b"12");
    assert!(f.sm.loaded_bytes(Uuid::new_v4()).is_err());
}
/// Files are read and written only where the user picked them in the backend's dialog,
/// each for its own use.
#[tokio::test]
async fn cell_files_only_through_the_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::open(
        AppPaths::in_dir(dir.path().join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap();
    let file = dir.path().join("x.bin");
    let path = file.to_string_lossy().into_owned();
    let e = data_cell_save_file_impl(&state, Uuid::new_v4(), path.clone())
        .await
        .unwrap_err();
    assert_eq!(field(&e), "path", "{e:?}");
    let e = data_cell_load_file_impl(&state, Uuid::new_v4(), object(), "qty".into(), path.clone())
        .await
        .unwrap_err();
    assert_eq!(field(&e), "path", "{e:?}");
    // Admitted for writing: still not for reading.
    state
        .file_picks
        .admit(FileAccess::CellValueWrite, file.clone())
        .unwrap();
    let e = data_cell_load_file_impl(&state, Uuid::new_v4(), object(), "qty".into(), path.clone())
        .await
        .unwrap_err();
    assert_eq!(field(&e), "path", "{e:?}");
    // The path passes; the token is unknown, and nothing is written.
    let e = data_cell_save_file_impl(&state, Uuid::new_v4(), path)
        .await
        .unwrap_err();
    assert_eq!(field(&e), "token", "{e:?}");
    assert!(!file.exists());
}
