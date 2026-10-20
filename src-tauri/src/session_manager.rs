//! `SessionManager`: the staged connect pipeline and one `ConnectionRuntime` per open
//! connection (tunnel, dedicated metadata sessions, `NodeCache`, `SchemaCache`, and the
//! query tabs' sessions of [`crate::query_sessions`]).
//!
//! Introspection only ever runs on metadata sessions, never on a user's query session:
//! MySQL keeps one (key `None`); PostgreSQL one per database (key `Some(db)`), because its
//! catalogs are per database. The session opened by the connect pipeline becomes the
//! metadata session of the connect database. Drivers do not reconnect during
//! introspection, so a call that fails with a lost connection (or finds the SSH tunnel
//! dead) reopens the tunnel and that session once and is retried once.

use crate::cache::{CompletionObject, NodeCache, NodeKey, SchemaCache, SchemaCacheTable};
use crate::commands::browse::{folder_key, object_key};
use crate::data_sessions::DataSessions;
use crate::edit_plans::{EditPlanStore, LoadedStore, LOADED_TTL, MAX_FETCH_BYTES};
use crate::query_sessions::{
    close_slot, PlanStore, QuerySessions, Runs, PLAN_TTL, SLOT_CLOSE_WAIT,
};
use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode, SshAuth, SshConfig};
use dnt_core::driver::{
    Driver, ExecOptions, Introspector, ResultSummary, RowSink, Session, DEFAULT_CANCEL_GRACE,
};
pub use dnt_core::error::StageTiming;
use dnt_core::error::{ssh_channel_error, ConnectStage, DbError, SshStage};
use dnt_core::meta::{ColumnBrief, ObjectKind, ObjectRef, Scope, TableDetail};
use dnt_core::secret::{SecretKind, SecretStore};
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::value::{ColumnMeta, Value};
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_tunnel::{KnownHosts, SshSecrets, Tunnel};
use secrecy::SecretString;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use uuid::Uuid;

pub use dnt_core::error::PASSWORD_REQUIRED;

/// Used when the connection sets no application name.
const DEFAULT_APPLICATION_NAME: &str = "DntDatabase";

/// How long closing a session that is probably dead may take before it is just dropped.
const CLOSE_DEAD_TIMEOUT: Duration = Duration::from_secs(2);

/// Default bound of one metadata call (drivers set no socket read timeouts). On expiry the
/// session is dropped and `Timeout{Execute}` returned, without a retry.
pub const METADATA_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a failed SchemaCache load is remembered: callers within this window get the
/// same error without a new load; the first caller after it retries.
pub const SCHEMA_ERROR_TTL: Duration = Duration::from_secs(10);

