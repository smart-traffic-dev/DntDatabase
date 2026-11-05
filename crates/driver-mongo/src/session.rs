//! The MongoDB session (spec §5.2): one `Client` per session, the database `use` moved to,
//! and the console's execution (`exec.rs`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dnt_core::command::{CommandClass, CommandLanguage, CommandSession, ParsedCommand};
use dnt_core::config::ResolvedConfig;
use dnt_core::data_access::{DataAccess, KeyEdit, PageSpec};
use dnt_core::driver::{
    CancelHandle, Engine, ExecOptions, ExecProgress, Introspector, RowSink, Session, StmtOutcome,
    StopSignal,
};
use dnt_core::edit::EditSet;
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::server::ServerInfo;
use dnt_core::sql::filter::DataFilter;
use dnt_core::tx::TxState;
use mongodb::bson::{doc, Uuid};
use mongodb::Client;
use secrecy::{ExposeSecret, SecretString};

use crate::cancel::{self, MongoCanceller};
use crate::client::{self, Opened};
use crate::exec::{self, Fail};
use crate::shell::MongoLanguage;

/// The command running now, shared with the canceller.
pub(crate) struct Running {
    /// The command's comment is `dnt:<uuid>`.
    pub uuid: String,
    /// What a stop says about its outcome; `execute_command` has it from the command, the
    /// canceller does not need it.
    #[allow(dead_code)]
    pub class: CommandClass,
}

/// One MongoDB connection: a `Client` of its own (rebuilt by a cancel that outlives its
/// grace), the session's database, and the command running now.
pub struct MongoSession {
    client: Arc<RwLock<Client>>,
    /// Rebuilds the `Client` after a cancel's grace ran out.
    cfg: ResolvedConfig,
    info: ServerInfo,
    /// Kept across a `Client` rebuild, so `DatabaseChanged` compares the right db.
    current_db: Arc<Mutex<String>>,
    read_only: bool,
    running: Arc<Mutex<Option<Running>>>,
    reconnects: AtomicU64,
}

/// Clears `running` when the command ends, however it ends.
struct RunningGuard<'a>(&'a Mutex<Option<Running>>);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// `ResolvedConfig` is not `Clone` (it holds the password); the session keeps its own.
fn copy_config(cfg: &ResolvedConfig) -> ResolvedConfig {
    ResolvedConfig {
        kind: cfg.kind,
        host: cfg.host.clone(),
        port: cfg.port,
        tls_server_name: cfg.tls_server_name.clone(),
        via_tunnel: cfg.via_tunnel,
        username: cfg.username.clone(),
        password: cfg
            .password
            .as_ref()
            .map(|p| SecretString::from(p.expose_secret().to_string())),
        database: cfg.database.clone(),
        ssl: cfg.ssl.clone(),
        connect_timeout: cfg.connect_timeout,
        read_only: cfg.read_only,
        application_name: cfg.application_name.clone(),
        driver: cfg.driver.clone(),
    }
}

impl MongoSession {
    pub(crate) fn new(opened: Opened, cfg: &ResolvedConfig) -> Self {
        MongoSession {
            client: Arc::new(RwLock::new(opened.client)),
            cfg: copy_config(cfg),
            info: opened.info,
            current_db: Arc::new(Mutex::new(opened.db)),
            read_only: cfg.read_only,
            running: Arc::new(Mutex::new(None)),
            reconnects: AtomicU64::new(0),
        }
    }

