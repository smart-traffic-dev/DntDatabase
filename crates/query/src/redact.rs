//! Password redaction for query history and logs (spec §8.1).
//!
//! Works over the shared `core` lexer, so strings, comments, E-strings and
//! dollar quotes follow the statement's dialect. The rules err on the side of
//! redacting: a literal is replaced whenever it may be a secret, e.g. the
//! right-hand side of `password = '…'` in a plain `SELECT` (accepted
//! over-redaction). Statements without a trigger stay byte-identical.
//!
//! Triggers (the literal after them, past an optional `=`, `:=`, `TO`, or a
//! parenthesised argument list whose literals are all redacted):
//! - `IDENTIFIED BY`, `IDENTIFIED WITH <plugin> BY|AS`, `IDENTIFIED VIA <plugin>
//!   USING` (any `BY`/`AS`/`USING` once `IDENTIFIED` was seen);
//! - any word or quoted identifier containing `PASSWORD` or `PASSWD`
//!   (`PASSWORD`, `MASTER_PASSWORD`, `SOURCE_PASSWORD`, `ENCRYPTED PASSWORD`,
//!   `OPTIONS (password '…')`, `PASSWORD(…)`, `@password := …`), and
//!   `ENCRYPTION_KEY`;
//! - `SET PASSWORD [FOR <user>] =`: every literal after the `=`;
//! - `REPLACE '<current password>'` (MySQL 8 `ALTER USER … REPLACE`);
//! - `CONNECTION '…'` (FEDERATED URLs, PG subscriptions), and any string whose
//!   content holds `password=` / `passwd=` / `pwd=` (libpq / ODBC connection
//!   strings) or a URL with `user:password@`.
//!
//! Under MySQL a statement is scanned twice: as the splitter lexes it, and with
//! every executable-comment opener (`/*!NNNNN`, `/*M!NNNNNN`) blanked, as a
//! server that runs the comment body lexes it. The union of both is redacted.
//!
//! Plain comments (`--`, `#`, `/* … */`, unterminated too) are scanned like
//! code; in a comment, a trigger with no redacted literal after it redacts the
//! rest of the body. Comment markers are never touched, so statement
//! boundaries do not move.
//!
//! Under PG, a plain `'…'` target holding `\` is redacted as far as its
//! backslash-escape reading goes too (`standard_conforming_strings=off`).
//!
//! Out of scope: application data in ordinary DML (`INSERT INTO users
//! (password) VALUES ('…')`, `AES_ENCRYPT(…, 'key')`), and literals reached
//! through `BINARY`, `CONCAT(…)` outside `PASSWORD(…)`, or an introducer
//! separated from its string by a space.

use dnt_core::sql::{tokenize, Dialect, TokKind, Token};

/// What every redacted literal becomes.
pub const REDACTED: &str = "'[REDACTED]'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub changed: bool,
}

/// Replaces every possible secret literal of `stmt` with [`REDACTED`]. Never
/// panics; linear in the length of `stmt`.
pub fn redact(stmt: &str, dialect: Dialect) -> Redacted {
    let mut spans = Vec::new();
    scan(stmt, 0, dialect, 0, &mut spans);
    if dialect.is_mysql() {
        if let Some(inlined) = blank_exec_openers(stmt) {
            scan(&inlined, 0, dialect, 0, &mut spans);
        }
    }
    if spans.is_empty() {
        return Redacted {
            text: stmt.to_string(),
            changed: false,
        };
    }
    spans.sort_unstable();
    let mut text = String::with_capacity(stmt.len());
    let (mut at, mut i) = (0, 0);
    while i < spans.len() {
        let (start, mut end) = spans[i];
        i += 1;
        while i < spans.len() && spans[i].0 <= end {
            end = end.max(spans[i].1);
            i += 1;
        }
        // Spans are char boundaries of `stmt` (blanking only rewrites ASCII).
        text.push_str(stmt.get(at..start).unwrap_or_default());
        text.push_str(REDACTED);
        at = end;
    }
    text.push_str(stmt.get(at..).unwrap_or_default());
    Redacted {
        text,
        changed: true,
    }
}

/// `stmt` with every `/*!digits` / `/*M!digits` opener replaced by spaces (same
/// byte offsets), or `None` when it has none.
fn blank_exec_openers(stmt: &str) -> Option<String> {
    let mut b = stmt.as_bytes().to_vec();
    let mut found = false;
    let mut i = 0;
    while i + 2 < b.len() {
        let len = if b[i..].starts_with(b"/*!") {
            3
        } else if b[i..].starts_with(b"/*M!") {
            4
        } else {
            i += 1;
            continue;
        };
        let mut j = i + len;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        b[i..j].fill(b' ');
        found = true;
        i = j;
    }
    found.then(|| String::from_utf8(b).ok()).flatten()
}

/// Comment bodies nested deeper than this are not tokenized again; see
/// [`raw_fallback`].
const MAX_COMMENT_DEPTH: usize = 4;

/// Pushes the spans to redact in `src`, which sits at byte `base` of the
/// statement: its code, then (recursively) the body of each plain comment.
/// `depth` is 0 for the statement and n for a body nested n comments deep.
fn scan(src: &str, base: usize, dialect: Dialect, depth: usize, out: &mut Vec<(usize, usize)>) {
    scan_in(src, src.as_bytes(), base, dialect, depth, out);
}

