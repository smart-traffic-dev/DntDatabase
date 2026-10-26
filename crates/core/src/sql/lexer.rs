//! Shared SQL lexer: the per-dialect rules for comments, strings, quoted
//! identifiers and words that `split`, `classify` and `strip_comments` (and the
//! drivers) all follow.

use serde::{Deserialize, Serialize};

use crate::server::DbKind;

/// The MySQL `sql_mode` flags that change how a script is lexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, specta::Type)]
pub struct MySqlMode {
    /// `ANSI_QUOTES`: `"…"` is a quoted identifier, with no backslash escape.
    #[serde(default)]
    pub ansi_quotes: bool,
    /// `NO_BACKSLASH_ESCAPES`: `'…'` and `"…"` strings have no backslash escape.
    #[serde(default)]
    pub no_backslash_escapes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "engine")]
pub enum Dialect {
    #[serde(rename = "mysql")]
    MySql(MySqlMode),
    #[serde(rename = "postgres")]
    Postgres,
}

impl Dialect {
    /// MySQL / MariaDB with the default `sql_mode` lexing flags (both off).
    pub const MYSQL: Dialect = Dialect::MySql(MySqlMode {
        ansi_quotes: false,
        no_backslash_escapes: false,
    });

    pub fn is_mysql(self) -> bool {
        matches!(self, Dialect::MySql(_))
    }

    /// The default dialect of a server kind (MySQL flags start off; the session
    /// refines them from `@@sql_mode`).
    pub fn for_kind(kind: DbKind) -> Dialect {
        match kind {
            DbKind::MySql => Dialect::MYSQL,
            DbKind::Postgres => Dialect::Postgres,
        }
    }
}

/// Whether `b` may continue an identifier: ASCII alphanumeric, `_`, `$`, or any
/// byte of a non-ASCII UTF-8 sequence (both MySQL and PG accept those in names).
pub fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// What [`skip_at`] stepped over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Skipped {
    Comment,
    /// MySQL/MariaDB executable comment `/*!NNNNN … */` or `/*M!NNNNNN … */`:
    /// the server runs its body, so it is never blank.
    ExecComment,
    /// String literal, quoted identifier or dollar-quoted body.
    Quoted,
}

/// Shared lexer step: if a comment, string, quoted identifier, E-string or
/// dollar-quoted body starts at byte `i`, returns its kind and the offset just
/// past it (the input length when unterminated). Line comments end before
/// their newline. Every offset returned is a char boundary.
pub(super) fn skip_at(b: &[u8], i: usize, dialect: Dialect) -> Option<(Skipped, usize)> {
    let mode = match dialect {
        Dialect::MySql(m) => Some(m),
        Dialect::Postgres => None,
    };
    let mysql = mode.is_some();
    // MySQL strings honour backslash escapes unless NO_BACKSLASH_ESCAPES; PG plain
    // strings never do (E-strings are handled below).
    let backslash = mode.is_some_and(|m| !m.no_backslash_escapes);
    let next = b.get(i + 1).copied();
    match b[i] {
        b'-' if next == Some(b'-') => {
            // MySQL needs whitespace (or end of input) after `--`.
            if mysql && !matches!(b.get(i + 2), None | Some(&(0..=b' '))) {
                return None;
            }
            Some((Skipped::Comment, line_end(b, i, !mysql)))
        }
        b'#' if mysql => Some((Skipped::Comment, line_end(b, i, false))),
        b'/' if next == Some(b'*') => {
            let rest = &b[i + 2..];
            let kind = if mysql && (rest.starts_with(b"!") || rest.starts_with(b"M!")) {
                Skipped::ExecComment
            } else {
                Skipped::Comment
            };
            Some((kind, block_comment_end(b, i, !mysql)))
        }
        b'\'' => Some((Skipped::Quoted, quote_end(b, i, b'\'', backslash))),
        // Under ANSI_QUOTES `"…"` is an identifier: no backslash escape.
        b'"' => {
            let esc = backslash && !mode.is_some_and(|m| m.ansi_quotes);
            Some((Skipped::Quoted, quote_end(b, i, b'"', esc)))
        }
        b'`' if mysql => Some((Skipped::Quoted, quote_end(b, i, b'`', false))),
        b'E' | b'e' if !mysql && next == Some(b'\'') && !prev_is_ident(b, i) => {
            Some((Skipped::Quoted, quote_end(b, i + 1, b'\'', true)))
        }
        b'$' if !mysql && !prev_is_ident(b, i) => {
            let body = dollar_tag_end(b, i)?;
            let tag = &b[i..body];
            let end = b[body..]
                .windows(tag.len())
                .position(|w| w == tag)
                .map_or(b.len(), |p| body + p + tag.len());
            Some((Skipped::Quoted, end))
        }
        _ => None,
    }
}

/// Where a line comment starting at `i` ends: before its newline. PostgreSQL's scanner
/// takes `\r` as a newline too (`newline [\n\r]` in scan.l); MySQL / MariaDB end `--`
/// and `#` comments only at `\n` (sql_lex.cc `MY_LEX_COMMENT`).
fn line_end(b: &[u8], i: usize, cr_ends: bool) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n' || (cr_ends && c == b'\r'))
        .map_or(b.len(), |p| i + p)
}