/// How long `close` waits for an in-flight metadata call to release the runtime before it
/// marks the runtime closed and tears down what it can reach without it.
const SHUTDOWN_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Returned by metadata calls on a runtime that was closed (also to calls that were
/// waiting while `close` ran): the connection is gone and is not reopened.
pub(crate) fn closed_error() -> DbError {
    DbError::ConnectionLost {
        was_in_tx: false,
        outcome_unknown: false,
        message: "the connection was closed".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    Disconnected,
    Connecting { stage: ConnectStage },
    Connected,
    Reconnecting,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct OpenResult {
    pub info: ServerInfo,
    pub timings: Vec<StageTiming>,
}

/// Secrets typed by the user for `SecretMode::Ask` (or to fill in missing saved ones).
pub type PromptSecrets = Option<NewSecrets>;

/// What a metadata call returns: a boxed `Send` future borrowing only the session.
///
/// Browse calls take `Fn(&dyn Introspector) -> MetaFuture` rather than an async closure:
/// rustc cannot prove an async closure's future `Send` for every session lifetime when it
/// captures references ("`Send` is not general enough"), and Tauri commands and the
/// SchemaCache task need `Send` futures. Capture owned values and clone them per call:
/// `move |i| { let scope = scope.clone(); Box::pin(async move { i.list_objects(&scope, k).await }) }`.
pub type MetaFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>;

fn password_required() -> DbError {
    DbError::Secret {
        message: PASSWORD_REQUIRED.into(),
    }
}

/// Locks `m`, recovering the data of a poisoned mutex (a panic elsewhere must not
/// take the connection or query state down with it). Shared by the backend modules.
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Secrets of one connection attempt, kept by the runtime to reopen tunnel and sessions.
#[derive(Default)]
struct ConnSecrets {
    db_password: Option<SecretString>,
    ssh_password: Option<SecretString>,
    ssh_key_passphrase: Option<SecretString>,
}

impl From<NewSecrets> for ConnSecrets {
    fn from(s: NewSecrets) -> Self {
        ConnSecrets {
            db_password: s.db_password,
            ssh_password: s.ssh_password,
            ssh_key_passphrase: s.ssh_key_passphrase,
        }
    }
}

impl ConnSecrets {
    fn ssh(&self) -> SshSecrets {
        SshSecrets {
            password: self.ssh_password.clone(),
            key_passphrase: self.ssh_key_passphrase.clone(),
        }
    }

    /// SSH password authentication cannot work without a password: ask for it up front.
    /// (Whether a key needs a passphrase is only known once the tunnel loads it.)
    fn check_ssh(&self, cfg: &ConnectionConfig) -> Result<(), DbError> {
        match &cfg.ssh {
            Some(ssh) if ssh.auth == SshAuth::Password && self.ssh_password.is_none() => {
                Err(password_required())
            }
            _ => Ok(()),
        }
    }
}

/// What the connect pipeline produces.
struct Opened {
    driver: Arc<dyn Driver>,
    tunnel: Option<Tunnel>,
    resolved: ResolvedConfig,
    session: Box<dyn Session>,
    info: ServerInfo,
}

impl Opened {
    async fn close(self) {
        close_quietly(self.session).await;
        if let Some(tunnel) = self.tunnel {
            tunnel.close().await;
        }
    }
}

fn record(timings: &mut Vec<StageTiming>, stage: ConnectStage, since: Instant) {
    timings.push(StageTiming {
        stage,
        ms: u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
}

async fn open_tunnel(
    ssh: &SshConfig,
    cfg: &ConnectionConfig,
    secrets: &ConnSecrets,
    known_hosts: &KnownHosts,
) -> Result<Tunnel, DbError> {
    Tunnel::open(
        ssh,
        secrets.ssh(),
        (cfg.host.clone(), cfg.port),
        known_hosts,
        Duration::from_secs(u64::from(cfg.options.ssh_timeout_s)),
    )
    .await
}

/// The address to dial is the tunnel's local end when there is one; TLS still verifies
/// the configured host name.
fn resolved_config(
    cfg: &ConnectionConfig,
    secrets: &ConnSecrets,
    tunnel: Option<&Tunnel>,
) -> ResolvedConfig {
    let (host, port) = match tunnel {
        Some(t) => ("127.0.0.1".to_string(), t.local_addr().port()),
        None => (cfg.host.clone(), cfg.port),
    };
    ResolvedConfig {
        kind: cfg.kind,
        host,
        port,
        tls_server_name: cfg.host.clone(),
        username: cfg.username.clone(),
        password: secrets.db_password.clone(),
        database: cfg.default_db.clone(),
        ssl: cfg.ssl.clone(),
        connect_timeout: Duration::from_secs(u64::from(cfg.options.connect_timeout_s)),
        read_only: cfg.read_only,
        application_name: cfg
            .options
            .application_name
            .clone()
            .unwrap_or_else(|| DEFAULT_APPLICATION_NAME.into()),
    }
}

/// `base` pointed at another database.
pub(crate) fn with_database(base: &ResolvedConfig, database: Option<String>) -> ResolvedConfig {
    ResolvedConfig {
        kind: base.kind,
        host: base.host.clone(),
        port: base.port,
        tls_server_name: base.tls_server_name.clone(),
        username: base.username.clone(),
        password: base.password.clone(),
        database,
        ssl: base.ssl.clone(),
        connect_timeout: base.connect_timeout,
        read_only: base.read_only,
        application_name: base.application_name.clone(),
    }
}

/// Driver connect bounded by the configured timeout, reported as the `ConnectDb` stage.
pub(crate) async fn connect_db(
    driver: &dyn Driver,
    resolved: &ResolvedConfig,
) -> Result<Box<dyn Session>, DbError> {
    tokio::time::timeout(resolved.connect_timeout, driver.connect(resolved))
        .await
        .map_err(|_| DbError::timeout(ConnectStage::ConnectDb))?
}

/// Host names compare case-insensitively, ignoring surrounding blanks.
fn same_host(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn same_ssh(a: &SshConfig, b: &SshConfig) -> bool {
    same_host(&a.host, &b.host) && a.port == b.port && a.user == b.user
}

/// Whether `cfg` (as typed) still takes the whole network path of `stored` to the same
/// server as the same user: engine, database host, port and user, whether SSH is used,
/// and, with SSH, the bastion's host, port and user. Gates the stored database password.
/// Turning SSH on or off is a different target: `host:port` then names another machine.
fn same_target(stored: &ConnectionConfig, cfg: &ConnectionConfig) -> bool {
    let ssh = match (&stored.ssh, &cfg.ssh) {
        (None, None) => true,
        (Some(a), Some(b)) => same_ssh(a, b),
        _ => false,
    };
    stored.kind == cfg.kind
        && same_host(&stored.host, &cfg.host)
        && stored.port == cfg.port
        && stored.username == cfg.username
        && ssh
}

/// Whether both configs use SSH through the same bastion host, port and user. Gates the
/// stored SSH password and key passphrase, which only ever reach the bastion.
fn same_ssh_endpoint(stored: &ConnectionConfig, cfg: &ConnectionConfig) -> bool {
    matches!((&stored.ssh, &cfg.ssh), (Some(a), Some(b)) if same_ssh(a, b))
}

/// Opening a saved connection whose SSH key is encrypted and whose passphrase is not
/// stored: ask for it (`password_required`) instead of failing the open.
fn ask_for_missing_key_passphrase(e: DbError) -> DbError {
    let missing = matches!(
        e.unstaged(),
        DbError::Ssh { stage: SshStage::Auth, message } if message == dnt_tunnel::KEY_PASSPHRASE_REQUIRED
    );
    match e {
        DbError::AtStage { stage, timings, .. } if missing => {
            DbError::at_stage(stage, timings, password_required())
        }
        e => e,
    }
}

/// Errors after which the metadata session (and tunnel) is worth reopening once.
pub(crate) fn is_lost(e: &DbError) -> bool {
    matches!(e, DbError::ConnectionLost { .. } | DbError::Connect { .. })
}

pub(crate) async fn close_quietly(session: Box<dyn Session>) {
    let _ = tokio::time::timeout(CLOSE_DEAD_TIMEOUT, session.close()).await;
}

pub struct SessionManager {
    drivers: HashMap<DbKind, Arc<dyn Driver>>,
    pub(crate) repo: Arc<ConnectionRepo>,
    secrets: Arc<dyn SecretStore>,
    known_hosts: KnownHosts,
    runtimes: RwLock<HashMap<Uuid, Arc<ConnectionRuntime>>>,
    /// State of connections being opened or that failed to open (no runtime).
    pending: Mutex<HashMap<Uuid, ConnState>>,
    /// Stage timings of the last `open` of each connection, successful or not.
    timings: Mutex<HashMap<Uuid, Vec<StageTiming>>>,
    /// Bumped by every `open` and `close` of a connection: an `open` whose generation is no
    /// longer current was superseded and must not install its runtime or touch `pending`.
    generations: Mutex<HashMap<Uuid, u64>>,
    metadata_timeout: Duration,
    schema_error_ttl: Duration,
    /// Run plans of query tabs (see `query_sessions`).
    pub(crate) plans: PlanStore,
    /// Edit plans of the data view (see `edit_plans`); they live for `plan_ttl` too.
    pub(crate) edit_plans: EditPlanStore,
    /// Complete cell values by token (`data_cell_fetch`); each lives `loaded_ttl` after
    /// its last use.
    pub(crate) loaded: LoadedStore,
    pub(crate) loaded_ttl: Duration,
    /// Largest complete value a fetch or file load takes.
    pub(crate) cell_fetch_cap: usize,
    pub(crate) plan_ttl: Duration,
    /// Runs in progress (see `query_sessions`).
    pub(crate) runs: Runs,
    /// `ExecOptions::cancel_grace` of query runs.
    pub(crate) cancel_grace: Duration,
    /// Table data requests (see `data_sessions`).
    pub(crate) data_timeout: Duration,
    pub(crate) data_cell_cap: usize,
    pub(crate) data_page_cap: usize,
    /// How long `close` waits for a running edit apply before it cancels it.
    pub(crate) apply_close_wait: Duration,
}

/// Resets `pending` of an `open` that did not finish (its future was dropped).
struct PendingGuard<'a> {
    sm: &'a SessionManager,
    id: Uuid,
    generation: u64,
    done: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.sm.remove_pending_if_current(self.id, self.generation);
        }
    }
}

impl SessionManager {
    pub fn new(
        repo: Arc<ConnectionRepo>,
        secrets: Arc<dyn SecretStore>,
        known_hosts: KnownHosts,
        drivers: HashMap<DbKind, Arc<dyn Driver>>,
    ) -> Self {
        SessionManager {
            drivers,
            repo,
            secrets,
            known_hosts,
            runtimes: RwLock::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            timings: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            metadata_timeout: METADATA_CALL_TIMEOUT,
            schema_error_ttl: SCHEMA_ERROR_TTL,
            plans: PlanStore::default(),
            edit_plans: EditPlanStore::default(),
            loaded: LoadedStore::default(),
            loaded_ttl: LOADED_TTL,
            cell_fetch_cap: MAX_FETCH_BYTES,
            plan_ttl: PLAN_TTL,
            runs: Runs::default(),
            cancel_grace: DEFAULT_CANCEL_GRACE,
            data_timeout: crate::data_sessions::DATA_READ_TIMEOUT,
            data_cell_cap: crate::data_sessions::MAX_CELL_BYTES,
            data_page_cap: crate::data_sessions::MAX_PAGE_BYTES,
            apply_close_wait: SLOT_CLOSE_WAIT,
        }
    }

    /// Overrides [`METADATA_CALL_TIMEOUT`] for runtimes opened afterwards (tests).
    pub fn with_metadata_timeout(mut self, timeout: Duration) -> Self {
        self.metadata_timeout = timeout;
        self
    }

    /// Overrides [`SCHEMA_ERROR_TTL`] for runtimes opened afterwards (tests).
    pub fn with_schema_error_ttl(mut self, ttl: Duration) -> Self {
        self.schema_error_ttl = ttl;
        self
    }

    /// Starts a new generation of `id`, superseding any `open` in flight.
    fn bump_generation(&self, id: Uuid) -> u64 {
        let mut generations = lock(&self.generations);
        let g = generations.entry(id).or_insert(0);
        *g += 1;
        *g
    }

    fn is_current(generations: &HashMap<Uuid, u64>, id: Uuid, generation: u64) -> bool {
        generations.get(&id) == Some(&generation)
    }

    /// Sets `pending` only while `generation` is current (a superseded open stays silent).
    fn set_pending_if_current(&self, id: Uuid, generation: u64, state: ConnState) {
        let generations = lock(&self.generations);
        if Self::is_current(&generations, id, generation) {
            lock(&self.pending).insert(id, state);
        }
    }

    fn remove_pending_if_current(&self, id: Uuid, generation: u64) {
        let generations = lock(&self.generations);
        if Self::is_current(&generations, id, generation) {
            lock(&self.pending).remove(&id);
        }
    }

    /// Runs the connect pipeline for an unsaved config with the given secrets, then closes
    /// everything again. Nothing is stored and no runtime is kept.
    ///
    /// For a saved connection (non-nil id, `Saved` mode) secrets the user did not type fall
    /// back to the stored ones, each only while its target is unchanged: the database
    /// password while the whole path is ([`same_target`]: engine, host, port, user, SSH on
    /// or off and the bastion), SSH secrets while the bastion's host, port and user are
    /// ([`same_ssh_endpoint`]). An edited target never gets the stored secret. Typed
    /// secrets always win. They never leave the backend.
    pub async fn test(
        &self,
        cfg: &ConnectionConfig,
        secrets: NewSecrets,
    ) -> Result<OpenResult, DbError> {
        cfg.validate()?;
        let mut timings = Vec::new();
        let started = Instant::now();
        let secrets = self
            .test_secrets(cfg, secrets)
            .map_err(|e| DbError::at_stage(ConnectStage::ResolveSecrets, Vec::new(), e))?;
        record(&mut timings, ConnectStage::ResolveSecrets, started);
        let opened = self
            .connect_pipeline(cfg, &secrets, &mut timings, &|_| {})
            .await?;
        let info = opened.info.clone();
        opened.close().await;
        Ok(OpenResult { info, timings })
    }

    /// The secrets `test` connects with: typed ones, completed from the store (see `test`).
    fn test_secrets(
        &self,
        cfg: &ConnectionConfig,
        secrets: NewSecrets,
    ) -> Result<ConnSecrets, DbError> {
        let mut secrets = ConnSecrets::from(secrets);
        let stored = (cfg.secret_mode == SecretMode::Saved && !cfg.id.is_nil())
            .then(|| self.repo.get(cfg.id).ok())
            .flatten();
        if let Some(stored) = stored {
            let fill = |slot: &mut Option<SecretString>, kind| -> Result<(), DbError> {
                if slot.is_none() {
                    *slot = self.secrets.get(cfg.id, kind)?;
                }
                Ok(())
            };
            if same_target(&stored, cfg) {
                fill(&mut secrets.db_password, SecretKind::DbPassword)?;
            }
            if same_ssh_endpoint(&stored, cfg) {
                fill(&mut secrets.ssh_password, SecretKind::SshPassword)?;
                fill(
                    &mut secrets.ssh_key_passphrase,
                    SecretKind::SshKeyPassphrase,
                )?;
            }
        }
        secrets.check_ssh(cfg)?;
        Ok(secrets)
    }

    /// Opens a saved connection. Opening one that is already connected returns its info
    /// and the timings of the open that connected it.
    ///
    /// A `close` (or a newer `open`) of the same connection while this one is in flight
    /// supersedes it: what it opened is closed again and it returns `DbError::Cancelled`.
    pub async fn open(&self, id: Uuid, prompt: PromptSecrets) -> Result<OpenResult, DbError> {
        if let Some(rt) = self.runtime_opt(id) {
            if rt.state() == ConnState::Connected {
                return Ok(OpenResult {
                    info: rt.info.clone(),
                    timings: self.last_timings(id).unwrap_or_default(),
                });
            }
            self.close(id).await;
        }
        let generation = self.bump_generation(id);
        let mut guard = PendingGuard {
            sm: self,
            id,
            generation,
            done: false,
        };
        self.set_pending_if_current(
            id,
            generation,
            ConnState::Connecting {
                stage: ConnectStage::ResolveSecrets,
            },
        );
        let mut timings = Vec::new();
        let result = self
            .open_pipeline(id, generation, prompt, &mut timings)
            .await;
        lock(&self.timings).insert(id, timings.clone());
        let (cfg, secrets, opened) = match result {
            Ok(r) => r,
            Err(e) => {
                guard.done = true;
                self.set_pending_if_current(id, generation, ConnState::Failed);
                return Err(e);
            }
        };
        let info = opened.info.clone();
        let rt = Arc::new(ConnectionRuntime::new(
            cfg,
            secrets,
            opened,
            self.known_hosts.clone(),
            self.metadata_timeout,
            self.schema_error_ttl,
        ));
        // Checked and installed under the generations lock, so a `close` either supersedes
        // this open or finds (and closes) its runtime.
        let installed = {
            let generations = lock(&self.generations);
            Self::is_current(&generations, id, generation).then(|| {
                lock(&self.pending).remove(&id);
                self.runtimes
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(id, Arc::clone(&rt))
            })
        };
        guard.done = true;
        let Some(replaced) = installed else {
            rt.shutdown(SLOT_CLOSE_WAIT).await;
            return Err(DbError::cancelled());
        };
        if let Some(old) = replaced {
            // Cannot normally happen (every open/close bumps the generation); never leak it.
            old.shutdown(SLOT_CLOSE_WAIT).await;
        }
        if let Err(e) = self.repo.touch_connected(id) {
            tracing::warn!(connection = %id, error = %e, "could not record the connection time");
        }
        // Warm the cache of the database a new query tab starts on.
        if let Some(home) = rt.home_database() {
            rt.spawn_schema_load(home);
        }
        Ok(OpenResult { info, timings })
    }

    async fn open_pipeline(
        &self,
        id: Uuid,
        generation: u64,
        prompt: PromptSecrets,
        timings: &mut Vec<StageTiming>,
    ) -> Result<(ConnectionConfig, ConnSecrets, Opened), DbError> {
        let started = Instant::now();
        let cfg = self.repo.get(id)?;
        let secrets = self
            .resolve_secrets(&cfg, prompt)
            .map_err(|e| DbError::at_stage(ConnectStage::ResolveSecrets, Vec::new(), e))?;
        record(timings, ConnectStage::ResolveSecrets, started);
        let report =
            |stage| self.set_pending_if_current(id, generation, ConnState::Connecting { stage });
        let opened = self
            .connect_pipeline(&cfg, &secrets, timings, &report)
            .await
            .map_err(ask_for_missing_key_passphrase)?;
        Ok((cfg, secrets, opened))
    }

    /// Ask mode: everything comes from `prompt`, which must carry the database password.
    /// Saved mode: prompted values win, the rest comes from the secret store (a missing
    /// database password is allowed: some servers have none).
    fn resolve_secrets(
        &self,
        cfg: &ConnectionConfig,
        prompt: PromptSecrets,
    ) -> Result<ConnSecrets, DbError> {
        let secrets = match cfg.secret_mode {
            SecretMode::Ask => {
                let prompt = prompt.ok_or_else(password_required)?;
                if prompt.db_password.is_none() {
                    return Err(password_required());
                }
                ConnSecrets::from(prompt)
            }
            SecretMode::Saved => {
                let prompt = prompt.map(ConnSecrets::from).unwrap_or_default();
                let stored = |given: Option<SecretString>, kind| match given {
                    Some(s) => Ok(Some(s)),
                    None => self.secrets.get(cfg.id, kind),
                };
                ConnSecrets {
                    db_password: stored(prompt.db_password, SecretKind::DbPassword)?,
                    ssh_password: stored(prompt.ssh_password, SecretKind::SshPassword)?,
                    ssh_key_passphrase: stored(
                        prompt.ssh_key_passphrase,
                        SecretKind::SshKeyPassphrase,
                    )?,
                }
            }
        };
        secrets.check_ssh(cfg)?;
        Ok(secrets)
    }

    /// `OpenTunnel` (with SSH), `ConnectDb` and `DetectServer`, each timed. Errors are
    /// wrapped with the failing stage and the timings of the completed ones
    /// ([`DbError::AtStage`]); anything opened is closed again.
    async fn connect_pipeline(
        &self,
        cfg: &ConnectionConfig,
        secrets: &ConnSecrets,
        timings: &mut Vec<StageTiming>,
        report: &(dyn Fn(ConnectStage) + Sync),
    ) -> Result<Opened, DbError> {
        let driver = self.driver(cfg.kind)?;
        let tunnel = match &cfg.ssh {
            Some(ssh) => {
                report(ConnectStage::OpenTunnel);
                let started = Instant::now();
                let tunnel = open_tunnel(ssh, cfg, secrets, &self.known_hosts)
                    .await
                    .map_err(|e| DbError::at_stage(ConnectStage::OpenTunnel, timings.clone(), e))?;
                record(timings, ConnectStage::OpenTunnel, started);
                Some(tunnel)
            }
            None => None,
        };

        report(ConnectStage::ConnectDb);
        let resolved = resolved_config(cfg, secrets, tunnel.as_ref());
        let started = Instant::now();
        let session = match connect_db(driver.as_ref(), &resolved).await {
            Ok(s) => s,
            Err(e) => {
                // Through a tunnel, a refused channel explains the failure better than
                // the driver's EOF: the bastion could not reach the database.
                let channel = tunnel.as_ref().and_then(Tunnel::last_channel_error);
                let e = match channel {
                    Some(cause) => ssh_channel_error(&cfg.host, cfg.port, &cause),
                    None => e,
                };
                if let Some(t) = tunnel {
                    t.close().await;
                }
                return Err(DbError::at_stage(
                    ConnectStage::ConnectDb,
                    timings.clone(),
                    e,
                ));
            }
        };
        record(timings, ConnectStage::ConnectDb, started);

        // Drivers detect the server while connecting; this stage only reads the result
        // (so it is ~0 ms) and is kept for a stable stage list.
        report(ConnectStage::DetectServer);
        let started = Instant::now();
        let info = session.server_info().clone();
        record(timings, ConnectStage::DetectServer, started);
        Ok(Opened {
            driver: Arc::clone(driver),
            tunnel,
            resolved,
            session,
            info,
        })
    }

    fn driver(&self, kind: DbKind) -> Result<&Arc<dyn Driver>, DbError> {
        self.drivers.get(&kind).ok_or_else(|| DbError::Internal {
            message: format!("no driver registered for {kind:?}"),
        })
    }

    pub(crate) fn runtime_opt(&self, id: Uuid) -> Option<Arc<ConnectionRuntime>> {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    pub(crate) fn runtime(&self, id: Uuid) -> Result<Arc<ConnectionRuntime>, DbError> {
        self.runtime_opt(id).ok_or(DbError::NotOpen)
    }

    /// Closes the tunnel and every metadata session and forgets the connection. An `open`
    /// in flight is superseded (it closes what it opened and returns `Cancelled`).
    pub async fn close(&self, id: Uuid) {
        // A running query is stopped first, so closing does not wait for it; the run
        // still ends with its `done` event.
        self.stop_runs(|r| r.conn == id).await;
        self.bump_generation(id);
        let rt = self
            .runtimes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        lock(&self.pending).remove(&id);
        if let Some(rt) = rt {
            rt.shutdown(self.apply_close_wait).await;
        }
        self.plans.drop_conn(id);
        self.edit_plans.drop_conn(id);
        self.loaded.drop_conn(id);
    }

    /// Every runtime, without holding the map's lock.
    pub(crate) fn runtimes_snapshot(&self) -> Vec<(Uuid, Arc<ConnectionRuntime>)> {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, rt)| (*id, Arc::clone(rt)))
            .collect()
    }

    pub fn state(&self, id: Uuid) -> ConnState {
        match self.runtime_opt(id) {
            Some(rt) => rt.state(),
            None => lock(&self.pending)
                .get(&id)
                .cloned()
                .unwrap_or(ConnState::Disconnected),
        }
    }

    /// Timings of the last `open` of `id`; after a failure, the stages that completed.
    pub fn last_timings(&self, id: Uuid) -> Option<Vec<StageTiming>> {
        lock(&self.timings).get(&id).cloned()
    }

    /// Ids of the connections that currently have a runtime.
    pub fn open_connections(&self) -> Vec<Uuid> {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect()
    }

    /// Runs `f` on the metadata session for `db` (`None` = the connect database), caching
    /// the result under `key`. `f` may run twice: once more after the session was reopened.
    pub async fn browse<T, F>(
        &self,
        id: Uuid,
        db: Option<&str>,
        key: NodeKey,
        f: F,
    ) -> Result<T, DbError>
    where
        T: Clone + Serialize + DeserializeOwned + Send,
        F: for<'a> Fn(&'a dyn Introspector) -> MetaFuture<'a, T> + Sync,
    {
        let rt = self.runtime(id)?;
        if let Some(hit) = rt.cache.get(&key) {
            // A key reused for another result type is simply fetched again.
            if let Ok(value) = serde_json::from_value(hit) {
                return Ok(value);
            }
        }
        // A refresh while the call runs makes its result stale: it is then not cached.
        let epoch = rt.cache.epoch();
        let value = rt.with_session(db, |s| f(s.introspector())).await?;
        let json = serde_json::to_value(&value).map_err(|e| DbError::Internal {
            message: format!("cache browse result: {e}"),
        })?;
        rt.cache.insert_if_epoch(key, json, epoch);
        Ok(value)
    }

    /// The table detail of `obj` as the server has it now (never the cached one), which
    /// then replaces the cached one: what an edit plan is built and checked against.
    pub async fn table_detail_fresh(
        &self,
        id: Uuid,
        obj: &ObjectRef,
    ) -> Result<TableDetail, DbError> {
        let rt = self.runtime(id)?;
        let epoch = rt.cache.epoch();
        let detail = rt
            .with_session(Some(&obj.database), |s| {
                let obj = obj.clone();
                Box::pin(async move { s.introspector().table_detail(&obj).await })
            })
            .await?;
        if let Ok(json) = serde_json::to_value(&detail) {
            rt.cache
                .insert_if_epoch(object_key(obj, "detail"), json, epoch);
        }
        Ok(detail)
    }

    /// Drops every cached browse result of `id` under `prefix`, and reloads (in the
    /// background) the SchemaCache of every loaded database the prefix covers: all of them
    /// for `[]`, database `x` for any prefix starting with `db:x`.
    pub fn refresh(&self, id: Uuid, prefix: &NodeKey) {
        if let Some(rt) = self.runtime_opt(id) {
            rt.cache.invalidate_prefix(prefix);
            let covered: Vec<String> = rt
                .schema_databases()
                .into_iter()
                .filter(|db| match prefix.0.first() {
                    None => true,
                    Some(first) => first.strip_prefix("db:") == Some(db.as_str()),
                })
                .collect();
            for db in covered {
                rt.refresh_schema_cache(&db);
            }
        }
    }

    /// The SchemaCache of `database`, loading it on first use. Concurrent first calls for
    /// one database share a single load; a failed load is not kept (the next call retries).
    pub async fn schema_cache_for(&self, id: Uuid, database: &str) -> Result<SchemaCache, DbError> {
        self.runtime(id)?.schema_cache_for(database).await
    }

    /// Drops the SchemaCache of `database` and reloads it in the background (e.g. after a
    /// DDL statement). Callers arriving meanwhile wait for the reload.
    pub fn refresh_schema_cache(&self, id: Uuid, database: &str) {
        if let Some(rt) = self.runtime_opt(id) {
            rt.refresh_schema_cache(database);
        }
    }

    /// The database whose SchemaCache `get_schema_cache` returns: the connect database
    /// (PostgreSQL: the one named after the user when none is set; MySQL: none when no
    /// default database is set).
    pub fn home_database(&self, id: Uuid) -> Result<Option<String>, DbError> {
        Ok(self.runtime(id)?.home_database())
    }

    /// Tables and views (and materialized views on PostgreSQL) for autocomplete after
    /// `db.` (MySQL: `schema` must be `None`) / `schema.` (PostgreSQL: `schema` is
    /// required, as every PostgreSQL folder key of the tree carries a `schema:` segment).
    /// Listed through [`SessionManager::browse`] under the tree's folder keys, so the tree
    /// and autocomplete share one cache.
    pub async fn completion_objects(
        &self,
        id: Uuid,
        database: &str,
        schema: Option<&str>,
    ) -> Result<Vec<CompletionObject>, DbError> {
        require_database(database)?;
        let kind = self.runtime(id)?.cfg.kind;
        match (kind, schema) {
            (DbKind::MySql, Some(_)) => {
                return Err(config_error(
                    "schema",
                    "MySQL has no schemas inside a database",
                ))
            }
            (DbKind::Postgres, None) => {
                return Err(config_error("schema", "a schema is required on PostgreSQL"))
            }
            _ => {}
        }
        let kinds: &[ObjectKind] = match kind {
            DbKind::MySql => &[ObjectKind::Table, ObjectKind::View],
            DbKind::Postgres => &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
            ],
        };
        let scope = Scope {
            database: database.to_string(),
            schema: schema.map(str::to_string),
        };
        let mut objects = Vec::new();
        for &kind in kinds {
            let listed = self
                .browse(id, Some(database), folder_key(&scope, kind), |i| {
                    let scope = scope.clone();
                    Box::pin(async move { i.list_objects(&scope, kind).await })
                })
                .await?;
            objects.extend(listed.into_iter().map(|o| CompletionObject {
                schema: scope.schema.clone(),
                name: o.name,
                kind,
            }));
        }
        Ok(objects)
    }
}

