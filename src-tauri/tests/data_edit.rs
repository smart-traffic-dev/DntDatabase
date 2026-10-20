//! Data view edits (`data_preview` / `data_apply`) against an in-process fake driver:
//! plans are consumed once and re-checked before anything is sent, atomic plans run in
//! one transaction, and every failure is reported with what is known about it.

use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag, ResolvedConfig, SecretMode};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, ExecOptions, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome,
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
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use dnt_query::edit::{Atomicity, CellInput, EditSet, RowEdit, RowRef};
use dnt_query::Verdict;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets, SecretChange, SecretUpdate};
use dnt_tunnel::KnownHosts;
use dntdatabase::edit_plans::{ApplyOutcome, ApplyReport};
use dntdatabase::session_manager::SessionManager;
use secrecy::SecretString;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// What the fake server holds and answers, and what it saw.
struct Server {
    detail: Mutex<TableDetail>,
    metas: Vec<ColumnMeta>,
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
}

impl Server {
    fn new(engine: &str) -> Arc<Server> {
        Arc::new(Server {
            detail: Mutex::new(items(engine)),
            metas: vec![meta("id"), meta("qty")],
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

fn result(sink: &mut dyn RowSink, cols: &[ColumnMeta], rows: Vec<Vec<Value>>) -> StmtOutcome {
    sink.result_start(cols);
    let n = rows.len() as u64;
    if !rows.is_empty() {
        sink.rows(rows);
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
        _opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let s = &self.server;
        s.executed
            .lock()
            .unwrap()
            .push((self.number, stmt.to_string()));
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
                Ok(result(sink, &[meta("cs")], vec![vec![Value::Text(cs)]]))
            }
            "SELECT" if stmt.starts_with("SELECT 1 FROM") => {
                let rows = if s.recheck_found.load(Ordering::SeqCst) {
                    vec![vec![Value::Int(1)]]
                } else {
                    vec![]
                };
                Ok(result(sink, &[meta("1")], rows))
            }
            "SELECT" if stmt.contains("LIMIT 0") => Ok(result(sink, &s.metas, vec![])),
            "SELECT" => Ok(result(
                sink,
                &s.metas,
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
    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }
    async fn close(self: Box<Self>) {
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