fn block_comment_end(b: &[u8], i: usize, nested: bool) -> usize {
    let mut depth = 1;
    let mut j = i + 2;
    while j < b.len() {
        if b[j..].starts_with(b"*/") {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return j;
            }
        } else if nested && b[j..].starts_with(b"/*") {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    b.len()
}

/// `open` is the index of the opening quote `q`. Doubled quotes always escape;
/// backslash escapes only when `backslash` is set.
fn quote_end(b: &[u8], open: usize, q: u8, backslash: bool) -> usize {
    let mut j = open + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
        } else if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

/// For `$tag$` / `$$` starting at `i`, returns the offset just past the opening tag.
fn dollar_tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j)
        .is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
    {
        while b.get(j).is_some_and(|&c| is_ident_byte(c) && c != b'$') {
            j += 1;
        }
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn prev_is_ident(b: &[u8], i: usize) -> bool {
    i > 0 && is_ident_byte(b[i - 1])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind<'a> {
    Word(&'a str),
    Open,
    Close,
    Quoted,
    ExecComment,
    Punct(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokKind<'a>,
    pub start: usize,
    pub end: usize,
}

/// Significant tokens of `s` (whitespace and comments dropped), built on the
/// splitter's lexer so strings and comments never affect paren depth. A word is
/// a run of [`is_ident_byte`] or `@` bytes.
pub fn tokenize(s: &str, dialect: Dialect) -> Vec<Token<'_>> {
    let b = s.as_bytes();
    let is_word = |c: u8| is_ident_byte(c) || c == b'@';
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let kind = if let Some((kind, end)) = skip_at(b, i, dialect) {
            i = end;
            match kind {
                Skipped::Comment => continue,
                Skipped::ExecComment => TokKind::ExecComment,
                Skipped::Quoted => TokKind::Quoted,
            }
        } else if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        } else if is_word(b[i]) {
            // A word also ends wherever `split` would start a literal, e.g. the
            // E-string or dollar quote in PG `<@e'…'` / `<@$$…$$`.
            i += 1;
            while i < b.len() && is_word(b[i]) && skip_at(b, i, dialect).is_none() {
                i += 1;
            }
            TokKind::Word(&s[start..i])
        } else {
            // Only ASCII reaches here (non-ASCII bytes are word bytes).
            i += 1;
            match b[start] {
                b'(' => TokKind::Open,
                b')' => TokKind::Close,
                c => TokKind::Punct(c),
            }
        };
        out.push(Token {
            kind,
            start,
            end: i,
        });
    }
    out
}

/// The ways the server may read `stmt`. Under MySQL, when it holds executable
/// comments (`/*!NNNNN … */`, `/*M!NNNNNN … */`), whether the server runs each
/// body depends on its version, so there are two readings: `[inlined, ignored]`
/// (bodies spliced in / comments dropped). Otherwise the only reading is `stmt`.
pub fn readings(stmt: &str, dialect: Dialect) -> Vec<String> {
    let toks = tokenize(stmt, dialect);
    if !toks.iter().any(|t| t.kind == TokKind::ExecComment) {
        return vec![stmt.to_string()];
    }
    let (mut inlined, mut ignored) = (String::new(), String::new());
    let mut from = 0;
    for t in toks.iter().filter(|t| t.kind == TokKind::ExecComment) {
        let before = &stmt[from..t.start];
        inlined.extend([before, " ", exec_comment_body(&stmt[t.start..t.end]), " "]);
        ignored.extend([before, " "]);
        from = t.end;
    }
    inlined.push_str(&stmt[from..]);
    ignored.push_str(&stmt[from..]);
    vec![inlined, ignored]
}

/// PostgreSQL: how a server with `standard_conforming_strings = off` reads `stmt`,
/// when that differs from the standard reading: every plain `'…'` string (not a
/// `B'…'` / `X'…'` bit string) honours backslash escapes there, so `'a\''` ends after
/// `a'`. Returned as the same text with those strings turned into `E'…'` (after a
/// space, so a preceding word stays apart), which [`tokenize`] reads the same way;
/// `None` when no such string holds a backslash (both readings agree).
///
/// The setting can be off on the server (`ALTER DATABASE … SET`, `postgresql.conf`)
/// or turned off by the session itself, so a guard judging `stmt` must accept both
/// readings.
pub fn pg_backslash_reading(stmt: &str) -> Option<String> {
    let b = stmt.as_bytes();
    let bit_string = |i: usize| {
        i >= 1 && matches!(b[i - 1], b'b' | b'B' | b'x' | b'X') && !prev_is_ident(b, i - 1)
    };
    let (mut out, mut from, mut i, mut differs) = (String::new(), 0, 0, false);
    while i < b.len() {
        if b[i] == b'\'' && !bit_string(i) {
            let end = quote_end(b, i, b'\'', true);
            differs |= b[i..end].contains(&b'\\');
            out.push_str(&stmt[from..i]);
            out.push_str(" E");
            from = i;
            i = end;
        } else if let Some((_, end)) = skip_at(b, i, Dialect::Postgres) {
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&stmt[from..]);
    differs.then_some(out)
}

/// The first `n` words of `stmt`, ASCII-uppercased. Comments, strings, quoted
/// identifiers, executable comments and punctuation are skipped (use
/// [`readings`] first to look inside executable comments).
pub fn leading_keywords(stmt: &str, dialect: Dialect, n: usize) -> Vec<String> {
    tokenize(stmt, dialect)
        .into_iter()
        .filter_map(|t| match t.kind {
            TokKind::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        })
        .take(n)
        .collect()
}

/// The word at token `i`, if that token is a word.
pub(super) fn word_at<'a>(toks: &[Token<'a>], i: usize) -> Option<&'a str> {
    match toks.get(i)?.kind {
        TokKind::Word(w) => Some(w),
        _ => None,
    }
}

