//! AI editor completion (spec `2026-11-04-ai-editor-completion-design.md`).

mod endpoint;
mod error;
pub mod provider;

pub use endpoint::{Endpoint, DEFAULT_TIMEOUT};
pub use error::AiError;
pub use provider::{FimClient, FimReply, FimRequest, HttpFimClient};
