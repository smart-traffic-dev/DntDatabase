//! AI editor completion in the app (spec `2026-11-04-ai-editor-completion-design.md`):
//! the settings stored in `app_settings`, and [`AiManager`], which owns the endpoint
//! client, the pause state of spec §7 and the in-flight requests.
//!
//! The API key lives in the OS keychain (`AppSecret::AiApiKey`); it is read here, just
//! before a client is built, and never leaves the backend.

use dnt_ai::{AiError, Endpoint, FimClient, HttpFimClient};
use dnt_core::error::DbError;
use dnt_core::meta::ObjectKind;
use dnt_core::secret::{AppSecret, SecretStore};
use dnt_core::server::DbKind;
use dnt_store::repo::ConnectionRepo;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use uuid::Uuid;

pub const BASE_URL_KEY: &str = "ai.base_url";
pub const MODEL_KEY: &str = "ai.model";
pub const TRIGGER_KEY: &str = "ai.trigger";
pub const MAX_TOKENS_KEY: &str = "ai.max_tokens";
pub const TIMEOUT_KEY: &str = "ai.timeout_ms";
pub const MAX_LINES_KEY: &str = "ai.max_lines";

pub const DEFAULT_MAX_TOKENS: u32 = 128;
pub const MIN_MAX_TOKENS: u32 = 16;
pub const MAX_MAX_TOKENS: u32 = 512;
pub const DEFAULT_TIMEOUT_MS: u32 = 5_000;
pub const MIN_TIMEOUT_MS: u32 = 1_000;
pub const MAX_TIMEOUT_MS: u32 = 30_000;
pub const DEFAULT_MAX_LINES: u32 = 8;
pub const MIN_MAX_LINES: u32 = 1;
pub const MAX_MAX_LINES: u32 = 20;
/// Pause after a 429 without `Retry-After`.
pub const DEFAULT_RATE_LIMIT_PAUSE: Duration = Duration::from_secs(30);

/// When the editor asks for a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum AiTrigger {
    /// After a pause in typing.
    Automatic,
    /// Only on Alt+\.
    Manual,
    Off,
}

impl AiTrigger {
    fn as_str(self) -> &'static str {
        match self {
            AiTrigger::Automatic => "automatic",
            AiTrigger::Manual => "manual",
            AiTrigger::Off => "off",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "automatic" => Some(AiTrigger::Automatic),
            "manual" => Some(AiTrigger::Manual),
            "off" => Some(AiTrigger::Off),
            _ => None,
        }
    }
}

/// The stored settings, as Settings → AI edits them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct AiConfig {
    /// OpenAI-compatible base URL, e.g. `https://api.example.com/v1`; `None` = AI not set up.
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub trigger: AiTrigger,
    pub max_tokens: u32,
    pub timeout_ms: u32,
    pub max_lines: u32,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            base_url: None,
            model: None,
            trigger: AiTrigger::Automatic,
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_lines: DEFAULT_MAX_LINES,
        }
    }
}

/// [`AiConfig`] plus what the UI may know about the key and the endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct AiSettings {
    pub config: AiConfig,
    /// A key is stored. The key itself is never sent to the UI.
    pub has_key: bool,
    /// `scheme://host[:port]` of the base URL: what consent is tied to.
    pub origin: Option<String>,
    /// Host of the base URL, for the consent text.
    pub host: Option<String>,
    /// The user agreed to send editor text and schema names to this origin. `false` after the
    /// base URL moved to another origin: they agree again, once.
    pub consented: bool,
    /// The consent covers MongoDB connections too: it was given to the consent text that names
    /// them (version 2). A consent from before only covers MySQL and PostgreSQL.
    pub consent_covers_mongo: bool,
}

fn config_err(field: &str, message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: message.into(),
    }
}

/// The stored settings; a missing, unreadable or out-of-range value is its default.
pub fn load_config(repo: &ConnectionRepo) -> AiConfig {
    let get = |key: &str| match repo.app_setting(key) {
        Ok(v) => v.filter(|s| !s.trim().is_empty()),
        Err(e) => {
            tracing::warn!(error = %e, key, "cannot read an AI setting");
            None
        }
    };
    let number = |key: &str, min: u32, max: u32, default: u32| {
        get(key)
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|n| (min..=max).contains(n))
            .unwrap_or(default)
    };
    AiConfig {
        // A stored URL that no longer parses counts as not set.
        base_url: get(BASE_URL_KEY).filter(|u| endpoint_of(u, None, DEFAULT_TIMEOUT_MS).is_ok()),
        model: get(MODEL_KEY),
        trigger: get(TRIGGER_KEY)
            .and_then(|t| AiTrigger::parse(&t))
            .unwrap_or(AiTrigger::Automatic),
        max_tokens: number(
            MAX_TOKENS_KEY,
            MIN_MAX_TOKENS,
            MAX_MAX_TOKENS,
            DEFAULT_MAX_TOKENS,
        ),
        timeout_ms: number(
            TIMEOUT_KEY,
            MIN_TIMEOUT_MS,
            MAX_TIMEOUT_MS,
            DEFAULT_TIMEOUT_MS,
        ),
        max_lines: number(
            MAX_LINES_KEY,
            MIN_MAX_LINES,
            MAX_MAX_LINES,
            DEFAULT_MAX_LINES,
        ),
    }
}