pub(crate) fn config_error(field: &str, message: &str) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: message.into(),
    }
}

fn require_database(database: &str) -> Result<(), DbError> {
    if database.is_empty() {
        Err(config_error("database", "a database is required"))
    } else {
        Ok(())
    }
}

/// What a SchemaSlot holds.
#[derive(Default)]
enum SlotState {
    #[default]
    Empty,
    Ready(SchemaCache),
    /// The last load failed at the given time; returned as is until the TTL runs out.
    Failed(DbError, Instant),
}

/// One database's SchemaCache. Its async mutex makes loads single-flight: the caller
/// holding it runs the load (in a task, so `refresh` can abort it), and the callers
/// waiting for it then find the result (success or error) in the state.
#[derive(Default)]
struct SchemaSlot {
    state: tokio::sync::Mutex<SlotState>,
    /// Abort handle of the load running for this slot.
    load: Mutex<Option<tokio::task::AbortHandle>>,
}

impl SchemaSlot {
    fn abort_load(&self) {
        if let Some(load) = lock(&self.load).take() {
            load.abort();
        }
    }
}

/// Resolved address and metadata sessions, held by one metadata call at a time.
pub(crate) struct Link {
    pub(crate) resolved: ResolvedConfig,
    metadata: HashMap<Option<String>, Box<dyn Session>>,
    /// Set by `shutdown` once it holds the link: nothing may be (re)opened any more.
    closed: bool,
}

