//! The mongosh-style console language (spec §4): lexer, literals, grammar,
//! classification and redaction, behind [`MongoLanguage`].

pub mod classify;
pub mod lexer;
pub mod literal;
pub mod parse;
pub mod redact;

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

/// How many times [`lex_script`] backs off to an earlier line for one bad region.
const MAX_LEX_RETRIES: usize = 16;

/// A piece of a script as [`lex_script`] reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Piece {
    /// The tokens of text that lexes; offsets are bytes of the whole script.
    Tokens(Vec<Token>),
    /// Whole lines (a byte range of the script) that do not lex.
    Raw(usize, usize),
}

/// Lexes a script for splitting and redaction, which must work on any text, into
/// pieces in order. When the text stops lexing, the lines from the bad one (backed off
/// to the line where an unfinished string or comment began) up to the end of the bad
/// line are one [`Piece::Raw`], and lexing resumes on the next line.
///
/// The name after a `use` that starts a statement (bracket depth 0, at a line start or
/// after `;`) is one name token whatever raw-name characters it holds (`use 2024_logs`,
/// which the JavaScript lexer refuses), so it does not stop the script from lexing.
pub(crate) fn lex_script(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut cur: Vec<Token> = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let Some(rest) = text.get(pos..) else {
            break;
        };
        let bad = match lex(rest) {
            Ok(t) => {
                cur.extend(shifted(t, pos));
                break;
            }
            Err(e) => pos + e.offset,
        };
        if let Some(t) = use_name(text, pos, bad, &cur) {
            pos = t.last().map_or(text.len(), |t| t.end);
            cur.extend(t);
            continue;
        }
        let (cut, prefix) = lexable_prefix(text, pos, line_start(text, bad).max(pos));
        cur.extend(prefix);
        if !cur.is_empty() {
            pieces.push(Piece::Tokens(std::mem::take(&mut cur)));
        }
        let mut end = line_end(text, bad);
        if end <= pos {
            end = text.len();
        }
        pieces.push(Piece::Raw(cut, end));
        pos = end;
    }
    if !cur.is_empty() {
        pieces.push(Piece::Tokens(cur));
    }
    pieces
}

fn shifted(tokens: Vec<Token>, by: usize) -> Vec<Token> {
    tokens
        .into_iter()
        .map(|t| Token {
            start: t.start + by,
            end: t.end + by,
            ..t
        })
        .collect()
}

/// The start of the line holding byte `i`.
fn line_start(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    b[..i.min(b.len())]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |p| p + 1)
}

/// Just past the end (`\n` included) of the line holding byte `i`.
fn line_end(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    b.get(i..)
        .and_then(|s| s.iter().position(|&c| c == b'\n'))
        .map_or(b.len(), |p| i + p + 1)
}

/// The tokens of `text[pos..cut]`, backing `cut` off to earlier line starts while it
/// does not lex (an unfinished string or comment began before it).
fn lexable_prefix(text: &str, pos: usize, mut cut: usize) -> (usize, Vec<Token>) {
    for _ in 0..MAX_LEX_RETRIES {
        let Some(prefix) = text.get(pos..cut) else {
            break;
        };
        match lex(prefix) {
            Ok(t) => return (cut, shifted(t, pos)),
            Err(e) => {
                let next = line_start(text, pos + e.offset).max(pos);
                if next >= cut {
                    break;
                }
                cut = next;
            }
        }
    }
    (pos, Vec::new())
}

/// A character of a raw `use` name: `[A-Za-z0-9_$-]` or any non-ASCII byte.
fn raw_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'-') || c >= 0x80
}

/// When the lex error at `bad` is inside the name of a `use` that starts a statement:
/// the tokens of `text[pos..]` up to and including that name. `cur`: the tokens before
/// `pos` in the same piece.
fn use_name(text: &str, pos: usize, bad: usize, cur: &[Token]) -> Option<Vec<Token>> {
    let b = text.as_bytes();
    if !b.get(bad).is_some_and(|&c| raw_name_byte(c)) {
        return None;
    }
    // The name around `bad` (whole characters: every byte of a non-ASCII one counts).
    let mut start = bad;
    while start > pos && raw_name_byte(b[start - 1]) {
        start -= 1;
    }
    let mut end = bad;
    while end < b.len() && raw_name_byte(b[end]) {
        end += 1;
    }
    // `use`, then at least one blank.
    let mut kw_end = start;
    while kw_end > pos && matches!(b[kw_end - 1], b' ' | b'\t') {
        kw_end -= 1;
    }
    if kw_end == start || kw_end < pos + 3 || &b[kw_end - 3..kw_end] != b"use" {
        return None;
    }
    let kw = kw_end - 3;
    if kw > 0
        && (b[kw - 1].is_ascii_alphanumeric()
            || matches!(b[kw - 1], b'_' | b'$')
            || b[kw - 1] >= 0x80)
    {
        return None;
    }
    let mut tokens = shifted(lex(text.get(pos..kw)?).ok()?, pos);
    // A statement start: depth 0 (as `split` counts it), after `;` or a line break.
    let depth = cur.iter().chain(&tokens).fold(0usize, |d, t| match t.kind {
        TokenKind::Punct('(' | '[' | '{') => d + 1,
        TokenKind::Punct(')' | ']' | '}') => d.saturating_sub(1),
        TokenKind::Punct(';') => 0,
        _ => d,
    });
    let starts = match tokens.last().or(cur.last()) {
        None => true,
        Some(t) if t.kind == TokenKind::Punct(';') => true,
        Some(t) => text.get(t.end..kw).is_some_and(|gap| gap.contains('\n')),
    };
    if depth != 0 || !starts {
        return None;
    }
    tokens.push(Token {
        kind: TokenKind::Ident("use".into()),
        start: kw,
        end: kw_end,
    });
    tokens.push(Token {
        kind: TokenKind::Ident(text.get(start..end)?.to_string()),
        start,
        end,
    });
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces as text: tokens joined by spaces, raw lines in brackets.
    fn pieces(text: &str) -> Vec<String> {
        lex_script(text)
            .into_iter()
            .map(|p| match p {
                Piece::Tokens(t) => t
                    .iter()
                    .map(|t| &text[t.start..t.end])
                    .collect::<Vec<_>>()
                    .join(" "),
                Piece::Raw(a, b) => format!("[{}]", &text[a..b]),
            })
            .collect()
    }

    #[test]
    fn lex_script_resumes_after_the_bad_line() {
        assert_eq!(
            pieces("db.a.find()\nuse 2024_logs\ndb.b.find({x: 'open)\ndb.c.find()"),
            [
                "db . a . find ( ) use 2024_logs",
                "[db.b.find({x: 'open)\n]",
                "db . c . find ( )",
            ]
        );
        // Back off to where an unfinished template began.
        assert_eq!(
            pieces("a\nb(`x\ny` 'bad)\nc"),
            ["a", "[b(`x\ny` 'bad)\n]", "c"]
        );
        // `use` after `;` starts a statement; inside brackets it does not.
        assert_eq!(pieces("x; use 01-a\ny"), ["x ; use 01-a y"]);
        assert_eq!(
            pieces("f({\nuse 01, a: 1})\ng"),
            ["f ( {", "[use 01, a: 1})\n]", "g"]
        );
        assert_eq!(pieces("'"), ["[']"]);
        assert!(lex_script("").is_empty());
    }
}