/// Validates and normalizes `input` (trimmed; blank URL / model = not set).
pub fn validate_config(input: AiConfig) -> Result<AiConfig, DbError> {
    let blank_none = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let base_url = blank_none(input.base_url);
    if let Some(url) = &base_url {
        endpoint_of(url, None, input.timeout_ms)
            .map_err(|e| config_err("base_url", e.to_string()))?;
    }
    let range = |field: &str, label: &str, v: u32, min: u32, max: u32| {
        if (min..=max).contains(&v) {
            Ok(v)
        } else {
            Err(config_err(
                field,
                format!("{label} must be between {min} and {max}."),
            ))
        }
    };
    Ok(AiConfig {
        base_url: base_url.map(|u| u.trim_end_matches('/').to_string()),
        model: blank_none(input.model),
        trigger: input.trigger,
        max_tokens: range(
            "max_tokens",
            "Max tokens",
            input.max_tokens,
            MIN_MAX_TOKENS,
            MAX_MAX_TOKENS,
        )?,
        timeout_ms: range(
            "timeout_ms",
            "The timeout",
            input.timeout_ms,
            MIN_TIMEOUT_MS,
            MAX_TIMEOUT_MS,
        )?,
        max_lines: range(
            "max_lines",
            "Max lines",
            input.max_lines,
            MIN_MAX_LINES,
            MAX_MAX_LINES,
        )?,
    })
}

/// Stores a config [`validate_config`] returned.
pub fn save_config(repo: &ConnectionRepo, cfg: &AiConfig) -> Result<(), DbError> {
    repo.set_app_setting(BASE_URL_KEY, cfg.base_url.as_deref().unwrap_or(""))?;
    repo.set_app_setting(MODEL_KEY, cfg.model.as_deref().unwrap_or(""))?;
    repo.set_app_setting(TRIGGER_KEY, cfg.trigger.as_str())?;
    repo.set_app_setting(MAX_TOKENS_KEY, &cfg.max_tokens.to_string())?;
    repo.set_app_setting(TIMEOUT_KEY, &cfg.timeout_ms.to_string())?;
    repo.set_app_setting(MAX_LINES_KEY, &cfg.max_lines.to_string())
}

pub fn endpoint_of(
    base_url: &str,
    key: Option<secrecy::SecretString>,
    timeout_ms: u32,
) -> Result<Endpoint, AiError> {
    Endpoint::parse(base_url, key, Duration::from_millis(u64::from(timeout_ms)))
}

/// `scheme://host[:port]` of the base URL of `cfg`: what consent is tied to.
pub fn origin_of(cfg: &AiConfig) -> Option<String> {
    cfg.base_url
        .as_deref()
        .and_then(|u| endpoint_of(u, None, cfg.timeout_ms).ok())
        .map(|e| e.origin())
}

/// Whether a consent given to the consent text of `version` covers a connection of `kind`:
/// MySQL and PostgreSQL were named from the first text, MongoDB from version 2. Redis gets no
/// suggestions.
pub fn consent_covers(kind: DbKind, version: u32) -> bool {
    match kind {
        DbKind::MySql | DbKind::Postgres => true,
        DbKind::Mongo => version >= 2,
        DbKind::Redis => false,
    }
}

/// The settings as the UI sees them; `consent` is the origin the user agreed to and the
/// version of the consent text.
pub fn settings_view(cfg: AiConfig, has_key: bool, consent: Option<(&str, u32)>) -> AiSettings {
    let endpoint = cfg
        .base_url
        .as_deref()
        .and_then(|u| endpoint_of(u, None, cfg.timeout_ms).ok());
    let origin = endpoint.as_ref().map(Endpoint::origin);
    let consented =
        origin.is_some() && origin.as_deref() == consent.map(|(consent_origin, _)| consent_origin);
    AiSettings {
        consented,
        consent_covers_mongo: consented
            && consent.is_some_and(|(_, version)| consent_covers(DbKind::Mongo, version)),
        origin,
        host: endpoint.as_ref().map(|e| e.host().to_string()),
        config: cfg,
        has_key,
    }
}