/// [`scan`] of `src`, a slice of `full` starting at `base`.
fn scan_in(
    src: &str,
    full: &[u8],
    base: usize,
    dialect: Dialect,
    depth: usize,
    out: &mut Vec<(usize, usize)>,
) {
    let toks = tokenize(src, dialect);
    let mut local = Vec::new();
    collect(src, &toks, dialect, &mut local);
    if depth == 0 {
        out.extend(local.iter().map(|&(s, e)| (base + s, base + e)));
    } else {
        comment_fallback(src, &toks, dialect, &mut local);
        for &(s, e) in &local {
            push_comment_span(full, base + s, base + e, out);
        }
    }
    for (bs, be) in comment_bodies(src, &toks, dialect) {
        let Some(body) = src.get(bs..be) else {
            continue;
        };
        if depth + 1 >= MAX_COMMENT_DEPTH {
            raw_fallback(body, full, base + bs, out);
        } else {
            scan_in(body, full, base + bs, dialect, depth + 1, out);
        }
    }
}

/// Pushes the comment-body span `s..e` of `full`, minus every `/` or `*` byte
/// that forms `/*` or `*/` with a neighbour (inside the span or not), so a
/// redaction never adds or removes a comment delimiter. The kept bytes split
/// the span; each piece is redacted on its own.
fn push_comment_span(full: &[u8], s: usize, e: usize, out: &mut Vec<(usize, usize)>) {
    let pair = |a: u8, b: u8| (a == b'/' && b == b'*') || (a == b'*' && b == b'/');
    let delim = |j: usize| {
        let c = full[j];
        (c == b'/' || c == b'*')
            && ((j > 0 && pair(full[j - 1], c)) || full.get(j + 1).is_some_and(|&n| pair(c, n)))
    };
    let mut run = None;
    for j in s..e.min(full.len()) {
        if delim(j) {
            if let Some(r) = run.take() {
                out.push((r, j));
            }
        } else if run.is_none() {
            run = Some(j);
        }
    }
    if let Some(r) = run {
        out.push((r, e.min(full.len())));
    }
}

/// In a comment body, prose may name a secret without quoting it
/// (`-- password: hunter2`): a trigger with no redacted literal after it (up
/// to the next trigger) redacts everything from it to the end of the body. So
/// does a trigger word the token pass could not see, e.g. inside a "string"
/// that an apostrophe in prose opened (`-- don't … IDENTIFIED BY 'x'`).
fn comment_fallback(src: &str, toks: &[Token], dialect: Dialect, local: &mut Vec<(usize, usize)>) {
    let end = src.trim_end().len();
    let b = src.as_bytes();
    let hidden = toks
        .iter()
        .filter(|t| !is_trigger_token(src, t, dialect))
        .find_map(|t| find_trigger(&b[t.start..t.end]).map(|p| t.start + p));
    if let Some(start) = hidden {
        local.push((start, end.max(start)));
    }
    local.sort_unstable();
    let triggers: Vec<usize> = toks
        .iter()
        .filter(|t| is_trigger_token(src, t, dialect))
        .map(|t| t.start)
        .collect();
    let mut p = 0;
    for (n, &start) in triggers.iter().enumerate() {
        let next = triggers.get(n + 1).copied().unwrap_or(usize::MAX);
        while local.get(p).is_some_and(|s| s.0 < start) {
            p += 1;
        }
        if !local.get(p).is_some_and(|s| s.0 < next) {
            local.push((start, end.max(start)));
            return;
        }
    }
}

/// The trigger words of comment prose (matched case-insensitively, anywhere).
const COMMENT_TRIGGERS: [&[u8]; 6] = [
    b"identified",
    b"password",
    b"passwd",
    b"pwd",
    b"encryption_key",
    b"connection",
];

/// Offset of the first [`COMMENT_TRIGGERS`] word in `b`.
fn find_trigger(b: &[u8]) -> Option<usize> {
    (0..b.len()).find(|&i| {
        COMMENT_TRIGGERS.iter().any(|w| {
            b.get(i..i + w.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(w))
        })
    })
}

/// Whether token `t` names a secret (the comment-fallback triggers).
fn is_trigger_token(src: &str, t: &Token, dialect: Dialect) -> bool {
    let text = src.get(t.start..t.end).unwrap_or_default();
    match t.kind {
        TokKind::Word(_) => find_trigger(text.as_bytes()).is_some(),
        TokKind::Quoted if is_ident_quote(text, dialect) => find_trigger(text.as_bytes()).is_some(),
        TokKind::Quoted => has_inline_secret(text.as_bytes()),
        _ => false,
    }
}

/// Past [`MAX_COMMENT_DEPTH`]: from each raw trigger word in `body` (at byte
/// `base` of `full`), redacts up to the next `/*`, `*/` or newline (so nesting
/// and line ends survive).
fn raw_fallback(body: &str, full: &[u8], base: usize, out: &mut Vec<(usize, usize)>) {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let Some(p) = find_trigger(&b[i..]) else {
            return;
        };
        i += p;
        let mut j = i;
        while j < b.len()
            && b[j] != b'\n'
            && !b[j..].starts_with(b"/*")
            && !b[j..].starts_with(b"*/")
        {
            j += 1;
        }
        // Back off trailing spaces; `j` and the result stay char boundaries.
        let mut e = j;
        while e > i && b[e - 1].is_ascii_whitespace() {
            e -= 1;
        }
        push_comment_span(full, base + i, base + e, out);
        i = j.max(i + 1);
    }
}

