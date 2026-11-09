//! The MongoDB session (spec §5.2): one `Client` per session, the database `use` moved to,
//! and the console's execution (`exec.rs`).

use std::future::IntoFuture;
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
    CancelHandle, Engine, ExecOptions, ExecProgress, Introspector, ResultSummary, RowSink,
    ServerNotices, Session, StmtOutcome, StopSignal,
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
use dnt_core::value::{ColumnMeta, FullValue, Value};
use mongodb::bson::{doc, Bson, Uuid};
use mongodb::error::{ErrorKind, WriteFailure, UNKNOWN_TRANSACTION_COMMIT_RESULT};
use mongodb::{Client, ClientSession};
use secrecy::{ExposeSecret, SecretString};

use crate::cancel::{self, MongoCanceller};
use crate::client::{self, Opened};
use crate::exec::{self, Fail};
use crate::shell::{MongoLanguage, Payload, ShellCommand};
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
    /// When the last command (or the server check of the connect) succeeded.
    last_ok: Mutex<Instant>,
    /// How long without a successful command makes the next one `ping` first
    /// ([`IDLE_PING_AFTER`]; tests set it with [`MongoSession::set_idle_after`]).
    idle_after: Mutex<Duration>,
    /// Called as an idle ping is answered ([`MongoSession::on_ping_answered`]).
    #[cfg(feature = "test-helpers")]
    ping_answered: Mutex<Option<PingHook>>,
}

#[cfg(feature = "test-helpers")]
type PingHook = Box<dyn Fn() + Send + Sync>;

/// Clears `running` when the command ends, however it ends.
struct RunningGuard<'a>(&'a Mutex<Option<Running>>);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Counts at most one reconnect for one `execute_command`, however it ends (as the SQL
/// drivers): an idle ping that reconnected and then the command's own dropped connection,
/// or a read dropped again when sent again, still replaced the connection for one
/// command. The counter only ever loses what this command added beyond one.
struct OneReconnect<'a> {
    counter: &'a AtomicU64,
    start: u64,
}

impl<'a> OneReconnect<'a> {
    fn new(counter: &'a AtomicU64) -> Self {
        OneReconnect {
            counter,
            start: counter.load(Ordering::SeqCst),
        }
    }
}

impl Drop for OneReconnect<'_> {
    fn drop(&mut self) {
        let extra = excess_reconnects(self.start, self.counter.load(Ordering::SeqCst));
        if extra > 0 {
            self.counter.fetch_sub(extra, Ordering::SeqCst);
        }
    }
}

