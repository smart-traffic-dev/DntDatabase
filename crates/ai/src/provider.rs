//! OpenAI-compatible fill-in-the-middle client: `POST {base}/completions` with
//! `suffix`, and `GET {base}/models`.
//!
//! - TLS is rustls on the ring provider with the OS certificate store (no aws-lc-rs,
//!   no native-tls), handed to reqwest as a preconfigured backend.
//! - Redirects are never followed, so the key is only ever sent to the configured host.
//! - Cancelling a request is dropping its future: reqwest closes the connection.
//! - Nothing here logs prompt, suffix or reply text; `debug` gets lengths and timings.

use crate::{AiError, Endpoint};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, LOCATION, RETRY_AFTER};
use reqwest::{redirect, RequestBuilder, StatusCode};
use rustls_platform_verifier::BuilderVerifierExt;
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_TOKENS: u32 = 128;
pub const DEFAULT_TEMPERATURE: f32 = 0.2;
/// Upper bound for connecting; the endpoint's whole-request timeout still applies.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Characters of a server error message kept in [`AiError`].
const MESSAGE_MAX: usize = 300;

/// One fill-in-the-middle request.
#[derive(Debug, Clone, PartialEq)]
pub struct FimRequest {
    pub model: String,
    pub prompt: String,
    pub suffix: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Sent as is; OpenAI accepts at most 4.
    pub stop: Vec<String>,
}

impl FimRequest {
    /// A request with the spec's defaults: 128 tokens, temperature 0.2, stop at a blank line.
    pub fn new(model: impl Into<String>, prompt: String, suffix: String) -> Self {
        FimRequest {
            model: model.into(),
            prompt,
            suffix,
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
            stop: vec!["\n\n".to_string()],
        }
    }
}

/// The model's raw text (before [`crate::post::clean`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FimReply {
    pub text: String,
    pub finish_reason: Option<String>,
    pub elapsed: Duration,
}

#[async_trait]
pub trait FimClient: Send + Sync {
    async fn complete(&self, req: &FimRequest) -> Result<FimReply, AiError>;
    /// Model ids of `GET {base}/models`, sorted and de-duplicated.
    async fn list_models(&self) -> Result<Vec<String>, AiError>;
}

/// [`FimClient`] over HTTP. Cheap to clone (reqwest pools connections).
#[derive(Clone)]
pub struct HttpFimClient {
    client: reqwest::Client,
    endpoint: Endpoint,
}

