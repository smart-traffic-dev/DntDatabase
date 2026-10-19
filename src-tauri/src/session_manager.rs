//! `SessionManager`: the staged connect pipeline and one `ConnectionRuntime` per open
//! connection (tunnel, dedicated metadata sessions, `NodeCache`, `SchemaCache`).
//!
//! Introspection only ever runs on metadata sessions, never on a user's query session:
//! MySQL keeps one (key `None`); PostgreSQL one per database (key `Some(db)`), because its
//! catalogs are per database. The session opened by the connect pipeline becomes the
//! metadata session of the connect database. Drivers do not reconnect during
//! introspection, so a call that fails with a lost connection (or finds the SSH tunnel
//! dead) reopens the tunnel and that session once and is retried once.

use crate::cache::{NodeCache, NodeKey, SchemaCache, SchemaCacheTable};
use dnt_core::config::{ConnectionConfig, ResolvedConfig, SecretMode, SshAuth, SshConfig};
use dnt_core::driver::{Driver, ExecOptions, Introspector, RowSink, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Error message telling the UI to prompt for the missing password(s) and call again.
pub const PASSWORD_REQUIRED: &str = "password_required";

/// Used when the connection sets no application name.
const DEFAULT_APPLICATION_NAME: &str = "DntDatabase";

/// How long closing a session that is probably dead may take before it is just dropped.
const CLOSE_DEAD_TIMEOUT: Duration = Duration::from_secs(2);

/// Default bound of one metadata call (drivers set no socket read timeouts). On expiry the
/// session is dropped and `Timeout{Execute}` returned, without a retry.
pub const METADATA_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `close` waits for an in-flight metadata call to release the runtime before it
/// marks the runtime closed and tears down what it can reach without it.
const SHUTDOWN_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Returned by metadata calls on a runtime that was closed (also to calls that were
/// waiting while `close` ran): the connection is gone and is not reopened.
fn closed_error() -> DbError {
    DbError::ConnectionLost {
        in_transaction: false,
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
pub struct StageTiming {
    pub stage: ConnectStage,
    /// Exported as a JS number (milliseconds).
    #[specta(type = f64)]
    pub ms: u64,
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
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
fn with_database(base: &ResolvedConfig, database: Option<String>) -> ResolvedConfig {
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
async fn connect_db(
    driver: &dyn Driver,
    resolved: &ResolvedConfig,
) -> Result<Box<dyn Session>, DbError> {
    tokio::time::timeout(resolved.connect_timeout, driver.connect(resolved))
        .await
        .map_err(|_| DbError::Timeout {
            stage: ConnectStage::ConnectDb,
        })?
}

/// Errors after which the metadata session (and tunnel) is worth reopening once.
fn is_lost(e: &DbError) -> bool {
    matches!(e, DbError::ConnectionLost { .. } | DbError::Connect { .. })
}

async fn close_quietly(session: Box<dyn Session>) {
    let _ = tokio::time::timeout(CLOSE_DEAD_TIMEOUT, session.close()).await;
}

pub struct SessionManager {
    drivers: HashMap<DbKind, Arc<dyn Driver>>,
    repo: Arc<ConnectionRepo>,
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
        }
    }

    /// Overrides [`METADATA_CALL_TIMEOUT`] for runtimes opened afterwards (tests).
    pub fn with_metadata_timeout(mut self, timeout: Duration) -> Self {
        self.metadata_timeout = timeout;
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
    pub async fn test(
        &self,
        cfg: &ConnectionConfig,
        secrets: NewSecrets,
    ) -> Result<OpenResult, DbError> {
        cfg.validate()?;
        let mut timings = Vec::new();
        let started = Instant::now();
        let secrets = ConnSecrets::from(secrets);
        secrets.check_ssh(cfg)?;
        record(&mut timings, ConnectStage::ResolveSecrets, started);
        let opened = self
            .connect_pipeline(cfg, &secrets, &mut timings, &|_| {})
            .await?;
        let info = opened.info.clone();
        opened.close().await;
        Ok(OpenResult { info, timings })
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
            rt.shutdown().await;
            return Err(DbError::Cancelled);
        };
        if let Some(old) = replaced {
            // Cannot normally happen (every open/close bumps the generation); never leak it.
            old.shutdown().await;
        }
        if let Err(e) = self.repo.touch_connected(id) {
            tracing::warn!(connection = %id, error = %e, "could not record the connection time");
        }
        rt.spawn_schema_cache();
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
        let secrets = self.resolve_secrets(&cfg, prompt)?;
        record(timings, ConnectStage::ResolveSecrets, started);
        let report =
            |stage| self.set_pending_if_current(id, generation, ConnState::Connecting { stage });
        let opened = self
            .connect_pipeline(&cfg, &secrets, timings, &report)
            .await?;
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
    /// returned as the failing stage produced them; anything opened is closed again.
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
                let tunnel = open_tunnel(ssh, cfg, secrets, &self.known_hosts).await?;
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
                if let Some(t) = tunnel {
                    t.close().await;
                }
                return Err(e);
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

    fn runtime_opt(&self, id: Uuid) -> Option<Arc<ConnectionRuntime>> {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    fn runtime(&self, id: Uuid) -> Result<Arc<ConnectionRuntime>, DbError> {
        self.runtime_opt(id).ok_or_else(|| DbError::Internal {
            message: format!("connection {id} is not open"),
        })
    }

    /// Closes the tunnel and every metadata session and forgets the connection. An `open`
    /// in flight is superseded (it closes what it opened and returns `Cancelled`).
    pub async fn close(&self, id: Uuid) {
        self.bump_generation(id);
        let rt = self
            .runtimes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        lock(&self.pending).remove(&id);
        if let Some(rt) = rt {
            rt.shutdown().await;
        }
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

    /// Drops every cached browse result of `id` under `prefix`.
    pub fn refresh(&self, id: Uuid, prefix: &NodeKey) {
        if let Some(rt) = self.runtime_opt(id) {
            rt.cache.invalidate_prefix(prefix);
        }
    }

    pub fn schema_cache(&self, id: Uuid) -> Option<SchemaCache> {
        self.runtime_opt(id)?
            .schema_cache
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Resolved address and metadata sessions, held by one metadata call at a time.
struct Link {
    resolved: ResolvedConfig,
    metadata: HashMap<Option<String>, Box<dyn Session>>,
    /// Set by `shutdown` once it holds the link: nothing may be (re)opened any more.
    closed: bool,
}

pub struct ConnectionRuntime {
    cfg: ConnectionConfig,
    info: ServerInfo,
    secrets: ConnSecrets,
    driver: Arc<dyn Driver>,
    known_hosts: KnownHosts,
    /// Metadata key of the connect database's session.
    home: Option<String>,
    /// Outside `link` so `shutdown` can close it while a metadata call holds the link.
    tunnel: Mutex<Option<Tunnel>>,
    link: tokio::sync::Mutex<Link>,
    /// Set first thing by `shutdown`, even when it cannot get the link.
    closed: AtomicBool,
    metadata_timeout: Duration,
    cache: NodeCache,
    schema_cache: RwLock<Option<SchemaCache>>,
    state: Mutex<ConnState>,
    schema_task: Mutex<Option<JoinHandle<()>>>,
}

impl ConnectionRuntime {
    fn new(
        cfg: ConnectionConfig,
        secrets: ConnSecrets,
        opened: Opened,
        known_hosts: KnownHosts,
        metadata_timeout: Duration,
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
            metadata_timeout,
            cache: NodeCache::default(),
            schema_cache: RwLock::new(None),
            state: Mutex::new(ConnState::Connected),
            schema_task: Mutex::new(None),
            cfg,
        }
    }

    fn state(&self) -> ConnState {
        lock(&self.state).clone()
    }

    fn set_state(&self, state: ConnState) {
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
            let session = link.metadata.get(&key).ok_or_else(|| DbError::Internal {
                message: "metadata session vanished".into(),
            })?;
            let outcome = tokio::time::timeout(self.metadata_timeout, f(session.as_ref())).await;
            let Ok(outcome) = outcome else {
                // The session may be stuck mid-protocol: drop it (no graceful close) and do
                // not retry; the next call opens a fresh one.
                link.metadata.remove(&key);
                return Err(DbError::Timeout {
                    stage: ConnectStage::Execute,
                });
            };
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
        if let Some(ssh) = &self.cfg.ssh {
            if self.tunnel_dead() {
                // Every session went through the dead tunnel.
                for (_, s) in link.metadata.drain() {
                    close_quietly(s).await;
                }
                // The dead tunnel stays until a new one is up, so a failed attempt is
                // retried by the next call.
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
            }
        }
        let resolved = with_database(&link.resolved, key.clone());
        let session = connect_db(self.driver.as_ref(), &resolved).await?;
        link.metadata.insert(key.clone(), session);
        Ok(())
    }

    fn tunnel_dead(&self) -> bool {
        lock(&self.tunnel).as_ref().is_some_and(|t| !t.is_alive())
    }

    fn ensure_open(&self, link: &Link) -> Result<(), DbError> {
        if link.closed || self.closed.load(Ordering::SeqCst) {
            Err(closed_error())
        } else {
            Ok(())
        }
    }

    /// Makes the runtime terminal: stops the SchemaCache task and closes every session and
    /// the tunnel. A metadata call holding the link longer than `SHUTDOWN_LOCK_TIMEOUT`
    /// keeps its sessions until it returns (they are dropped with the runtime); the
    /// tunnel is closed regardless, and the call finds the runtime closed.
    async fn shutdown(&self) {
        {
            let mut task = lock(&self.schema_task);
            self.closed.store(true, Ordering::SeqCst);
            if let Some(task) = task.take() {
                task.abort();
            }
        }
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

    /// Starts loading the SchemaCache; does nothing on a closed runtime.
    fn spawn_schema_cache(self: &Arc<Self>) {
        // `closed` is set under the same lock by `shutdown`, so either the task is not
        // started or `shutdown` finds (and aborts) it.
        let mut slot = lock(&self.schema_task);
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let rt = Arc::clone(self);
        let task = tokio::spawn(async move {
            match rt.load_schema_cache().await {
                Ok(Some(cache)) => {
                    *rt.schema_cache
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = Some(cache);
                }
                Ok(None) => {}
                // DbError messages never carry secrets.
                Err(e) => tracing::warn!(
                    connection = %rt.cfg.id,
                    error = %e,
                    "could not load the schema cache"
                ),
            }
        });
        *slot = Some(task);
    }

    /// Tables and views (with column names) of the default database; on PostgreSQL of
    /// the schemas in `current_schemas(false)`. `None` for MySQL without a default database.
    async fn load_schema_cache(&self) -> Result<Option<SchemaCache>, DbError> {
        let (database, schemas, kinds): (String, Vec<Option<String>>, &[ObjectKind]) =
            match (self.cfg.kind, &self.home) {
                (DbKind::Postgres, Some(home)) => {
                    let schemas = self
                        .with_session(None, |s| Box::pin(current_schemas(s)))
                        .await?;
                    (
                        home.clone(),
                        schemas.into_iter().map(Some).collect(),
                        &[
                            ObjectKind::Table,
                            ObjectKind::View,
                            ObjectKind::MaterializedView,
                        ],
                    )
                }
                _ => match &self.cfg.default_db {
                    Some(db) => (
                        db.clone(),
                        vec![None],
                        &[ObjectKind::Table, ObjectKind::View],
                    ),
                    None => return Ok(None),
                },
            };

        let mut tables = Vec::new();
        for schema in &schemas {
            for &kind in kinds {
                let scope = Scope {
                    database: database.clone(),
                    schema: schema.clone(),
                };
                let objects = self
                    .with_session(Some(&database), |s| {
                        let scope = scope.clone();
                        Box::pin(async move { s.introspector().list_objects(&scope, kind).await })
                    })
                    .await?;
                for object in objects {
                    let obj = ObjectRef {
                        database: database.clone(),
                        schema: schema.clone(),
                        name: object.name,
                        kind,
                    };
                    let detail = self
                        .with_session(Some(&database), |s| {
                            let obj = obj.clone();
                            Box::pin(async move { s.introspector().table_detail(&obj).await })
                        })
                        .await;
                    let columns = match detail {
                        Ok(d) => d.columns.into_iter().map(|c| c.name).collect(),
                        // Connection trouble (or a stuck server) ends the whole load.
                        Err(e) if is_lost(&e) || matches!(e, DbError::Timeout { .. }) => {
                            return Err(e)
                        }
                        // E.g. a view whose definition no longer resolves: keep its name.
                        Err(_) => Vec::new(),
                    };
                    tables.push(SchemaCacheTable {
                        schema: obj.schema,
                        name: obj.name,
                        kind,
                        columns,
                    });
                }
            }
        }
        Ok(Some(SchemaCache { database, tables }))
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
    fn result_end(&mut self, _affected: Option<u64>, _elapsed: Duration) {}
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