/// An [`AiError`] as the command layer reports it: settings problems are `Config`, the
/// endpoint's are `Ai` (kind `ai`, its own message). Never `Internal`: the UI reports
/// those as bugs, and these are the endpoint's or the settings' problems.
pub fn to_db_error(e: AiError) -> DbError {
    let retryable = matches!(
        e,
        AiError::RateLimited { .. }
            | AiError::Server { .. }
            | AiError::Timeout
            | AiError::Network { .. }
    );
    match e {
        AiError::Config { message } => config_err("ai", message),
        other => DbError::Ai {
            message: other.to_string(),
            retryable,
        },
    }
}

/// Builds the endpoint client (tests inject a fake).
pub type ClientFactory = Arc<dyn Fn(Endpoint) -> Result<Arc<dyn FimClient>, AiError> + Send + Sync>;

/// Suggestions are paused (spec §7) until `until`, or until the settings or key change.
#[derive(Debug, Clone)]
struct Pause {
    message: String,
    until: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    client: Option<Arc<dyn FimClient>>,
    pause: Option<Pause>,
    inflight: HashMap<String, AbortHandle>,
    /// Requests from their start: generation and cancel signal.
    requests: HashMap<String, (u64, watch::Sender<bool>)>,
    next_request: u64,
}

/// A load shared by everyone waiting for it.
pub type SharedLoad<V> = Shared<BoxFuture<'static, Option<V>>>;
/// A load to start ([`Flights::join`]), or `None` when there is nothing to load.
pub type LoadStart<V> = Option<BoxFuture<'static, Option<V>>>;
/// The collections and views of a MongoDB database, by name.
pub type MongoObjects = Vec<(String, ObjectKind)>;

enum Flight<V> {
    Loading(SharedLoad<V>),
    Failed(Instant),
}

/// Single-flight loads, detached from who asked, with a short memory of failures: while a load
/// of a key runs, everyone gets the same one; a failed load is not tried again for
/// `fail_ttl`. A load that succeeded is forgotten (its result is in the tree's cache).
pub struct Flights<K, V> {
    map: Mutex<HashMap<K, Flight<V>>>,
    fail_ttl: Duration,
}

