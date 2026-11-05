//! Redaction (spec §4.5): the form of a script that is safe to keep in History, logs and
//! Favorites. Secrets are replaced with `***`, in two tiers, per command of
//! [`split`](super::parse::split):
//!
//! - **Precise**, for a command that lexes and parses. Masked: the value of every key
//!   whose (decoded) name holds `pwd`, `password` or `secret` (any case, at any depth:
//!   `ldapQueryPassword`, `client_secret`), and every string or regex whose decoded value
//!   holds a `mongodb://` / `mongodb+srv://` URI with a password (a `:` between `//` and
//!   the first `@`, whatever `/`, `?` or `#` come before it) or with user info (an `@`
//!   before the first `/`, `?` or `#`). The password of a URI in the raw text is also
//!   masked, from that `:` up to the last `@` before whitespace or a quote. Comments
//!   around and between the tokens are coarse.
//! - **Coarse**, for everything else. That is a run of commands that do not parse,
//!   together with the text around it up to the commands that parse on either side
//!   (comments); the text between parsed commands; and comments inside a parsed command.
//!   A region that holds any marker is replaced
//!   WHOLE by `***`; one without stays as it is. Markers, in any case:
//!   - `pwd`, `password` and `secret` anywhere;
//!   - `auth` as a whole word (`db.auth`, `db . auth`, `db['auth']`);
//!   - `mongodb`;
//!   - any backslash: an escape or a line continuation can spell a marker (`p\u0077d`,
//!     `p\wd`).
//!
//!   Such commands are blocked by the guard, so nothing useful is lost.
//!
//! Never panics.

use dnt_core::command::CommandLanguage;
use dnt_core::redact::Redacted;

use super::parse::{split, use_range};
use super::{lex_script, MongoLanguage, Piece, Token, TokenKind};

/// What a secret is replaced with.
pub const MASK: &str = "***";

/// The redacted form of `text`.
pub fn redact(text: &str) -> Redacted {
    let mut ranges = Vec::new();
    // The end of the last command, and where the current coarse region began: the end
    // of the last command that parsed (so the region holds the comments around a run of
    // bad commands: `mongodb://…` outside a string lexes as `mongodb:` and a comment).
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

const SCHEMES: [&str; 2] = ["mongodb://", "mongodb+srv://"];

/// Every `mongodb://` / `mongodb+srv://` URI in `s` (any case), as the byte range of the
/// text after `//` up to whitespace or a `stop` character.
fn uri_stretches(s: &str, stop: &[u8]) -> Vec<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let b = s.as_bytes();
    let mut out = Vec::new();
    for scheme in SCHEMES {
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|r| r.find(scheme)) {
            let begin = from + i + scheme.len();
            let mut end = begin;
            while end < b.len() && !b[end].is_ascii_whitespace() && !stop.contains(&b[end]) {
                end += 1;
            }
            out.push((begin, end));
            from = begin;
        }
    }
    out
}

/// The password of the URI whose text after `//` is `s[begin..end]`: from just after the
/// first `:` to the LAST `@` (a password may hold `@`: `app:p@ss@h`). A URI has one when
/// the text before its first `@` holds a `:`, whatever `/`, `?` or `#` come before that
/// `@` (`u:pa/ss@h`, unencoded); `h/x?appName=a@b` has none.
fn password(s: &str, begin: usize, end: usize) -> Option<(usize, usize)> {
    let b = &s.as_bytes()[begin..end];
    let first_at = b.iter().position(|&c| c == b'@')?;
    let colon = b[..first_at].iter().position(|&c| c == b':')?;
    let last_at = b.iter().rposition(|&c| c == b'@')?;
    Some((begin + colon + 1, begin + last_at))
}