impl std::fmt::Debug for HttpFimClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpFimClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl HttpFimClient {
    pub fn new(endpoint: Endpoint) -> Result<Self, AiError> {
        let tls_error = |e: rustls::Error| AiError::Config {
            message: format!("Could not set up TLS: {e}"),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .with_platform_verifier()
        .map_err(tls_error)?
        .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT.min(endpoint.timeout))
            .timeout(endpoint.timeout)
            .user_agent(concat!("DntDatabase/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AiError::Config {
                message: format!("Could not create the HTTP client: {}", e.without_url()),
            })?;
        Ok(HttpFimClient { client, endpoint })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Adds `Authorization: Bearer …` (marked sensitive) when a key is set.
    fn authorize(&self, rb: RequestBuilder) -> Result<RequestBuilder, AiError> {
        let Some(key) = &self.endpoint.api_key else {
            return Ok(rb);
        };
        let mut value = HeaderValue::from_str(&format!("Bearer {}", key.expose_secret().trim()))
            .map_err(|_| AiError::Config {
                message: "The API key contains characters that cannot be sent.".to_string(),
            })?;
        value.set_sensitive(true);
        Ok(rb.header(AUTHORIZATION, value))
    }

    /// Sends `rb`; a non-2xx status becomes its [`AiError`].
    async fn send(&self, rb: RequestBuilder) -> Result<(StatusCode, Value), AiError> {
        let resp = self.authorize(rb)?.send().await.map_err(transport_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(status_error(status, &headers, &body, &self.endpoint));
        }
        let value = serde_json::from_slice(&body).map_err(|_| AiError::InvalidResponse {
            message: "the body is not JSON".to_string(),
        })?;
        Ok((status, value))
    }
}

#[async_trait]
impl FimClient for HttpFimClient {
    async fn complete(&self, req: &FimRequest) -> Result<FimReply, AiError> {
        let started = Instant::now();
        let body = json!({
            "model": req.model,
            "prompt": req.prompt,
            "suffix": req.suffix,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stop": req.stop,
            "stream": false,
        });
        let rb = self
            .client
            .post(self.endpoint.url("completions"))
            .json(&body);
        let result = self.send(rb).await.and_then(|(_, v)| parse_completion(&v));
        let elapsed = started.elapsed();
        tracing::debug!(
            model = %req.model,
            ok = result.is_ok(),
            elapsed_ms = elapsed.as_millis() as u64,
            prompt_len = req.prompt.len(),
            suffix_len = req.suffix.len(),
            reply_len = result.as_ref().map(|r| r.0.len()).unwrap_or(0),
            "ai completion"
        );
        let (text, finish_reason) = result?;
        Ok(FimReply {
            text,
            finish_reason,
            elapsed,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let rb = self.client.get(self.endpoint.url("models"));
        let (_, v) = match self.send(rb).await {
            // DeepSeek serves FIM at `/beta/completions` but its models at `/models`.
            Err(AiError::BadRequest { status: 404, .. })
                if self.endpoint.parent_url("models").is_some() =>
            {
                let url = self.endpoint.parent_url("models").expect("checked above");
                self.send(self.client.get(url)).await?
            }
            other => other?,
        };
        let data =
            v.get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| AiError::InvalidResponse {
                    message: "no `data` list of models".to_string(),
                })?;
        let mut ids: Vec<String> = data
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
}

/// `choices[0].text` and `choices[0].finish_reason` of an OpenAI `/completions` body.
fn parse_completion(v: &Value) -> Result<(String, Option<String>), AiError> {
    let invalid = |message: &str| AiError::InvalidResponse {
        message: message.to_string(),
    };
    let choice = v
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("no `choices`"))?
        .first()
        .ok_or_else(|| invalid("`choices` is empty"))?;
    let text = choice
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("`choices[0].text` is not a string"))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((text.to_string(), finish_reason))
}

/// A reqwest failure before a status was read: timeout, or connect / IO / TLS.
fn transport_error(e: reqwest::Error) -> AiError {
    if e.is_timeout() {
        return AiError::Timeout;
    }
    let e = e.without_url();
    let mut message = format!("Could not reach the endpoint: {e}");
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        message.push_str(": ");
        message.push_str(&s.to_string());
        source = s.source();
    }
    AiError::Network {
        message: cut_message(&message),
    }
}

/// The [`AiError`] of a non-2xx response.
fn status_error(status: StatusCode, headers: &HeaderMap, body: &[u8], ep: &Endpoint) -> AiError {
    let code = status.as_u16();
    if status.is_redirection() {
        let host = headers
            .get(LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| ep.url("").join(l).ok())
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        return AiError::Redirected { host };
    }
    match code {
        401 | 403 => AiError::Unauthorized { status: code },
        429 => AiError::RateLimited {
            retry_after: headers
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs),
        },
        400..=499 => {
            let message = error_message(body);
            if message.to_ascii_lowercase().contains("suffix") {
                AiError::FimUnsupported { message }
            } else {
                AiError::BadRequest {
                    status: code,
                    message,
                }
            }
        }
        _ => AiError::Server { status: code },
    }
}

/// `error.message` (or `error` / `message` as a string) of an OpenAI-style error
/// body, else the body text; one line, at most [`MESSAGE_MAX`] characters.
fn error_message(body: &[u8]) -> String {
    let from_json = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
        let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        s(v.pointer("/error/message"))
            .or_else(|| s(v.get("error")))
            .or_else(|| s(v.get("message")))
    });
    let text = from_json.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    cut_message(&text)
}

/// Control characters become spaces, then the text is trimmed and cut to
/// [`MESSAGE_MAX`] characters.
fn cut_message(text: &str) -> String {
    let one_line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = one_line.trim();
    if trimmed.chars().count() <= MESSAGE_MAX {
        return trimmed.to_string();
    }
    let mut cut: String = trimmed.chars().take(MESSAGE_MAX - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_message_is_one_line_and_bounded() {
        let long = "é\n".repeat(1000);
        let out = cut_message(&long);
        assert!(out.chars().count() <= MESSAGE_MAX);
        assert!(!out.contains('\n'));
        assert!(out.ends_with('…'));
    }

    #[test]
    fn error_message_prefers_openai_shape() {
        assert_eq!(error_message(br#"{"error":{"message":"bad"}}"#), "bad");
        assert_eq!(error_message(br#"{"error":"flat"}"#), "flat");
        assert_eq!(error_message(br#"{"message":"top"}"#), "top");
        assert_eq!(error_message(b"plain text"), "plain text");
    }
}
