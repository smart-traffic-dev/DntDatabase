//! Errors of the AI endpoint client. Messages never carry the API key, the prompt or
//! the request URL's query (base URLs have none, see [`Endpoint::parse`]).
//!
//! [`Endpoint::parse`]: crate::Endpoint::parse

use std::time::Duration;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum AiError {
    /// The settings are unusable (base URL, model).
    #[error("{message}")]
    Config { message: String },
    /// 401 / 403.
    #[error("The API key was rejected ({status}).")]
    Unauthorized { status: u16 },
    /// 429, with `Retry-After` when it was given in seconds.
    #[error("Rate limited by the endpoint.")]
    RateLimited { retry_after: Option<Duration> },
    /// A 4xx whose message names the `suffix` parameter.
    #[error("This endpoint does not support fill-in-the-middle: {message}")]
    FimUnsupported { message: String },
    /// Any other 4xx.
    #[error("The endpoint rejected the request ({status}){}", reason(.message))]
    BadRequest { status: u16, message: String },
    /// 5xx.
    #[error("The endpoint failed ({status}).")]
    Server { status: u16 },
    /// 3xx: redirects are never followed, so the key never goes elsewhere.
    #[error("The endpoint redirected to {host}. Use that address as the base URL.")]
    Redirected { host: String },
    #[error("The request timed out.")]
    Timeout,
    /// Connect, IO or TLS failure.
    #[error("{message}")]
    Network { message: String },
    /// A 2xx whose body is not the OpenAI shape.
    #[error("Unexpected response: {message}")]
    InvalidResponse { message: String },
}

/// `": {message}"`, or `"."` when the endpoint gave no reason.
fn reason(message: &str) -> String {
    if message.trim().is_empty() {
        ".".to_string()
    } else {
        format!(": {message}")
    }
}
