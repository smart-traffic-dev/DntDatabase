//! The mongosh-style console language (spec §4): lexer, literals, grammar,
//! classification and redaction, behind [`MongoLanguage`].

pub mod classify;
pub mod lexer;
pub mod literal;
pub mod parse;
pub mod redact;

use std::borrow::Cow;
use std::sync::Arc;

use dnt_core::command::{CommandLanguage, CommandParseError, CommandSpan, ParsedCommand};
use dnt_core::redact::Redacted;

pub use classify::{has_out_or_merge, OutKind};
pub use lexer::{lex, Token, TokenKind};
pub use literal::{parse_document, parse_literal};
pub use parse::{CollMethod, DbMethod, Modifier, ShellCommand, ShowWhat, Verbosity, JS_REFUSED};

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

/// The console language: split (§4.1), parse and classify (§4.2–§4.4), redact (§4.5).
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoLanguage;

/// The driver's form of a parsed command (`ParsedCommand::payload`).
// Read by the session's executor (`exec.rs`, a later task).
#[allow(dead_code)]
pub(crate) struct Payload {
    pub cmd: ShellCommand,
}

impl CommandLanguage for MongoLanguage {
    fn split(&self, text: &str) -> Vec<CommandSpan> {
        parse::split(text)
    }

    fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError> {
        let (cmd, at) = parse::parse_command(text).map_err(|e| CommandParseError {
            message: e.message,
            offset: Some(e.offset),
        })?;
        let c = classify::classify(&cmd).map_err(|message| CommandParseError {
            message,
            offset: Some(at),
        })?;
        Ok(ParsedCommand {
            name: c.name,
            text: text.to_string(),
            class: c.class,
            targets: c.targets,
            unbounded: c.unbounded,
            drops: c.drops,
            session_effect: c.session_effect,
            payload: Arc::new(Payload { cmd }),
        })
    }

    fn redact(&self, text: &str) -> Redacted {
        redact::redact(text)
    }
}

/// How many times [`lex_script`] backs off to an earlier line before giving up.
const MAX_LEX_RETRIES: usize = 16;

/// Lexes a script for splitting and redaction, which must work on any text: the tokens
/// of the longest prefix that lexes, cut at a line start, and where that prefix ends
/// (`text.len()` when all of it lexes). The word after a `use` that starts a line is
/// lexed as a plain name whatever it holds (`use 2024_logs`), so it never stops the rest
/// of the script from lexing. Offsets are bytes of `text`.
pub(crate) fn lex_script(text: &str) -> (Vec<Token>, usize) {
    let masked = mask_use_words(text);
    let mut cut = masked.len();
    for _ in 0..MAX_LEX_RETRIES {
        let Some(prefix) = masked.get(..cut) else {
            break;
        };
        match lex(prefix) {
            Ok(tokens) => return (tokens, cut),
            Err(e) => {
                // Back off to the start of the line the error is on.
                let next = prefix
                    .get(..e.offset.min(cut))
                    .and_then(|s| s.rfind('\n'))
                    .map_or(0, |i| i + 1);
                if next >= cut {
                    break;
                }
                cut = next;
            }
        }
    }
    (Vec::new(), 0)
}

/// `text` with the word after each line-leading `use` replaced by `x`s of the same byte
/// length (a quoted name is left to the lexer).
fn mask_use_words(text: &str) -> Cow<'_, str> {
    let mut bytes: Option<Vec<u8>> = None;
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        if let Some(after) = line[indent..].strip_prefix("use") {
            let gap = after.len() - after.trim_start_matches([' ', '\t']).len();
            let word = &after[gap..];
            if gap > 0 && !word.starts_with(['\'', '"', '`']) {
                let len = word
                    .find(|c: char| c.is_whitespace() || c == ';')
                    .unwrap_or(word.len());
                let from = line_start + indent + "use".len() + gap;
                if let Some(b) = bytes
                    .get_or_insert_with(|| text.as_bytes().to_vec())
                    .get_mut(from..from + len)
                {
                    b.fill(b'x');
                }
            }
        }
        line_start += line.len();
    }
    match bytes {
        None => Cow::Borrowed(text),
        // Whole characters were replaced by ASCII, so this cannot fail.
        Some(b) => String::from_utf8(b).map_or(Cow::Borrowed(text), Cow::Owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_script_backs_off_to_the_bad_line() {
        let text = "db.a.find()\nuse 2024_logs\ndb.b.find({x: 'open)\ndb.c.find()";
        let (tokens, cut) = lex_script(text);
        assert_eq!(cut, text.find("db.b").unwrap());
        // `use 2024_logs` lexed as two names.
        let words: Vec<&str> = tokens.iter().map(|t| &text[t.start..t.end]).collect();
        assert_eq!(
            words,
            ["db", ".", "a", ".", "find", "(", ")", "use", "2024_logs"]
        );
        assert_eq!(lex_script("'").1, 0);
        assert_eq!(lex_script("").1, 0);
    }
}
