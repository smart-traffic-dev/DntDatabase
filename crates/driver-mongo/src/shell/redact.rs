//! Redaction (spec §4.5): the form of a script that is safe to keep in History, logs and
//! Favorites. Secrets are replaced with `***`, in two tiers, per command of
//! [`split`](super::parse::split):
//!
//! - **Precise**, for a command that lexes and parses: the value of every key whose name
//!   holds `pwd`, `password` or `secret` (any case, at any depth: `ldapQueryPassword`,
//!   `client_secret`), and the password of a `mongodb://` / `mongodb+srv://` URI up to
//!   the last `@` before the host (so a password holding `@` is masked whole).
//! - **Coarse**, for everything else: a run of commands that do not parse (with the
//!   text between them), the text between commands (comments), and comments inside a
//!   parsed command. Everything after the FIRST secret marker of the region is masked,
//!   to the region's end. The markers are those key names anywhere (as names or inside
//!   quotes), `.auth` / `.changeUserPassword` (masked after `(` and a quoted first
//!   argument), and a `mongodb://` / `mongodb+srv://` URI with user info (masked after
//!   the scheme). Over-masking is accepted; leaking is not.
//!
//! `db.auth(…)` and `db.changeUserPassword(…)` are not in the grammar, so they are
//! always coarse. Never panics.

use dnt_core::command::CommandLanguage;
use dnt_core::redact::Redacted;

use super::parse::split;
use super::{lex, MongoLanguage, Token, TokenKind};

/// What a secret is replaced with.
pub const MASK: &str = "***";

/// The redacted form of `text`.
pub fn redact(text: &str) -> Redacted {
    let mut ranges = Vec::new();
    // The end of the last command, and where the current run of bad commands began.
    let mut at = 0;
    let mut bad_from: Option<usize> = None;
    for s in split(text) {
        let (start, end) = s.range;
        if MongoLanguage.parse(&s.text).is_ok() {
            coarse(text, bad_from.take().unwrap_or(at), start, &mut ranges);
            precise(text, start, &s.text, &mut ranges);
        } else if bad_from.is_none() {
            bad_from = Some(at);
        }
        at = end;
    }
    coarse(text, bad_from.unwrap_or(at), text.len(), &mut ranges);
    uri_ranges(text, &mut ranges);
    apply(text, ranges)
}

fn is_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("pwd") || key.contains("password") || key.contains("secret")
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

// ---------------------------------------------------------------------------------
// Precise: a command that parses

/// The secret values of the parsed command `span` (at byte `start` of `text`), and the
/// comments between its tokens (coarse).
fn precise(text: &str, start: usize, span: &str, out: &mut Vec<(usize, usize)>) {
    let Ok(toks) = lex(span) else {
        // `use 2024_logs`: parses, but is not JavaScript tokens.
        coarse(text, start, start + span.len(), out);
        return;
    };
    let toks: Vec<Token> = toks
        .into_iter()
        .map(|t| Token {
            start: t.start + start,
            end: t.end + start,
            ..t
        })
        .collect();
    for (i, t) in toks.iter().enumerate() {
        if let TokenKind::Ident(key) | TokenKind::Str(key) = &t.kind {
            if is_secret(key) && punct(&toks, i + 1, ':') && i + 2 < toks.len() {
                let end = value_end(&toks, i + 2);
                out.push((toks[i + 2].start, toks[end - 1].end));
            }
        }
    }
    for w in toks.windows(2) {
        coarse(text, w[0].end, w[1].start, out);
    }
}

fn punct(toks: &[Token], i: usize, c: char) -> bool {
    matches!(toks.get(i).map(|t| &t.kind), Some(TokenKind::Punct(p)) if *p == c)
}