    /// The current `Client` (a cheap handle; the lock is never held across an await).
    fn client(&self) -> Client {
        self.client
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn db(&self) -> String {
        self.current_db
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn start(&self, uuid: String, class: CommandClass) -> RunningGuard<'_> {
        *self.running.lock().unwrap_or_else(|p| p.into_inner()) = Some(Running { uuid, class });
        RunningGuard(&self.running)
    }

    /// The `DbError` of a driver error, counting a network error in `reconnects()`.
    fn mapped(&self, e: &mongodb::error::Error, class: &CommandClass) -> DbError {
        let (err, network) = exec::map_error(e, class);
        if network {
            self.reconnects.fetch_add(1, Ordering::SeqCst);
        }
        err
    }

    /// Sends `plan` and waits for its result, watching for `opts.stop` and, for a command
    /// whose `maxTimeMS` does not carry it, `opts.timeout` (spec §5.4, §5.5).
    ///
    /// Either one emits [`ExecProgress::CancelSent`] and kills the command (`killOp` by its
    /// comment; on a stop the backend's canceller does the same, and whichever comes first
    /// acts), then waits up to `opts.cancel_grace`. A command stopped by the kill is
    /// `Cancelled` (`Timeout` after a timeout), with the outcome unknown unless it reads. One
    /// that completes during the grace keeps its result, which is late. One still running at
    /// the deadline is given up ([`ExecProgress::GraceExpired`]): its future is dropped and
    /// the `Client` rebuilt.
    async fn run_watched(
        &self,
        plan: exec::Plan,
        opts: &ExecOptions,
        class: &CommandClass,
        sink: &mut dyn RowSink,
    ) -> Result<(exec::Output, bool), DbError> {
        let client = self.client();
        let comment = plan.comment.clone();
        let limit = if plan.read { None } else { opts.timeout };
        let mut fut = Box::pin(exec::run(&client, plan, opts, class));
        let expiry = async {
            match limit {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        let trigger = tokio::select! {
            biased;
            res = fut.as_mut() => {
                // The canceller's kill can end the command before the stop is polled.
                if !opts.stop.is_stopped() {
                    return self.ended(res, class, None).map(|o| (o, false));
                }
                sink.progress(ExecProgress::CancelSent);
                return self.ended(res, class, Some(Trigger::Stop)).map(|o| (o, true));
            }
            () = stop_raised(&opts.stop) => Trigger::Stop,
            () = expiry => Trigger::Timeout,
        };
        let deadline = tokio::time::Instant::now() + opts.cancel_grace;
        sink.progress(ExecProgress::CancelSent);
        let mut kill = Box::pin(cancel::kill(&client, &comment));
        let mut killing = true;
        let res = loop {
            tokio::select! {
                biased;
                res = fut.as_mut() => break Some(res),
                // Best effort: a failed kill leaves the grace and the rebuild.
                _ = kill.as_mut(), if killing => killing = false,
                () = tokio::time::sleep_until(deadline) => break None,
            }
        };
        match res {
            Some(res) => self.ended(res, class, Some(trigger)).map(|o| (o, true)),
            None => {
                sink.progress(ExecProgress::GraceExpired);
                drop(kill);
                drop(fut);
                self.rebuild().await;
                Err(trigger.error(class))
            }
        }
    }

    /// The result of a command that ended, `after` a stop or a timeout or not.
    fn ended(
        &self,
        res: Result<exec::Output, Fail>,
        class: &CommandClass,
        after: Option<Trigger>,
    ) -> Result<exec::Output, DbError> {
        match res {
            Ok(out) => Ok(out),
            // Killed: by this session's cancel, or by someone else's `killOp`.
            Err(f) if f.interrupted() => Err(after.unwrap_or(Trigger::Stop).error(class)),
            Err(Fail::Db(e)) => Err(e),
            Err(Fail::Mongo(e)) => Err(self.mapped(&e, class)),
        }
    }

    /// Replaces the `Client` with a fresh one; the old one is shut down in the background
    /// (the given-up command's connection with it). The canceller reads the `Client`
    /// through the shared lock, so its handles keep working. Should the new `Client` fail
    /// to build, the old one stays: the crate closes a connection dropped in the middle of a
    /// command, so the next command gets a fresh connection anyway.
    async fn rebuild(&self) {
        if let Ok(fresh) = client::rebuild(&self.cfg).await {
            let old = std::mem::replace(
                &mut *self.client.write().unwrap_or_else(|p| p.into_inner()),
                fresh,
            );
            tokio::spawn(async move {
                old.shutdown().immediate(true).await;
            });
        }
        self.reconnects.fetch_add(1, Ordering::SeqCst);
    }
}

/// What made a running command be cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// `opts.stop` (the user's cancel).
    Stop,
    /// `opts.timeout`, for a command that does not carry it as `maxTimeMS`.
    Timeout,
}

impl Trigger {
    /// The error of a command it stopped: the outcome is unknown unless it reads.
    fn error(self, class: &CommandClass) -> DbError {
        let outcome_unknown = *class != CommandClass::Read;
        match self {
            Trigger::Stop => DbError::Cancelled {
                outcome_unknown,
                was_in_tx: false,
            },
            Trigger::Timeout => DbError::Timeout {
                stage: ConnectStage::Execute,
                outcome_unknown,
                was_in_tx: false,
            },
        }
    }
}

/// How often [`stop_raised`] looks at the stop signal (it has no wake-up of its own).
const STOP_POLL: Duration = Duration::from_millis(20);

/// Completes once `stop` is raised.
async fn stop_raised(stop: &StopSignal) {
    while !stop.is_stopped() {
        tokio::time::sleep(STOP_POLL).await;
    }
}

#[async_trait]
impl Session for MongoSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    fn engine(&self) -> Engine<'_> {
        Engine::Command(self)
    }

