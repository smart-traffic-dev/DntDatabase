//! The MongoDB session (spec §5.2): one `Client` per session, the database `use` moved to,
//! and the console's execution (`exec.rs`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dnt_core::command::{
    AtomicFailure, CommandClass, CommandLanguage, CommandSession, ParsedCommand,
};
use dnt_core::config::ResolvedConfig;
use dnt_core::data_access::{DataAccess, KeyEdit, PageSpec};
use dnt_core::driver::{
    CancelHandle, Engine, ExecOptions, ExecProgress, Introspector, ResultSummary, RowSink, Session,
    StmtOutcome, StopSignal,
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
use dnt_core::value::{ColumnMeta, Value};
use mongodb::bson::{doc, Bson, Uuid};
use mongodb::error::UNKNOWN_TRANSACTION_COMMIT_RESULT;
use mongodb::{Client, ClientSession};
use secrecy::{ExposeSecret, SecretString};

use crate::cancel::{self, MongoCanceller};
use crate::client::{self, Opened};
use crate::exec::{self, Fail};
use crate::shell::{MongoLanguage, ShellCommand};
use crate::{data, introspect};

/// The command running now, shared with the canceller.
pub(crate) struct Running {
    /// The command's comment is `dnt:<uuid>`.
    pub uuid: String,
    /// What a stop says about its outcome; `execute_command` has it from the command, the
    /// canceller does not need it.
    #[allow(dead_code)]
    pub class: CommandClass,
    /// The command's own `opts.stop`: the canceller kills only once it is raised, so a
    /// cancel meant for an earlier run cannot reach this command.
    pub stop: StopSignal,
    /// The command's `opts.cancel_grace`, which bounds the canceller's kill.
    pub grace: Duration,
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

    fn start(&self, uuid: String, class: CommandClass, opts: &ExecOptions) -> RunningGuard<'_> {
        *self.running.lock().unwrap_or_else(|p| p.into_inner()) = Some(Running {
            uuid,
            class,
            stop: opts.stop.clone(),
            grace: opts.cancel_grace,
        });
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
    ///
    /// With `session` (a transaction of `execute_atomic`), the command runs in it, on the
    /// session's `Client`.
    async fn run_watched(
        &self,
        plan: exec::Plan,
        opts: &ExecOptions,
        class: &CommandClass,
        sink: &mut dyn RowSink,
        session: Option<&mut ClientSession>,
    ) -> Result<(exec::Output, bool), DbError> {
        let client = match &session {
            Some(s) => s.client(),
            None => self.client(),
        };
        let comment = plan.comment.clone();
        let limit = if plan.read { None } else { opts.timeout };
        let mut fut = Box::pin(exec::run(&client, plan, opts, class, session));
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

    /// A data-view read (`cmd`) with `filter` coerced by the field types of the fixed
    /// sample (spec §6.1, §6.2), read first like any command: same comment (a cancel stops
    /// it), same timeout, every sampled document kept whatever `opts.max_rows` says.
    async fn typed_read(
        &self,
        cmd: &ShellCommand,
        filter: &DataFilter,
        opts: &ExecOptions,
        comment: &str,
        sink: &mut dyn RowSink,
    ) -> Result<ShellCommand, DbError> {
        let ShellCommand::Coll { db, coll, .. } = cmd else {
            return Err(DbError::Internal {
                message: "a typed filter belongs to a collection read".into(),
            });
        };
        let sample = introspect::sample_command(db.clone(), coll.clone());
        let sample_opts = ExecOptions {
            max_rows: introspect::SAMPLE_SIZE,
            ..opts.clone()
        };
        let plan = exec::plan(&sample, &self.db(), &sample_opts, comment)?;
        let (out, late) = self
            .run_watched(plan, &sample_opts, &CommandClass::Read, sink, None)
            .await?;
        // A sample that outlived a stop (or a timeout) ends the read: it is not sent.
        if let Some(trigger) = sample_ended_by(late, opts.stop.is_stopped()) {
            return Err(trigger.error(&CommandClass::Read));
        }
        data::resolve(cmd, filter, &introspect::field_types(&out.docs))
    }

    /// The `DbError` of an introspection failure.
    fn failed(&self, f: Fail) -> DbError {
        match f {
            Fail::Db(e) => e,
            Fail::Mongo(e) => self.mapped(&e, &CommandClass::Read),
        }
    }

    /// The result of a command that ended, `after` a stop or a timeout or not.
    fn ended(
        &self,
        res: Result<exec::Output, Fail>,
        class: &CommandClass,
        after: Option<Trigger>,
    ) -> Result<exec::Output, DbError> {
        match (res, after) {
            (Ok(out), _) => Ok(out),
            // Killed by this session's cancel. Without a stop or a timeout of ours, the kill
            // came from elsewhere (`killOp`, `killSessions`): a server error like any other
            // (`Server { code: "Interrupted" }`, spec §5.6).
            (Err(f), Some(trigger)) if f.interrupted() => Err(trigger.error(class)),
            (Err(Fail::Db(e)), _) => Err(e),
            (Err(Fail::Mongo(e)), _) => Err(self.mapped(&e, class)),
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

/// What ends a typed read once its sample is back: a stop raised by then, or a sample that
/// completed late (after a stop or timeout) — `None`: the read is sent.
fn sample_ended_by(late: bool, stopped: bool) -> Option<Trigger> {
    match (stopped, late) {
        (true, _) => Some(Trigger::Stop),
        (false, true) => Some(Trigger::Timeout),
        (false, false) => None,
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
        let _running = self.start(uuid, cmd.class.clone(), opts);
        let typed;
        let shell = match &payload.typed {
            None => &payload.cmd,
            Some(filter) => {
                typed = self
                    .typed_read(&payload.cmd, filter, opts, &comment, sink)
                    .await?;
                &typed
            }
        };
        let plan = exec::plan(shell, &self.db(), opts, &comment)?;
        let moves_to = (plan.shape == exec::Shape::Use).then(|| plan.db.clone());
        let (output, late) = self.run_watched(plan, opts, &cmd.class, sink, None).await?;
        // An edit applied on its own (no transactions): a document gone stops the apply.
        if let Some(id) = &payload.document {
            data::check_found(id, output.affected)?;
        }
        if let Some(db) = moves_to {
            *self.current_db.lock().unwrap_or_else(|p| p.into_inner()) = db;
        }
        let summary = exec::emit(sink, output, opts, started)?;
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: late,
        })
    }

    /// `cmds` as one transaction (spec §6.4): a `ClientSession` starts a transaction, the
    /// commands run in it in order (each as `execute_command` runs it: comment, timeout,
    /// stop and cancel grace), and it commits.
    ///
    /// - Without transactions (a standalone server): the default `Unsupported`, index 0,
    ///   nothing sent.
    /// - Every command is checked and planned before anything is sent; a refused one
    ///   fails at its index, rolled back (nothing sent), except an `Unsupported` one,
    ///   which is reported as the default's is (index 0, not rolled back).
    /// - A command that fails, an edit whose document no longer exists
    ///   (`DocumentNotFound`) or a stop aborts the transaction: `rolled_back: true`. Its
    ///   error then has `outcome_unknown: false` and `was_in_tx: true`, since nothing of
    ///   an uncommitted transaction takes effect.
    /// - A commit whose result is unknown (`UnknownTransactionCommitResult`) is retried
    ///   once, the only automatic retry of the driver (commit is idempotent by
    ///   `txnNumber`); still unknown, the failure is `ConnectionLost { outcome_unknown:
    ///   true }` at the last index, not rolled back. Any other commit error is known not
    ///   to have committed: rolled back. An abort's single retry is the crate's own (it
    ///   retries `abortTransaction` once and ignores its errors, as the transactions spec
    ///   says; a second `abort_transaction` call is refused by the crate).
    async fn execute_atomic(
        &self,
        cmds: &[ParsedCommand],
        opts: &ExecOptions,
    ) -> Result<Vec<StmtOutcome>, AtomicFailure> {
        if !self.info.capabilities.transactions {
            return Err(AtomicFailure {
                index: 0,
                error: DbError::Unsupported {
                    feature: "transactions".into(),
                    version: String::new(),
                },
                rolled_back: false,
            });
        }
        let unsent = |index: usize, error: DbError| AtomicFailure {
            index,
            error,
            rolled_back: true,
        };
        let uuid = Uuid::new().to_string();
        let comment = cancel::comment_of(&uuid);
        let mut steps = Vec::with_capacity(cmds.len());
        for (index, cmd) in cmds.iter().enumerate() {
            steps.push(self.atomic_step(cmd, opts, &comment).map_err(|e| match e {
                // As the default's: the backend reports it as a plain error, nothing sent.
                DbError::Unsupported { .. } => AtomicFailure {
                    index: 0,
                    error: e,
                    rolled_back: false,
                },
                e => unsent(index, e),
            })?);
        }
        let Some(last) = steps.len().checked_sub(1) else {
            return Ok(Vec::new());
        };
        if opts.stop.is_stopped() {
            return Err(unsent(0, DbError::cancelled()));
        }
        let _running = self.start(uuid, CommandClass::Write, opts);
        let mut session = self
            .client()
            .start_session()
            .await
            .map_err(|e| unsent(0, self.mapped(&e, &CommandClass::Read)))?;
        let mut start = session.start_transaction();
        if let Some(limit) = opts.timeout {
            start = start.max_commit_time(limit);
        }
        start
            .await
            .map_err(|e| unsent(0, self.mapped(&e, &CommandClass::Read)))?;
        let mut outcomes = Vec::with_capacity(steps.len());
        for (index, step) in steps.into_iter().enumerate() {
            let ran = if opts.stop.is_stopped() {
                Err(DbError::cancelled())
            } else {
                self.run_step(step, opts, &mut session).await
            };
            match ran {
                Ok(out) => outcomes.push(out),
                Err(error) => {
                    abort(&mut session).await;
                    return Err(AtomicFailure {
                        index,
                        error: in_transaction(error),
                        rolled_back: true,
                    });
                }
            }
        }
        // Stopped after the last command: not committed.
        if opts.stop.is_stopped() {
            abort(&mut session).await;
            return Err(AtomicFailure {
                index: last,
                error: in_transaction(DbError::cancelled()),
                rolled_back: true,
            });
        }
        match commit(&mut session).await {
            Ok(()) => Ok(outcomes),
            Err(e) => {
                let unknown = e.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT);
                let mapped = self.mapped(&e, &CommandClass::Write);
                Err(if unknown {
                    AtomicFailure {
                        index: last,
                        error: DbError::ConnectionLost {
                            was_in_tx: true,
                            outcome_unknown: true,
                            message: format!("the commit's outcome is unknown: {mapped}"),
                        },
                        rolled_back: false,
                    }
                } else {
                    AtomicFailure {
                        index: last,
                        error: in_transaction(mapped),
                        rolled_back: true,
                    }
                })
            }
        }
    }
}

/// One command of an `execute_atomic` unit, planned before anything is sent.
struct AtomicStep {
    plan: exec::Plan,
    class: CommandClass,
    /// The edited document's `_id` ([`crate::shell::Payload::document`]).
    document: Option<Bson>,
}

impl MongoSession {
    /// Checks and plans `cmd` for a transaction: read-only refuses it as
    /// `execute_command` would, and only a command without a cursor runs in one.
    fn atomic_step(
        &self,
        cmd: &ParsedCommand,
        opts: &ExecOptions,
        comment: &str,
    ) -> Result<AtomicStep, DbError> {
        let payload = exec::payload_of(cmd)?;
        if self.read_only {
            exec::check_read_only(cmd)?;
        }
        let plan = exec::plan(&payload.cmd, &self.db(), opts, comment)?;
        if plan.cursor || payload.typed.is_some() {
            return Err(DbError::Internal {
                message: format!("`{}` cannot run in a transaction", cmd.name),
            });
        }
        Ok(AtomicStep {
            plan,
            class: cmd.class.clone(),
            document: payload.document.clone(),
        })
    }

    /// Runs one planned command in the transaction of `session`.
    async fn run_step(
        &self,
        step: AtomicStep,
        opts: &ExecOptions,
        session: &mut ClientSession,
    ) -> Result<StmtOutcome, DbError> {
        let started = Instant::now();
        let (output, late) = self
            .run_watched(step.plan, opts, &step.class, &mut Discard, Some(session))
            .await?;
        if let Some(id) = &step.document {
            data::check_found(id, output.affected)?;
        }
        let summary = exec::emit(&mut Discard, output, opts, started)?;
        Ok(StmtOutcome {
            results: vec![summary],
            late_completion: late,
        })
    }
}

/// The results of an atomic unit are not streamed.
struct Discard;

impl RowSink for Discard {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// How long an abort may take. It only ends the transaction sooner: one that is never
/// committed never takes effect, and the server ends it on its own.
const ABORT_LIMIT: Duration = Duration::from_secs(5);

/// Aborts the transaction of `session`. The crate retries `abortTransaction` once and
/// ignores its errors.
async fn abort(session: &mut ClientSession) {
    let _ = tokio::time::timeout(ABORT_LIMIT, session.abort_transaction()).await;
}

/// Commits the transaction of `session`, once more when the result is unknown.
async fn commit(session: &mut ClientSession) -> mongodb::error::Result<()> {
    match session.commit_transaction().await {
        Err(e) if e.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT) => {
            session.commit_transaction().await
        }
        done => done,
    }
}

/// The error of a command in a transaction that was then aborted: whatever it did was
/// rolled back, so its outcome is known.
fn in_transaction(e: DbError) -> DbError {
    match e {
        DbError::Cancelled { .. } => DbError::Cancelled {
            outcome_unknown: false,
            was_in_tx: true,
        },
        DbError::Timeout { stage, .. } => DbError::Timeout {
            stage,
            outcome_unknown: false,
            was_in_tx: true,
        },
        DbError::ConnectionLost { message, .. } => DbError::ConnectionLost {
            was_in_tx: true,
            outcome_unknown: false,
            message,
        },
        other => other,
    }
}

/// The tree (spec §6.1), through the session's current `Client`.
#[async_trait]
impl Introspector for MongoSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        introspect::list_databases(&self.client())
            .await
            .map_err(|f| self.failed(f))
    }

    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(Vec::new())
    }

    async fn list_objects(
        &self,
        scope: &Scope,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        introspect::list_objects(&self.client(), &scope.database, kind)
            .await
            .map_err(|f| self.failed(f))
    }

    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(Vec::new())
    }

    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError> {
        introspect::table_detail(&self.client(), obj)
            .await
            .map_err(|f| self.failed(f))
    }

    async fn object_ddl(&self, obj: &ObjectRef) -> Result<String, DbError> {
        introspect::object_ddl(&self.client(), obj)
            .await
            .map_err(|f| self.failed(f))
    }
}