/// Byte ranges of the plain-comment bodies of `src` (markers excluded; a line
/// comment's body stops before its `\r\n`/`\n`). Comments are exactly the
/// non-blank text between tokens.
fn comment_bodies(src: &str, toks: &[Token], dialect: Dialect) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut gap = |lo: usize, hi: usize| {
        let mut i = lo;
        while i < hi {
            let rest = &b[i..hi];
            if rest.starts_with(b"--") || (rest[0] == b'#' && dialect.is_mysql()) {
                let body = i + if rest[0] == b'#' { 1 } else { 2 };
                let nl = rest.iter().position(|&c| c == b'\n').map_or(hi, |p| i + p);
                let end = if nl > body && b[nl - 1] == b'\r' {
                    nl - 1
                } else {
                    nl
                };
                out.push((body, end.max(body)));
                i = nl;
            } else if rest.starts_with(b"/*") {
                let (close, next) = block_end(b, i, hi, !dialect.is_mysql());
                out.push((i + 2, close));
                i = next;
            } else {
                i += 1;
            }
        }
    };
    let mut prev = 0;
    for t in toks {
        gap(prev, t.start);
        prev = t.end;
    }
    gap(prev, b.len());
    out
}

/// For the block comment opening at `i`: (start of its `*/`, offset past it),
/// or `(hi, hi)` when unterminated before `hi`.
fn block_end(b: &[u8], i: usize, hi: usize, nested: bool) -> (usize, usize) {
    let mut depth = 1;
    let mut j = i + 2;
    while j + 1 < hi {
        if b[j..].starts_with(b"*/") {
            depth -= 1;
            if depth == 0 {
                return (j, j + 2);
            }
            j += 2;
        } else if nested && b[j..].starts_with(b"/*") {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    (hi, hi)
}

/// Pushes the byte spans of `src` to redact, per the triggers in the module doc.
fn collect(src: &str, toks: &[Token], dialect: Dialect, out: &mut Vec<(usize, usize)>) {
    let mut scan = Scan {
        src,
        toks,
        out,
        pg: !dialect.is_mysql(),
        covered: None,
        run_end: 0,
        tail_searched: false,
    };
    let mut identified = false;
    for i in 0..toks.len() {
        match toks[i].kind {
            TokKind::Word(w) => {
                let up = w.to_ascii_uppercase();
                match up.as_str() {
                    "IDENTIFIED" => identified = true,
                    "PASSWORD" if is_word(toks, i.wrapping_sub(1), "SET") => scan.set_password(i),
                    "BY" | "AS" | "USING" if identified => scan.target(i + 1, true, true),
                    "REPLACE" => scan.target(i + 1, false, false),
                    "CONNECTION" => scan.target(i + 1, false, true),
                    _ if is_secret_name(&up) => scan.target(i + 1, true, true),
                    _ => {}
                }
            }
            TokKind::Quoted => {
                let t = toks[i];
                let text = src.get(t.start..t.end).unwrap_or_default();
                if is_ident_quote(text, dialect) {
                    if is_secret_name(&text.to_ascii_uppercase()) {
                        scan.target(i + 1, true, true);
                    }
                } else if has_inline_secret(text.as_bytes()) {
                    let end = scan.lit_end(&t);
                    scan.out.push((t.start, end));
                }
            }
            _ => {}
        }
    }
}

struct Scan<'t, 'a, 'o> {
    src: &'t str,
    toks: &'t [Token<'a>],
    out: &'o mut Vec<(usize, usize)>,
    /// PostgreSQL: a plain `'…'` literal may also be read with backslash escapes.
    pg: bool,
    /// Every literal up to this token index was already redacted by a region.
    covered: Option<usize>,
    /// Tokens before this index belong to an already redacted literal run.
    run_end: usize,
    /// A `SET PASSWORD FOR …` search for `=` already ran to the end.
    tail_searched: bool,
}