/// What a command counted beyond one reconnect, from `start` to `now`.
fn excess_reconnects(start: u64, now: u64) -> u64 {
    now.saturating_sub(start).saturating_sub(1)
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
            last_ok: Mutex::new(Instant::now()),
            idle_after: Mutex::new(IDLE_PING_AFTER),
            #[cfg(feature = "test-helpers")]
            ping_answered: Mutex::new(None),
        }
    }

    /// Calls `f` each time an idle ping is answered, before the command goes on (a test
    /// raises a stop there, the race of a stop with the ping's answer).
    #[cfg(feature = "test-helpers")]
    pub fn on_ping_answered(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.ping_answered.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(f));
    }

    /// Sets how long without a successful command makes the next one `ping` first
    /// (`Duration::ZERO`: every command), so a test need not sit idle.
    #[cfg(feature = "test-helpers")]
    pub fn set_idle_after(&self, after: Duration) {
        *self.idle_after.lock().unwrap_or_else(|p| p.into_inner()) = after;
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
    ) -> Result<(exec::Output, bool), SendError> {
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
                Err(trigger.error(class).into())
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
    ) -> Result<ShellCommand, SendError> {
        let ShellCommand::Coll { db, coll, .. } = cmd else {
            return Err(DbError::Internal {
                message: "a typed filter belongs to a collection read".into(),
            }
            .into());
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
            return Err(trigger.error(&CommandClass::Read).into());
        }
        Ok(data::resolve(
            cmd,
            filter,
            &introspect::field_types(&out.docs),
        )?)
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
    ) -> Result<exec::Output, SendError> {
        match (res, after) {
            (Ok(out), _) => Ok(out),
            // Killed by this session's cancel. Without a stop or a timeout of ours, the kill
            // came from elsewhere (`killOp`, `killSessions`): a server error like any other
            // (`Server { code: "Interrupted" }`, spec §5.6).
            (Err(f), Some(trigger)) if f.interrupted() => Err(trigger.error(class).into()),
            (Err(Fail::Db(e)), _) => Err(e.into()),
            (Err(Fail::Mongo(e)), after) => Err(SendError {
                dropped: dropped_unprompted(&e, after),
                error: self.mapped(&e, class),
            }),
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

    fn mark_ok(&self) {
        *self.last_ok.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    /// Before a command after an idle spell ([`needs_ping`]): a `ping` on `admin` finds a
    /// connection a server or NAT dropped meanwhile before the command is sent, as the SQL
    /// drivers reconnect before sending on a connection known dead.
    ///
    /// A ping whose connection dropped (the crate clears the pool) or that got no answer
    /// within [`ping_limit`] (the `Client` is rebuilt) is sent once more, on a fresh
    /// connection, counting one reconnect: answered, the command goes on. Failing again,
    /// it is `ConnectionLost` with the outcome known, since nothing was sent. A stop
    /// meanwhile is a cancel, nothing sent. `Ok(true)`: it reconnected.
    async fn wake(&self, opts: &ExecOptions) -> Result<bool, DbError> {
        let idle = self
            .last_ok
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed();
        let after = *self.idle_after.lock().unwrap_or_else(|p| p.into_inner());
        if !needs_ping(idle, after) {
            return Ok(false);
        }
        let limit = ping_limit(opts.timeout, self.cfg.connect_timeout);
        let cause = match self.ping_once(opts, limit).await {
            Pinged::Answered => return Ok(false),
            Pinged::Stopped => return Err(DbError::cancelled()),
            Pinged::Failed(e) => return Err(e),
            Pinged::Dropped(text) => {
                self.reconnects.fetch_add(1, Ordering::SeqCst);
                format!("the idle connection was dropped ({text})")
            }
            Pinged::NoAnswer => {
                self.rebuild().await;
                format!("the idle connection did not answer a ping within {limit:?}")
            }
        };
        let lost = |message: String| DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: false,
            message,
        };
        match self.ping_once(opts, limit).await {
            Pinged::Answered => Ok(true),
            Pinged::Stopped => Err(DbError::cancelled()),
            Pinged::Failed(e) => Err(e),
            Pinged::Dropped(text) => Err(lost(format!("{cause}; reconnecting failed: {text}"))),
            Pinged::NoAnswer => Err(lost(format!(
                "{cause}; the new connection did not answer within {limit:?} either"
            ))),
        }
    }

    /// One `ping` on `admin`, within `limit` and until `opts.stop`.
    async fn ping_once(&self, opts: &ExecOptions, limit: Duration) -> Pinged {
        let client = self.client();
        let admin = client.database("admin");
        let ping = admin.run_command(doc! { "ping": 1 }).into_future();
        tokio::select! {
            biased;
            res = ping => match res {
                Ok(_) => {
                    self.mark_ok();
                    #[cfg(feature = "test-helpers")]
                    if let Some(f) = &*self.ping_answered.lock().unwrap_or_else(|p| p.into_inner()) {
                        f();
                    }
                    Pinged::Answered
                }
                Err(e) if exec::connection_dropped(&e) => Pinged::Dropped(exec::error_text(&e)),
                Err(e) => Pinged::Failed(self.mapped(&e, &CommandClass::Read)),
            },
            () = stop_raised(&opts.stop) => Pinged::Stopped,
            () = tokio::time::sleep(limit) => Pinged::NoAnswer,
        }
    }

    /// Sends the command of `payload` (a typed read's sample first) and waits for its
    /// result, as [`MongoSession::run_watched`] does.
    async fn send(
        &self,
        payload: &Payload,
        class: &CommandClass,
        opts: &ExecOptions,
        comment: &str,
        sink: &mut dyn RowSink,
    ) -> Result<Sent, SendError> {
        let typed;
        let shell = match &payload.typed {
            None => &payload.cmd,
            Some(filter) => {
                typed = self
                    .typed_read(&payload.cmd, filter, opts, comment, sink)
                    .await?;
                &typed
            }
        };
        let plan = exec::plan(shell, &self.db(), opts, comment)?;
        let moves_to = (plan.shape == exec::Shape::Use).then(|| plan.db.clone());
        let (output, late) = self.run_watched(plan, opts, class, sink, None).await?;
        Ok(Sent {
            output,
            late,
            moves_to,
        })
    }
}

/// Why sending a command failed: its error, and whether the connection dropped under it
/// ([`exec::connection_dropped`]).
struct SendError {
    error: DbError,
    dropped: bool,
}

impl From<DbError> for SendError {
    fn from(error: DbError) -> Self {
        SendError {
            error,
            dropped: false,
        }
    }
}

/// What a sent command gave: its result, whether it completed late (after a stop or a
/// timeout), and the database a `use` moves to.
struct Sent {
    output: exec::Output,
    late: bool,
    moves_to: Option<String>,
}

/// How a [`MongoSession::ping_once`] ended.
enum Pinged {
    Answered,
    /// The connection dropped under it: the error's (redacted) text.
    Dropped(String),
    /// No answer within the limit.
    NoAnswer,
    Stopped,
    /// Any other error (no server to select, ...).
    Failed(DbError),
}

/// Whether `e` is a connection that dropped under the command with no stop or timeout of
/// ours first (`after`): a command already being cancelled is never sent again.
fn dropped_unprompted(e: &mongodb::error::Error, after: Option<Trigger>) -> bool {
    after.is_none() && exec::connection_dropped(e)
}

/// Whether a command `idle` after the last successful one `ping`s first.
fn needs_ping(idle: Duration, after: Duration) -> bool {
    idle >= after
}

/// How long the idle ping waits: the command's `timeout`, but never longer than the
/// connect timeout (a connection that does not answer that long is given up).
fn ping_limit(timeout: Option<Duration>, connect: Duration) -> Duration {
    timeout.map_or(connect, |t| t.min(connect))
}

/// Whether a command whose connection dropped under it is sent again, once, on a fresh
/// connection: only a read, and only while nothing of its result reached the sink (as the
/// SQL drivers re-run a read that emitted nothing), and not when the idle ping already
/// reconnected for it (one reconnect per command, as the SQL drivers). A command that may
/// write is never sent again: it may have been applied.
fn may_resend(class: &CommandClass, emitted: bool, reconnected: bool) -> bool {
    *class == CommandClass::Read && !emitted && !reconnected
}

/// Passes everything on to `sink`, noting whether any of a result reached it.
struct Watched<'a> {
    sink: &'a mut dyn RowSink,
    emitted: bool,
}

