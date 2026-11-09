//! AI editor completion commands (spec `2026-11-04-ai-editor-completion-design.md`):
//! Settings → AI (settings, key, models, Test), consent (once, to the endpoint) and each
//! connection's own on / off choice, and the suggestion request of the query editor.
//!
//! The API key reaches the backend in `ai_key_set` and nothing ever sends it back.

use super::browse::{filter_command_objects, folder_key, object_key};
use super::{blocking, guard};
use crate::ai::{
    consent_covers, load_config, origin_of, save_config, settings_view, to_db_error,
    validate_config, AiConfig, AiSettings, AiTrigger, LoadStart, MongoObjects, RequestToken,
    SharedLoad,
};
use crate::state::AppState;
use dnt_ai::post::{
    clean, judge_probe, Language, PostCtx, ProbeVerdict, PROBE_PROMPT, PROBE_SUFFIX,
};
use dnt_ai::prompt::{build, FimPrompt, PromptInput, TableBrief};
use dnt_ai::{mongo_draft, AiError, CollectionBrief, FimClient, FimRequest, MongoDraft};
use dnt_core::command::CommandLanguage;
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef, ObjectSummary, Scope, TableDetail};
use dnt_core::secret::AppSecret;
use dnt_core::server::{DbKind, ServerVariant};
use dnt_core::sql::Dialect;
use dnt_store::ai::AiScope;
use futures_util::future::{select, Either, FutureExt};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::State;
use uuid::Uuid;

fn config_err(field: &str, message: &str) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: message.into(),
    }
}

async fn settings(state: &AppState) -> Result<AiSettings, DbError> {
    let repo = Arc::clone(&state.repo);
    let secrets = Arc::clone(&state.secrets);
    blocking(move || {
        let has_key = secrets.has_app(AppSecret::AiApiKey)?;
        let consent = repo.ai_consent(AiScope::Editor)?;
        Ok(settings_view(
            load_config(&repo),
            has_key,
            consent.as_ref().map(|c| (c.origin.as_str(), c.version)),
        ))
    })
    .await
}

async fn config(state: &AppState) -> Result<AiConfig, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || Ok(load_config(&repo))).await
}

/// The client for `cfg` (`None` when no base URL is set).
async fn client(state: &AppState, cfg: &AiConfig) -> Result<Option<Arc<dyn FimClient>>, DbError> {
    let ai = Arc::clone(&state.ai);
    let secrets = Arc::clone(&state.secrets);
    let cfg = cfg.clone();
    blocking(move || ai.client(&cfg, &*secrets)).await
}

async fn require_client(state: &AppState, cfg: &AiConfig) -> Result<Arc<dyn FimClient>, DbError> {
    client(state, cfg)
        .await?
        .ok_or_else(|| config_err("base_url", "Set the base URL first."))
}

pub async fn ai_settings_get_impl(state: &AppState) -> Result<AiSettings, DbError> {
    settings(state).await
}

pub async fn ai_settings_set_impl(
    state: &AppState,
    input: AiConfig,
) -> Result<AiSettings, DbError> {
    let cfg = validate_config(input)?;
    let repo = Arc::clone(&state.repo);
    blocking(move || save_config(&repo, &cfg)).await?;
    state.ai.invalidate();
    settings(state).await
}

pub async fn ai_key_set_impl(state: &AppState, key: String) -> Result<AiSettings, DbError> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(config_err("api_key", "Enter an API key."));
    }
    let secrets = Arc::clone(&state.secrets);
    blocking(move || secrets.set_app(AppSecret::AiApiKey, &SecretString::from(key))).await?;
    state.ai.invalidate();
    settings(state).await
}

pub async fn ai_key_clear_impl(state: &AppState) -> Result<AiSettings, DbError> {
    let secrets = Arc::clone(&state.secrets);
    blocking(move || secrets.delete_app(AppSecret::AiApiKey)).await?;
    state.ai.invalidate();
    settings(state).await
}

pub async fn ai_models_impl(state: &AppState) -> Result<Vec<String>, DbError> {
    let cfg = config(state).await?;
    let client = require_client(state, &cfg).await?;
    client.list_models().await.map_err(to_db_error)
}

