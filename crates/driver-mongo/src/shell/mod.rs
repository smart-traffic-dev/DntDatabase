//! The mongosh-style console language (spec §4): lexer, literals, and (later tasks)
//! grammar, classification and redaction.

pub mod lexer;
pub mod literal;

pub use lexer::{lex, Token, TokenKind};
pub use literal::{parse_document, parse_literal};

/// A lex or parse error. `offset` is a byte offset into the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError {
    pub message: String,
    pub offset: usize,
}

impl ShellError {
    pub(crate) fn new(message: impl Into<String>, offset: usize) -> Self {
        Self {
            message: message.into(),
            offset,
        }
    }
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ShellError {}
