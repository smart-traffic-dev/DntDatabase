//! AI completion in the query editor (spec `2026-11-04-ai-editor-completion-design.md`).
//!
//! - [`prompt::build`] turns the editor document, cursor and schema cache into a
//!   fill-in-the-middle prompt: secrets redacted, text cut, a schema header of SQL
//!   comments. It refuses when the cursor is in a secret.
//! - [`mongo::mongo_draft`] / [`mongo::MongoDraft::build`] do the same for the MongoDB
//!   console, with the console language's redaction (any secret refuses) and a header of
//!   `//` comments naming the database's collections and their sampled fields.
//! - [`HttpFimClient`] sends it to an OpenAI-compatible endpoint (`/completions` with
//!   `suffix`, and `/models`); nothing else is spoken. Cancelling a request is dropping
//!   its future.
//! - [`post::clean`] turns the reply into the suggestion; [`post::judge_probe`] judges
//!   the Settings → Test probe.
//!
//! Prompt, suffix and reply text are never logged; `debug` gets lengths and timings.
//! The API key is only sent to the configured host (no redirects) and never appears in
//! `Debug` output or errors.

mod endpoint;
mod error;
pub mod mongo;
pub mod post;
pub mod prompt;
pub mod provider;
#[cfg(test)]
mod test_lang;

pub use endpoint::{Endpoint, DEFAULT_TIMEOUT};
pub use error::AiError;
pub use mongo::{mongo_draft, CollectionBrief, MongoDraft};
pub use post::{clean, judge_probe, Language, PostCtx, ProbeVerdict, PROBE_PROMPT, PROBE_SUFFIX};
pub use prompt::{build, FimPrompt, PromptInput, Skip, TableBrief};
pub use provider::{FimClient, FimReply, FimRequest, HttpFimClient};