/// What Settings → Test concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum AiProbeVerdict {
    Ok,
    /// The reply continues past the suffix: the model most likely ignores `suffix`.
    IgnoresSuffix,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct AiTestResult {
    pub verdict: AiProbeVerdict,
    pub elapsed_ms: u32,
    /// The cleaned suggestion for the probe.
    pub sample: Option<String>,
}

/// Sends the probe of spec §6.2 with the stored settings.
pub async fn ai_test_impl(state: &AppState) -> Result<AiTestResult, DbError> {
    let cfg = config(state).await?;
    let client = require_client(state, &cfg).await?;
    let model = cfg
        .model
        .clone()
        .ok_or_else(|| config_err("model", "Choose a model first."))?;
    let mut req = FimRequest::new(model, PROBE_PROMPT.to_string(), PROBE_SUFFIX.to_string());
    req.max_tokens = cfg.max_tokens;
    let reply = client.complete(&req).await.map_err(|e| {
        state.ai.note_error(&e);
        to_db_error(e)
    })?;
    state.ai.note_success();
    let ctx = PostCtx {
        language: Language::Sql(Dialect::Postgres),
        statement_head: PROBE_PROMPT,
        prefix_tail: PROBE_PROMPT,
        suffix: PROBE_SUFFIX,
        max_lines: cfg.max_lines as usize,
    };
    Ok(AiTestResult {
        verdict: match judge_probe(&reply.text) {
            ProbeVerdict::Ok => AiProbeVerdict::Ok,
            ProbeVerdict::IgnoresSuffix => AiProbeVerdict::IgnoresSuffix,
            ProbeVerdict::Empty => AiProbeVerdict::Empty,
        },
        elapsed_ms: u32::try_from(reply.elapsed.as_millis()).unwrap_or(u32::MAX),
        sample: clean(&reply.text, &ctx),
    })
}

/// Agrees (to the current endpoint's origin) or stops agreeing that editor text and schema
/// names may leave the machine, for every connection AI is on for.
pub async fn ai_consent_set_impl(state: &AppState, granted: bool) -> Result<AiSettings, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || {
        if granted {
            let origin = origin_of(&load_config(&repo))
                .ok_or_else(|| config_err("base_url", "Set the base URL first."))?;
            repo.ai_consent_grant(AiScope::Editor, &origin)
        } else {
            repo.ai_consent_revoke(AiScope::Editor)
        }
    })
    .await?;
    settings(state).await
}

/// A connection's own AI choice. A connection without one is on, unless tagged Production
/// ([`ai_on_by_default`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct AiConnectionChoice {
    pub connection_id: Uuid,
    pub enabled: bool,
}

/// Whether AI is on for a connection that made no choice of its own. (The UI has the same rule.)
pub fn ai_on_by_default(cfg: &ConnectionConfig) -> bool {
    cfg.env_tag != Some(EnvTag::Production)
}

pub async fn ai_connections_impl(state: &AppState) -> Result<Vec<AiConnectionChoice>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || {
        Ok(repo
            .ai_connection_choices(AiScope::Editor)?
            .into_iter()
            .map(|(connection_id, enabled)| AiConnectionChoice {
                connection_id,
                enabled,
            })
            .collect())
    })
    .await
}

/// Turns AI on or off for connection `conn_id`, whatever the default.
pub async fn ai_connection_set_impl(
    state: &AppState,
    conn_id: Uuid,
    enabled: bool,
) -> Result<Vec<AiConnectionChoice>, DbError> {
    let repo = Arc::clone(&state.repo);
    blocking(move || repo.ai_connection_set(conn_id, AiScope::Editor, enabled)).await?;
    ai_connections_impl(state).await
}