/// A `mongodb://` / `mongodb+srv://` URI with a password, or with user info (an `@`
/// before the first `/`, `?` or `#`: `mongodb://u@h`), in a decoded string.
fn has_uri_with_user_info(s: &str) -> bool {
    uri_stretches(s, &[]).into_iter().any(|(begin, end)| {
        password(s, begin, end).is_some()
            || s.as_bytes()[begin..end]
                .iter()
                .take_while(|&&c| !matches!(c, b'/' | b'?' | b'#'))
                .any(|&c| c == b'@')
    })
}

/// The source of a regex as the text it matches, near enough to find a URI in it: `\/`
/// is `/`, and `\x40` and `\u{40}` are `@`.
fn regex_text(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut it = pattern.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let hex = |it: &mut std::iter::Peekable<std::str::Chars>, n: usize| {
            let digits: String = (0..n)
                .filter_map(|_| it.next_if(char::is_ascii_hexdigit))
                .collect();
            u32::from_str_radix(&digits, 16)
                .ok()
                .and_then(char::from_u32)
        };
        match it.next() {
            Some('x') => out.extend(hex(&mut it, 2)),
            Some('u') if it.next_if_eq(&'{').is_some() => {
                out.extend(hex(&mut it, 6));
                it.next_if_eq(&'}');
            }
            Some('u') => out.extend(hex(&mut it, 4)),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------
// Precise: a command that parses

/// The secrets of the parsed command `span` (at byte `start` of `text`), and the
/// comments around and between its tokens (coarse).
fn precise(text: &str, start: usize, span: &str, out: &mut Vec<(usize, usize)>) {
    // `use` takes its name raw (`use 2024_pwd_logs`, `use a'b`): what is around the
    // keyword and the name is comments, read as the parser reads them.
    if let Some((kw, end)) = use_range(span) {
        coarse(text, start, start + kw, out);
        coarse(text, start + end, start + span.len(), out);
        return;
    }
    let mut pieces = lex_script(span);
    let toks = match (pieces.pop(), pieces.is_empty()) {
        (Some(Piece::Tokens(toks)), true) if !toks.is_empty() => toks,
        _ => {
            coarse(text, start, start + span.len(), out);
            return;
        }
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
        match &t.kind {
            TokenKind::Ident(key) | TokenKind::Str(key)
                if is_secret(key) && punct(&toks, i + 1, ':') && i + 2 < toks.len() =>
            {
                let end = value_end(&toks, i + 2);
                out.push((toks[i + 2].start, toks[end - 1].end));
            }
            // String values are decoded: `mongodb:\/\/u:p\x40h` is a URI too.
            TokenKind::Str(value) if has_uri_with_user_info(value) => {
                out.push((t.start, t.end));
            }
            TokenKind::Regex { pattern, .. } if has_uri_with_user_info(&regex_text(pattern)) => {
                out.push((t.start, t.end));
            }
            _ => {}
        }
    }
    // Comments: before the first token, between tokens, after the last one.
    let mut at = start;
    for t in &toks {
        coarse(text, at, t.start, out);
        at = t.end;
    }
    coarse(text, at, start + span.len(), out);
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

/// Masks the whole of `text[from..to]` when it holds a marker (but the whitespace and
/// `;` at its edges, which hold no secret).
fn coarse(text: &str, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    let Some(region) = text.get(from..to) else {
        return;
    };
    if has_marker(region) {
        let edge = |c: char| c.is_whitespace() || c == ';';
        let start = from + (region.len() - region.trim_start_matches(edge).len());
        let end = from + region.trim_end_matches(edge).len();
        if start < end {
            out.push((start, end));
        }
    }
}

/// Whether `s` holds a secret marker (see the module docs).
fn has_marker(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if lower.contains('\\')
        || lower.contains("mongodb")
        || ["pwd", "password", "secret"]
            .iter()
            .any(|k| lower.contains(k))
    {
        return true;
    }
    let b = lower.as_bytes();
    let mut from = 0;
    while let Some(i) = lower.get(from..).and_then(|r| r.find("auth")) {
        let i = from + i;
        let e = i + "auth".len();
        let whole = (i == 0 || !is_word(b[i - 1])) && !b.get(e).is_some_and(|&c| is_word(c));
        if whole {
            return true;
        }
        from = e;
    }
    false
}

// ---------------------------------------------------------------------------------
// URIs in the raw text

/// The password ([`password`]) of every `mongodb://` / `mongodb+srv://` URI written out
/// in the text. A URI also ends at a quote, which ends the string holding it.
fn uri_ranges(text: &str, out: &mut Vec<(usize, usize)>) {
    for (begin, end) in uri_stretches(text, b"'\"`") {
        out.extend(password(text, begin, end));
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
            // An escaped key is read decoded.
            (
                "db.c.insertOne({'p\\u0077d': 'hunter2'})",
                "db.c.insertOne({'p\\u0077d': ***})",
            ),
            // Nothing to redact: the text is kept byte for byte.
            ("db.c.find({name: 'pwd'})", "db.c.find({name: 'pwd'})"),
            ("db.c.find({auth: 1})", "db.c.find({auth: 1})"),
            ("use 2024_logs\nshow dbs", "use 2024_logs\nshow dbs"),
            // A comment with a marker is masked whole.
            (
                "db.c.find({name: 'x'}) // password: none\nshow dbs // plain",
                "db.c.find({name: 'x'}) ***\nshow dbs // plain",
            ),
            (
                "db.c.find({a: 1 /* pwd: x */, b: 2})",
                "db.c.find({a: 1 ***, b: 2})",
            ),
            // A command that does not parse, with a marker: masked whole.
            ("db.c.insertOne({secret: 'a' + 'b', n: 1})", "***"),
            ("db.auth('admin', 'hunter2')", "***"),
            (
                "db.getSiblingDB('admin').changeUserPassword('u', \"n3w\" )",
                "***",
            ),
            ("db.auth({user: 'u', pwd: 'p'})", "***"),
            ("db.auth('u')", "***"),
            // A parsed command after a bad one is precise again.
            (
                "db.auth('u', 'p')\ndb.c.insertOne({pwd: 'q', n: 1})",
                "***\ndb.c.insertOne({pwd: ***, n: 1})",
            ),
            // Without a marker, a bad command is kept.
            ("db.c.find({a: 'open", "db.c.find({a: 'open"),
        ]);
    }

    #[test]
    fn redacts_uri_password_with_at_sign() {
        check(&[
            // In a parsed command: the whole string.
            (
                "db.c.insertOne({uri: 'mongodb://app:p@ss@db.example.com:27017/x?tls=true'})",
                "db.c.insertOne({uri: ***})",
            ),
            (
                "db.c.insertOne({a: 'MONGODB+SRV://u:x@h', b: 'mongodb://v@h2', n: 1})",
                "db.c.insertOne({a: ***, b: ***, n: 1})",
            ),
            // No user info: unchanged.
            (
                "db.c.insertOne({u: 'mongodb://h:27017/db'})",
                "db.c.insertOne({u: 'mongodb://h:27017/db'})",
            ),
            // Not a command: masked whole.
            (
                "mongodb+srv://u:s3cr@t@cluster0.example.net/?retryWrites=true",
                "***",
            ),
            ("mongodb://h:27017/db", "***"),
        ]);
        // Escapes and line continuations are decoded before the URI is looked for.
        for text in [
            "db.c.insertOne({u: 'mongodb:\\/\\/u:hunter2@h'})",
            "db.c.insertOne({u: 'mongodb://u:hunter2\\x40h'})",
            "db.c.insertOne({u: 'mongodb://u:hunter2\\u0040h'})",
            "db.c.insertOne({u: '\\x6dongodb://u:hunter2@h'})",
            "db.c.insertOne({u: 'mongodb\\x3a//u:hunter2@h'})",
            "db.c.insertOne({u: 'mongodb://u:hunter2\\\n@h'})",
        ] {
            assert!(MongoLanguage.parse(text).is_ok(), "{text}");
            assert_eq!(
                MongoLanguage.redact(text).text,
                "db.c.insertOne({u: ***})",
                "{text}"
            );
        }
    }

    #[test]
    fn redacts_unparsable_lines() {
        // Every one of these does not parse and holds a marker: nothing is left.
        for text in [
            "db.createUser({user: 'u', pwd: 'hunter2",
            "db.auth('u', 'hunter2",
            "db.changeUserPassword('u', \"n3w",
            "db.c.insertOne({password: \"a,b\", note: 'open",
            "{secret: `multi\nline` 'x",
            "x = {'Password' : abc def, y: 1} 'oops",
            "db.createUser({\n  user: 'u',\n  pwd: 'p1'\n})\ndb.auth('u', 'p2\nmongodb://u:p3@h",
            "db.runCommand({createUser: 'app', pwd:\n  'S3cret!, roles: []})",
            "db.auth('admin',\n  'hunter2",
            "x({a: 'open\n pwd:\n 'p'",
            "x('a, y.auth('u',\r\n 'hunter4'",
        ] {
            check(&[(text, "***")]);
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
            "db.c.insertOne({pwd: 'a\\\nhunter2'})",
            "db.c.insertOne({pwd: 'a\\\nhunter2",
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
    fn redacts_disguised_markers() {
        for text in [
            "db. auth('admin', 'hunter2')",
            "db.\nauth('admin', 'hunter2')",
            "db./*x*/auth('admin', 'hunter2')",
            "db['auth']('admin', 'hunter2')",
            "db[\"auth\"]('admin', 'hunter2')",
            "db.getSiblingDB('admin')['auth']('u', 'hunter2')",
            "db.AUTH('u', 'hunter2')",
            "db.c.insertOne({'p\\u0077d': 'hunter2",
            "db.c.insertOne({a: 1, \\u0070wd: 'hunter2'})",
            "db.c.insertOne({'p\\wd': 'hunter2",
            "db.c.insertOne({'p\\\nwd': 'hunter2",
            "var pw = 'hunter2'; db.createUser({user: 'u', pwd: pw, roles: []})",
        ] {
            hidden(text, &["hunter2"]);
        }
        // The command before the bad one is kept.
        check(&[
            (
                "db.c.find({a: 1});db['auth']('u','hunter2')",
                "db.c.find({a: 1});***",
            ),
            (
                "db.c.insertOne({a: 1})\ndb . auth('u', 'hunter2')\nshow dbs",
                "db.c.insertOne({a: 1})\n***\nshow dbs",
            ),
            // `auth` only as a whole word.
            ("db.c.find({author: 'open", "db.c.find({author: 'open"),
        ]);
    }

    #[test]
    fn redacts_comments_at_the_edges_of_a_command() {
        // Comments before the first token and after the last one of a command are read
        // too (here the bad region is one command, so they are coarse).
        for text in [
            "'}\\\ndb.c.find()//db.c.insertOne({pwd: 'HUNTER9'})})\n\"",
            "/*)\r/*/*db.c.insertOne({pwd: 'HUNTER9'})\":/*/db.d.drop()",
            "use 2024_pwd_logs'oopsa\u{301}0x //*/0xdb.db.changeUserPassword('u', 'HUNTER9')\
             db.c.find({u: 'MONGODB+SRV://u:HUNTER9@h'}) x",
        ] {
            hidden(text, &["HUNTER9"]);
        }
        check(&[
            ("/* pwd: x */ use test", "*** use test"),
            ("use test // secret: x", "use test ***"),
            ("use test; /* plain */", "use test; /* plain */"),
        ]);
    }

    #[test]
    fn uri_user_info_is_in_the_authority() {
        check(&[
            // An `@` after the host is not user info.
            (
                "db.c.find({a: 'see mongodb://h/db or mail u@x.com'})",
                "db.c.find({a: 'see mongodb://h/db or mail u@x.com'})",
            ),
            (
                "db.c.find({a: 'mongodb://h/x?appName=a@b'})",
                "db.c.find({a: 'mongodb://h/x?appName=a@b'})",
            ),
            (
                "db.c.find({a: 'mongodb://h#x@y'})",
                "db.c.find({a: 'mongodb://h#x@y'})",
            ),
            // The raw-text rule stops at the quote that ends the string.
            (
                "db.c.insertOne({a:'mongodb://h:1/x',b:'u@y'})",
                "db.c.insertOne({a:'mongodb://h:1/x',b:'u@y'})",
            ),
            // User info is masked, in any URI of the string.
            (
                "db.c.insertOne({u: 'mongodb://u:p@h'})",
                "db.c.insertOne({u: ***})",
            ),
            (
                "db.c.insertOne({u: 'mongodb://h/x mongodb://u:p@h2'})",
                "db.c.insertOne({u: ***})",
            ),
            (
                "db.c.insertOne({u: 'mongodb://app:p@ss@h:1/x?a=b@c'})",
                "db.c.insertOne({u: ***})",
            ),
            // A `:` before the first `@` is a password, whatever `/`, `?` or `#` come
            // before that `@` (unencoded in the password).
            (
                "db.c.insertOne({u: 'mongodb://u:pa/ss@h'})",
                "db.c.insertOne({u: ***})",
            ),
            (
                "db.c.insertOne({u: 'mongodb://u:pa?ss@h'})",
                "db.c.insertOne({u: ***})",
            ),
            (
                "db.c.insertOne({u: 'mongodb://u:pa#ss@h'})",
                "db.c.insertOne({u: ***})",
            ),
            (
                "db.c.insertOne({u: 'mongodb+srv://u:hunter2/x@h/db'})",
                "db.c.insertOne({u: ***})",
            ),
            (
                "db.c.find({a: /mongodb:\\/\\/u:hunter2\\/x@h/})",
                "db.c.find({a: ***})",
            ),
            // Accepted by the ruling: a port before an `@` in the query counts.
            (
                "db.c.insertOne({u: 'mongodb://h:27017/x?appName=a@b'})",
                "db.c.insertOne({u: ***})",
            ),
        ]);
        // In the raw text (a comment here, `mongodb` absent): from the `:` to the last
        // `@` before whitespace or a quote.
        let mut ranges = Vec::new();
        let text = "x mongodb://app:p/@ss@h/db y 'mongodb://u:pa?ss@h' mongodb://h/x?a=b@c";
        super::uri_ranges(text, &mut ranges);
        let masked: Vec<&str> = ranges.iter().map(|&(a, b)| &text[a..b]).collect();
        assert_eq!(masked, ["p/@ss", "pa?ss"]);
    }

    #[test]
    fn redacts_uri_in_a_regex() {
        check(&[
            (
                "db.c.find({a: /mongodb:\\/\\/u:hunter2@h/})",
                "db.c.find({a: ***})",
            ),
            (
                "db.c.find({a: /mongodb:\\/\\/u:hunter2\\x40h/i})",
                "db.c.find({a: ***})",
            ),
            (
                "db.c.find({a: /mongodb:\\/\\/u:hunter2\\u{40}h/u})",
                "db.c.find({a: ***})",
            ),
            (
                "db.c.find({a: /mongodb:\\/\\/h\\/db/})",
                "db.c.find({a: /mongodb:\\/\\/h\\/db/})",
            ),
        ]);
    }

    #[test]
    fn use_name_is_not_a_marker() {
        check(&[
            ("use 2024_pwd_logs", "use 2024_pwd_logs"),
            ("use 2024_pwd_logs\nshow dbs", "use 2024_pwd_logs\nshow dbs"),
            ("use 'secret_db';", "use 'secret_db';"),
            ("use 2024_pwd_logs // pwd", "use 2024_pwd_logs ***"),
        ]);
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
