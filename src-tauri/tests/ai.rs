//! AI settings, key, consent and `ai_complete` through the `*_impl` functions, with a
//! scripted `FimClient` instead of the network.

use async_trait::async_trait;
use dnt_ai::{AiError, Endpoint, FimClient, FimReply, FimRequest};
use dnt_core::error::DbError;
use dnt_core::secret::{AppSecret, MemorySecretStore, SecretStore};
use dnt_core::test_util::sample_config;
use dntdatabase::ai::{AiConfig, AiTrigger, ClientFactory};
use dntdatabase::commands::ai::{
    ai_cancel_impl, ai_complete_impl, ai_key_clear_impl, ai_key_set_impl, ai_models_impl,
    ai_opt_in_list_impl, ai_opt_in_set_impl, ai_settings_get_impl, ai_settings_set_impl,
    ai_test_impl, AiCompleteRequest, AiCompletion, AiProbeVerdict,
};
use dntdatabase::commands::connections::{create_connection_impl, SecretsInput};
use dntdatabase::state::{AppPaths, AppState};
use secrecy::ExposeSecret;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const KEY: &str = "sk-ai-very-secret-123";

enum Step {
    Reply(Result<String, AiError>),
    /// Never answers.
    Hang,
}

#[derive(Default)]
struct Fake {
    script: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<FimRequest>>,
    /// Endpoints the factory was given (one per client built).
    built: Mutex<Vec<Endpoint>>,
}

impl Fake {
    fn push(&self, step: Step) {
        self.script.lock().unwrap().push_back(step);
    }
    fn calls(&self) -> Vec<FimRequest> {
        self.calls.lock().unwrap().clone()
    }
    fn built(&self) -> usize {
        self.built.lock().unwrap().len()
    }
}

struct FakeClient(Arc<Fake>);

#[async_trait]
impl FimClient for FakeClient {
    async fn complete(&self, req: &FimRequest) -> Result<FimReply, AiError> {
        self.0.calls.lock().unwrap().push(req.clone());
        let step = self.0.script.lock().unwrap().pop_front();
        match step {
            Some(Step::Reply(r)) => r.map(|text| FimReply {
                text,
                finish_reason: Some("stop".into()),
                elapsed: Duration::from_millis(12),
            }),
            Some(Step::Hang) => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                unreachable!()
            }
            None => panic!("unscripted request"),
        }
    }
    async fn list_models(&self) -> Result<Vec<String>, AiError> {
        Ok(vec!["coder-a".into(), "coder-b".into()])
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: AppState,
    secrets: Arc<MemorySecretStore>,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let state = AppState::open(AppPaths::in_dir(dir.path().join("data")), secrets.clone()).unwrap();
    let fake = Arc::new(Fake::default());
    let f2 = Arc::clone(&fake);
    let factory: ClientFactory = Arc::new(move |ep: Endpoint| {
        f2.built.lock().unwrap().push(ep);
        Ok(Arc::new(FakeClient(Arc::clone(&f2))) as Arc<dyn FimClient>)
    });
    state.ai.set_client_factory(factory);
    Fixture {
        _dir: dir,
        state,
        secrets,
        fake,
    }
}

fn cfg(base_url: &str) -> AiConfig {
    AiConfig {
        base_url: Some(base_url.into()),
        model: Some("coder".into()),
        ..AiConfig::default()
    }
}

async fn connection(state: &AppState) -> Uuid {
    let mut c = sample_config();
    c.id = Uuid::nil();
    let secrets: SecretsInput = serde_json::from_value(serde_json::json!({})).unwrap();
    create_connection_impl(state, c, secrets).await.unwrap().id
}

/// Set up, one opted-in MySQL connection.
async fn ready() -> (Fixture, Uuid) {
    let f = fixture();
    ai_settings_set_impl(&f.state, cfg("https://api.example.com/v1"))
        .await
        .unwrap();
    let id = connection(&f.state).await;
    ai_opt_in_set_impl(&f.state, id, true).await.unwrap();
    (f, id)
}

/// A request with the cursor at `|`.
fn request(conn_id: Uuid, marked: &str) -> AiCompleteRequest {
    let at = marked.find('|').unwrap();
    let text = marked.replacen('|', "", 1);
    AiCompleteRequest {
        request_id: "r1".into(),
        conn_id,
        database: None,
        schema: None,
        cursor: text[..at].encode_utf16().count() as u32,
        text,
    }
}

fn config_message(e: DbError) -> (Option<String>, String) {
    match e {
        DbError::Config { field, message } => (field, message),
        other => panic!("expected Config, got {other:?}"),
    }
}