pub struct ConnectionRuntime {
    pub(crate) cfg: ConnectionConfig,
    info: ServerInfo,
    secrets: ConnSecrets,
    pub(crate) driver: Arc<dyn Driver>,
    known_hosts: KnownHosts,
    /// Metadata key of the connect database's session.
    home: Option<String>,
    /// Outside `link` so `shutdown` can close it while a metadata call holds the link.
    tunnel: Mutex<Option<Tunnel>>,
    pub(crate) link: tokio::sync::Mutex<Link>,
    /// Set first thing by `shutdown`, even when it cannot get the link.
    pub(crate) closed: AtomicBool,
    /// Bumped every time the SSH tunnel is replaced: a tab session opened through an
    /// older tunnel is stale.
    pub(crate) tunnel_gen: AtomicU64,
    metadata_timeout: Duration,
    schema_error_ttl: Duration,
    cache: NodeCache,
    /// `database → SchemaCache`, each loaded on first use.
    schema_caches: Mutex<HashMap<String, Arc<SchemaSlot>>>,
    state: Mutex<ConnState>,
    /// Background SchemaCache loads (home database on open, refreshes); aborted by
    /// `shutdown`.
    schema_tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The query tabs on this connection, each with its own session.
    pub(crate) queries: QuerySessions,
    /// Sessions of the table data view.
    pub(crate) data: DataSessions,
}

