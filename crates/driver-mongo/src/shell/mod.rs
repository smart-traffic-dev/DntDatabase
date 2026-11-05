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
use lexer::lex_strict;
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
    /// Whole lines (a byte range of the script) that do not lex: one command, however
    /// many lines it has.
    Raw(usize, usize),
    /// From an unterminated template literal or comment to the end of the script (a
    /// byte range): nothing in it may be read as a command of its own.
    Unclosed(usize, usize),
}

/// Lexes a script for splitting and redaction, which must work on any text, into
/// pieces in order. Lexing is strict ([`lex_strict`]): where JavaScript may read the
/// text another way, it stops as on a lex error. When the text stops lexing, the lines
/// from the bad one (backed off to the line where an unfinished string or comment
/// began) up to the end of the bad line are one [`Piece::Raw`], and lexing resumes on
/// the next line. An unterminated template literal or comment never ends, so from where
/// it begins the rest of the text is one [`Piece::Unclosed`]; so is the rest of the text
/// from a bad region whose line, from the error on, could begin such a construct.
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
        let (bad, message) = match lex_strict(rest, pos > 0) {
            Ok(t) => {
                cur.extend(shifted(t, pos));
                break;
            }
            Err(e) => (pos + e.offset, e.message),
        };
        if let Some(t) = use_name(text, pos, bad, &cur) {
            pos = t.last().map_or(text.len(), |t| t.end);
            cur.extend(t);
            continue;
        }
        // Lexing never resumes inside a template literal or a comment that does not end:
        // from where it begins, the rest of the text is one piece.
        if let Some(begin) = unclosed_begin(text, pos, bad, &message) {
            let (cut, prefix) = lexable_prefix(text, pos, begin);
            cur.extend(prefix);
            if !cur.is_empty() {
                pieces.push(Piece::Tokens(std::mem::take(&mut cur)));
            }
            pieces.push(Piece::Unclosed(cut, text.len()));
            break;
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
        // From the error on, this lexer no longer knows how JavaScript reads the line.
        // When the rest of it could open something that runs over lines (a template, a
        // block comment, a string continued with `\`), nothing after it may be read as a
        // command of its own either.
        if may_open_lines(text.get(bad..end).unwrap_or_default()) {
            pieces.push(Piece::Unclosed(cut, text.len()));
            break;
        }
        pieces.push(Piece::Raw(cut, end));
        pos = end;
    }
    if !cur.is_empty() {
        pieces.push(Piece::Tokens(cur));
    }
    pieces
}

/// Whether the rest of a line (`s`, its line break included) holds what could begin a
/// construct that runs over lines: a backtick, `/*`, or a `\` before the line break.
fn may_open_lines(s: &str) -> bool {
    s.contains('`') || s.contains("/*") || s.trim_end_matches(['\n', '\r']).ends_with('\\')
}

/// When the lex error `message` at `bad` (lexing from `pos`) is inside a construct that
/// spans lines and does not end, where that construct begins. Such constructs are:
///
/// - an unterminated template literal or comment;
/// - an unterminated string continued over a line (`\` at a line end);
/// - a `${` inside a template literal;
/// - a bad escape inside a template literal or inside a string continued over a line.
fn unclosed_begin(text: &str, pos: usize, bad: usize, message: &str) -> Option<usize> {
    const TEMPLATE: &str = "Unterminated template literal";
    const STRING: &str = "Unterminated string";
    // Lexing up to the error ends inside the construct holding it, at its first quote.
    let enclosing = || match lex_strict(text.get(pos..bad)?, pos > 0) {
        Err(e) if e.message == TEMPLATE => Some(pos + e.offset),
        Err(e) if e.message == STRING && continues_over_a_line(text, pos + e.offset) => {
            Some(pos + e.offset)
        }
        _ => None,
    };
    match message {
        TEMPLATE | "Unterminated comment" => Some(bad),
        STRING => continues_over_a_line(text, bad).then_some(bad),
        m if m.starts_with("Template literal substitutions") => enclosing(),
        "Invalid escape sequence" => enclosing(),
        _ => None,
    }
}

