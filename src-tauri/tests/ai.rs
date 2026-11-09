//! AI settings, key, consent and `ai_complete` through the `*_impl` functions, with a
//! scripted `FimClient` instead of the network.

use async_trait::async_trait;
use dnt_ai::{AiError, Endpoint, FimClient, FimReply, FimRequest};
use dnt_core::config::{ConnectionConfig, DriverOptions, EnvTag, SecretMode};
use dnt_core::driver::Driver;
use dnt_core::error::DbError;
use dnt_core::secret::{AppSecret, MemorySecretStore, SecretStore};
use dnt_core::server::DbKind;
use dnt_core::test_util::sample_config;
use dnt_store::db::open_in_memory;
use dnt_store::repo::{ConnectionRepo, NewSecrets};
use dnt_testkit::fake_command::{collection_key, FakeCommandDriver, FakeValue};
use dnt_tunnel::KnownHosts;
use dntdatabase::ai::{AiConfig, AiTrigger, ClientFactory};
use dntdatabase::commands::ai::{
    ai_cancel_impl, ai_complete_impl, ai_connection_set_impl, ai_connections_impl,
    ai_consent_set_impl, ai_key_clear_impl, ai_key_set_impl, ai_models_impl, ai_settings_get_impl,
    ai_settings_set_impl, ai_test_impl, AiCompleteRequest, AiCompletion, AiConnectionChoice,
    AiProbeVerdict,
};
use dntdatabase::commands::connections::{create_connection_impl, SecretsInput};
use dntdatabase::export_paths::ExportPaths;
use dntdatabase::file_picks::FilePicks;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::{AppPaths, AppState};
use secrecy::ExposeSecret;
use std::collections::{BTreeMap, HashMap, VecDeque};
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

async fn connection_tagged(state: &AppState, env_tag: Option<EnvTag>) -> Uuid {
    let mut c = sample_config();
    c.id = Uuid::nil();
    c.env_tag = env_tag;
    let secrets: SecretsInput = serde_json::from_value(serde_json::json!({})).unwrap();
    create_connection_impl(state, c, secrets).await.unwrap().id
}

async fn connection(state: &AppState) -> Uuid {
    connection_tagged(state, None).await
}

/// Set up and agreed to, one MySQL connection (on by default).
async fn ready() -> (Fixture, Uuid) {
    let f = fixture();
    ai_settings_set_impl(&f.state, cfg("https://api.example.com/v1"))
        .await
        .unwrap();
    ai_consent_set_impl(&f.state, true).await.unwrap();
    let id = connection(&f.state).await;
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
        DbError::Ai { message, retryable } => {
            assert_eq!(message, "The API key was rejected (401).");
            assert!(!retryable);
        }
        other => panic!("expected Ai, got {other:?}"),
    }
}

#[tokio::test]
async fn consent_follows_the_origin() {
    let f = fixture();
    let id = connection(&f.state).await;
    let e = ai_consent_set_impl(&f.state, true).await.unwrap_err();
    assert_eq!(config_message(e).1, "Set the base URL first.");

    let s = ai_settings_set_impl(&f.state, cfg("https://one.example/v1"))
        .await
        .unwrap();
    assert!(!s.consented);
    // Set up, not agreed to: nothing is sent.
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);

    let s = ai_consent_set_impl(&f.state, true).await.unwrap();
    assert!(s.consented);
    assert!(ai_settings_get_impl(&f.state).await.unwrap().consented);

    // Another origin: consent no longer counts, and nothing is sent.
    let s = ai_settings_set_impl(&f.state, cfg("https://two.example/v1"))
        .await
        .unwrap();
    assert!(!s.consented);
    let out = ai_complete_impl(&f.state, request(id, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());

    // Same origin, other path: still the same consent.
    let s = ai_settings_set_impl(&f.state, cfg("https://one.example/v2"))
        .await
        .unwrap();
    assert!(s.consented);

    let s = ai_consent_set_impl(&f.state, false).await.unwrap();
    assert!(!s.consented);
}