impl ConnectionRuntime {
    fn new(
        cfg: ConnectionConfig,
        secrets: ConnSecrets,
        opened: Opened,
        known_hosts: KnownHosts,
        metadata_timeout: Duration,
        schema_error_ttl: Duration,
    ) -> Self {
        let home = match cfg.kind {
            DbKind::MySql => None,
            // Like libpq, PostgreSQL connects to the database named after the user by default.
            DbKind::Postgres => Some(
                cfg.default_db
                    .clone()
                    .unwrap_or_else(|| cfg.username.clone()),
            ),
        };
        let metadata = HashMap::from([(home.clone(), opened.session)]);
        ConnectionRuntime {
            info: opened.info,
            secrets,
            driver: opened.driver,
            known_hosts,
            home,
            tunnel: Mutex::new(opened.tunnel),
            link: tokio::sync::Mutex::new(Link {
                resolved: opened.resolved,
                metadata,
                closed: false,
            }),
            closed: AtomicBool::new(false),
            tunnel_gen: AtomicU64::new(0),
            metadata_timeout,
            schema_error_ttl,
            cache: NodeCache::default(),
            schema_caches: Mutex::new(HashMap::new()),
            state: Mutex::new(ConnState::Connected),
            schema_tasks: Mutex::new(Vec::new()),
            queries: QuerySessions::default(),
            data: DataSessions::default(),
            cfg,
        }
    }