/// Just past the bracket that closes the one at `open`.
fn closing(toks: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokenKind::Punct('(' | '[' | '{') => depth += 1,
            TokenKind::Punct(')' | ']' | '}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// Just past the value that starts at `i` (a literal of a parsed command: a scalar, a
/// helper call or a bracketed value, `new` and a sign included). At least `i + 1`.
fn value_end(toks: &[Token], mut i: usize) -> usize {
    while matches!(toks.get(i).map(|t| &t.kind), Some(TokenKind::Punct('-')))
        || matches!(toks.get(i).map(|t| &t.kind), Some(TokenKind::Ident(w)) if w == "new")
    {
        i += 1;
    }
    let end = match toks.get(i).map(|t| &t.kind) {
        None => toks.len(),
        Some(TokenKind::Punct('(' | '[' | '{')) => closing(toks, i),
        Some(TokenKind::Ident(_)) if punct(toks, i + 1, '(') => closing(toks, i + 1),
        Some(_) => i + 1,
    };
    end.min(toks.len()).max(1)
}

// ---------------------------------------------------------------------------------
// Coarse: everything else

/// Masks `text[from..to]` from just after its first secret marker to `to` (trailing
/// whitespace, which holds no secret, is kept).
fn coarse(text: &str, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    let Some(region) = text.get(from..to) else {
        return;
    };
    if let Some(m) = first_marker(region) {
        let end = region.trim_end().len();
        if m < end {
            out.push((from + m, from + end));
        }
    }
}

/// Where masking starts in `s`: just after its first secret marker.
fn first_marker(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    // (where the marker starts, where masking starts)
    let mut best: Option<(usize, usize)> = None;
    let mut consider = |start: usize, mask_from: usize| {
        if best.is_none_or(|(s0, _)| start < s0) {
            best = Some((start, mask_from));
        }
    };
    for key in ["pwd", "password", "secret"] {
        if let Some(i) = lower.find(key) {
            let mut e = i + key.len();
            while e < b.len() && is_word(b[e]) {
                e += 1;
            }
            consider(i, e);
        }
    }
    for name in [".auth", ".changeuserpassword"] {
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|r| r.find(name)) {
            let i = from + i;
            let e = i + name.len();
            if !b.get(e).is_some_and(|&c| is_word(c)) {
                consider(i, after_first_argument(b, e));
                break;
            }
            from = e;
        }
    }
    for scheme in ["mongodb://", "mongodb+srv://"] {
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|r| r.find(scheme)) {
            let i = from + i;
            let e = i + scheme.len();
            let uri_end = b[e..]
                .iter()
                .position(u8::is_ascii_whitespace)
                .map_or(b.len(), |p| e + p);
            if b[e..uri_end].contains(&b'@') {
                consider(i, e);
                break;
            }
            from = e;
        }
    }
    best.map(|(_, m)| m)
}

/// After `.auth` (ending at `e`): past `(` and a quoted first argument and its `,` when
/// they are there, else past what is there of them. Never past the password.
fn after_first_argument(b: &[u8], e: usize) -> usize {
    let open = skip_space(b, e);
    if b.get(open) != Some(&b'(') {
        return e;
    }
    let arg = skip_space(b, open + 1);
    let Some(&q @ (b'\'' | b'"' | b'`')) = b.get(arg) else {
        return open + 1;
    };
    let mut k = arg + 1;
    while k < b.len() && b[k] != q {
        k += if b[k] == b'\\' { 2 } else { 1 };
    }
    let comma = skip_space(b, k + 1);
    if k < b.len() && b.get(comma) == Some(&b',') {
        comma + 1
    } else {
        open + 1
    }
}

// ---------------------------------------------------------------------------------
// URIs

/// The password of every `mongodb://` / `mongodb+srv://` URI: from the first `:` of the
/// user info to the last `@` before the next whitespace.
fn uri_ranges(text: &str, out: &mut Vec<(usize, usize)>) {
    let lower = text.to_ascii_lowercase();
    let b = text.as_bytes();
    for scheme in ["mongodb://", "mongodb+srv://"] {
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|s| s.find(scheme)) {
            let s = from + i + scheme.len();
            let mut e = s;
            while e < b.len() && !b[e].is_ascii_whitespace() {
                e += 1;
            }
            let at = b[s..e].iter().rposition(|&c| c == b'@').map(|p| s + p);
            if let Some(at) = at {
                if let Some(colon) = b[s..at].iter().position(|&c| c == b':') {
                    out.push((s + colon + 1, at));
                }
            }
            from = s;
        }
    }
}

// ---------------------------------------------------------------------------------

