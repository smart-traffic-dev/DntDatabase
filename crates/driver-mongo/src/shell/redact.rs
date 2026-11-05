//! Redaction (spec §4.5): the form of a command that is safe to keep in History, logs
//! and Favorites. Masked with `***`:
//!
//! - the value of every key whose name holds `pwd`, `password` or `secret` (any case, at
//!   any depth: `ldapQueryPassword`, `client_secret`);
//! - the password of a `mongodb://` / `mongodb+srv://` URI, up to the last `@` before the
//!   host, so a password holding `@` is masked whole;
//! - the second argument of `….auth(…)` and `….changeUserPassword(…)`.
//!
//! Text that does not lex is scanned without tokens, from the line it fails on, for the
//! same keys and calls; URIs are always scanned in the raw text. Never panics.

use dnt_core::redact::Redacted;

use super::{lex_script, Token, TokenKind};

/// What a secret is replaced with.
pub const MASK: &str = "***";

/// The redacted form of `text`.
pub fn redact(text: &str) -> Redacted {
    let mut ranges = Vec::new();
    let (tokens, cut) = lex_script(text);
    token_ranges(&tokens, &mut ranges);
    raw_ranges(text, cut, &mut ranges);
    uri_ranges(text, &mut ranges);
    apply(text, ranges)
}

fn is_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("pwd") || key.contains("password") || key.contains("secret")
}

fn is_auth_call(name: &str) -> bool {
    name.eq_ignore_ascii_case("auth") || name.eq_ignore_ascii_case("changeUserPassword")
}

// ---------------------------------------------------------------------------------
// Tokens

fn punct(toks: &[Token], i: usize, c: char) -> bool {
    matches!(toks.get(i).map(|t| &t.kind), Some(TokenKind::Punct(p)) if *p == c)
}

/// Secret values (`key: value`, `key = value`) and auth passwords, as byte ranges.
fn token_ranges(toks: &[Token], out: &mut Vec<(usize, usize)>) {
    for (i, t) in toks.iter().enumerate() {
        let (TokenKind::Ident(word) | TokenKind::Str(word)) = &t.kind else {
            continue;
        };
        // Checked first and on its own: `changeUserPassword` is also a secret-looking word.
        let call = matches!(t.kind, TokenKind::Ident(_))
            && is_auth_call(word)
            && i > 0
            && punct(toks, i - 1, '.')
            && punct(toks, i + 1, '(');
        if call {
            let first_end = arg_end(toks, i + 1);
            if punct(toks, first_end, ',') {
                push_tokens(toks, first_end + 1, arg_end(toks, first_end), out);
            }
        }
        let assign = punct(toks, i + 1, ':')
            || (punct(toks, i + 1, '=') && !punct(toks, i + 2, '=') && !punct(toks, i + 2, '>'));
        if is_secret(word) && assign {
            push_tokens(toks, i + 2, value_end(toks, i + 2), out);
        }
    }
}

/// The bytes of `toks[from..to]`, when that is not empty.
fn push_tokens(toks: &[Token], from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    if from < to && to <= toks.len() {
        out.push((toks[from].start, toks[to - 1].end));
    }
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

/// The `,`, closing bracket or `;` that ends the argument after `open` (the token before
/// its first one), or the end.
fn arg_end(toks: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open + 1) {
        match t.kind {
            TokenKind::Punct('(' | '[' | '{') => depth += 1,
            TokenKind::Punct(')' | ']' | '}') if depth == 0 => return i,
            TokenKind::Punct(')' | ']' | '}') => depth -= 1,
            TokenKind::Punct(',') if depth == 0 => return i,
            TokenKind::Punct(';') => return i,
            _ => {}
        }
    }
    toks.len()
}

/// Just past the value that starts at `i`: a literal, a call, a bracketed value, and any
/// member access, call or arithmetic that continues it.
fn value_end(toks: &[Token], mut i: usize) -> usize {
    let p = |i: usize| match toks.get(i).map(|t| &t.kind) {
        Some(TokenKind::Punct(c)) => Some(*c),
        _ => None,
    };
    loop {
        // Prefix operators and `new`.
        while matches!(p(i), Some('-' | '+' | '!' | '~'))
            || matches!(toks.get(i).map(|t| &t.kind), Some(TokenKind::Ident(w)) if w == "new")
        {
            i += 1;
        }
        i = match toks.get(i).map(|t| &t.kind) {
            None => return toks.len(),
            Some(TokenKind::Punct('(' | '[' | '{')) => closing(toks, i),
            Some(TokenKind::Punct(_)) => return i,
            Some(TokenKind::Ident(_)) if p(i + 1) == Some('(') => closing(toks, i + 1),
            Some(_) => i + 1,
        };
        while matches!(p(i), Some('(' | '[')) {
            i = closing(toks, i);
        }
        match p(i) {
            Some('.' | '+' | '-' | '*' | '/' | '%') => i += 1,
            _ => return i,
        }
    }
}

// ---------------------------------------------------------------------------------
// Raw text (what does not lex)

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn skip_blanks(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    i
}

/// Just past the quote that closes the one at `at`; `'` and `"` strings end at the end
/// of their line when unterminated, templates at the end of the text.
fn skip_quoted(b: &[u8], at: usize) -> usize {
    let q = b[at];
    let mut k = at + 1;
    while k < b.len() {
        match b[k] {
            b'\\' => k += 2,
            c if c == q => return k + 1,
            b'\n' if q != b'`' => return k,
            _ => k += 1,
        }
    }
    b.len()
}