    fn state(&self) -> ConnState {
        lock(&self.state).clone()
    }

    pub(crate) fn set_state(&self, state: ConnState) {
        *lock(&self.state) = state;
    }

    fn metadata_key(&self, db: Option<&str>) -> Option<String> {
        match self.cfg.kind {
            DbKind::MySql => None,
            DbKind::Postgres => db.map(str::to_string).or_else(|| self.home.clone()),
        }
    }

    /// Runs `f` on the metadata session for `db`, opening it if needed. A dead tunnel, or
    /// a call failing with a lost connection, reopens tunnel and session once; after a
    /// reopen caused by a failure the call is retried once.
    async fn with_session<T, F>(&self, db: Option<&str>, f: F) -> Result<T, DbError>
    where
        F: for<'a> Fn(&'a dyn Session) -> MetaFuture<'a, T> + Sync,
    {
        let key = self.metadata_key(db);
        let mut link = self.link.lock().await;
        let mut reopened = false;
        if self.tunnel_dead() {
            self.ensure_open(&link)?;
            self.reopen(&mut link, &key).await?;
            reopened = true;
        }
        loop {
            self.ensure_open(&link)?;
            if !link.metadata.contains_key(&key) {
                let resolved = with_database(&link.resolved, key.clone());
                let session = connect_db(self.driver.as_ref(), &resolved).await?;
                link.metadata.insert(key.clone(), session);
            }
            // Taken out of the map for the call: if this future is dropped mid-call (an
            // aborted SchemaCache load), the session, possibly mid-protocol, is dropped with
            // it instead of being reused.
            let session = link
                .metadata
                .remove(&key)
                .ok_or_else(|| DbError::Internal {
                    message: "metadata session vanished".into(),
                })?;
            let outcome = tokio::time::timeout(self.metadata_timeout, f(session.as_ref())).await;
            let Ok(outcome) = outcome else {
                // The session may be stuck mid-protocol: drop it (no graceful close) and do
                // not retry; the next call opens a fresh one.
                drop(session);
                return Err(DbError::timeout(ConnectStage::Execute));
            };
            link.metadata.insert(key.clone(), session);
            match outcome {
                Err(e) if !reopened && is_lost(&e) => {
                    self.ensure_open(&link)?;
                    self.reopen(&mut link, &key).await?;
                    reopened = true;
                }
                result => {
                    if result.is_ok() && self.state() == ConnState::Failed {
                        // An earlier reopen failed; the lazily reopened session works.
                        self.set_state(ConnState::Connected);
                    }
                    return result;
                }
            }
        }
    }

    /// Replaces the session for `key` (and the tunnel, when it died) with fresh ones.
    async fn reopen(&self, link: &mut Link, key: &Option<String>) -> Result<(), DbError> {
        self.set_state(ConnState::Reconnecting);
        let result = self.reopen_inner(link, key).await;
        self.set_state(if result.is_ok() {
            ConnState::Connected
        } else {
            ConnState::Failed
        });
        result
    }

    async fn reopen_inner(&self, link: &mut Link, key: &Option<String>) -> Result<(), DbError> {
        if let Some(old) = link.metadata.remove(key) {
            close_quietly(old).await;
        }
        self.replace_dead_tunnel(link).await?;
        let resolved = with_database(&link.resolved, key.clone());
        let session = connect_db(self.driver.as_ref(), &resolved).await?;
        link.metadata.insert(key.clone(), session);
        Ok(())
    }

    /// With SSH and a dead tunnel: closes every metadata session (they all went through
    /// it), opens a new tunnel and points `link` at it. Otherwise does nothing. (Query
    /// tab sessions through the dead tunnel fail on their next use.)
    pub(crate) async fn replace_dead_tunnel(&self, link: &mut Link) -> Result<(), DbError> {
        let Some(ssh) = &self.cfg.ssh else {
            return Ok(());
        };
        if !self.tunnel_dead() {
            return Ok(());
        }
        for (_, s) in link.metadata.drain() {
            close_quietly(s).await;
        }
        // The dead tunnel stays until a new one is up, so a failed attempt is retried by
        // the next call.
        let tunnel = open_tunnel(ssh, &self.cfg, &self.secrets, &self.known_hosts).await?;
        let port = tunnel.local_addr().port();
        let old = {
            let mut slot = lock(&self.tunnel);
            if self.closed.load(Ordering::SeqCst) {
                // `shutdown` ran meanwhile: do not resurrect the connection.
                Err(tunnel)
            } else {
                Ok(slot.replace(tunnel))
            }
        };
        match old {
            Err(fresh) => {
                fresh.close().await;
                return Err(closed_error());
            }
            Ok(Some(old)) => old.close().await,
            Ok(None) => {}
        }
        link.resolved.port = port;
        self.tunnel_gen.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) fn tunnel_dead(&self) -> bool {
        lock(&self.tunnel).as_ref().is_some_and(|t| !t.is_alive())
    }

    pub(crate) fn ensure_open(&self, link: &Link) -> Result<(), DbError> {
        if link.closed || self.closed.load(Ordering::SeqCst) {
            Err(closed_error())
        } else {
            Ok(())
        }
    }

    /// Makes the runtime terminal: stops the SchemaCache task and closes every session and
    /// the tunnel. A metadata call holding the link longer than `SHUTDOWN_LOCK_TIMEOUT`
    /// keeps its sessions until it returns (they are dropped with the runtime); the
    /// tunnel is closed regardless, and the call finds the runtime closed. A running edit
    /// apply gets up to `apply_wait` to end before it is cancelled (it then reports what
    /// is known, see `edit_plans`).
    async fn shutdown(&self, apply_wait: Duration) {
        {
            let mut tasks = lock(&self.schema_tasks);
            self.closed.store(true, Ordering::SeqCst);
            for task in tasks.drain(..) {
                task.abort();
            }
        }
        for (_, slot) in lock(&self.schema_caches).drain() {
            slot.abort_load();
        }
        // `closed` is set: no tab slot can be created any more.
        let tabs = self.queries.drain();
        futures_util::future::join(
            futures_util::future::join_all(
                tabs.into_iter()
                    .map(|(tab, slot)| close_slot(tab, slot, SLOT_CLOSE_WAIT)),
            ),
            self.data.end_applies(apply_wait),
        )
        .await;
        self.data.shutdown().await;
        match tokio::time::timeout(SHUTDOWN_LOCK_TIMEOUT, self.link.lock()).await {
            Ok(mut link) => {
                link.closed = true;
                for (_, s) in link.metadata.drain() {
                    close_quietly(s).await;
                }
            }
            Err(_) => tracing::warn!(
                connection = %self.cfg.id,
                "a metadata call is still running; closing the connection without it"
            ),
        }
        let tunnel = lock(&self.tunnel).take();
        if let Some(tunnel) = tunnel {
            tunnel.close().await;
        }
        self.set_state(ConnState::Disconnected);
    }

    /// The connect database: PostgreSQL's `home`, MySQL's default database (if any).
    pub(crate) fn home_database(&self) -> Option<String> {
        match self.cfg.kind {
            DbKind::MySql => self.cfg.default_db.clone(),
            DbKind::Postgres => self.home.clone(),
        }
    }

    /// The slot of `database`, created empty on first use.
    fn schema_slot(&self, database: &str) -> Arc<SchemaSlot> {
        Arc::clone(
            lock(&self.schema_caches)
                .entry(database.to_string())
                .or_default(),
        )
    }

    /// Whether `slot` is still the slot of `database` (not replaced by a refresh).
    fn is_current_slot(&self, database: &str, slot: &Arc<SchemaSlot>) -> bool {
        lock(&self.schema_caches)
            .get(database)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
    }

    /// Databases that have a SchemaCache slot (loaded or loading).
    fn schema_databases(&self) -> Vec<String> {
        lock(&self.schema_caches).keys().cloned().collect()
    }

    /// Single-flight per database: one load at a time, and its result (a cache, or an
    /// error kept for `schema_error_ttl`) is what every waiter gets. A caller whose load
    /// (or slot) is replaced by a refresh follows the new slot.
    async fn schema_cache_for(self: &Arc<Self>, database: &str) -> Result<SchemaCache, DbError> {
        require_database(database)?;
        loop {
            let slot = self.schema_slot(database);
            let mut state = slot.state.lock().await;
            match &*state {
                SlotState::Ready(cache) => return Ok(cache.clone()),
                SlotState::Failed(e, at) if at.elapsed() < self.schema_error_ttl => {
                    return Err(e.clone())
                }
                _ => {}
            }
            if !self.is_current_slot(database, &slot) {
                continue;
            }
            self.ensure_not_closed()?;
            let rt = Arc::clone(self);
            let db = database.to_string();
            let load = tokio::spawn(async move { rt.load_schema_cache(&db).await });
            *lock(&slot.load) = Some(load.abort_handle());
            let outcome = load.await;
            lock(&slot.load).take();
            match outcome {
                Ok(Ok(cache)) => {
                    *state = SlotState::Ready(cache.clone());
                    return Ok(cache);
                }
                Ok(Err(e)) => {
                    *state = SlotState::Failed(e.clone(), Instant::now());
                    return Err(e);
                }
                // Aborted by a refresh (follow the new slot) or by `shutdown`.
                Err(join) if join.is_cancelled() => {
                    self.ensure_not_closed()?;
                }
                Err(_) => {
                    return Err(DbError::Internal {
                        message: "the schema cache load failed unexpectedly".into(),
                    })
                }
            }
        }
    }

    fn ensure_not_closed(&self) -> Result<(), DbError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(closed_error())
        } else {
            Ok(())
        }
    }

    /// Replaces the slot of `database` with an empty one, aborts the load running on the
    /// old one (so the two loads do not compete for the metadata link), and loads the new
    /// one in the background. Callers waiting on the old slot follow the new one.
    pub(crate) fn refresh_schema_cache(self: &Arc<Self>, database: &str) {
        let old = lock(&self.schema_caches).insert(database.to_string(), Arc::default());
        if let Some(old) = old {
            old.abort_load();
        }
        self.spawn_schema_load(database.to_string());
    }

    /// Loads the SchemaCache of `database` in the background; does nothing on a closed
    /// runtime.
    fn spawn_schema_load(self: &Arc<Self>, database: String) {
        // `closed` is set under the same lock by `shutdown`, so either the task is not
        // started or `shutdown` finds (and aborts) it.
        let mut tasks = lock(&self.schema_tasks);
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        tasks.retain(|t| !t.is_finished());
        let rt = Arc::clone(self);
        tasks.push(tokio::spawn(async move {
            if let Err(e) = rt.schema_cache_for(&database).await {
                // DbError messages never carry secrets.
                tracing::warn!(
                    connection = %rt.cfg.id,
                    error = %e,
                    "could not load the schema cache"
                );
            }
        }));
    }

    /// Tables and views (with column names and types) of `database`; on PostgreSQL of the
    /// schemas in `current_schemas(false)` of that database's metadata session.
    async fn load_schema_cache(&self, database: &str) -> Result<SchemaCache, DbError> {
        let database = database.to_string();
        let (schemas, kinds): (Vec<Option<String>>, &[ObjectKind]) = match self.cfg.kind {
            DbKind::Postgres => {
                let schemas = self
                    .with_session(Some(&database), |s| Box::pin(current_schemas(s)))
                    .await?;
                (
                    schemas.into_iter().map(Some).collect(),
                    &[
                        ObjectKind::Table,
                        ObjectKind::View,
                        ObjectKind::MaterializedView,
                    ],
                )
            }
            DbKind::MySql => (vec![None], &[ObjectKind::Table, ObjectKind::View]),
        };

        // Per scope: one listing per object kind plus ONE bulk column query; never a
        // per-table `table_detail` (that reads statistics, ~6-8 queries per table).
        let mut tables = Vec::new();
        for schema in &schemas {
            let scope = Scope {
                database: database.clone(),
                schema: schema.clone(),
            };
            let mut columns: HashMap<String, Vec<ColumnBrief>> = self
                .with_session(Some(&database), |s| {
                    let scope = scope.clone();
                    Box::pin(async move { s.introspector().list_columns(&scope).await })
                })
                .await?
                .into_iter()
                .map(|t| (t.table, t.columns))
                .collect();
            for &kind in kinds {
                let objects = self
                    .with_session(Some(&database), |s| {
                        let scope = scope.clone();
                        Box::pin(async move { s.introspector().list_objects(&scope, kind).await })
                    })
                    .await?;
                for object in objects {
                    tables.push(SchemaCacheTable {
                        schema: schema.clone(),
                        columns: columns.remove(&object.name).unwrap_or_default(),
                        name: object.name,
                        kind,
                    });
                }
            }
        }
        Ok(SchemaCache { database, tables })
    }
}