#[tokio::test]
async fn settings_defaults_and_validation() {
    let f = fixture();
    let s = ai_settings_get_impl(&f.state).await.unwrap();
    assert_eq!(s.config, AiConfig::default());
    assert_eq!(s.config.trigger, AiTrigger::Automatic);
    assert!(!s.has_key);
    assert_eq!(s.origin, None);

    let e = ai_settings_set_impl(&f.state, cfg("api.example.com"))
        .await
        .unwrap_err();
    assert_eq!(
        config_message(e),
        (
            Some("base_url".into()),
            "The base URL must start with http:// or https://.".into()
        )
    );
    let e = ai_settings_set_impl(
        &f.state,
        AiConfig {
            max_tokens: 5,
            ..cfg("https://api.example.com/v1")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        config_message(e),
        (
            Some("max_tokens".into()),
            "Max tokens must be between 16 and 512.".into()
        )
    );

    let s = ai_settings_set_impl(
        &f.state,
        AiConfig {
            model: Some("  coder  ".into()),
            trigger: AiTrigger::Manual,
            ..cfg(" https://api.example.com/v1/ ")
        },
    )
    .await
    .unwrap();
    assert_eq!(
        s.config.base_url.as_deref(),
        Some("https://api.example.com/v1")
    );
    assert_eq!(s.config.model.as_deref(), Some("coder"));
    assert_eq!(s.config.trigger, AiTrigger::Manual);
    assert_eq!(s.origin.as_deref(), Some("https://api.example.com"));
    assert_eq!(s.host.as_deref(), Some("api.example.com"));
    // Stored: a fresh read returns the same.
    assert_eq!(ai_settings_get_impl(&f.state).await.unwrap(), s);

    // Blank clears.
    let s = ai_settings_set_impl(
        &f.state,
        AiConfig {
            base_url: Some("  ".into()),
            ..AiConfig::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(s.config.base_url, None);
    assert_eq!(s.origin, None);
}

#[tokio::test]
async fn key_is_stored_never_returned() {
    let f = fixture();
    let e = ai_key_set_impl(&f.state, "   ".into()).await.unwrap_err();
    assert_eq!(config_message(e).1, "Enter an API key.");

    let s = ai_key_set_impl(&f.state, format!(" {KEY} ")).await.unwrap();
    assert!(s.has_key);
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains(KEY), "{json}");
    let stored = f.secrets.get_app(AppSecret::AiApiKey).unwrap().unwrap();
    assert_eq!(stored.expose_secret(), KEY);

    let s = ai_key_clear_impl(&f.state).await.unwrap();
    assert!(!s.has_key);
    assert!(f.secrets.get_app(AppSecret::AiApiKey).unwrap().is_none());
}

#[tokio::test]
async fn client_is_cached_and_rebuilt_after_a_change() {
    let f = fixture();
    let e = ai_models_impl(&f.state).await.unwrap_err();
    assert_eq!(config_message(e).1, "Set the base URL first.");

    ai_settings_set_impl(&f.state, cfg("https://api.example.com/v1"))
        .await
        .unwrap();
    assert_eq!(
        ai_models_impl(&f.state).await.unwrap(),
        ["coder-a", "coder-b"]
    );
    ai_models_impl(&f.state).await.unwrap();
    assert_eq!(f.fake.built(), 1);
    assert!(f.fake.built.lock().unwrap()[0].api_key.is_none());

    ai_key_set_impl(&f.state, KEY.into()).await.unwrap();
    ai_models_impl(&f.state).await.unwrap();
    assert_eq!(f.fake.built(), 2);
    let built = f.fake.built.lock().unwrap();
    assert_eq!(built[1].api_key.as_ref().unwrap().expose_secret(), KEY);
    assert_eq!(built[1].origin(), "https://api.example.com");
}

#[tokio::test]
async fn test_reports_verdicts() {
    let f = fixture();
    ai_settings_set_impl(
        &f.state,
        AiConfig {
            model: None,
            ..cfg("https://api.example.com/v1")
        },
    )
    .await
    .unwrap();
    let e = ai_test_impl(&f.state).await.unwrap_err();
    assert_eq!(
        config_message(e),
        (Some("model".into()), "Choose a model first.".into())
    );

    ai_settings_set_impl(&f.state, cfg("https://api.example.com/v1"))
        .await
        .unwrap();
    f.fake.push(Step::Reply(Ok("active = 1".into())));
    let r = ai_test_impl(&f.state).await.unwrap();
    assert_eq!(r.verdict, AiProbeVerdict::Ok);
    assert_eq!(r.sample.as_deref(), Some("active = 1"));
    assert_eq!(r.elapsed_ms, 12);
    let call = &f.fake.calls()[0];
    assert_eq!(call.prompt, dnt_ai::PROBE_PROMPT);
    assert_eq!(call.suffix, dnt_ai::PROBE_SUFFIX);
    assert_eq!(call.model, "coder");

    f.fake
        .push(Step::Reply(Ok("active = 1\nORDER BY id;".into())));
    assert_eq!(
        ai_test_impl(&f.state).await.unwrap().verdict,
        AiProbeVerdict::IgnoresSuffix
    );

    f.fake
        .push(Step::Reply(Err(AiError::Unauthorized { status: 401 })));
    match ai_test_impl(&f.state).await.unwrap_err() {
        DbError::Auth { message } => assert_eq!(message, "The API key was rejected (401)."),
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn consent_follows_the_origin() {
    let f = fixture();
    let id = connection(&f.state).await;
    let e = ai_opt_in_set_impl(&f.state, id, true).await.unwrap_err();
    assert_eq!(config_message(e).1, "Set the base URL first.");

    ai_settings_set_impl(&f.state, cfg("https://one.example/v1"))
        .await
        .unwrap();
    let list = ai_opt_in_set_impl(&f.state, id, true).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].connection_id, id);
    assert_eq!(list[0].name, "Local MySQL");
    assert!(list[0].active);

    // Another origin: consent no longer counts, and nothing is sent.
    ai_settings_set_impl(&f.state, cfg("https://two.example/v1"))
        .await
        .unwrap();
    assert!(!ai_opt_in_list_impl(&f.state).await.unwrap()[0].active);
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());

    // Same origin, other path: still the same consent.
    ai_settings_set_impl(&f.state, cfg("https://one.example/v2"))
        .await
        .unwrap();
    assert!(ai_opt_in_list_impl(&f.state).await.unwrap()[0].active);

    let list = ai_opt_in_set_impl(&f.state, id, false).await.unwrap();
    assert!(list.is_empty());
}

#[tokio::test]
async fn complete_returns_the_cleaned_suggestion() {
    let (f, id) = ready().await;
    f.fake.push(Step::Reply(Ok("id = 1;\nSELECT 2;".into())));
    let out = ai_complete_impl(&f.state, request(id, "SELECT * FROM t WHERE |"))
        .await
        .unwrap();
    assert_eq!(
        out,
        AiCompletion::Suggestion {
            text: "id = 1;".into()
        }
    );
    let call = &f.fake.calls()[0];
    assert_eq!(call.model, "coder");
    assert_eq!(call.max_tokens, 128);
    assert!(
        call.prompt.starts_with("-- Dialect: MySQL\n"),
        "{}",
        call.prompt
    );
    assert!(
        call.prompt.ends_with("SELECT * FROM t WHERE "),
        "{}",
        call.prompt
    );
    assert_eq!(call.suffix, "");
}

#[tokio::test]
async fn not_opted_in_or_off_sends_nothing() {
    let (f, id) = ready().await;
    let other = connection(&f.state).await;
    let out = ai_complete_impl(&f.state, request(other, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);

    ai_settings_set_impl(
        &f.state,
        AiConfig {
            trigger: AiTrigger::Off,
            ..cfg("https://api.example.com/v1")
        },
    )
    .await
    .unwrap();
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn cursor_in_a_secret_sends_nothing() {
    let (f, id) = ready().await;
    let out = ai_complete_impl(&f.state, request(id, "CREATE USER u IDENTIFIED BY 'hunt|"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn unauthorized_pauses_until_the_key_changes() {
    let (f, id) = ready().await;
    f.fake
        .push(Step::Reply(Err(AiError::Unauthorized { status: 401 })));
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(
        out,
        AiCompletion::Paused {
            message: "The API key was rejected (401).".into()
        }
    );
    // Paused: the endpoint is not called again.
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert!(matches!(out, AiCompletion::Paused { .. }), "{out:?}");
    assert_eq!(f.fake.calls().len(), 1);

    ai_key_set_impl(&f.state, KEY.into()).await.unwrap();
    f.fake.push(Step::Reply(Ok("1".into())));
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Suggestion { text: "1".into() });
}

#[tokio::test]
async fn rate_limit_pauses_for_retry_after() {
    let (f, id) = ready().await;
    f.fake.push(Step::Reply(Err(AiError::RateLimited {
        retry_after: Some(Duration::from_millis(200)),
    })));
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert!(matches!(out, AiCompletion::Paused { .. }), "{out:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.fake.push(Step::Reply(Ok("1".into())));
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Suggestion { text: "1".into() });
}

#[tokio::test]
async fn transient_errors_are_errors_not_internal() {
    let (f, id) = ready().await;
    for e in [
        AiError::Timeout,
        AiError::Server { status: 503 },
        AiError::Network {
            message: "Could not reach the endpoint".into(),
        },
    ] {
        f.fake.push(Step::Reply(Err(e.clone())));
        let err = ai_complete_impl(&f.state, request(id, "SELECT |"))
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Connect { .. }), "{e:?} -> {err:?}");
    }
    // Not paused.
    f.fake.push(Step::Reply(Ok("1".into())));
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Suggestion { text: "1".into() });
}

#[tokio::test]
async fn cancel_ends_the_request_empty() {
    let (f, id) = ready().await;
    f.fake.push(Step::Hang);
    let state = Arc::new(f.state);
    let s2 = Arc::clone(&state);
    let pending = tokio::spawn(async move { ai_complete_impl(&s2, request(id, "SELECT |")).await });
    // Wait until the request reached the client.
    for _ in 0..100 {
        if !f.fake.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ai_cancel_impl(&state, "r1");
    let out = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .expect("cancel ends the request")
        .unwrap()
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
}