/// `text` with every range replaced by [`MASK`] (overlapping ranges merged).
fn apply(text: &str, ranges: Vec<(usize, usize)>) -> Redacted {
    let floor = |mut i: usize| {
        i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let ceil = |mut i: usize| {
        i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i += 1;
        }
        i
    };
    let mut ranges: Vec<(usize, usize)> = ranges
        .into_iter()
        .map(|(a, b)| (floor(a), ceil(b)))
        .filter(|(a, b)| a < b)
        .collect();
    if ranges.is_empty() {
        return Redacted {
            text: text.to_string(),
            changed: false,
        };
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (a, b) in merged {
        out.push_str(&text[at..a]);
        out.push_str(MASK);
        at = b;
    }
    out.push_str(&text[at..]);
    Redacted {
        text: out,
        changed: true,
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::MongoLanguage;
    use dnt_core::command::CommandLanguage;

    fn check(cases: &[(&str, &str)]) {
        for (text, want) in cases {
            let r = MongoLanguage.redact(text);
            assert_eq!(r.text, *want, "{text}");
            assert_eq!(r.changed, text != want, "{text}");
        }
    }

    /// None of `secrets` is left in the redacted `text`.
    fn hidden(text: &str, secrets: &[&str]) {
        let r = MongoLanguage.redact(text).text;
        for s in secrets {
            assert!(!r.contains(s), "{text:?} -> {r:?} keeps {s}");
        }
    }

    #[test]
    fn redacts_password_keys_at_any_depth() {
        // Commands that parse: only the values.
        check(&[
            (
                "db.runCommand({createUser: 'u', pwd: 'hunter2', roles: []})",
                "db.runCommand({createUser: 'u', pwd: ***, roles: []})",
            ),
            (
                "db.c.insertOne({a: {b: [{Password: \"x\"}]}})",
                "db.c.insertOne({a: {b: [{Password: ***}]}})",
            ),
            (
                "db.adminCommand({setParameter: 1, ldapQueryPassword: 'p'})",
                "db.adminCommand({setParameter: 1, ldapQueryPassword: ***})",
            ),
            (
                "db.c.insertOne({'client_secret': NumberLong(5), \"MY_PWD\": {a: 1, b: [2]}, n: -1, \
                 secretN: -2, s: new Date()})",
                "db.c.insertOne({'client_secret': ***, \"MY_PWD\": ***, n: -1, secretN: ***, s: new Date()})",
            ),
            (
                "db.c.insertOne({\n  user: 'u',\n  pwd: 'p',\n})\ndb.c.find()",
                "db.c.insertOne({\n  user: 'u',\n  pwd: ***,\n})\ndb.c.find()",
            ),
            (
                "use 2024logs\ndb.c.insertOne({pwd: 'x'})",
                "use 2024logs\ndb.c.insertOne({pwd: ***})",
            ),
            // Nothing to redact: the text is kept byte for byte.
            ("db.c.find({name: 'pwd'})", "db.c.find({name: 'pwd'})"),
            ("use 2024_logs\nshow dbs", "use 2024_logs\nshow dbs"),
            // A comment is coarse: everything after its marker.
            (
                "db.c.find({name: 'x'}) // password: none\nshow dbs",
                "db.c.find({name: 'x'}) // password***\nshow dbs",
            ),
            (
                "db.c.find({a: 1 /* pwd: x */, b: 2})",
                "db.c.find({a: 1 /* pwd***, b: 2})",
            ),
            // Not parsed: everything after the first marker.
            (
                "db.c.insertOne({secret: 'a' + 'b', n: 1})",
                "db.c.insertOne({secret***",
            ),
            ("db.auth('admin', 'hunter2')", "db.auth('admin',***"),
            (
                "db.getSiblingDB('admin').changeUserPassword('u', \"n3w\" )",
                "db.getSiblingDB('admin').changeUserPassword('u',***",
            ),
            ("db.auth({user: 'u', pwd: 'p'})", "db.auth(***"),
            ("db.auth('u')", "db.auth(***"),
            // A parsed command after a bad one is precise again.
            (
                "db.auth('u', 'p')\ndb.c.insertOne({pwd: 'q', n: 1})",
                "db.auth('u',***\ndb.c.insertOne({pwd: ***, n: 1})",
            ),
        ]);
    }

    #[test]
    fn redacts_uri_password_with_at_sign() {
        check(&[
            (
                "db.c.insertOne({uri: 'mongodb://app:p@ss@db.example.com:27017/x?tls=true'})",
                "db.c.insertOne({uri: 'mongodb://app:***@db.example.com:27017/x?tls=true'})",
            ),
            (
                "db.c.insertOne({u: 'mongodb+srv://u:s3cr@t@cluster0.example.net/?w=1'})",
                "db.c.insertOne({u: 'mongodb+srv://u:***@cluster0.example.net/?w=1'})",
            ),
            (
                "db.c.insertOne({a: 'MONGODB://u:x@h', b: 'mongodb://v:y@h2'})",
                "db.c.insertOne({a: 'MONGODB://u:***@h', b: 'mongodb://v:***@h2'})",
            ),
            // Not a command: coarse, after the scheme.
            (
                "mongodb+srv://u:s3cr@t@cluster0.example.net/?retryWrites=true",
                "mongodb+srv://***",
            ),
            // No user info: unchanged.
            (
                "db.c.insertOne({u: 'mongodb://u@h/db'})",
                "db.c.insertOne({u: 'mongodb://u@h/db'})",
            ),
            ("mongodb://h:27017/db", "mongodb://h:27017/db"),
        ]);
    }

    #[test]
    fn redacts_unparsable_lines() {
        check(&[
            (
                "db.createUser({user: 'u', pwd: 'hunter2",
                "db.createUser({user: 'u', pwd***",
            ),
            ("db.auth('u', 'hunter2", "db.auth('u',***"),
            (
                "db.changeUserPassword('u', \"n3w",
                "db.changeUserPassword('u',***",
            ),
            (
                "db.c.insertOne({password: \"a,b\", note: 'open",
                "db.c.insertOne({password***",
            ),
            ("{secret: `multi\nline` 'x", "{secret***"),
            (
                "x = {'Password' : abc def, y: 1} 'oops",
                "x = {'Password***",
            ),
            // A bad run masks to its end, past the lines that do lex.
            (
                "db.createUser({\n  user: 'u',\n  pwd: 'p1'\n})\ndb.auth('u', 'p2\nmongodb://u:p3@h",
                "db.createUser({\n  user: 'u',\n  pwd***",
            ),
            ("db.c.find({a: 'open", "db.c.find({a: 'open"),
            (
                "db.runCommand({createUser: 'app', pwd:\n  'S3cret!, roles: []})",
                "db.runCommand({createUser: 'app', pwd***",
            ),
            ("db.auth('admin',\n  'hunter2", "db.auth('admin',***"),
            ("x({a: 'open\n pwd:\n 'p'", "x({a: 'open\n pwd***"),
            ("x('a, y.auth('u',\r\n 'hunter4'", "x('a, y.auth('u',***"),
        ]);
        for (text, secret) in [
            (
                "db.runCommand({createUser: 'app', pwd:\n  'S3cret!, roles: []})",
                "S3cret",
            ),
            ("db.auth('admin',\n  'hunter2", "hunter2"),
            ("x({a: 'open\n pwd:\n 'p'", "'p'"),
        ] {
            hidden(text, &[secret]);
        }
    }

    #[test]
    fn redacts_values_that_continue_past_a_bad_line() {
        for text in [
            "db.c.insertOne({pwd: 'a' +\n 'hunter2})",
            "db.auth('admin', 'a' +\n 'hunter2",
            "db.c.insertOne({pwd:\n 'a' +\n 'hunter2})",
            "{a: 'x', pwd: x.y\n.z('hunter2})",
            "db.c.insertOne({a: 'x', pwd\n: 'hunter2})",
            "db.c.insertOne({a: 'x', 'pwd'\n: 'hunter2})",
            "db.c.insertOne({a: 'x', pwd\n= 'hunter2})",
            "db.c.insertOne({a: 'x', pwd: f(\n'hunter2})",
            "db.auth\n('u', 'hunter2",
            "db.auth(\n'u'\n, 'hunter2",
            "x(`unterminated\nb' pwd: 'hunter2",
            "/* note\nsay \"x, pwd: \" + 'hunter2'",
            "db.c.insertOne({note: `Bob's\nit's ok', pwd: 'hunter2'})",
            "db.c.find({a: 1}); db.auth('u',\n'hunter2'); show dbs",
            "// pwd\n'hunter2",
        ] {
            hidden(text, &["hunter2"]);
        }
        // A stray apostrophe: it lexes, but does not parse.
        hidden(
            "db.c.insertOne({note: 'it's', pwd: 'S3cret's'})",
            &["S3cret"],
        );
    }

    #[test]
    fn use_inside_a_document_hides_nothing() {
        // `use` at a line start inside a document is a key, not a `use` statement.
        let text = "db.c.insertOne({a: 1,\nuse :'x',pwd:'hunter2'})";
        let r = MongoLanguage.redact(text);
        assert_eq!(r.text, "db.c.insertOne({a: 1,\nuse :'x',pwd:***})");
        let p = MongoLanguage.parse(text).unwrap();
        assert_eq!(
            (p.name.as_str(), p.class),
            ("insertOne", dnt_core::command::CommandClass::Write)
        );
        // A name the lexer refuses there is not taken as a `use` name either.
        hidden("db.c.insertOne({a: 1,\nuse 2024_x, pwd: 'p'})", &["'p'"]);
    }
}