    fn introspector(&self) -> &dyn Introspector {
        self
    }

    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(MongoCanceller {
            client: Arc::clone(&self.client),
            running: Arc::clone(&self.running),
        })
    }

    fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::SeqCst)
    }

    /// `ping` on `admin`.
    async fn ping(&self) -> Result<(), DbError> {
        self.client()
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .map(|_| ())
            .map_err(|e| self.mapped(&e, &CommandClass::Read))
    }

    async fn close(self: Box<Self>) {
        self.client().shutdown().immediate(true).await;
    }
}

#[async_trait]
impl CommandSession for MongoSession {
    fn language(&self) -> &dyn CommandLanguage {
        &MongoLanguage
    }

    /// Always `Idle`: a transaction only lives inside one `execute_atomic`.
    fn tx_state(&self) -> TxState {
        TxState::Idle
    }

    fn current_database(&self) -> Option<String> {
        Some(self.db())
    }

    fn data_access(&self) -> &dyn DataAccess {
        self
    }

    async fn execute_command(
        &self,
        cmd: &ParsedCommand,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let payload = exec::payload_of(cmd)?;
        if self.read_only {
            exec::check_read_only(cmd)?;
        }
        if opts.stop.is_stopped() {
            return Err(DbError::cancelled());
        }
        let started = Instant::now();
        let uuid = Uuid::new().to_string();
        let comment = cancel::comment_of(&uuid);
        let _running = self.start(uuid, cmd.class.clone());
        let plan = exec::plan(&payload.cmd, &self.db(), opts, &comment)?;
        let moves_to = (plan.shape == exec::Shape::Use).then(|| plan.db.clone());
        let (output, late) = self.run_watched(plan, opts, &cmd.class, sink).await?;
        if let Some(db) = moves_to {
            *self.current_db.lock().unwrap_or_else(|p| p.into_inner()) = db;
        }
        let summary = exec::emit(sink, output, opts, started)?;
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: late,
        })
    }
}

/// The object tree and data view come with plan Task 11.
fn not_yet(what: &str) -> DbError {
    DbError::Unsupported {
        feature: format!("{what} on MongoDB"),
        version: String::new(),
    }
}

#[async_trait]
impl Introspector for MongoSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Err(not_yet("listing databases"))
    }

    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(Vec::new())
    }

    async fn list_objects(
        &self,
        _scope: &Scope,
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Err(not_yet("listing collections"))
    }

    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(Vec::new())
    }

    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Err(not_yet("collection details"))
    }

    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Err(not_yet("collection definitions"))
    }
}

impl DataAccess for MongoSession {
    fn page(&self, _obj: &ObjectRef, _spec: &PageSpec) -> Result<ParsedCommand, DbError> {
        Err(not_yet("the data view"))
    }

    fn count(
        &self,
        _obj: &ObjectRef,
        _filter: Option<&DataFilter>,
    ) -> Result<ParsedCommand, DbError> {
        Err(not_yet("the data view"))
    }

    fn build_edits(
        &self,
        _obj: &ObjectRef,
        _detail: &TableDetail,
        _edits: &EditSet,
    ) -> Result<Vec<ParsedCommand>, DbError> {
        Err(not_yet("editing documents"))
    }

    fn build_key_edit(&self, _db: &str, _edit: &KeyEdit) -> Result<Vec<ParsedCommand>, DbError> {
        Err(not_yet("key edits"))
    }
}