/// One suggestion request of the query editor.
#[derive(Debug, Clone, Deserialize, specta::Type)]
pub struct AiCompleteRequest {
    /// Chosen by the UI; `ai_cancel` names it.
    pub request_id: String,
    pub conn_id: Uuid,
    pub database: Option<String>,
    pub schema: Option<String>,
    /// The whole editor document.
    pub text: String,
    /// Monaco offset (UTF-16).
    pub cursor: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiCompletion {
    Suggestion {
        text: String,
    },
    /// Nothing to show (not set up, not allowed here, cursor in a secret, cancelled, or
    /// the model had nothing useful).
    Empty,
    /// Suggestions are paused after an endpoint error (spec §7).
    Paused {
        message: String,
    },
}

/// Whether editor text of connection `conn` may go to the endpoint of `cfg`: the user agreed to
/// its origin, and AI is on for the connection. Blocking.
fn may_send(
    repo: &dnt_store::repo::ConnectionRepo,
    cfg: &AiConfig,
    conn: &ConnectionConfig,
) -> Result<bool, DbError> {
    let Some(origin) = origin_of(cfg) else {
        return Ok(false);
    };
    let Some(consent) = repo.ai_consent(AiScope::Editor)? else {
        return Ok(false);
    };
    // The consent text the user agreed to must name the connection's engine (MongoDB from
    // version 2).
    if consent.origin != origin || !consent_covers(conn.kind, consent.version) {
        return Ok(false);
    }
    let choice = repo
        .ai_connection_choices(AiScope::Editor)?
        .into_iter()
        .find(|(id, _)| *id == conn.id)
        .map(|(_, enabled)| enabled);
    Ok(choice.unwrap_or_else(|| ai_on_by_default(conn)))
}

fn server_name(variant: ServerVariant) -> &'static str {
    match variant {
        ServerVariant::MySql57 | ServerVariant::MySql8 => "MySQL",
        ServerVariant::MariaDb => "MariaDB",
        ServerVariant::Postgres => "PostgreSQL",
        ServerVariant::Mongo => "MongoDB",
        ServerVariant::Redis => "Redis",
    }
}

/// The schema unqualified names resolve to on PostgreSQL when the editor names none: as the
/// editor's own completion has it (`currentSchemaOf`), `public` when the database has it, else
/// the schema of the first table. The cache does not say which `search_path` entry is first.
fn current_schema(tables: &[TableBrief]) -> Option<String> {
    let mut schemas = tables.iter().filter_map(|t| t.schema.as_deref());
    let first = schemas.clone().next()?;
    Some(
        if schemas.any(|s| s == "public") {
            "public"
        } else {
            first
        }
        .to_string(),
    )
}

/// The language a prompt is written in, and its suggestion read in.
enum PromptLanguage {
    Sql(Dialect),
    /// The MongoDB console, from the registered driver.
    Mongo(Arc<dyn CommandLanguage>),
}

