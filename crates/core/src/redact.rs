//! The redacted form of a statement or command, safe to keep in History, logs and
//! Favorites. Each engine family redacts its own language (`dnt_query::redact` for SQL,
//! [`crate::command::CommandLanguage::redact`] for commands).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    /// Whether anything was replaced: `false` means `text` is the input, byte for byte.
    pub changed: bool,
}