impl Scan<'_, '_, '_> {
    /// `SET PASSWORD` at token `i`: with `=` or `FOR`, every literal after the
    /// `=`; otherwise (`OPTIONS (SET password '…')`) a plain trigger.
    fn set_password(&mut self, i: usize) {
        let toks = self.toks;
        if is_punct(toks, i + 1, b'=') {
            self.region(i + 2, toks.len());
        } else if is_word(toks, i + 1, "FOR") {
            if !self.tail_searched {
                self.tail_searched = true;
                if let Some(eq) = (i + 2..toks.len()).find(|&j| is_punct(toks, j, b'=')) {
                    self.region(eq + 1, toks.len());
                }
            }
        } else {
            self.target(i + 1, true, true);
        }
    }

    /// Redacts the literal at token `k`. With `skip`, first steps over up to
    /// three `=`, `:` or `TO` tokens, and a `(` there redacts every literal up
    /// to its matching `)`. `number` also redacts a bare numeric literal.
    fn target(&mut self, k: usize, number: bool, skip: bool) {
        let toks = self.toks;
        let mut k = k;
        if skip {
            let mut stepped = 0;
            while stepped < 3
                && (is_punct(toks, k, b'=') || is_punct(toks, k, b':') || is_word(toks, k, "TO"))
            {
                k += 1;
                stepped += 1;
            }
            if toks.get(k).is_some_and(|t| t.kind == TokKind::Open) {
                // Inside an earlier region, every literal is already redacted.
                if self.covered.is_some_and(|c| k <= c) {
                    return;
                }
                let end = matching_close(toks, k);
                self.region(k + 1, end);
                return;
            }
        }
        // Charset introducers and prefixes glued to the string: `_utf8mb4'…'`,
        // `N'…'`, `X'…'`, PG `U&'…'`.
        if glued(toks, k) {
            k += 1;
        } else if is_word(toks, k, "U") && is_punct(toks, k + 1, b'&') && glued(toks, k + 1) {
            k += 2;
        }
        let Some(t) = toks.get(k) else { return };
        match t.kind {
            TokKind::Quoted => {
                if k < self.run_end {
                    return;
                }
                // Adjacent literals are concatenated by the server.
                let mut end = self.lit_end(t);
                let mut j = k + 1;
                while let Some(n) = toks.get(j).filter(|n| n.kind == TokKind::Quoted) {
                    end = end.max(self.lit_end(n));
                    j += 1;
                }
                self.run_end = j;
                self.out.push((t.start, end));
            }
            TokKind::Word(w) if number && w.starts_with(|c: char| c.is_ascii_digit()) => {
                self.out.push((t.start, t.end));
            }
            _ => {}
        }
    }

    /// Redacts every literal among tokens `from..to`, unless an enclosing
    /// region already did (keeps nested `PASSWORD(PASSWORD(…))` linear).
    fn region(&mut self, from: usize, to: usize) {
        if self.covered.is_some_and(|c| from <= c) {
            return;
        }
        for t in self.toks.get(from..to).unwrap_or_default() {
            match t.kind {
                TokKind::Quoted => self.out.push((t.start, self.lit_end(t))),
                TokKind::Word(w) if w.starts_with(|c: char| c.is_ascii_digit()) => {
                    self.out.push((t.start, t.end))
                }
                _ => {}
            }
        }
        self.covered = Some(to);
    }

    /// End of the literal token `t`. Under PG a plain `'…'` holding `\` may be
    /// read with backslash escapes (`standard_conforming_strings=off`), which
    /// can end it later: then the span runs to that end, widened to the end of
    /// the token it lands in (or to the next token when it lands in a gap) so
    /// the redacted text still lexes the same way.
    fn lit_end(&self, t: &Token) -> usize {
        let b = self.src.as_bytes();
        let plain = b.get(t.start) == Some(&b'\'');
        if !self.pg || !plain || !b[t.start..t.end].contains(&b'\\') {
            return t.end;
        }
        let e = backslash_quote_end(b, t.start);
        if e <= t.end {
            return t.end;
        }
        let idx = self.toks.partition_point(|x| x.end < e);
        match self.toks.get(idx) {
            Some(x) if x.start < e => x.end,
            Some(x) => x.start,
            None => b.len(),
        }
    }
}