/// Whether `word` is the keyword `kw` (ASCII case-insensitive).
pub(super) fn is_kw(word: Option<&str>, kw: &str) -> bool {
    word.is_some_and(|w| w.eq_ignore_ascii_case(kw))
}

/// Body of a `/*!NNNNN … */` / `/*M!NNNNNN … */` executable comment.
fn exec_comment_body(c: &str) -> &str {
    let body = c.strip_prefix("/*").unwrap_or(c);
    let body = body.strip_prefix('M').unwrap_or(body);
    let body = body.strip_prefix('!').unwrap_or(body);
    let body = body.trim_start_matches(|ch: char| ch.is_ascii_digit());
    body.strip_suffix("*/").unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_ident_byte_treats_non_ascii_as_identifier() {
        assert!((0x80..=0xFFu8).all(is_ident_byte));
        assert!(b"_$aZ0".iter().copied().all(is_ident_byte));
        assert!(!b" ;('.".iter().copied().any(is_ident_byte));
    }

    #[test]
    fn leading_keywords_skip_comments_and_keep_non_ascii_words() {
        assert_eq!(
            leading_keywords("/* x */ ưset SET a=1", Dialect::MYSQL, 2),
            ["ưSET", "SET"]
        );
        assert_eq!(
            leading_keywords("-- c\nset search_path = x", Dialect::Postgres, 2),
            ["SET", "SEARCH_PATH"]
        );
    }

    #[test]
    fn pg_backslash_reading_reads_plain_strings_with_escapes() {
        // No backslash in a plain string: both readings agree.
        assert_eq!(pg_backslash_reading("SELECT 'a', E'\\n', $$\\$$"), None);
        // `'x\''` ends after `x'` with the setting off: what follows is code.
        let r = pg_backslash_reading("SELECT 'x\\'' , 1 /* ' */").unwrap();
        assert_eq!(r, "SELECT  E'x\\'' , 1 /* ' */");
        let words: Vec<_> = tokenize(&r, Dialect::Postgres)
            .into_iter()
            .map(|t| t.kind)
            .collect();
        assert_eq!(
            words,
            [
                TokKind::Word("SELECT"),
                TokKind::Quoted,
                TokKind::Punct(b','),
                TokKind::Word("1")
            ]
        );
        // Bit strings, quoted names, comments and dollar bodies keep their reading;
        // a word right before a string stays apart.
        assert_eq!(pg_backslash_reading("SELECT B'1\\', X'f\\'"), None);
        assert_eq!(
            pg_backslash_reading("SELECT \"a\\\" -- '\\'\n, $q$'\\'$q$"),
            None
        );
        assert_eq!(
            pg_backslash_reading("SELECT n'\\'").as_deref(),
            Some("SELECT n E'\\'")
        );
    }

    #[test]
    fn readings_of_exec_comment() {
        let r: Vec<String> = readings("/*!40101 SET NAMES utf8 */", Dialect::MYSQL)
            .iter()
            .map(|s| s.trim().into())
            .collect();
        assert_eq!(r, ["SET NAMES utf8", ""]);
        assert_eq!(readings("/*! x */ select 1", Dialect::Postgres).len(), 1);
    }

    #[test]
    fn dialect_json_shape() {
        assert_eq!(
            serde_json::to_value(Dialect::MYSQL).unwrap(),
            json!({"engine":"mysql","ansi_quotes":false,"no_backslash_escapes":false})
        );
        assert_eq!(
            serde_json::to_value(Dialect::Postgres).unwrap(),
            json!({"engine":"postgres"})
        );
    }

    #[test]
    fn mysql_mode_flags_default_when_missing() {
        assert_eq!(
            serde_json::from_value::<Dialect>(json!({"engine":"mysql"})).unwrap(),
            Dialect::MYSQL
        );
        assert_eq!(
            serde_json::from_value::<Dialect>(json!({"engine":"mysql","ansi_quotes":true}))
                .unwrap(),
            Dialect::MySql(MySqlMode {
                ansi_quotes: true,
                no_backslash_escapes: false,
            })
        );
    }
}