/// Whether the `'` / `"` string that starts at byte `quote` holds a line continuation
/// (`\` before a line break) before it ends or meets a plain line break.
fn continues_over_a_line(text: &str, quote: usize) -> bool {
    let b = text.as_bytes();
    let Some(&q) = b.get(quote) else {
        return false;
    };
    let mut k = quote + 1;
    while k < b.len() {
        match b[k] {
            b'\\' if matches!(b.get(k + 1), Some(b'\n' | b'\r')) => return true,
            b'\\' => k += 2,
            b'\n' | b'\r' => return false,
            c if c == q => return false,
            _ => k += 1,
        }
    }
    false
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

/// The start of the line holding byte `i` (lines end at LF, CR or CRLF, as the lexer
/// ends a string).
fn line_start(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    b[..i.min(b.len())]
        .iter()
        .rposition(|&c| matches!(c, b'\n' | b'\r'))
        .map_or(0, |p| p + 1)
}

/// Just past the end (its LF, CR or CRLF included) of the line holding byte `i`.
fn line_end(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    match b
        .get(i..)
        .and_then(|s| s.iter().position(|&c| matches!(c, b'\n' | b'\r')))
    {
        None => b.len(),
        Some(p) if b[i + p] == b'\r' && b.get(i + p + 1) == Some(&b'\n') => i + p + 2,
        Some(p) => i + p + 1,
    }
}

/// The tokens of `text[pos..cut]`, backing `cut` off to earlier line starts while it
/// does not lex (an unfinished string or comment began before it).
fn lexable_prefix(text: &str, pos: usize, mut cut: usize) -> (usize, Vec<Token>) {
    for _ in 0..MAX_LEX_RETRIES {
        let Some(prefix) = text.get(pos..cut) else {
            break;
        };
        match lex_strict(prefix, pos > 0) {
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

/// A character of a raw `use` name: `[A-Za-z0-9_$-]`, or a non-ASCII character that is
/// not whitespace (U+FEFF counts as whitespace, as in JavaScript).
fn raw_name_char(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '-')
    } else {
        !c.is_whitespace() && c != '\u{feff}'
    }
}

/// When the lex error at `bad` is inside the name of a `use` that starts a statement:
/// the tokens of `text[pos..]` up to and including that name. `cur`: the tokens before
/// `pos` in the same piece.
fn use_name(text: &str, pos: usize, bad: usize, cur: &[Token]) -> Option<Vec<Token>> {
    let b = text.as_bytes();
    if !text.get(bad..)?.chars().next().is_some_and(raw_name_char) {
        return None;
    }
    // The name around `bad`, in whole characters; it ends at whitespace, as the parser's
    // raw name does (U+2028 too, which also ends a `//` comment).
    let back: usize = text
        .get(pos..bad)?
        .chars()
        .rev()
        .take_while(|&c| raw_name_char(c))
        .map(char::len_utf8)
        .sum();
    let start = bad - back;
    let fwd: usize = text
        .get(bad..)?
        .chars()
        .take_while(|&c| raw_name_char(c))
        .map(char::len_utf8)
        .sum();
    let end = bad + fwd;
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
    let mut tokens = shifted(lex_strict(text.get(pos..kw)?, pos > 0).ok()?, pos);
    // `use` must be a token of its own: the text before it can lex because it is cut
    // inside a `//` comment that holds the `use`.
    let with_kw = lex_strict(text.get(pos..kw_end)?, pos > 0).ok()?;
    if with_kw.len() != tokens.len() + 1 || with_kw.last().is_none_or(|t| pos + t.start != kw) {
        return None;
    }
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
        Some(t) => text
            .get(t.end..kw)
            .is_some_and(|gap| gap.contains(['\n', '\r'])),
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
                Piece::Unclosed(a, b) => format!("<{}>", &text[a..b]),
            })
            .collect()
    }

    #[test]
    fn lex_script_stops_at_an_unclosed_template_or_comment() {
        assert_eq!(
            pieces("db.e.find({\n  b: `bad\n}); db.c.drop()"),
            ["db . e . find ( { b :", "<`bad\n}); db.c.drop()>"]
        );
        assert_eq!(pieces("a()\n/* note\nb()"), ["a ( )", "</* note\nb()>"]);
        // `${` and a bad escape inside a template: from the template's backtick.
        assert_eq!(pieces("f(`${x}\n`)\ng()"), ["f (", "<`${x}\n`)\ng()>"]);
        assert_eq!(pieces("f(`\\1\n`)\ng()"), ["f (", "<`\\1\n`)\ng()>"]);
        // A bad escape in a string is one bad line.
        assert_eq!(pieces("f('\\1')\ng()"), ["[f('\\1')\n]", "g ( )"]);
        // From an error on, a backtick, `/*` or a `\` at the line end may begin something
        // JavaScript reads over lines: the rest is one piece.
        assert_eq!(pieces("a\nb 1_0 /*\nc()\n*/"), ["a", "<b 1_0 /*\nc()\n*/>"]);
        assert_eq!(pieces("f('\\1' + `\nc()\n`)"), ["<f('\\1' + `\nc()\n`)>"]);
        assert_eq!(pieces("a 1_0 'x \\\nc()'"), ["<a 1_0 'x \\\nc()'>"]);
        // A backtick before the error is not one.
        assert_eq!(pieces("`t` 'bad\nc()"), ["[`t` 'bad\n]", "c ( )"]);
    }

    #[test]
    fn lex_script_is_strict() {
        // Where JavaScript may read the text another way, the line is a bad one.
        assert_eq!(
            pieces("f()\nif (a) /x/\ng()"),
            ["f ( )", "[if (a) /x/\n]", "g ( )"]
        );
        // Lexing resumed after a bad line does not know what came before a `/`.
        assert_eq!(
            pieces("f(1_0)\n/x/\ng()"),
            ["[f(1_0)\n]", "[/x/\n]", "g ( )"]
        );
        // A bad region over lines is kept whole.
        assert_eq!(
            pieces("/* off:\ndb.c.drop()\n*/ x = 'oops"),
            ["[/* off:\ndb.c.drop()\n*/ x = 'oops]"]
        );
        // A lone CR starts a statement for `use` too.
        assert_eq!(pieces("a()\ruse 2024_x"), ["a ( ) use 2024_x"]);
        // A `use` name stops at whitespace (U+2028 here, which ends the `//` comment that
        // holds the `use`): nothing after the comment is taken for its name.
        assert_eq!(
            pieces("// a ;use \u{2028}\u{203f}\n"),
            ["[// a ;use \u{2028}\u{203f}\n]"]
        );
        let text = "x; use 1é\u{a0}y";
        let names: Vec<&str> = match lex_script(text).as_slice() {
            [Piece::Tokens(t)] => t.iter().map(|t| &text[t.start..t.end]).collect(),
            other => panic!("{other:?}"),
        };
        assert_eq!(names, ["x", ";", "use", "1é", "y"]);
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