impl RowSink for Watched<'_> {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.emitted = true;
        self.sink.result_start(cols);
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.emitted = true;
        self.sink.rows(batch);
    }
    fn full_rows(&mut self, batch: Vec<Vec<FullValue>>) {
        self.emitted = true;
        self.sink.full_rows(batch);
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.sink.result_end(summary);
    }
    fn progress(&mut self, p: ExecProgress) {
        self.sink.progress(p);
    }
    fn server_notices(&mut self, notices: ServerNotices) {
        self.sink.server_notices(notices);
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

/// A command after this long without a successful one `ping`s first: a server or NAT may
/// have dropped the idle connection meanwhile.
const IDLE_PING_AFTER: Duration = Duration::from_secs(30);

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

    /// Runs one console command, as the SQL drivers do after a dropped connection: after
    /// [`IDLE_PING_AFTER`] without a successful command it `ping`s first
    /// ([`MongoSession::wake`]), and a read whose connection dropped under it before any of
    /// its result reached `sink` is sent again once on a fresh connection
    /// ([`may_resend`]), unless the ping already reconnected for it or a stop or timeout
    /// of ours came first. Either reconnect is counted in `reconnects()`. Nothing else is
    /// ever sent again: a command of any other class whose connection dropped is
    /// `ConnectionLost` with the outcome unknown. A server that cannot be selected sent
    /// nothing: `ConnectionLost` with the outcome known, whatever the class.
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
        let _once = OneReconnect::new(&self.reconnects);
        let reconnected = self.wake(opts).await?;
        // A stop that came with the ping's answer: still nothing sent.
        if opts.stop.is_stopped() {
            return Err(DbError::cancelled());
        }
        let mut sink = Watched {
            sink,
            emitted: false,
        };
        let sent = match self
            .send(payload, &cmd.class, opts, &comment, &mut sink)
            .await
        {
            // The crate cleared the pool: a read goes again on a fresh connection, once
            // (the first failure counted the reconnect).
            Err(e) if e.dropped && may_resend(&cmd.class, sink.emitted, reconnected) => {
                if opts.stop.is_stopped() {
                    return Err(DbError::cancelled());
                }
                self.send(payload, &cmd.class, opts, &comment, &mut sink)
                    .await
            }
            sent => sent,
        };
        let Sent {
            output,
            late,
            moves_to,
        } = sent.map_err(|e| {
            if payload.edit {
                edit_error(e.error)
            } else {
                e.error
            }
        })?;
        self.mark_ok();
        // An edit applied on its own (no transactions): a document gone stops the apply.
        if let Some(id) = &payload.document {
            data::check_found(id, output.affected)?;
        }
        if let Some(db) = moves_to {
            *self.current_db.lock().unwrap_or_else(|p| p.into_inner()) = db;
        }
        let summary = exec::emit(&mut sink, output, opts, started)?;
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
    /// - Nothing is sent again after a dropped connection (no idle `ping` either, unlike
    ///   `execute_command`): a failed command aborts the transaction as above.
    /// - A commit whose result is unknown (`UnknownTransactionCommitResult`) is retried
    ///   once, the only retry of a write (commit is idempotent by `txnNumber`); still
    ///   unknown, the failure is `ConnectionLost { outcome_unknown: true }` at the last
    ///   index, not rolled back. So is a commit with a write concern
    ///   error (it committed, but the write concern was not met), which is not retried.
    ///   Any other commit error is known not to have committed: rolled back. An abort's
    ///   single retry is the crate's own (it retries `abortTransaction` once and ignores
    ///   its errors, as the transactions spec says; a second `abort_transaction` call is
    ///   refused by the crate).
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
                let unknown = commit_unknown(&e);
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
            .await
            .map_err(|e| e.error)?;
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

/// The error of a data-view edit applied on its own: a write concern error means the
/// write was applied but its write concern was not met, so its outcome is unknown (as a
/// commit's with one), never "not applied".
fn edit_error(e: DbError) -> DbError {
    if exec::is_write_concern_error(&e) {
        DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: true,
            message: format!("the write's outcome is unknown: {e}"),
        }
    } else {
        e
    }
}

