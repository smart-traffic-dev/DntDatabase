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

use super::{lex_script, Piece, Token, TokenKind};

/// What a secret is replaced with.
pub const MASK: &str = "***";

/// The redacted form of `text`.
pub fn redact(text: &str) -> Redacted {
    let mut ranges = Vec::new();
    for piece in lex_script(text) {
        match piece {
            Piece::Tokens(tokens) => token_ranges(text, &tokens, &mut ranges),
            Piece::Raw(from, to) => raw_ranges(text, from, to, &mut ranges),
        }
    }
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

/// Secret values (`key: value`, `key = value`) and auth passwords in the tokens of one
/// piece of `text`, as byte ranges. A value that runs to the end of the tokens (the
/// text after them does not lex, or nothing follows) is also scanned in the raw text.
fn token_ranges(text: &str, toks: &[Token], out: &mut Vec<(usize, usize)>) {
    let b = text.as_bytes();
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
            let second_end = if punct(toks, first_end, ',') {
                let end = arg_end(toks, first_end);
                push_tokens(toks, first_end + 1, end, out);
                end
            } else {
                first_end
            };
            if second_end >= toks.len() {
                raw_auth(b, toks[i + 1].start, out);
            }
        }
        let assign = punct(toks, i + 1, ':')
            || (punct(toks, i + 1, '=') && !punct(toks, i + 2, '=') && !punct(toks, i + 2, '>'));
        if is_secret(word) && assign {
            let end = value_end(toks, i + 2);
            push_tokens(toks, i + 2, end, out);
            if end >= toks.len() {
                raw_value_at(b, toks[i + 1].end, out);
            }
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

/// Past any whitespace, line breaks included (a value may start on the next line).
fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
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

/// Masks the value that starts after byte `from` (past whitespace and line breaks).
/// Returns where the value ends.
fn raw_value_at(b: &[u8], from: usize, out: &mut Vec<(usize, usize)>) -> usize {
    let v = skip_space(b, from);
    let (end, _) = raw_value(b, v);
    if end > v {
        out.push((v, end));
    }
    end
}

/// Masks the second argument of the call whose `(` is at byte `open`. Returns where the
/// scan ended.
fn raw_auth(b: &[u8], open: usize, out: &mut Vec<(usize, usize)>) -> usize {
    let (_, stop) = raw_value(b, skip_space(b, open + 1));
    let stop = skip_space(b, stop);
    if b.get(stop) == Some(&b',') {
        raw_value_at(b, stop + 1, out)
    } else {
        stop
    }
}

/// The keys and auth calls that start in `text[from..to]`, scanned without tokens; a
/// value may run past `to`.
fn raw_ranges(text: &str, from: usize, to: usize, out: &mut Vec<(usize, usize)>) {
    let b = text.as_bytes();
    let to = to.min(b.len());
    let mut i = from;
    while i < to {
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
        let open = skip_space(b, i);
        if is_auth_call(word) && start > 0 && b[start - 1] == b'.' && b.get(open) == Some(&b'(') {
            i = i.max(raw_auth(b, open, out));
        } else if is_secret(word) {
            // `key:`, `'key':`, `key =` (not `==` / `=>`).
            let mut j = i;
            if matches!(b.get(j), Some(b'\'' | b'"' | b'`')) {
                j += 1;
            }
            j = skip_space(b, j);
            let assign = match (b.get(j), b.get(j + 1)) {
                (Some(b':'), _) => true,
                (Some(b'='), next) => !matches!(next, Some(b'=' | b'>')),
                _ => false,
            };
            if assign {
                i = i.max(raw_value_at(b, j + 1, out));
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
            // A value on the line after its key or comma, where the text stops lexing.
            (
                "db.runCommand({createUser: 'app', pwd:\n  'S3cret!, roles: []})",
                "db.runCommand({createUser: 'app', pwd:\n  ***",
            ),
            ("db.auth('admin',\n  'hunter2", "db.auth('admin',\n  ***"),
            ("x({a: 'open\n pwd:\n 'p'", "x({a: 'open\n pwd:\n ***"),
            ("x('a, pwd:\n 'hunter3')", "x('a, pwd:\n ***)"),
            ("x('a, y.auth('u',\r\n 'hunter4'", "x('a, y.auth('u',\r\n ***"),
        ]);
        for (text, secret) in [
            (
                "db.runCommand({createUser: 'app', pwd:\n  'S3cret!, roles: []})",
                "S3cret",
            ),
            ("db.auth('admin',\n  'hunter2", "hunter2"),
            ("x({a: 'open\n pwd:\n 'p'", "'p'"),
        ] {
            assert!(!MongoLanguage.redact(text).text.contains(secret), "{text}");
        }
    }

    #[test]
    fn use_inside_a_document_hides_nothing() {
        // `use` at a line start inside a document is a key, not a `use` statement.
        let text = "db.c.insertOne({a: 1,\nuse :'x',pwd:'hunter2'})";
        let r = MongoLanguage.redact(text);
        assert_eq!(r.text, "db.c.insertOne({a: 1,\nuse :'x',pwd:***})");
        assert!(matches!(
            MongoLanguage.parse(text).unwrap().class,
            dnt_core::command::CommandClass::Write
        ));
        assert_eq!(MongoLanguage.parse(text).unwrap().name, "insertOne");
        // A name the lexer refuses there is not taken as a `use` name either.
        let text = "db.c.insertOne({a: 1,\nuse 2024_x, pwd: 'p'})";
        assert!(!MongoLanguage.redact(text).text.contains("'p'"));
    }
}