impl<K, V> Flights<K, V>
where
    K: Hash + Eq + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(fail_ttl: Duration) -> Self {
        Flights {
            map: Mutex::new(HashMap::new()),
            fail_ttl,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, Flight<V>>> {
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The running load of `key`, or a new one from `start` (spawned: it finishes even when
    /// nobody waits for it any more). `None` while a failure of `key` is remembered, or when
    /// `start` has nothing to load (it returns `None`, and nothing is remembered).
    pub fn join(
        self: &Arc<Self>,
        key: K,
        start: impl FnOnce() -> LoadStart<V>,
    ) -> Option<SharedLoad<V>> {
        let mut map = self.lock();
        match map.get(&key) {
            Some(Flight::Loading(load)) => return Some(load.clone()),
            Some(Flight::Failed(at)) if at.elapsed() < self.fail_ttl => return None,
            _ => {}
        }
        let load = start()?;
        let me = Arc::clone(self);
        let k = key.clone();
        // The task takes the map lock to record its outcome: held here until the entry is in.
        let task = tokio::spawn(async move {
            let out = load.await;
            let mut map = me.lock();
            match out {
                Some(_) => map.remove(&k),
                None => map.insert(k, Flight::Failed(Instant::now())),
            };
            out
        });
        let shared = async move { task.await.ok().flatten() }.boxed().shared();
        map.insert(key, Flight::Loading(shared.clone()));
        Some(shared)
    }
}

/// How long a request waits for the MongoDB metadata of its prompt before it goes on with
/// what is cached (the loads go on, detached).
pub const METADATA_WAIT: Duration = Duration::from_millis(1500);
/// How long a failed metadata load is not tried again.
pub const METADATA_FAIL_TTL: Duration = Duration::from_secs(60);

/// A suggestion request from its start: [`AiManager::cancel`] of its id (or a newer request
/// with the same id) cancels it. Ends (unregisters) when dropped.
pub struct RequestToken {
    ai: Arc<AiManager>,
    id: String,
    generation: u64,
    cancelled: watch::Receiver<bool>,
}

impl RequestToken {
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Resolves once the request is cancelled.
    pub async fn cancelled(&mut self) {
        if self.cancelled.wait_for(|c| *c).await.is_err() {
            // Unregistered without being cancelled: never.
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for RequestToken {
    fn drop(&mut self) {
        let mut inner = self.ai.lock();
        if inner
            .requests
            .get(&self.id)
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            inner.requests.remove(&self.id);
        }
    }
}

/// MongoDB collections and views of a database, by connection and database.
pub type MongoListKey = (Uuid, String);
/// The sampled fields of a collection, by connection, database and collection.
pub type MongoFieldsKey = (Uuid, String, String);

pub struct AiManager {
    factory: Mutex<ClientFactory>,
    inner: Mutex<Inner>,
    metadata_wait: Mutex<Duration>,
    /// Collections and views of a MongoDB database, for prompts.
    pub mongo_lists: Arc<Flights<MongoListKey, MongoObjects>>,
    /// Sampled top-level fields of a MongoDB collection, for prompts.
    pub mongo_fields: Arc<Flights<MongoFieldsKey, Vec<String>>>,
}

impl Default for AiManager {
    fn default() -> Self {
        AiManager::new()
    }
}

impl AiManager {
    pub fn new() -> Self {
        let factory: ClientFactory =
            Arc::new(|ep| Ok(Arc::new(HttpFimClient::new(ep)?) as Arc<dyn FimClient>));
        AiManager {
            factory: Mutex::new(factory),
            inner: Mutex::new(Inner::default()),
            metadata_wait: Mutex::new(METADATA_WAIT),
            mongo_lists: Arc::new(Flights::new(METADATA_FAIL_TTL)),
            mongo_fields: Arc::new(Flights::new(METADATA_FAIL_TTL)),
        }
    }

    /// Replaces how clients are built (tests); drops the cached client.
    pub fn set_client_factory(&self, factory: ClientFactory) {
        *self.factory.lock().unwrap_or_else(|p| p.into_inner()) = factory;
        self.invalidate();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Settings or key changed: rebuild the client on next use and resume suggestions.
    pub fn invalidate(&self) {
        let mut inner = self.lock();
        inner.client = None;
        inner.pause = None;
    }

    /// The client for `cfg`, built (reading the key from `secrets`) when not cached.
    /// `None` when no base URL is set. Blocking: reads the keychain.
    pub fn client(
        &self,
        cfg: &AiConfig,
        secrets: &dyn SecretStore,
    ) -> Result<Option<Arc<dyn FimClient>>, DbError> {
        let Some(base_url) = cfg.base_url.as_deref() else {
            return Ok(None);
        };
        if let Some(client) = self.lock().client.clone() {
            return Ok(Some(client));
        }
        let key = secrets.get_app(AppSecret::AiApiKey)?;
        let endpoint = endpoint_of(base_url, key, cfg.timeout_ms).map_err(to_db_error)?;
        let factory = self
            .factory
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let client = factory(endpoint).map_err(to_db_error)?;
        self.lock().client = Some(Arc::clone(&client));
        Ok(Some(client))
    }

    /// The pause message while suggestions are paused.
    pub fn paused(&self) -> Option<String> {
        let mut inner = self.lock();
        match &inner.pause {
            Some(p) if p.until.is_some_and(|u| Instant::now() >= u) => {
                inner.pause = None;
                None
            }
            Some(p) => Some(p.message.clone()),
            None => None,
        }
    }

    /// Records what `e` means for later suggestions (spec §7).
    pub fn note_error(&self, e: &AiError) {
        let pause = match e {
            AiError::Unauthorized { .. }
            | AiError::FimUnsupported { .. }
            | AiError::BadRequest { .. } => Some(Pause {
                message: e.to_string(),
                until: None,
            }),
            AiError::RateLimited { retry_after } => Some(Pause {
                message: e.to_string(),
                until: Some(Instant::now() + retry_after.unwrap_or(DEFAULT_RATE_LIMIT_PAUSE)),
            }),
            _ => None,
        };
        if let Some(p) = pause {
            self.lock().pause = Some(p);
        }
    }

    /// A request succeeded: any pause is over.
    pub fn note_success(&self) {
        self.lock().pause = None;
    }

    /// Registers request `id`'s task so [`AiManager::cancel`] can abort it. An earlier
    /// request with the same id is aborted.
    pub fn track(&self, id: String, handle: AbortHandle) {
        if let Some(old) = self.lock().inflight.insert(id, handle) {
            old.abort();
        }
    }

    pub fn untrack(&self, id: &str) {
        self.lock().inflight.remove(id);
    }

    /// How long a request waits for the MongoDB metadata of its prompt ([`METADATA_WAIT`]).
    pub fn metadata_wait(&self) -> Duration {
        *self.metadata_wait.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Replaces [`METADATA_WAIT`] (tests).
    pub fn set_metadata_wait(&self, wait: Duration) {
        *self.metadata_wait.lock().unwrap_or_else(|p| p.into_inner()) = wait;
    }

    /// Registers request `id` at its start; an earlier request with the same id is
    /// cancelled.
    pub fn begin(self: &Arc<Self>, id: &str) -> RequestToken {
        let (tx, rx) = watch::channel(false);
        let mut inner = self.lock();
        inner.next_request += 1;
        let generation = inner.next_request;
        if let Some((_, old)) = inner.requests.insert(id.to_string(), (generation, tx)) {
            old.send_replace(true);
        }
        RequestToken {
            ai: Arc::clone(self),
            id: id.to_string(),
            generation,
            cancelled: rx,
        }
    }

    /// Cancels request `id`: before it reaches the endpoint it sends nothing, after it the
    /// endpoint call is aborted. An unknown id is ignored.
    pub fn cancel(&self, id: &str) {
        let mut inner = self.lock();
        if let Some((_, tx)) = inner.requests.remove(id) {
            tx.send_replace(true);
        }
        if let Some(h) = inner.inflight.remove(id) {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn flights_share_one_load_and_remember_failures() {
        let flights: Arc<Flights<&'static str, u32>> =
            Arc::new(Flights::new(Duration::from_millis(200)));
        let started = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<u32>>();
        let s = Arc::clone(&started);
        let a = flights
            .join("k", move || {
                s.fetch_add(1, Ordering::SeqCst);
                Some(async move { rx.await.ok().flatten() }.boxed())
            })
            .unwrap();
        let b = flights
            .join("k", || panic!("a second load of a running key"))
            .unwrap();
        // Nobody waits: the load still finishes.
        drop(a);
        tx.send(Some(7)).unwrap();
        assert_eq!(b.await, Some(7));
        assert_eq!(started.load(Ordering::SeqCst), 1);

        // Done: forgotten, the next join loads again.
        let c = flights.join("k", || Some(async { None }.boxed())).unwrap();
        assert_eq!(c.await, None);
        // Failed: not tried again for a while.
        assert!(flights.join("k", || panic!("retried at once")).is_none());
        tokio::time::sleep(Duration::from_millis(250)).await;
        let d = flights
            .join("k", || Some(async { Some(1) }.boxed()))
            .unwrap();
        assert_eq!(d.await, Some(1));
        // Nothing to load: nothing remembered.
        assert!(flights.join("n", || None).is_none());
        assert!(flights
            .join("n", || Some(async { Some(2) }.boxed()))
            .is_some());
    }

    #[tokio::test]
    async fn request_tokens_cancel_by_id() {
        let ai = Arc::new(AiManager::new());
        let mut first = ai.begin("r");
        assert!(!first.is_cancelled());
        // A newer request with the same id cancels the older one.
        let second = ai.begin("r");
        first.cancelled().await;
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        // The older one ending leaves the newer registered.
        drop(first);
        ai.cancel("r");
        assert!(second.is_cancelled());
        drop(second);
        // Unknown ids are ignored.
        ai.cancel("r");
        assert!(ai.lock().requests.is_empty());
    }

    #[test]
    fn consent_covers_mongo_from_version_2() {
        assert!(consent_covers(DbKind::MySql, 1));
        assert!(consent_covers(DbKind::Postgres, 1));
        assert!(!consent_covers(DbKind::Mongo, 1));
        assert!(consent_covers(DbKind::Mongo, 2));
        assert!(!consent_covers(DbKind::Redis, 2));
    }

    #[test]
    fn rate_limit_pause_expires() {
        let m = AiManager::new();
        m.note_error(&AiError::RateLimited {
            retry_after: Some(Duration::from_millis(0)),
        });
        assert_eq!(m.paused(), None);
        m.note_error(&AiError::RateLimited {
            retry_after: Some(Duration::from_secs(60)),
        });
        assert!(m.paused().is_some());
        m.invalidate();
        assert_eq!(m.paused(), None);
    }

    #[test]
    fn transient_errors_do_not_pause() {
        let m = AiManager::new();
        for e in [
            AiError::Timeout,
            AiError::Server { status: 503 },
            AiError::Network {
                message: "x".into(),
            },
        ] {
            m.note_error(&e);
            assert_eq!(m.paused(), None, "{e:?}");
        }
        m.note_error(&AiError::Unauthorized { status: 401 });
        assert_eq!(
            m.paused().as_deref(),
            Some("The API key was rejected (401).")
        );
        m.note_success();
        assert_eq!(m.paused(), None);
    }
}
