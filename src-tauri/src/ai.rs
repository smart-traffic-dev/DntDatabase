//! AI editor completion in the app (spec `2026-11-04-ai-editor-completion-design.md`):
//! the settings stored in `app_settings`, and [`AiManager`], which owns the endpoint
//! client, the pause state of spec §7 and the in-flight requests.
//!
//! The API key lives in the OS keychain (`AppSecret::AiApiKey`); it is read here, just
//! before a client is built, and never leaves the backend.

use dnt_ai::{AiError, Endpoint, FimClient, HttpFimClient};
use dnt_core::error::DbError;
use dnt_core::secret::{AppSecret, SecretStore};
use dnt_store::repo::ConnectionRepo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::task::AbortHandle;

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

/// The settings as the UI sees them.
pub fn settings_view(cfg: AiConfig, has_key: bool) -> AiSettings {
    let endpoint = cfg
        .base_url
        .as_deref()
        .and_then(|u| endpoint_of(u, None, cfg.timeout_ms).ok());
    AiSettings {
        origin: endpoint.as_ref().map(Endpoint::origin),
        host: endpoint.as_ref().map(|e| e.host().to_string()),
        config: cfg,
        has_key,
    }
}

/// An [`AiError`] as the command layer reports it. Never `Internal`: the UI reports
/// those as bugs, and these are the endpoint's or the settings' problems.
pub fn to_db_error(e: AiError) -> DbError {
    match e {
        AiError::Config { message } => config_err("ai", message),
        AiError::Unauthorized { .. } => DbError::Auth {
            message: e.to_string(),
        },
        other => DbError::Connect {
            message: other.to_string(),
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
}

pub struct AiManager {
    factory: Mutex<ClientFactory>,
    inner: Mutex<Inner>,
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

    /// Aborts request `id`; an unknown id is ignored.
    pub fn cancel(&self, id: &str) {
        if let Some(h) = self.lock().inflight.remove(id) {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