/// Whether a failed commit may have committed: the crate's `UnknownTransactionCommitResult`
/// label, or any write concern error (the commit was applied but the write concern was not
/// met: the crate labels only some codes, not `UnknownReplWriteConcern` or
/// `UnsatisfiableWriteConcern`).
fn commit_unknown(e: &mongodb::error::Error) -> bool {
    e.contains_label(UNKNOWN_TRANSACTION_COMMIT_RESULT)
        || matches!(
            e.kind.as_ref(),
            ErrorKind::Write(WriteFailure::WriteConcernError(_))
        )
}

/// Commits the transaction of `session`, once more when the result is unknown by the
/// crate's label (a write concern error without it is not retried).
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
    fn an_edit_with_a_write_concern_error_may_have_applied() {
        let server = |message: &str| DbError::Server {
            code: Some("UnsatisfiableWriteConcern".into()),
            sqlstate: None,
            message: message.into(),
            position: None,
        };
        match edit_error(server("Write concern error: not enough nodes")) {
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                message,
            } => assert!(
                message.contains("Write concern error: not enough nodes"),
                "{message}"
            ),
            other => panic!("{other:?}"),
        }
        // Any other error of an edit stays as it is (it was not applied).
        assert!(matches!(
            edit_error(server("E11000 duplicate key error")),
            DbError::Server { .. }
        ));
    }

    #[test]
    fn an_idle_session_pings_first() {
        let after = IDLE_PING_AFTER;
        assert_eq!(after, Duration::from_secs(30));
        assert!(!needs_ping(Duration::ZERO, after));
        assert!(!needs_ping(after - Duration::from_millis(1), after));
        assert!(needs_ping(after, after));
        assert!(needs_ping(Duration::from_secs(3600), after));
        // A test's zero: every command pings.
        assert!(needs_ping(Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn only_a_read_that_emitted_nothing_is_sent_again() {
        assert!(may_resend(&CommandClass::Read, false, false));
        assert!(!may_resend(&CommandClass::Read, true, false));
        // The idle ping already reconnected for this command: one reconnect at most.
        assert!(!may_resend(&CommandClass::Read, false, true));
        for class in [
            CommandClass::Write,
            CommandClass::Admin,
            CommandClass::Ddl,
            CommandClass::Unknown,
            CommandClass::SessionChange,
        ] {
            for (emitted, reconnected) in [(false, false), (true, false), (false, true)] {
                assert!(!may_resend(&class, emitted, reconnected), "{class:?}");
            }
        }
    }

    #[test]
    fn the_ping_waits_for_the_shorter_limit() {
        let connect = Duration::from_secs(10);
        assert_eq!(ping_limit(None, connect), connect);
        assert_eq!(
            ping_limit(Some(Duration::from_secs(2)), connect),
            Duration::from_secs(2)
        );
        assert_eq!(ping_limit(Some(Duration::from_secs(60)), connect), connect);
    }

    #[test]
    fn one_reconnect_is_counted_per_command() {
        assert_eq!(excess_reconnects(0, 0), 0);
        assert_eq!(excess_reconnects(0, 1), 0);
        assert_eq!(excess_reconnects(0, 2), 1);
        assert_eq!(excess_reconnects(5, 8), 2);
        // The idle ping reconnected (one), then the command's own connection dropped
        // (`mapped` counts another): the command adds one.
        let counter = AtomicU64::new(3);
        {
            let _once = OneReconnect::new(&counter);
            counter.fetch_add(1, Ordering::SeqCst);
            counter.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 4);
        // One reconnect, or none, stays as counted.
        {
            let _once = OneReconnect::new(&counter);
            counter.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 5);
        {
            let _once = OneReconnect::new(&counter);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn a_dropped_connection_is_told_apart() {
        use mongodb::error::Error;
        let io = Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(exec::connection_dropped(&io));
        let other = Error::custom("not a network error");
        assert!(!exec::connection_dropped(&other));
        // Only a drop no stop or timeout of ours came first is one to send again on.
        assert!(dropped_unprompted(&io, None));
        assert!(!dropped_unprompted(&io, Some(Trigger::Timeout)));
        assert!(!dropped_unprompted(&io, Some(Trigger::Stop)));
        assert!(!dropped_unprompted(&other, None));
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