/// A value from `from`: up to a `,`, `;`, closing bracket or line end outside brackets
/// and quotes. Returns its end without trailing blanks, and where the scan stopped.
fn raw_value(b: &[u8], from: usize) -> (usize, usize) {
    let mut depth = 0usize;
    let mut k = from;
    while k < b.len() {
        match b[k] {
            b'\'' | b'"' | b'`' => {
                k = skip_quoted(b, k);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => break,
            b')' | b']' | b'}' => depth -= 1,
            b',' | b';' | b'\n' if depth == 0 => break,
            _ => {}
        }
        k += 1;
    }
    let stop = k.min(b.len());
    let mut end = stop;
    while end > from && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    (end, stop)
}

/// The keys and auth calls of `text[from..]`, scanned without tokens.
fn raw_ranges(text: &str, from: usize, out: &mut Vec<(usize, usize)>) {
    let b = text.as_bytes();
    let mut i = from;
    while i < b.len() {
        if !is_word(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        // ASCII bytes on both sides: char boundaries.
        let word = &text[start..i];
        // A call first: `changeUserPassword` is also a secret-looking word.
        let open = skip_blanks(b, i);
        let value_from = if is_auth_call(word)
            && start > 0
            && b[start - 1] == b'.'
            && b.get(open) == Some(&b'(')
        {
            let (_, stop) = raw_value(b, skip_blanks(b, open + 1));
            (b.get(stop) == Some(&b',')).then_some(stop + 1)
        } else if is_secret(word) {
            // `key:`, `'key':`, `key =` (not `==` / `=>`).
            let mut j = i;
            if matches!(b.get(j), Some(b'\'' | b'"' | b'`')) {
                j += 1;
            }
            j = skip_blanks(b, j);
            match (b.get(j), b.get(j + 1)) {
                (Some(b':'), _) => Some(j + 1),
                (Some(b'='), next) if !matches!(next, Some(b'=' | b'>')) => Some(j + 1),
                _ => None,
            }
        } else {
            None
        };
        if let Some(v) = value_from {
            let v = skip_blanks(b, v);
            let (end, _) = raw_value(b, v);
            if end > v {
                out.push((v, end));
                i = i.max(end);
            }
        }
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

    #[test]
    fn redacts_password_keys_at_any_depth() {
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
                "db.c.insertOne({'client_secret': NumberLong(5), \"MY_PWD\": {a: 1, b: [2]}, n: 1})",
                "db.c.insertOne({'client_secret': ***, \"MY_PWD\": ***, n: 1})",
            ),
            (
                "db.c.insertOne({secret: 'a' + 'b', n: 1})",
                "db.c.insertOne({secret: ***, n: 1})",
            ),
            (
                "db.c.insertOne({\n  user: 'u',\n  pwd: 'p',\n})\ndb.c.find()",
                "db.c.insertOne({\n  user: 'u',\n  pwd: ***,\n})\ndb.c.find()",
            ),
            // db.auth / db.changeUserPassword: the second argument.
            ("db.auth('admin', 'hunter2')", "db.auth('admin', ***)"),
            (
                "db.getSiblingDB('admin').changeUserPassword('u', \"n3w\" )",
                "db.getSiblingDB('admin').changeUserPassword('u', *** )",
            ),
            (
                "db.auth({user: 'u', pwd: 'p'})",
                "db.auth({user: 'u', pwd: ***})",
            ),
            // Nothing to redact: the text is kept byte for byte.
            (
                "db.c.find({name: 'pwd'}) // password: none",
                "db.c.find({name: 'pwd'}) // password: none",
            ),
            ("db.auth('u')", "db.auth('u')"),
            ("use 2024_logs\nshow dbs", "use 2024_logs\nshow dbs"),
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
                "mongodb+srv://u:s3cr@t@cluster0.example.net/?retryWrites=true",
                "mongodb+srv://u:***@cluster0.example.net/?retryWrites=true",
            ),
            ("MONGODB://u:x@h", "MONGODB://u:***@h"),
            (
                "a mongodb://a:1@h b mongodb+srv://b:2@h2",
                "a mongodb://a:***@h b mongodb+srv://b:***@h2",
            ),
            // No password: unchanged.
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
                "db.createUser({user: 'u', pwd: ***",
            ),
            ("db.auth('u', 'hunter2", "db.auth('u', ***"),
            (
                "db.changeUserPassword('u', \"n3w",
                "db.changeUserPassword('u', ***",
            ),
            (
                "db.c.insertOne({password: \"a,b\", note: 'open",
                "db.c.insertOne({password: ***, note: 'open",
            ),
            ("{secret: `multi\nline` 'x", "{secret: ***"),
            ("x = {'Password' : abc def, y: 1} 'oops", "x = {'Password' : ***, y: 1} 'oops"),
            // The lines before the bad one are read as tokens; the URI anywhere.
            (
                "db.createUser({\n  user: 'u',\n  pwd: 'p1'\n})\ndb.auth('u', 'p2\nmongodb://u:p3@h",
                "db.createUser({\n  user: 'u',\n  pwd: ***\n})\ndb.auth('u', ***\nmongodb://u:***@h",
            ),
            (
                "use 2024logs\ndb.c.insertOne({pwd: 'x'})",
                "use 2024logs\ndb.c.insertOne({pwd: ***})",
            ),
            ("db.c.find({a: 'open", "db.c.find({a: 'open"),
        ]);
    }
}