/// The SQL prompt: the schema cache as completion uses it as the header (no tables when it
/// cannot be had). `None` when the cursor is in a secret.
async fn sql_prompt(
    state: &AppState,
    req: &AiCompleteRequest,
    dialect: Dialect,
) -> Option<FimPrompt> {
    let conn_id = req.conn_id;
    let version = state.sessions.runtime_opt(conn_id).map(|rt| {
        let info = rt.server_info();
        let (a, b, c) = info.version;
        format!("{} {a}.{b}.{c}", server_name(info.variant))
    });
    let tables: Vec<TableBrief> = match req.database.as_deref() {
        Some(db) => match state.sessions.schema_cache_for(conn_id, db).await {
            Ok(cache) => cache
                .tables
                .into_iter()
                .map(|t| TableBrief {
                    schema: t.schema,
                    name: t.name,
                    columns: t.columns,
                })
                .collect(),
            Err(e) => {
                tracing::debug!(error = %e, "AI completion without the schema cache");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let schema = req.schema.clone().or_else(|| {
        matches!(dialect, Dialect::Postgres)
            .then(|| current_schema(&tables))
            .flatten()
    });
    build(&PromptInput {
        dialect,
        server_version: version.as_deref(),
        database: req.database.as_deref(),
        schema: schema.as_deref(),
        text: &req.text,
        cursor_utf16: req.cursor,
        tables: &tables,
    })
    .ok()
}

/// Collections whose fields are sampled for one prompt: those the statement names first.
const MONGO_SAMPLED_MAX: usize = 3;

/// The MongoDB console prompt: the database's collections as the header, with the sampled
/// fields of those the statement names. `None` when the text holds a secret or the request
/// was cancelled meanwhile.
async fn mongo_prompt(
    state: &AppState,
    req: &AiCompleteRequest,
    language: &dyn CommandLanguage,
    token: &mut RequestToken,
) -> Option<FimPrompt> {
    let draft = mongo_draft(language, &req.text, req.cursor).ok()?;
    let collections = match req.database.as_deref() {
        Some(db) => mongo_collections(state, req.conn_id, db, &draft, token).await?,
        None => Vec::new(),
    };
    Some(draft.build(req.database.as_deref(), &collections))
}

/// What waiting for a load until `deadline` gave: its value, `Some(None)` when it failed or
/// the deadline came first (the caller takes what is cached), `None` when the request was
/// cancelled.
async fn wait_load<V: Clone>(
    load: Option<SharedLoad<V>>,
    deadline: tokio::time::Instant,
    token: &mut RequestToken,
) -> Option<Option<V>> {
    let Some(load) = load else {
        return Some(None);
    };
    let timed = Box::pin(tokio::time::timeout_at(deadline, load));
    let cancelled = Box::pin(token.cancelled());
    match select(timed, cancelled).await {
        Either::Left((Ok(value), _)) => Some(value),
        Either::Left((Err(_), _)) => Some(None),
        Either::Right(_) => None,
    }
}

fn mongo_scope(db: &str) -> Scope {
    Scope {
        database: db.to_string(),
        schema: None,
    }
}

/// The collections, then the views, of `db` as the tree lists them (system ones hidden),
/// loaded through the tree's cache; `None` when the connection is not open.
fn mongo_list_load(state: &AppState, id: Uuid, db: &str) -> LoadStart<MongoObjects> {
    let mut loads = Vec::new();
    for kind in [ObjectKind::Collection, ObjectKind::View] {
        let scope = mongo_scope(db);
        let key = folder_key(&scope, kind);
        let load = state
            .sessions
            .browse_owned(id, Some(db.to_string()), key, move |i| {
                let scope = scope.clone();
                Box::pin(async move { i.list_objects(&scope, kind).await })
            })
            .ok()?;
        loads.push((kind, load));
    }
    Some(
        async move {
            let mut out = Vec::new();
            for (kind, load) in loads {
                match load.await {
                    Ok(list) => out.extend(
                        filter_command_objects(DbKind::Mongo, list, false)
                            .into_iter()
                            .map(|o| (o.name, kind)),
                    ),
                    // Without the collections nothing is worth listing.
                    Err(e) if kind == ObjectKind::Collection => {
                        tracing::debug!(error = %e, "AI completion without the MongoDB collections");
                        return None;
                    }
                    Err(e) => tracing::debug!(error = %e, "AI completion without the MongoDB views"),
                }
            }
            Some(out)
        }
        .boxed(),
    )
}

/// [`mongo_list_load`]'s result as the tree's cache has it now (nothing loaded).
fn mongo_list_cached(state: &AppState, id: Uuid, db: &str) -> MongoObjects {
    let scope = mongo_scope(db);
    let mut out = Vec::new();
    for kind in [ObjectKind::Collection, ObjectKind::View] {
        let list: Vec<ObjectSummary> = state
            .sessions
            .cached(id, &folder_key(&scope, kind))
            .unwrap_or_default();
        out.extend(
            filter_command_objects(DbKind::Mongo, list, false)
                .into_iter()
                .map(|o| (o.name, kind)),
        );
    }
    out
}

/// The sampled top-level fields of `obj` (`table_detail`, through the tree's cache);
/// `None` when the connection is not open.
fn mongo_fields_load(state: &AppState, id: Uuid, obj: ObjectRef) -> LoadStart<Vec<String>> {
    let key = object_key(&obj, "detail");
    let db = obj.database.clone();
    let load = state
        .sessions
        .browse_owned(id, Some(db), key, move |i| {
            let obj = obj.clone();
            Box::pin(async move { i.table_detail(&obj).await })
        })
        .ok()?;
    Some(
        async move {
            match load.await {
                Ok(detail) => Some(detail.columns.into_iter().map(|c| c.name).collect()),
                Err(e) => {
                    tracing::debug!(error = %e, "AI completion without a collection's fields");
                    None
                }
            }
        }
        .boxed(),
    )
}

/// The collections and views of `db`, with the sampled top-level fields of the ones `draft`'s
/// statement names. The loads are single-flight and detached ([`crate::ai::Flights`]): the request waits
/// for them at most [`AiManager::metadata_wait`] in all, then goes on with what the tree's
/// cache has (names only, or no fields), while they finish and fill the cache for the next
/// request. A failed load is not tried again for a minute. `None` when the request was
/// cancelled meanwhile.
async fn mongo_collections(
    state: &AppState,
    id: Uuid,
    db: &str,
    draft: &MongoDraft<'_>,
    token: &mut RequestToken,
) -> Option<Vec<CollectionBrief>> {
    let deadline = tokio::time::Instant::now() + state.ai.metadata_wait();
    let load = state
        .ai
        .mongo_lists
        .join((id, db.to_string()), || mongo_list_load(state, id, db));
    let objects = match wait_load(load, deadline, token).await? {
        Some(objects) => objects,
        None => mongo_list_cached(state, id, db),
    };
    let names: Vec<&str> = objects.iter().map(|(name, _)| name.as_str()).collect();
    let referenced = draft.referenced(&names);
    // Every load starts before the first is waited for: they share the deadline.
    let mut loads = Vec::new();
    for k in referenced.into_iter().take(MONGO_SAMPLED_MAX) {
        let (name, kind) = &objects[k];
        let obj = ObjectRef {
            database: db.to_string(),
            schema: None,
            name: name.clone(),
            kind: *kind,
        };
        let o = obj.clone();
        let load = state
            .ai
            .mongo_fields
            .join((id, db.to_string(), name.clone()), || {
                mongo_fields_load(state, id, o)
            });
        loads.push((k, obj, load));
    }
    let mut fields: Vec<Vec<String>> = vec![Vec::new(); objects.len()];
    for (k, obj, load) in loads {
        fields[k] = match wait_load(load, deadline, token).await? {
            Some(f) => f,
            None => state
                .sessions
                .cached::<TableDetail>(id, &object_key(&obj, "detail"))
                .map(|d| d.columns.into_iter().map(|c| c.name).collect())
                .unwrap_or_default(),
        };
    }
    Some(
        objects
            .into_iter()
            .zip(fields)
            .map(|((name, _), fields)| CollectionBrief { name, fields })
            .collect(),
    )
}

pub async fn ai_complete_impl(
    state: &AppState,
    req: AiCompleteRequest,
) -> Result<AiCompletion, DbError> {
    // From its start: `ai_cancel` while the prompt is being built stops it before it is sent.
    let mut token = state.ai.begin(&req.request_id);
    let cfg = config(state).await?;
    let Some(model) = cfg.model.clone() else {
        return Ok(AiCompletion::Empty);
    };
    if cfg.trigger == AiTrigger::Off || cfg.base_url.is_none() {
        return Ok(AiCompletion::Empty);
    }
    let repo = Arc::clone(&state.repo);
    let (cfg2, conn_id) = (cfg.clone(), req.conn_id);
    let (allowed, kind) = blocking(move || {
        let conn = repo.get(conn_id)?;
        Ok((may_send(&repo, &cfg2, &conn)?, conn.kind))
    })
    .await?;
    if !allowed {
        return Ok(AiCompletion::Empty);
    }
    if let Some(message) = state.ai.paused() {
        return Ok(AiCompletion::Paused { message });
    }
    let language = match kind {
        DbKind::MySql | DbKind::Postgres => {
            PromptLanguage::Sql(Dialect::for_kind(kind.require_sql()?))
        }
        DbKind::Mongo => match state.sessions.language_for(kind) {
            Some(language) => PromptLanguage::Mongo(language),
            // A build without the MongoDB driver: nothing to redact with, nothing is sent.
            None => return Ok(AiCompletion::Empty),
        },
        // No Redis prompt yet.
        DbKind::Redis => return Ok(AiCompletion::Empty),
    };
    let prompt = match &language {
        PromptLanguage::Sql(dialect) => sql_prompt(state, &req, *dialect).await,
        PromptLanguage::Mongo(language) => mongo_prompt(state, &req, &**language, &mut token).await,
    };
    let Some(prompt) = prompt else {
        return Ok(AiCompletion::Empty);
    };

    let Some(client) = client(state, &cfg).await? else {
        return Ok(AiCompletion::Empty);
    };
    if token.is_cancelled() {
        return Ok(AiCompletion::Empty);
    }
    let mut fim = FimRequest::new(model, prompt.prompt.clone(), prompt.suffix.clone());
    fim.max_tokens = cfg.max_tokens;
    let task = tokio::spawn(async move { client.complete(&fim).await });
    state.ai.track(req.request_id.clone(), task.abort_handle());
    let joined = task.await;
    state.ai.untrack(&req.request_id);
    let result: Result<_, AiError> = match joined {
        Ok(r) => r,
        // Aborted by `ai_cancel` (or a newer request with the same id).
        Err(_) => return Ok(AiCompletion::Empty),
    };
    match result {
        Ok(reply) => {
            state.ai.note_success();
            let ctx = PostCtx {
                language: match &language {
                    PromptLanguage::Sql(dialect) => Language::Sql(*dialect),
                    PromptLanguage::Mongo(language) => Language::Command(&**language),
                },
                statement_head: &prompt.statement_head,
                prefix_tail: prompt.prefix_tail(),
                suffix: &prompt.suffix,
                max_lines: cfg.max_lines as usize,
            };
            Ok(match clean(&reply.text, &ctx) {
                Some(text) => AiCompletion::Suggestion { text },
                None => AiCompletion::Empty,
            })
        }
        Err(e) => {
            state.ai.note_error(&e);
            match state.ai.paused() {
                Some(message) => Ok(AiCompletion::Paused { message }),
                None => Err(to_db_error(e)),
            }
        }
    }
}

pub fn ai_cancel_impl(state: &AppState, request_id: &str) {
    state.ai.cancel(request_id);
}

#[tauri::command]
#[specta::specta]
pub async fn ai_settings_get(state: State<'_, AppState>) -> Result<AiSettings, AppError> {
    guard(ai_settings_get_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_settings_set(
    state: State<'_, AppState>,
    config: AiConfig,
) -> Result<AiSettings, AppError> {
    guard(ai_settings_set_impl(&state, config)).await
}

/// Stores the API key in the OS keychain. The only command that receives it.
#[tauri::command]
#[specta::specta]
pub async fn ai_key_set(state: State<'_, AppState>, key: String) -> Result<AiSettings, AppError> {
    guard(ai_key_set_impl(&state, key)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_key_clear(state: State<'_, AppState>) -> Result<AiSettings, AppError> {
    guard(ai_key_clear_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_models(state: State<'_, AppState>) -> Result<Vec<String>, AppError> {
    guard(ai_models_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_test(state: State<'_, AppState>) -> Result<AiTestResult, AppError> {
    guard(ai_test_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_consent_set(
    state: State<'_, AppState>,
    granted: bool,
) -> Result<AiSettings, AppError> {
    guard(ai_consent_set_impl(&state, granted)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_connections(
    state: State<'_, AppState>,
) -> Result<Vec<AiConnectionChoice>, AppError> {
    guard(ai_connections_impl(&state)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_connection_set(
    state: State<'_, AppState>,
    conn_id: Uuid,
    enabled: bool,
) -> Result<Vec<AiConnectionChoice>, AppError> {
    guard(ai_connection_set_impl(&state, conn_id, enabled)).await
}

#[tauri::command]
#[specta::specta]
pub async fn ai_complete(
    state: State<'_, AppState>,
    request: AiCompleteRequest,
) -> Result<AiCompletion, AppError> {
    guard(ai_complete_impl(&state, request)).await
}

#[tauri::command]
#[specta::specta]
pub fn ai_cancel(state: State<'_, AppState>, request_id: String) {
    ai_cancel_impl(&state, &request_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(schema: Option<&str>, name: &str) -> TableBrief {
        TableBrief {
            schema: schema.map(str::to_string),
            name: name.to_string(),
            columns: Vec::new(),
        }
    }

    #[test]
    fn current_schema_prefers_public_then_the_first_table() {
        assert_eq!(current_schema(&[]), None);
        assert_eq!(current_schema(&[t(None, "a")]), None);
        let pg = [t(Some("sales"), "a"), t(Some("public"), "b")];
        assert_eq!(current_schema(&pg).as_deref(), Some("public"));
        let pg = [t(Some("sales"), "a"), t(Some("audit"), "b")];
        assert_eq!(current_schema(&pg).as_deref(), Some("sales"));
    }
}