/// First column of every row, as text.
#[derive(Default)]
struct TextColumn(Vec<String>);

impl RowSink for TextColumn {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.0.extend(
            batch
                .into_iter()
                .filter_map(|row| match row.into_iter().next() {
                    Some(Value::Text(s)) => Some(s),
                    _ => None,
                }),
        );
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// The schemas of the session's effective `search_path` (PostgreSQL).
async fn current_schemas(session: &dyn Session) -> Result<Vec<String>, DbError> {
    let mut sink = TextColumn::default();
    session
        .execute(
            "SELECT unnest(current_schemas(false))::text",
            ExecOptions::default(),
            &mut sink,
        )
        .await?;
    Ok(sink.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn manager_is_send_and_sync() {
        assert_send_sync::<SessionManager>();
        assert_send_sync::<ConnectionRuntime>();
    }

    mod target {
        use super::super::{same_ssh_endpoint, same_target};
        use dnt_core::config::ConnectionConfig;
        use dnt_core::server::DbKind;
        use dnt_core::test_util::{sample_config, sample_ssh};

        fn with_ssh() -> ConnectionConfig {
            ConnectionConfig {
                ssh: Some(sample_ssh()),
                ..sample_config()
            }
        }

        #[test]
        fn ssh_on_to_off_is_different_target() {
            let stored = with_ssh();
            let cfg = ConnectionConfig {
                ssh: None,
                ..stored.clone()
            };
            assert!(!same_target(&stored, &cfg));
            assert!(!same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn ssh_off_to_on_is_different_target() {
            let stored = sample_config();
            let cfg = with_ssh();
            assert!(!same_target(&stored, &cfg));
            assert!(!same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn ssh_host_changed_is_different_target() {
            let stored = with_ssh();
            let mut cfg = stored.clone();
            cfg.ssh.as_mut().unwrap().host = "other-bastion.example.com".into();
            assert!(!same_target(&stored, &cfg));
            assert!(!same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn ssh_port_changed_is_different_target() {
            let stored = with_ssh();
            let mut cfg = stored.clone();
            cfg.ssh.as_mut().unwrap().port = 2222;
            assert!(!same_target(&stored, &cfg));
            assert!(!same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn ssh_user_changed_is_different_target() {
            let stored = with_ssh();
            let mut cfg = stored.clone();
            cfg.ssh.as_mut().unwrap().user = "someone".into();
            assert!(!same_target(&stored, &cfg));
            assert!(!same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn unchanged_ssh_is_same_target() {
            let stored = with_ssh();
            let mut cfg = stored.clone();
            cfg.name = "renamed".into();
            cfg.default_db = Some("other".into());
            assert!(same_target(&stored, &cfg));
            assert!(same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn unchanged_direct_connection_is_same_target() {
            let stored = sample_config();
            assert!(same_target(&stored, &stored.clone()));
            // No SSH on either side: there is no SSH secret to fall back to.
            assert!(!same_ssh_endpoint(&stored, &stored.clone()));
        }

        #[test]
        fn db_host_case_only_change_is_same_target() {
            let stored = ConnectionConfig {
                host: "db.example.com".into(),
                ..with_ssh()
            };
            let mut cfg = stored.clone();
            cfg.host = "  DB.Example.COM ".into();
            cfg.ssh.as_mut().unwrap().host = " Bastion.EXAMPLE.com".into();
            assert!(same_target(&stored, &cfg));
            assert!(same_ssh_endpoint(&stored, &cfg));
        }

        #[test]
        fn db_endpoint_or_engine_changed_is_different_target() {
            let stored = with_ssh();
            type Edit = (&'static str, fn(&mut ConnectionConfig));
            let edits: [Edit; 4] = [
                ("engine", |c| c.kind = DbKind::Postgres),
                ("host", |c| c.host = "10.0.0.9".into()),
                ("port", |c| c.port = 3307),
                ("user", |c| c.username = "someone".into()),
            ];
            for (what, edit) in edits {
                let mut cfg = stored.clone();
                edit(&mut cfg);
                assert!(!same_target(&stored, &cfg), "changed {what}");
                // The bastion is unchanged: its secrets may still be reused.
                assert!(same_ssh_endpoint(&stored, &cfg), "changed {what}");
            }
        }
    }

    #[test]
    fn conn_state_serializes_with_stage() {
        let j = serde_json::to_value(ConnState::Connecting {
            stage: ConnectStage::OpenTunnel,
        })
        .unwrap();
        assert_eq!(
            j,
            serde_json::json!({"state": "connecting", "stage": "open_tunnel"})
        );
    }
}