/// The data view (spec §6.2): commands built here, run by `execute_command`.
impl DataAccess for MongoSession {
    fn page(&self, obj: &ObjectRef, spec: &PageSpec) -> Result<ParsedCommand, DbError> {
        data::page(obj, spec)
    }

    fn count(
        &self,
        obj: &ObjectRef,
        filter: Option<&DataFilter>,
    ) -> Result<ParsedCommand, DbError> {
        data::count(obj, filter)
    }

    fn build_edits(
        &self,
        obj: &ObjectRef,
        detail: &TableDetail,
        edits: &EditSet,
    ) -> Result<Vec<ParsedCommand>, DbError> {
        data::build_edits(obj, detail, edits)
    }

    /// MongoDB has no keys.
    fn build_key_edit(&self, _db: &str, _edit: &KeyEdit) -> Result<Vec<ParsedCommand>, DbError> {
        Err(DbError::Unsupported {
            feature: "key edits on MongoDB".into(),
            version: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_aborted_transaction_applied_nothing() {
        let unknown = [
            DbError::Cancelled {
                outcome_unknown: true,
                was_in_tx: false,
            },
            DbError::Timeout {
                stage: ConnectStage::Execute,
                outcome_unknown: true,
                was_in_tx: false,
            },
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                message: "reset".into(),
            },
        ];
        for e in unknown {
            match in_transaction(e) {
                DbError::Cancelled {
                    outcome_unknown: false,
                    was_in_tx: true,
                }
                | DbError::Timeout {
                    outcome_unknown: false,
                    was_in_tx: true,
                    ..
                }
                | DbError::ConnectionLost {
                    outcome_unknown: false,
                    was_in_tx: true,
                    ..
                } => {}
                other => panic!("{other:?}"),
            }
        }
        // A server error stays as it is.
        let server = DbError::Server {
            code: Some("DocumentNotFound".into()),
            sqlstate: None,
            message: "m".into(),
            position: None,
        };
        assert!(matches!(
            in_transaction(server),
            DbError::Server { code: Some(c), .. } if c == "DocumentNotFound"
        ));
    }

    #[test]
    fn a_stopped_or_late_sample_sends_no_read() {
        assert_eq!(sample_ended_by(false, false), None);
        assert_eq!(sample_ended_by(true, true), Some(Trigger::Stop));
        assert_eq!(sample_ended_by(false, true), Some(Trigger::Stop));
        assert_eq!(sample_ended_by(true, false), Some(Trigger::Timeout));
        // A read's stop is a cancel whose outcome is known.
        assert!(matches!(
            Trigger::Stop.error(&CommandClass::Read),
            DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false
            }
        ));
        assert!(matches!(
            Trigger::Timeout.error(&CommandClass::Read),
            DbError::Timeout {
                outcome_unknown: false,
                ..
            }
        ));
    }
}
