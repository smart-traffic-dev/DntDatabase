//! AI editor completion (spec `2026-11-04-ai-editor-completion-design.md`).

mod endpoint;
mod error;

pub use endpoint::{Endpoint, DEFAULT_TIMEOUT};
pub use error::AiError;