#[tokio::test]
async fn connections_are_on_by_default_except_production() {
    let (f, local) = ready().await;
    let prod = connection_tagged(&f.state, Some(EnvTag::Production)).await;
    assert!(ai_connections_impl(&f.state).await.unwrap().is_empty());

    // Production, no choice: off.
    let out = ai_complete_impl(&f.state, request(prod, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());

    // Its own choice turns it on; the other one's turns it off.
    ai_connection_set_impl(&f.state, prod, true).await.unwrap();
    let list = ai_connection_set_impl(&f.state, local, false)
        .await
        .unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.contains(&AiConnectionChoice {
        connection_id: prod,
        enabled: true
    }));
    assert!(list.contains(&AiConnectionChoice {
        connection_id: local,
        enabled: false
    }));

    let out = ai_complete_impl(&f.state, request(local, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());
    f.fake.push(Step::Reply(Ok("1".into())));
    let out = ai_complete_impl(&f.state, request(prod, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Suggestion { text: "1".into() });

    // Withdrawn consent stops every connection, whatever its choice.
    ai_consent_set_impl(&f.state, false).await.unwrap();
    let out = ai_complete_impl(&f.state, request(prod, "SELECT |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert_eq!(f.fake.calls().len(), 1);
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
async fn turned_off_or_trigger_off_sends_nothing() {
    let (f, id) = ready().await;
    let other = connection(&f.state).await;
    ai_connection_set_impl(&f.state, other, false)
        .await
        .unwrap();
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
        match &err {
            DbError::Ai { message, retryable } => {
                assert_eq!(message, &e.to_string());
                assert!(*retryable, "{e:?}");
            }
            other => panic!("{e:?} -> {other:?}"),
        }
        let app = dnt_core::error::AppError::from(err);
        assert_eq!(app.kind, "ai");
        // A sentence: the endpoint's reason, with a full stop.
        assert!(
            app.message.starts_with(e.to_string().trim_end_matches('.')),
            "{}",
            app.message
        );
        assert!(app.message.ends_with('.'), "{}", app.message);
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

// --- MongoDB console (and Redis, which gets nothing) ---

fn mongo_cfg(env_tag: Option<EnvTag>) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Mongo,
        name: "docs".into(),
        port: 27017,
        default_db: Some("shop".into()),
        env_tag,
        ..sample_config()
    };
    c.options.driver = DriverOptions::Mongo {
        srv: false,
        auth_source: None,
        replica_set: None,
        direct_connection: false,
        extra_hosts: vec![],
    };
    c
}

fn redis_cfg() -> ConnectionConfig {
    let mut c = ConnectionConfig {
        id: Uuid::nil(),
        kind: DbKind::Redis,
        name: "cache".into(),
        port: 6379,
        ..sample_config()
    };
    c.options.driver = DriverOptions::Redis { db: 0 };
    c
}

async fn create(state: &AppState, cfg: ConnectionConfig) -> Uuid {
    let secrets: SecretsInput = serde_json::from_value(serde_json::json!({})).unwrap();
    create_connection_impl(state, cfg, secrets)
        .await
        .unwrap()
        .id
}

/// Set up and agreed to, with the app's drivers and one (not open) connection `conn`.
async fn ready_with(conn: ConnectionConfig) -> (Fixture, Uuid) {
    let f = fixture();
    ai_settings_set_impl(&f.state, cfg("https://api.example.com/v1"))
        .await
        .unwrap();
    ai_consent_set_impl(&f.state, true).await.unwrap();
    let id = create(&f.state, conn).await;
    (f, id)
}

fn in_db(mut req: AiCompleteRequest, db: &str) -> AiCompleteRequest {
    req.database = Some(db.into());
    req
}

const MONGO_FIRST: &str = "// MongoDB shell (mongosh subset, no JavaScript)\n";

#[tokio::test]
async fn mongo_sends_a_console_prompt_and_ends_the_suggestion_with_its_command() {
    // The connection is not open: no collections can be listed, the header goes without.
    let (f, id) = ready_with(mongo_cfg(None)).await;
    f.fake.push(Step::Reply(Ok(
        "status: 'paid' })\ndb.orders.countDocuments()".into(),
    )));
    let out = ai_complete_impl(&f.state, in_db(request(id, "db.orders.find({ |"), "shop"))
        .await
        .unwrap();
    assert_eq!(
        out,
        AiCompletion::Suggestion {
            text: "status: 'paid' })".into()
        }
    );
    let call = &f.fake.calls()[0];
    assert_eq!(
        call.prompt,
        format!("{MONGO_FIRST}// Database: shop\ndb.orders.find({{ ")
    );
    assert_eq!(call.suffix, "");

    // A `;` that ends it is kept; JavaScript-like text is not read as SQL (`$$` opens no
    // dollar quote).
    f.fake
        .push(Step::Reply(Ok("x: \"$$v\" });\ndb.b.find()".into())));
    let out = ai_complete_impl(&f.state, request(id, "db.c.find({ |\n"))
        .await
        .unwrap();
    assert_eq!(
        out,
        AiCompletion::Suggestion {
            text: "x: \"$$v\" });".into()
        }
    );
    let call = &f.fake.calls()[1];
    assert_eq!(call.prompt, format!("{MONGO_FIRST}db.c.find({{ "));
    assert_eq!(call.suffix, "\n");
}

#[tokio::test]
async fn mongo_secrets_send_nothing() {
    let (f, id) = ready_with(mongo_cfg(None)).await;
    for marked in [
        "db.runCommand({createUser:'u', pwd:'p'})\ndb.orders.find({ |",
        // After the cursor too.
        "db.orders.find({ |\ndb.runCommand({createUser:'u', pwd:'p'})",
        "db.orders.insertOne({ uri: 'mongodb://app:hunter2@db.example.com/shop' })\ndb.orders.find({ |",
        "// mongodb://admin:hunter2@db.example.com\ndb.orders.find({ |",
    ] {
        let out = ai_complete_impl(&f.state, request(id, marked))
            .await
            .unwrap();
        assert_eq!(out, AiCompletion::Empty, "{marked}");
    }
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn mongo_follows_consent_production_and_pause_as_sql_does() {
    let (f, local) = ready_with(mongo_cfg(None)).await;
    let prod = create(&f.state, mongo_cfg(Some(EnvTag::Production))).await;
    let ask = |id: Uuid| ai_complete_impl(&f.state, request(id, "db.orders.find({ |"));

    // Production, no choice: off.
    assert_eq!(ask(prod).await.unwrap(), AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());

    // Its own choice turns it on; the other one's turns it off.
    ai_connection_set_impl(&f.state, prod, true).await.unwrap();
    ai_connection_set_impl(&f.state, local, false)
        .await
        .unwrap();
    assert_eq!(ask(local).await.unwrap(), AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());
    f.fake.push(Step::Reply(Ok("a: 1 })".into())));
    assert_eq!(
        ask(prod).await.unwrap(),
        AiCompletion::Suggestion {
            text: "a: 1 })".into()
        }
    );

    // Paused after a rejected key: the endpoint is not called again.
    f.fake
        .push(Step::Reply(Err(AiError::Unauthorized { status: 401 })));
    assert!(matches!(
        ask(prod).await.unwrap(),
        AiCompletion::Paused { .. }
    ));
    assert!(matches!(
        ask(prod).await.unwrap(),
        AiCompletion::Paused { .. }
    ));
    assert_eq!(f.fake.calls().len(), 2);

    // Withdrawn consent stops it, whatever its choice.
    ai_consent_set_impl(&f.state, false).await.unwrap();
    assert_eq!(ask(prod).await.unwrap(), AiCompletion::Empty);
    assert_eq!(f.fake.calls().len(), 2);
}

#[tokio::test]
async fn redis_sends_nothing() {
    let (f, id) = ready_with(redis_cfg()).await;
    let out = ai_complete_impl(&f.state, request(id, "GET |"))
        .await
        .unwrap();
    assert_eq!(out, AiCompletion::Empty);
    assert!(f.fake.calls().is_empty());
}

/// The fake Command driver as MongoDB (its databases are `0`–`15`), open, with `orders` and
/// `audit` in database `0`; AI set up and agreed to.
async fn fake_mongo() -> (Fixture, Uuid) {
    let driver = FakeCommandDriver::with_kind(DbKind::Mongo);
    let store = driver.store();
    for name in ["orders", "audit"] {
        store.set("0", &collection_key(name), FakeValue::Hash(BTreeMap::new()));
    }
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let store_secrets: Arc<dyn SecretStore> = secrets.clone();
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::clone(&store_secrets),
    ));
    let sessions = SessionManager::new(
        Arc::clone(&repo),
        Arc::clone(&store_secrets),
        KnownHosts::new(dir.path().join("known_hosts")),
        HashMap::from([(DbKind::Mongo, Arc::new(driver) as Arc<dyn Driver>)]),
    );
    let mut conn = mongo_cfg(None);
    conn.default_db = Some("0".into());
    conn.secret_mode = SecretMode::Saved;
    let new_secrets = NewSecrets {
        db_password: None,
        ssh_password: None,
        ssh_key_passphrase: None,
    };
    let id = repo.create(conn, new_secrets).unwrap().id;
    sessions.open(id, None).await.unwrap();
    let state = AppState {
        repo,
        secrets: store_secrets,
        sessions,
        paths: AppPaths::in_dir(dir.path().join("data")),
        export_paths: ExportPaths::default(),
        file_picks: FilePicks::default(),
        ai: Default::default(),
    };
    let fake = Arc::new(Fake::default());
    let f2 = Arc::clone(&fake);
    state.ai.set_client_factory(Arc::new(move |ep: Endpoint| {
        f2.built.lock().unwrap().push(ep);
        Ok(Arc::new(FakeClient(Arc::clone(&f2))) as Arc<dyn FimClient>)
    }));
    ai_settings_set_impl(&state, cfg("https://api.example.com/v1"))
        .await
        .unwrap();
    ai_consent_set_impl(&state, true).await.unwrap();
    let f = Fixture {
        _dir: dir,
        state,
        secrets,
        fake,
    };
    (f, id)
}

#[tokio::test]
async fn mongo_header_lists_the_collections_and_the_sampled_fields() {
    let (f, id) = fake_mongo().await;
    f.fake.push(Step::Reply(Ok("status: 1 })".into())));
    let out = ai_complete_impl(&f.state, in_db(request(id, "db.orders.find({ |})"), "0"))
        .await
        .unwrap();
    assert_eq!(
        out,
        AiCompletion::Suggestion {
            text: "status: 1".into()
        }
    );
    let call = &f.fake.calls()[0];
    assert_eq!(
        call.prompt,
        format!(
            "{MONGO_FIRST}// Database: 0\n\
             // Collections:\n\
             //   orders(_id, value)\n\
             // Other collections: audit\n\
             db.orders.find({{ "
        )
    );
    assert_eq!(call.suffix, "})");

    // Nothing named: the names alone.
    f.fake.push(Step::Reply(Ok("db.audit.find()".into())));
    ai_complete_impl(&f.state, in_db(request(id, "|"), "0"))
        .await
        .unwrap();
    assert_eq!(
        f.fake.calls()[1].prompt,
        format!("{MONGO_FIRST}// Database: 0\n// Other collections: audit, orders\n")
    );
}