/// Offset past the `'…'` literal opening at `open`, read with backslash
/// escapes (the input length when unterminated).
fn backslash_quote_end(b: &[u8], open: usize) -> usize {
    let mut j = open + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'\'' if b.get(j + 1) == Some(&b'\'') => j += 2,
            b'\'' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// Index of the `)` matching the `(` at `open`, or the token count.
fn matching_close(toks: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (j, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// Token `k` is a word or `&` immediately followed (no space) by a quoted token.
fn glued(toks: &[Token], k: usize) -> bool {
    match (toks.get(k), toks.get(k + 1)) {
        (Some(a), Some(b)) => {
            matches!(a.kind, TokKind::Word(_) | TokKind::Punct(b'&'))
                && b.kind == TokKind::Quoted
                && a.end == b.start
        }
        _ => false,
    }
}

fn is_word(toks: &[Token], i: usize, kw: &str) -> bool {
    matches!(toks.get(i), Some(Token { kind: TokKind::Word(w), .. }) if w.eq_ignore_ascii_case(kw))
}

fn is_punct(toks: &[Token], i: usize, c: u8) -> bool {
    toks.get(i).is_some_and(|t| t.kind == TokKind::Punct(c))
}

/// An uppercased name that marks the next literal as a secret.
fn is_secret_name(up: &str) -> bool {
    up.contains("PASSWORD") || up.contains("PASSWD") || up == "ENCRYPTION_KEY"
}

/// Whether the quoted token `text` is an identifier rather than a string.
fn is_ident_quote(text: &str, dialect: Dialect) -> bool {
    match (text.as_bytes().first(), dialect) {
        (Some(b'`'), _) | (Some(b'"'), Dialect::Postgres) => true,
        (Some(b'"'), Dialect::MySql(m)) => m.ansi_quotes,
        _ => false,
    }
}

/// Whether a string literal holds a credential: a `password=` / `passwd=` /
/// `pwd=` key, or a URL with `user:password@`.
fn has_inline_secret(b: &[u8]) -> bool {
    has_kv_secret(b) || has_url_password(b)
}

/// `scheme://user:password@…`: a `:` in the authority before an `@`.
fn has_url_password(b: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 <= b.len() {
        if &b[i..i + 3] != b"://" {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        let mut colon = false;
        while j < b.len()
            && !matches!(b[j], b'/' | b'@' | b'?' | b'#' | b'\'' | b'"')
            && !b[j].is_ascii_whitespace()
        {
            colon |= b[j] == b':';
            j += 1;
        }
        if colon && b.get(j) == Some(&b'@') {
            return true;
        }
        i = j.max(i + 1);
    }
    false
}

/// Whether a string literal holds a `password=` / `passwd=` / `pwd=` key
/// (case-insensitive, optional spaces before `=`), as connection strings do.
fn has_kv_secret(b: &[u8]) -> bool {
    (0..b.len()).any(|i| {
        [&b"password"[..], b"passwd", b"pwd"].iter().any(|key| {
            b.get(i..i + key.len())
                .is_some_and(|w| w.eq_ignore_ascii_case(key))
                && b[i + key.len()..]
                    .iter()
                    .find(|c| !c.is_ascii_whitespace())
                    .is_some_and(|&c| c == b'=')
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::sql::{split, MySqlMode};

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;

    fn red(s: &str, d: Dialect) -> String {
        let r = redact(s, d);
        assert_eq!(r.changed, r.text != s, "changed flag for {s:?}");
        r.text
    }

    fn assert_red(input: &str, d: Dialect, want: &str) {
        let r = redact(input, d);
        assert_eq!(r.text, want, "input {input:?}");
        assert!(r.changed, "changed for {input:?}");
    }

    #[test]
    fn redact_identified_by() {
        assert_red(
            "CREATE USER 'u'@'%' IDENTIFIED BY 's3cr''et'",
            MY,
            "CREATE USER 'u'@'%' IDENTIFIED BY '[REDACTED]'",
        );
    }

    #[test]
    fn redact_identified_with_by() {
        assert_red(
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY 'x'",
            MY,
            "ALTER USER u IDENTIFIED WITH caching_sha2_password BY '[REDACTED]'",
        );
        assert_red(
            "CREATE USER u IDENTIFIED WITH mysql_native_password AS '*ABC'",
            MY,
            "CREATE USER u IDENTIFIED WITH mysql_native_password AS '[REDACTED]'",
        );
        assert_red(
            "GRANT ALL ON *.* TO u IDENTIFIED BY PASSWORD '*ABC'",
            MY,
            "GRANT ALL ON *.* TO u IDENTIFIED BY PASSWORD '[REDACTED]'",
        );
    }

    #[test]
    fn redact_identified_via_using() {
        assert_red(
            "CREATE USER u IDENTIFIED VIA ed25519 USING PASSWORD('x')",
            MY,
            "CREATE USER u IDENTIFIED VIA ed25519 USING PASSWORD('[REDACTED]')",
        );
        assert_red(
            "CREATE USER u IDENTIFIED VIA ed25519 USING 'hash' OR unix_socket",
            MY,
            "CREATE USER u IDENTIFIED VIA ed25519 USING '[REDACTED]' OR unix_socket",
        );
    }

    #[test]
    fn redact_set_password_forms() {
        assert_red(
            "SET PASSWORD FOR 'u'@'h' = 'x'",
            MY,
            "SET PASSWORD FOR 'u'@'h' = '[REDACTED]'",
        );
        assert_red(
            "SET PASSWORD = PASSWORD('x')",
            MY,
            "SET PASSWORD = PASSWORD('[REDACTED]')",
        );
        assert_red(
            "SET PASSWORD FOR u = OLD_PASSWORD(CONCAT('a', 'b'))",
            MY,
            "SET PASSWORD FOR u = OLD_PASSWORD(CONCAT('[REDACTED]', '[REDACTED]'))",
        );
        assert_red(
            "ALTER USER u IDENTIFIED BY 'new' REPLACE 'old' RETAIN CURRENT PASSWORD",
            MY,
            "ALTER USER u IDENTIFIED BY '[REDACTED]' REPLACE '[REDACTED]' RETAIN CURRENT PASSWORD",
        );
    }

    #[test]
    fn redact_replication_passwords() {
        assert_red(
            "CHANGE MASTER TO MASTER_PASSWORD='x', MASTER_USER='u'",
            MY,
            "CHANGE MASTER TO MASTER_PASSWORD='[REDACTED]', MASTER_USER='u'",
        );
        assert_red(
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = \"x\"",
            MY,
            "CHANGE REPLICATION SOURCE TO SOURCE_PASSWORD = '[REDACTED]'",
        );
    }

    #[test]
    fn redact_pg_role_passwords() {
        assert_red(
            "CREATE ROLE r LOGIN PASSWORD 'x'",
            PG,
            "CREATE ROLE r LOGIN PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER USER r WITH ENCRYPTED PASSWORD E'x\\'y'",
            PG,
            "ALTER USER r WITH ENCRYPTED PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD $p$x$p$",
            PG,
            "ALTER ROLE r PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD 'x' VALID UNTIL '2030-01-01'",
            PG,
            "ALTER ROLE r PASSWORD '[REDACTED]' VALID UNTIL '2030-01-01'",
        );
    }

    #[test]
    fn redact_pg_server_options() {
        assert_red(
            "CREATE USER MAPPING FOR u SERVER s OPTIONS (user 'u', password 'x')",
            PG,
            "CREATE USER MAPPING FOR u SERVER s OPTIONS (user 'u', password '[REDACTED]')",
        );
        assert_red(
            "ALTER SERVER s OPTIONS (SET password 'x')",
            PG,
            "ALTER SERVER s OPTIONS (SET password '[REDACTED]')",
        );
        assert_red(
            "ALTER USER MAPPING FOR u SERVER s OPTIONS (ADD \"password\" 'x')",
            PG,
            "ALTER USER MAPPING FOR u SERVER s OPTIONS (ADD \"password\" '[REDACTED]')",
        );
        assert_red(
            "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (USER 'u', PASSWORD 'x')",
            MY,
            "CREATE SERVER s FOREIGN DATA WRAPPER mysql OPTIONS (USER 'u', PASSWORD '[REDACTED]')",
        );
    }

    #[test]
    fn redact_encryption_key() {
        assert_red(
            "ALTER TABLE t ENCRYPTION_KEY = 'x'",
            MY,
            "ALTER TABLE t ENCRYPTION_KEY = '[REDACTED]'",
        );
    }

    #[test]
    fn redact_quotes_in_password() {
        let cases = [
            (MY, "CREATE USER u IDENTIFIED BY 'a\\'b$c'", "a\\'b$c"),
            (PG, "ALTER ROLE r PASSWORD 'a''b\\c$'", "a''b\\c$"),
            (PG, "ALTER ROLE r PASSWORD $$a'b$$", "a'b"),
        ];
        for (d, input, secret) in cases {
            let input = format!("{input}; SELECT 1");
            let first = &split(&input, d)[0].text;
            let out = red(first, d);
            assert!(!out.contains(secret), "{out}");
            assert!(out.contains(REDACTED), "{out}");
            let joined = format!("{out}; SELECT 2");
            assert_eq!(split(&joined, d).len(), 2, "{joined}");
        }
    }

    #[test]
    fn redact_untouched_statements_are_byte_identical() {
        for d in [MY, PG] {
            for s in [
                "SELECT 'password' AS label",
                "SELECT 1",
                "UPDATE t SET note = 'by'",
                "INSERT INTO t VALUES ('IDENTIFIED BY')",
                "KILL CONNECTION 5",
                "SELECT REPLACE('a', 'b', 'c')",
                "ALTER USER u PASSWORD EXPIRE INTERVAL 90 DAY",
                "",
            ] {
                let r = redact(s, d);
                assert_eq!(r.text, s);
                assert!(!r.changed, "{s}");
            }
        }
    }

    #[test]
    fn redact_password_column_comparison_is_redacted_conservatively() {
        assert_red(
            "SELECT * FROM u WHERE password = 'x'",
            MY,
            "SELECT * FROM u WHERE password = '[REDACTED]'",
        );
        assert_red(
            "UPDATE u SET pass_hash = 1, user_password = 'x' WHERE id = 1",
            PG,
            "UPDATE u SET pass_hash = 1, user_password = '[REDACTED]' WHERE id = 1",
        );
    }

    #[test]
    fn redact_literal_variants() {
        // Charset introducers, national and Unicode strings, adjacent literals
        // (concatenated by the server) and bare numbers.
        assert_red(
            "CREATE USER u IDENTIFIED BY _utf8mb4'x' 'y'",
            MY,
            "CREATE USER u IDENTIFIED BY _utf8mb4'[REDACTED]'",
        );
        assert_red(
            "SELECT * FROM t WHERE password = N'x'",
            MY,
            "SELECT * FROM t WHERE password = N'[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD U&'x'",
            PG,
            "ALTER ROLE r PASSWORD U&'[REDACTED]'",
        );
        assert_red(
            "SELECT * FROM t WHERE password = 123456",
            MY,
            "SELECT * FROM t WHERE password = '[REDACTED]'",
        );
        assert_red("SET @password := 'x'", MY, "SET @password := '[REDACTED]'");
    }

    #[test]
    fn redact_inside_executable_comments() {
        assert_red(
            "/*!50706 CREATE USER u IDENTIFIED BY 'x' */",
            MY,
            "/*!50706 CREATE USER u IDENTIFIED BY '[REDACTED]' */",
        );
        // The executing server reads `'a*/b'` as one string.
        let out = red("/*!50000 CREATE USER u IDENTIFIED BY 'a*/b' */", MY);
        assert!(!out.contains("a*/b") && !out.contains(" b"), "{out}");
        // The splitter (and a server ignoring the comment) reads `'pw'` as code.
        let out = red(
            "/*!40101 SET @x='a */ CREATE USER u IDENTIFIED BY 'pw' -- '",
            MY,
        );
        assert!(!out.contains("pw"), "{out}");
        // In PG it is a plain comment, scanned the same way.
        assert_red(
            "/*!50706 CREATE USER u IDENTIFIED BY 'x' */ SELECT 1",
            PG,
            "/*!50706 CREATE USER u IDENTIFIED BY '[REDACTED]' */ SELECT 1",
        );
    }

    #[test]
    fn redact_connection_strings() {
        assert_red(
            "CREATE SUBSCRIPTION s CONNECTION 'host=h password=x' PUBLICATION p",
            PG,
            "CREATE SUBSCRIPTION s CONNECTION '[REDACTED]' PUBLICATION p",
        );
        assert_red(
            "SELECT dblink_connect('c', 'dbname=d Password = x')",
            PG,
            "SELECT dblink_connect('c', '[REDACTED]')",
        );
        assert_red(
            "CREATE TABLE t (a INT) ENGINE=FEDERATED CONNECTION='mysql://u:p@h/db/t'",
            MY,
            "CREATE TABLE t (a INT) ENGINE=FEDERATED CONNECTION='[REDACTED]'",
        );
    }

    #[test]
    fn redact_url_userinfo_and_passwd_keys() {
        assert_red(
            "SELECT dblink_connect('postgresql://u:s3cret@h/db')",
            PG,
            "SELECT dblink_connect('[REDACTED]')",
        );
        assert_red(
            "SELECT dblink('user=u passwd=x', 'select 1')",
            PG,
            "SELECT dblink('[REDACTED]', 'select 1')",
        );
        for s in [
            "SELECT 'http://h:80/x', 'mailto:a@b', 'postgresql://u@h/db'",
            "SELECT 'a://b:c'",
        ] {
            assert_eq!(red(s, PG), s);
        }
    }

    fn same_split(input: &str, out: &str, d: Dialect) {
        assert_eq!(
            split(input, d).len(),
            split(out, d).len(),
            "{input:?} -> {out:?}"
        );
    }

    #[test]
    fn redact_inside_plain_comments() {
        let cases = [
            (
                MY,
                "SELECT 1 -- ALTER USER u IDENTIFIED BY 'pw'\n; SELECT 2",
                "SELECT 1 -- ALTER USER u IDENTIFIED BY '[REDACTED]'\n; SELECT 2",
            ),
            (
                PG,
                "SELECT 1 -- ALTER USER u IDENTIFIED BY 'pw'\r\n; SELECT 2",
                "SELECT 1 -- ALTER USER u IDENTIFIED BY '[REDACTED]'\r\n; SELECT 2",
            ),
            (
                MY,
                "# CREATE USER u IDENTIFIED BY 'pw'\nSELECT 1; SELECT 2",
                "# CREATE USER u IDENTIFIED BY '[REDACTED]'\nSELECT 1; SELECT 2",
            ),
            (
                PG,
                "/* CREATE ROLE r PASSWORD 'pw' */ SELECT 1; SELECT 2",
                "/* CREATE ROLE r PASSWORD '[REDACTED]' */ SELECT 1; SELECT 2",
            ),
            (
                MY,
                "/* CREATE ROLE r PASSWORD 'pw' */ SELECT 1; SELECT 2",
                "/* CREATE ROLE r PASSWORD '[REDACTED]' */ SELECT 1; SELECT 2",
            ),
            // A trigger without a literal after it: redact to the end of the body.
            (
                MY,
                "SELECT 1 -- old password: hunter2\n; SELECT 2",
                "SELECT 1 -- old '[REDACTED]'\n; SELECT 2",
            ),
            (
                PG,
                "SELECT 1 /* IDENTIFIED BY hunter2 */; SELECT 2",
                "SELECT 1 /* '[REDACTED]' */; SELECT 2",
            ),
            // Unterminated block comment.
            (
                PG,
                "SELECT 1 /* password hunter2",
                "SELECT 1 /* '[REDACTED]'",
            ),
            (
                MY,
                "SELECT 1 /* ALTER USER u IDENTIFIED BY 'p;w",
                "SELECT 1 /* ALTER USER u IDENTIFIED BY '[REDACTED]'",
            ),
            // Nested PG comment and a comment inside a comment.
            (
                PG,
                "/* a /* PASSWORD 'x' */ b */ SELECT 1",
                "/* a /* PASSWORD '[REDACTED]' */ b */ SELECT 1",
            ),
            (
                MY,
                "/* a -- PASSWORD hunter2\n b */ SELECT 1",
                "/* a -- '[REDACTED]'\n b */ SELECT 1",
            ),
        ];
        for (d, input, want) in cases {
            assert_red(input, d, want);
            same_split(input, want, d);
        }
        // Untouched comments stay byte-identical.
        for d in [MY, PG] {
            let s = "SELECT 1 -- just a note\n/* by 'x' */ SELECT 2";
            assert_eq!(red(s, d), s);
        }
        // Deep nesting falls back to redacting from the trigger to the end.
        let deep = "/*".repeat(100) + " PASSWORD hunter2 " + &"*/".repeat(100) + " SELECT 1";
        let out = red(&deep, PG);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.ends_with("*/ SELECT 1"), "{out}");
        same_split(&deep, &out, PG);
    }

    #[test]
    fn redact_comment_prose_apostrophe_does_not_hide_triggers() {
        let cases = [
            (
                MY,
                "SELECT 1 -- don't forget: ALTER USER u IDENTIFIED BY 'hunter2'",
            ),
            (
                PG,
                "SELECT 1 -- don't forget: ALTER USER u IDENTIFIED BY 'hunter2'",
            ),
            (
                MY,
                "SELECT 1 # don't forget: ALTER USER u IDENTIFIED BY 'hunter2'\n; SELECT 2",
            ),
            (
                PG,
                "SELECT 1 /* don't forget: ALTER USER u IDENTIFIED BY 'hunter2' */; SELECT 2",
            ),
            (MY, "SELECT 1 /* it's the pwd: hunter2 */; SELECT 2"),
        ];
        for (d, input) in cases {
            let out = red(input, d);
            assert!(!out.contains("hunter2"), "{out}");
            assert!(out.starts_with("SELECT 1 "), "{out}");
            same_split(input, &out, d);
        }
    }

    #[test]
    fn redact_never_moves_comment_delimiters() {
        let cases = [
            (PG, "SELECT 0 /* PASSWORD '/*' */ SELECT 1; SELECT 2"),
            (PG, "SELECT 0 /* PASSWORD 'a*/' */ SELECT 1; SELECT 2"),
            (
                PG,
                "SELECT 0 /* IDENTIFIED BY x /* y */ z */ SELECT 1; SELECT 2",
            ),
            (MY, "SELECT 0 /* PASSWORD 'a*/ SELECT 1; SELECT 2"),
            (MY, "SELECT 0 -- PASSWORD '/*'\n; SELECT 2"),
            (PG, "SELECT 0 /* x */ /* PASSWORD p/*q*/r */; SELECT 2"),
        ];
        for (d, input) in cases {
            let out = red(input, d);
            assert!(out.contains(REDACTED), "{out}");
            same_split(input, &out, d);
            let count = |s: &str, p: &str| s.matches(p).count();
            assert_eq!(count(input, "/*"), count(&out, "/*"), "{out}");
            assert_eq!(count(input, "*/"), count(&out, "*/"), "{out}");
        }
    }

    #[test]
    fn redact_pg_backslash_reading_of_plain_literals() {
        // With standard_conforming_strings=off, `'a\' VALID UNTIL '` is the
        // password: redact as far as the backslash reading goes.
        assert_red(
            "ALTER ROLE r PASSWORD 'a\\' VALID UNTIL '2030'",
            PG,
            "ALTER ROLE r PASSWORD '[REDACTED]'",
        );
        assert_red(
            "ALTER ROLE r PASSWORD 'a\\b' VALID UNTIL '2030'",
            PG,
            "ALTER ROLE r PASSWORD '[REDACTED]' VALID UNTIL '2030'",
        );
    }

    #[test]
    fn redact_quoted_identifier_triggers_stay_linear() {
        let s = "\"password\" ".repeat(50_000);
        let t = std::time::Instant::now();
        let out = red(&s, PG);
        assert!(t.elapsed().as_millis() < 500, "{:?}", t.elapsed());
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn redact_respects_sql_mode() {
        let nbe = Dialect::MySql(MySqlMode {
            ansi_quotes: false,
            no_backslash_escapes: true,
        });
        // Under NO_BACKSLASH_ESCAPES `'a\'` ends the password.
        assert_eq!(
            red("CREATE USER u IDENTIFIED BY 'a\\' COMMENT 'c'", nbe),
            "CREATE USER u IDENTIFIED BY '[REDACTED]' COMMENT 'c'"
        );
    }

    #[test]
    fn redact_never_panics_and_is_linear() {
        // (input, redacted under MySQL, redacted under PG)
        for (s, my, pg) in [
            ("PASSWORD", false, false),
            ("PASSWORD =", false, false),
            ("PASSWORD (", false, false),
            ("SET PASSWORD FOR", false, false),
            ("IDENTIFIED BY", false, false),
            ("IDENTIFIED BY 'x", true, true),
            ("PASSWORD $$x", false, true),
            ("PASSWORD E'", true, true),
            ("/*!", false, false),
            ("/*!5", false, false),
            ("PASSWORD 'ư😀", true, true),
            ("ưPASSWORD = 'é'", true, true),
            ("))((PASSWORD))", false, false),
        ] {
            for (d, want) in [(MY, my), (PG, pg)] {
                let out = red(s, d);
                assert_eq!(out.contains(REDACTED), want, "{s} -> {out}");
                assert!(!out.contains("'x"), "{s} -> {out}");
            }
        }
        let mix = "/*!50000 SET PASSWORD FOR 'ư'@'h' = PASSWORD(E'a\\'😀$q$') */ \
                   IDENTIFIED VIA p USING U&'x' REPLACE N'y' /*M!100 x */ $t$z$t$ -- c\r\n";
        for (i, _) in mix.char_indices() {
            for d in [MY, PG] {
                let _ = redact(&mix[..i], d);
                let _ = redact(&mix[i..], d);
            }
        }
        let big = "PASSWORD(".repeat(50_000) + "'x'" + &")".repeat(50_000);
        let t = std::time::Instant::now();
        for d in [MY, PG] {
            assert!(!red(&big, d).contains("'x'"));
            let many = "SELECT password = 'x', ".repeat(20_000);
            assert!(!red(&many, d).contains("'x'"));
        }
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }
}
