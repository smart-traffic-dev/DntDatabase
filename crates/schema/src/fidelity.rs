//! The fidelity check (spec §5): every element of a draft built by [`crate::from_detail`]
//! is rendered with the same fragment functions as `render_create` and compared with
//! the server's own definition of it; whatever differs is locked, so the designer shows
//! it and never touches it.
//!
//! MySQL / MariaDB compare against the lines of `SHOW CREATE TABLE`; PostgreSQL against
//! `format_type` / `pg_get_expr` (column type and default), `pg_get_indexdef` and
//! `pg_get_constraintdef` (the `definition` fields of the detail).
//!
//! Both sides are normalised before the comparison: whitespace and the spacing of
//! punctuation, keyword case, string literal escapes (`''` and `\'` read the same),
//! PostgreSQL identifier quotes that change nothing. MySQL lines also drop what the
//! server adds or leaves out by itself: `DEFAULT NULL` / `NULL` on nullable columns,
//! `CHARACTER SET` / `COLLATE` equal to the table's, integer display widths equal to
//! the server default for the type (`int(11)`, `int(10) unsigned`; `tinyint(1)` stays)
//! and `year(4)` (5.7, MariaDB), `USING BTREE`, `RESTRICT` / `NO ACTION` referential actions
//! (the same in InnoDB), the quotes around numeric defaults (MariaDB prints them bare,
//! render always quotes, ruling T7-3) and MariaDB's `current_timestamp()` parentheses.
//! A `/*!NNNNN … */` version comment counts as its content.
//!
//! Fragments are rendered with `Dialect::for_kind` (MySQL: default `sql_mode` lexing,
//! ruling T7-1): literal escapes are normalised away, so the session's own flags do
//! not change the result. `SHOW CREATE TABLE` must be read without `ANSI_QUOTES`;
//! otherwise every element locks (the safe side).

use crate::from_detail::{is_cast_suffix, unquote};
use crate::model::*;
use crate::render::{check_sql, column_sql, fk_sql, index_sql, primary_key_sql, RenderError};
use crate::types::{default_display_width, type_text};
use dnt_core::error::DbError;
use dnt_core::meta::{ColumnDetail, IndexDetail, TableDetail};
use dnt_core::server::{ServerInfo, SqlKind};
use dnt_core::sql::ident::quote_ident;
use dnt_core::sql::Dialect;
use serde::{Deserialize, Serialize};
use specta::Type;

/// An element the designer must not touch, with why and the server's own text for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Locked {
    pub element: ElementRef,
    pub reason: String,
    pub server_text: String,
}

const MISMATCH: &str = "DntDatabase cannot reproduce this definition exactly";
const NOT_EDITABLE: &str = "The designer cannot edit this definition.";
const FULLTEXT: &str = "FULLTEXT and SPATIAL indexes cannot be edited in the designer.";
const NO_DDL: &str = "The table definition could not be read from the server.";
const UNKNOWN: &str = "The table has a definition the designer cannot show.";

/// The locks for `def` (built by [`crate::from_detail`] from `detail`), compared with
/// the server's DDL: `mysql_create` is the `SHOW CREATE TABLE` text (MySQL / MariaDB;
/// `None` there makes the table read-only), unused on PostgreSQL.
///
/// Elements already locked by `from_detail` are reported too (with their server text),
/// so every locked element has a reason. A MySQL line no draft element matches is
/// reported for an element [`apply_locks`] adds (index, check) or locks the table.
///
/// `Unsupported` for a non-SQL server.
pub fn fidelity(
    def: &TableDef,
    detail: &TableDetail,
    mysql_create: Option<&str>,
    server: &ServerInfo,
) -> Result<Vec<Locked>, DbError> {
    let kind = server.kind.require_sql()?;
    let d = Dialect::for_kind(kind);
    Ok(match kind {
        SqlKind::MySql => mysql_fidelity(def, mysql_create, server, d),
        SqlKind::Postgres => pg_fidelity(def, detail, server, d),
    })
}

/// Marks the locked elements of `def`; an index or check the draft lacks is added from
/// its server text (locked), and a check whose text cannot be read makes the table
/// read-only, as does [`ElementRef::Table`]. Never unlocks anything and keeps an
/// existing `read_only_reason`.
pub fn apply_locks(def: &mut TableDef, locks: &[Locked]) {
    fn is(original: &Option<String>, name: &str, want: &str) -> bool {
        original.as_deref().unwrap_or(name) == want
    }
    for l in locks {
        match &l.element {
            ElementRef::Table => {
                if def.read_only_reason.is_none() {
                    def.read_only_reason = Some(l.reason.clone());
                }
            }
            ElementRef::Column(n) => {
                for c in def
                    .columns
                    .iter_mut()
                    .filter(|c| is(&c.original, &c.name, n))
                {
                    c.locked = true;
                }
            }
            ElementRef::PrimaryKey => {
                if let Some(k) = &mut def.primary_key {
                    k.locked = true;
                }
            }
            ElementRef::Index(n) => {
                match def.indexes.iter_mut().find(|i| is(&i.original, &i.name, n)) {
                    Some(i) => i.locked = true,
                    None => def.indexes.push(index_from_line(n, &l.server_text)),
                }
            }
            ElementRef::ForeignKey(n) => {
                for f in def
                    .foreign_keys
                    .iter_mut()
                    .filter(|f| is(&f.original, &f.name, n))
                {
                    f.locked = true;
                }
            }
            ElementRef::Check(n) => {
                match def.checks.iter_mut().find(|c| is(&c.original, &c.name, n)) {
                    Some(c) => c.locked = true,
                    None => match check_expression(&l.server_text) {
                        Some(expression) => def.checks.push(CheckDef {
                            original: Some(n.clone()),
                            name: n.clone(),
                            expression,
                            locked: true,
                        }),
                        // It cannot be shown, so nothing on this table may be edited.
                        None => {
                            if def.read_only_reason.is_none() {
                                def.read_only_reason = Some(UNKNOWN.into());
                            }
                        }
                    },
                }
            }
            ElementRef::Options => def.options.locked = true,
        }
    }
}

fn lock(element: ElementRef, reason: &str, server_text: impl Into<String>) -> Locked {
    Locked {
        element,
        reason: reason.into(),
        server_text: server_text.into(),
    }
}

fn key<'a>(original: &'a Option<String>, name: &'a str) -> &'a str {
    original.as_deref().unwrap_or(name)
}

// ---------------------------------------------------------------------------------
// Tokens

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A bare word or number, ASCII-lowercased.
    Word(String),
    /// A quoted identifier (MySQL backticks, PostgreSQL double quotes), unescaped.
    Ident(String),
    /// A string literal, unescaped.
    Str(String),
    Punct(char),
    /// A comment or an unterminated quote, kept verbatim.
    Opaque(String),
}

fn is_word(t: Option<&Tok>, w: &str) -> bool {
    matches!(t, Some(Tok::Word(x)) if x == w)
}

fn is_punct(t: Option<&Tok>, c: char) -> bool {
    matches!(t, Some(Tok::Punct(p)) if *p == c)
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// MySQL: backticks quote names; `'` and `"` strings with `''` / `\x` escapes (default
/// `sql_mode`); `/*!NNNNN … */` is its content. PostgreSQL: `"` quotes names; `'`
/// strings with `''` only (`standard_conforming_strings`).
fn tokens(s: &str, kind: SqlKind) -> Vec<Tok> {
    let (versioned_comments, backtick_names, double_quote_names, backslash_escapes) = match kind {
        SqlKind::MySql => (true, true, false, true),
        SqlKind::Postgres => (false, false, true, false),
    };
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut end = i + 2;
            while end < chars.len() && !(chars[end] == '*' && chars.get(end + 1) == Some(&'/')) {
                end += 1;
            }
            let body: String = chars[i + 2..end.min(chars.len())].iter().collect();
            i = (end + 2).min(chars.len());
            match body.strip_prefix('!') {
                Some(rest) if versioned_comments => {
                    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
                    out.extend(tokens(rest, kind));
                }
                _ => out.push(Tok::Opaque(format!("/*{body}*/"))),
            }
            continue;
        }
        let quoted = match c {
            '`' if backtick_names => Some(true),
            '"' => Some(double_quote_names),
            '\'' => Some(false),
            _ => None,
        };
        if let Some(ident) = quoted {
            let mut text = String::new();
            let mut j = i + 1;
            let mut closed = false;
            while j < chars.len() {
                let ch = chars[j];
                if ch == c {
                    if chars.get(j + 1) == Some(&c) {
                        text.push(c);
                        j += 2;
                        continue;
                    }
                    closed = true;
                    j += 1;
                    break;
                }
                if backslash_escapes && !ident && ch == '\\' && j + 1 < chars.len() {
                    text.push(match chars[j + 1] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '0' => '\0',
                        'b' => '\u{8}',
                        'Z' => '\u{1a}',
                        other => other,
                    });
                    j += 2;
                    continue;
                }
                text.push(ch);
                j += 1;
            }
            if !closed {
                out.push(Tok::Opaque(chars[i..].iter().collect()));
                break;
            }
            out.push(if ident {
                Tok::Ident(text)
            } else {
                Tok::Str(text)
            });
            i = j;
            continue;
        }
        if word_char(c) {
            let mut j = i;
            while j < chars.len() && word_char(chars[j]) {
                j += 1;
            }
            let w: String = chars[i..j].iter().collect();
            out.push(Tok::Word(w.to_ascii_lowercase()));
            i = j;
            continue;
        }
        out.push(Tok::Punct(c));
        i += 1;
    }
    out
}

/// `[-+]digits[.digits][e[-+]digits]`.
fn is_number(s: &str) -> bool {
    fn digits(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    digits(int)
        && frac.is_none_or(digits)
        && exp.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e)))
}

/// The inner text of the parenthesised group `s` starts with (quotes respected).
fn paren_group(s: &str) -> Option<&str> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' && q != '`' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------------
// MySQL / MariaDB

/// A `SHOW CREATE TABLE` element line (or a rendered fragment), normalised. `column`:
/// the nullable-column rules apply.
fn mysql_norm(s: &str, table: &TableOptionsDef, column: bool) -> Vec<Tok> {
    let mut t = tokens(s, SqlKind::MySql);
    if is_punct(t.last(), ',') {
        t.pop();
    }
    let same = |v: Option<&Tok>, table: &Option<String>| matches!((v, table), (Some(Tok::Word(v)), Some(w)) if v.eq_ignore_ascii_case(w));
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let at = |j: usize| t.get(i + j);
        let word = |j: usize, w: &str| is_word(at(j), w);
        let punct = |j: usize, c: char| is_punct(at(j), c);
        if let Some(Tok::Word(w)) = at(0) {
            // Only the width the server prints when none was given: `tinyint(1)` or
            // `int(5)` mean something and stay.
            let base = if w == "integer" { "int" } else { w.as_str() };
            let unsigned = word(4, "unsigned");
            let default = default_display_width(base, unsigned)
                .map(|n| n.to_string())
                .or_else(|| (w == "year").then(|| "4".to_string()));
            let width = matches!((at(2), &default), (Some(Tok::Word(n)), Some(d)) if n == d);
            if punct(1, '(') && width && punct(3, ')') {
                out.push(t[i].clone());
                i += 4;
                continue;
            }
        }
        if word(0, "current_timestamp") && punct(1, '(') && punct(2, ')') {
            out.push(t[i].clone());
            i += 3;
            continue;
        }
        if word(0, "character") && word(1, "set") && same(at(2), &table.charset) {
            i += 3;
            continue;
        }
        if word(0, "charset") && same(at(1), &table.charset) {
            i += 2;
            continue;
        }
        if word(0, "collate") && same(at(1), &table.collation) {
            i += 2;
            continue;
        }
        if word(0, "using") && word(1, "btree") {
            i += 2;
            continue;
        }
        if word(0, "on") && (word(1, "delete") || word(1, "update")) {
            if word(2, "restrict") {
                i += 3;
                continue;
            }
            if word(2, "no") && word(3, "action") {
                i += 4;
                continue;
            }
        }
        if word(0, "default") {
            if let Some(Tok::Str(v)) = at(1) {
                if is_number(v) {
                    out.push(t[i].clone());
                    out.extend(tokens(v, SqlKind::MySql));
                    i += 2;
                    continue;
                }
            }
        }
        out.push(t[i].clone());
        i += 1;
    }
    if column {
        let mut kept: Vec<Tok> = Vec::new();
        let mut i = 0;
        while i < out.len() {
            if is_word(out.get(i), "default") && is_word(out.get(i + 1), "null") {
                i += 2;
                continue;
            }
            if is_word(out.get(i), "null") && !is_word(kept.last(), "not") {
                i += 1;
                continue;
            }
            kept.push(out[i].clone());
            i += 1;
        }
        out = kept;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LineKind {
    Column(String),
    PrimaryKey,
    Index(String),
    ForeignKey(String),
    Check(String),
    Unknown,
}

fn classify(line: &str) -> LineKind {
    let t = tokens(line, SqlKind::MySql);
    let w = |j: usize, s: &str| is_word(t.get(j), s);
    let ident = |j: usize| match t.get(j) {
        Some(Tok::Ident(n)) => Some(n.clone()),
        _ => None,
    };
    let key_word = |j: usize| w(j, "key") || w(j, "index");
    if let Some(n) = ident(0) {
        return LineKind::Column(n);
    }
    if w(0, "primary") && w(1, "key") {
        return LineKind::PrimaryKey;
    }
    if w(0, "unique") || w(0, "fulltext") || w(0, "spatial") {
        if let Some(n) = if key_word(1) { ident(2) } else { ident(1) } {
            return LineKind::Index(n);
        }
    }
    if key_word(0) {
        if let Some(n) = ident(1) {
            return LineKind::Index(n);
        }
    }
    if w(0, "constraint") {
        if let Some(n) = ident(1) {
            if w(2, "foreign") && w(3, "key") {
                return LineKind::ForeignKey(n);
            }
            if w(2, "check") {
                return LineKind::Check(n);
            }
        }
    }
    LineKind::Unknown
}

fn is_fulltext(line: &str) -> bool {
    let t = tokens(line, SqlKind::MySql);
    is_word(t.first(), "fulltext") || is_word(t.first(), "spatial")
}

struct Line {
    text: String,
    kind: LineKind,
    used: bool,
}

/// The element lines and the trailer (from the closing `)`) of `SHOW CREATE TABLE`.
fn split_create(create: &str) -> Option<(Vec<Line>, String)> {
    let mut lines = create.lines();
    if !lines.next()?.trim_end().ends_with('(') {
        return None;
    }
    let mut body = Vec::new();
    let mut trailer: Vec<&str> = Vec::new();
    for l in lines {
        if trailer.is_empty() && !l.starts_with(')') {
            let text = l.trim();
            let text = text
                .strip_suffix(',')
                .unwrap_or(text)
                .trim_end()
                .to_string();
            body.push(Line {
                kind: classify(&text),
                text,
                used: false,
            });
        } else {
            trailer.push(l.trim());
        }
    }
    if trailer.is_empty() {
        return None;
    }
    Some((body, trailer.join(" ")))
}

fn take(lines: &mut [Line], kind: &LineKind) -> Option<String> {
    let l = lines.iter_mut().find(|l| !l.used && &l.kind == kind)?;
    l.used = true;
    Some(l.text.clone())
}

fn mysql_fidelity(
    def: &TableDef,
    create: Option<&str>,
    server: &ServerInfo,
    d: Dialect,
) -> Vec<Locked> {
    let Some(create) = create else {
        return vec![lock(ElementRef::Table, NO_DDL, "")];
    };
    let Some((mut lines, trailer)) = split_create(create) else {
        return vec![lock(ElementRef::Table, NO_DDL, create)];
    };
    let table = &def.options;
    let mut out = Vec::new();
    let compare = |out: &mut Vec<Locked>,
                   lines: &mut [Line],
                   element: ElementRef,
                   kind: LineKind,
                   locked: bool,
                   rendered: &dyn Fn() -> Result<String, RenderError>,
                   column: bool| {
        let Some(text) = take(lines, &kind) else {
            out.push(lock(element, MISMATCH, ""));
            return;
        };
        if locked {
            out.push(lock(element, NOT_EDITABLE, text));
            return;
        }
        let same = rendered()
            .is_ok_and(|r| mysql_norm(&r, table, column) == mysql_norm(&text, table, column));
        if !same {
            out.push(lock(element, MISMATCH, text));
        }
    };

    for c in &def.columns {
        let k = key(&c.original, &c.name).to_string();
        compare(
            &mut out,
            &mut lines,
            ElementRef::Column(k.clone()),
            LineKind::Column(k),
            c.locked,
            &|| column_sql(c, server, d),
            true,
        );
    }
    if let Some(pk) = &def.primary_key {
        compare(
            &mut out,
            &mut lines,
            ElementRef::PrimaryKey,
            LineKind::PrimaryKey,
            pk.locked,
            &|| primary_key_sql(pk, server, d),
            false,
        );
    }
    for i in &def.indexes {
        let k = key(&i.original, &i.name).to_string();
        let fulltext = i.method.as_deref().is_some_and(|m| {
            m.eq_ignore_ascii_case("FULLTEXT") || m.eq_ignore_ascii_case("SPATIAL")
        });
        if fulltext {
            let text = take(&mut lines, &LineKind::Index(k.clone())).unwrap_or_default();
            out.push(lock(ElementRef::Index(k), FULLTEXT, text));
            continue;
        }
        compare(
            &mut out,
            &mut lines,
            ElementRef::Index(k.clone()),
            LineKind::Index(k),
            i.locked,
            &|| index_sql(i, server, d),
            false,
        );
    }
    for f in &def.foreign_keys {
        let k = key(&f.original, &f.name).to_string();
        compare(
            &mut out,
            &mut lines,
            ElementRef::ForeignKey(k.clone()),
            LineKind::ForeignKey(k),
            f.locked,
            &|| fk_sql(f, server, d),
            false,
        );
    }
    for c in &def.checks {
        let k = key(&c.original, &c.name).to_string();
        compare(
            &mut out,
            &mut lines,
            ElementRef::Check(k.clone()),
            LineKind::Check(k),
            c.locked,
            &|| check_sql(c, server, d),
            false,
        );
    }

    // Server lines no draft element matched: shown locked, or the table is read-only.
    for l in lines.iter().filter(|l| !l.used) {
        out.push(match &l.kind {
            LineKind::Index(n) => {
                let reason = if is_fulltext(&l.text) {
                    FULLTEXT
                } else {
                    MISMATCH
                };
                lock(ElementRef::Index(n.clone()), reason, l.text.clone())
            }
            LineKind::Check(n) => lock(ElementRef::Check(n.clone()), MISMATCH, l.text.clone()),
            _ => lock(ElementRef::Table, UNKNOWN, l.text.clone()),
        });
    }

    if !options_match(&trailer, table) {
        out.push(lock(ElementRef::Options, MISMATCH, trailer));
    }
    out
}

/// The trailer's options as `(key, value)`; `None` when it holds anything else.
/// `AUTO_INCREMENT` and `COMMENT` are read but not compared (the counter moves; the
/// comment is its own field).
fn trailer_options(trailer: &str) -> Option<Vec<(&'static str, String)>> {
    let t = tokens(trailer, SqlKind::MySql);
    if !is_punct(t.first(), ')') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < t.len() {
        let w = |j: usize| match t.get(i + j) {
            Some(Tok::Word(w)) => Some(w.as_str()),
            _ => None,
        };
        let (key, n) = match (w(0), w(1), w(2)) {
            (Some("default"), Some("charset"), _) => ("charset", 2),
            (Some("default"), Some("character"), Some("set")) => ("charset", 3),
            (Some("default"), Some("collate"), _) => ("collate", 2),
            (Some("charset"), _, _) => ("charset", 1),
            (Some("character"), Some("set"), _) => ("charset", 2),
            (Some("collate"), _, _) => ("collate", 1),
            (Some("engine"), _, _) => ("engine", 1),
            (Some("row_format"), _, _) => ("row_format", 1),
            (Some("auto_increment"), _, _) => ("auto_increment", 1),
            (Some("comment"), _, _) => ("comment", 1),
            _ => return None,
        };
        i += n;
        if is_punct(t.get(i), '=') {
            i += 1;
        }
        let value = match t.get(i) {
            Some(Tok::Word(v) | Tok::Str(v)) => v.clone(),
            _ => return None,
        };
        i += 1;
        out.push((key, value));
    }
    Some(out)
}

/// Whether the trailer parses and agrees with the draft's options. An option the
/// server leaves out (e.g. an implicit `ROW_FORMAT`) is not compared.
fn options_match(trailer: &str, o: &TableOptionsDef) -> bool {
    let Some(options) = trailer_options(trailer) else {
        return false;
    };
    options.iter().all(|(k, v)| {
        let want = match *k {
            "engine" => &o.engine,
            "charset" => &o.charset,
            "collate" => &o.collation,
            "row_format" => &o.row_format,
            _ => return true,
        };
        want.as_deref().is_some_and(|w| w.eq_ignore_ascii_case(v))
    })
}

/// A locked index for a server line no draft index matched: its columns (with prefix
/// lengths), uniqueness and FULLTEXT / SPATIAL kind; expression parts are left out.
fn index_from_line(name: &str, line: &str) -> IndexDef {
    let t = tokens(line, SqlKind::MySql);
    let first = |w: &str| is_word(t.first(), w);
    let method = if first("fulltext") {
        Some("FULLTEXT".to_string())
    } else if first("spatial") {
        Some("SPATIAL".to_string())
    } else {
        None
    };
    let mut parts = Vec::new();
    if let Some(open) = t.iter().position(|x| is_punct(Some(x), '(')) {
        let mut i = open + 1;
        let mut depth = 1usize;
        while i < t.len() && depth > 0 {
            match &t[i] {
                Tok::Punct('(') => depth += 1,
                Tok::Punct(')') => depth -= 1,
                Tok::Ident(c) if depth == 1 => {
                    let mut part = IndexPartDef {
                        column: Some(c.clone()),
                        expression: None,
                        prefix: None,
                        descending: false,
                    };
                    if is_punct(t.get(i + 1), '(') && is_punct(t.get(i + 3), ')') {
                        if let Some(Tok::Word(n)) = t.get(i + 2) {
                            part.prefix = n.parse().ok();
                            i += 3;
                        }
                    }
                    if is_word(t.get(i + 1), "desc") {
                        part.descending = true;
                    }
                    parts.push(part);
                }
                _ => {}
            }
            i += 1;
        }
    }
    IndexDef {
        original: Some(name.into()),
        name: name.into(),
        unique: first("unique"),
        parts,
        method,
        include: Vec::new(),
        visible: !t
            .iter()
            .any(|x| is_word(Some(x), "invisible") || is_word(Some(x), "ignored")),
        comment: None,
        constraint: false,
        locked: true,
    }
}

/// The expression of a `CONSTRAINT `n` CHECK (expr) …` line.
fn check_expression(line: &str) -> Option<String> {
    let rest = line.trim();
    let rest = match rest.get(..10) {
        Some(p) if p.eq_ignore_ascii_case("CONSTRAINT") => rest[10..].trim_start(),
        _ => rest,
    };
    // Skip the quoted name.
    let rest = match rest.strip_prefix('`') {
        Some(r) => {
            let mut chars = r.char_indices().peekable();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                if c == '`' {
                    if chars.next_if(|&(_, c)| c == '`').is_some() {
                        continue;
                    }
                    end = Some(i + 1);
                    break;
                }
            }
            r[end?..].trim_start()
        }
        None => rest,
    };
    let rest = match rest.get(..5) {
        Some(p) if p.eq_ignore_ascii_case("CHECK") => rest[5..].trim_start(),
        _ => return None,
    };
    paren_group(rest).map(|e| e.trim().to_string())
}

// ---------------------------------------------------------------------------------
// PostgreSQL

/// Tokens with identifier quotes that change nothing removed (`"id"` → `id`).
fn pg_norm(s: &str) -> Vec<Tok> {
    tokens(s, SqlKind::Postgres)
        .into_iter()
        .map(|t| match t {
            Tok::Ident(n) if is_plain_pg_name(&n) => Tok::Word(n),
            other => other,
        })
        .collect()
}

fn is_plain_pg_name(n: &str) -> bool {
    let mut chars = n.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
}

/// Quoted numbers read as numbers (`'-1'::integer` is how `pg_get_expr` prints -1).
fn unquote_numbers(t: Vec<Tok>) -> Vec<Tok> {
    t.into_iter()
        .flat_map(|t| match t {
            Tok::Str(s) if is_number(&s) => tokens(&s, SqlKind::Postgres),
            other => vec![other],
        })
        .collect()
}

/// A rendered constraint without its leading `CONSTRAINT <name>`.
fn without_constraint_name(mut t: Vec<Tok>) -> Vec<Tok> {
    if is_word(t.first(), "constraint") && t.len() >= 2 {
        t.drain(..2);
    }
    t
}

/// The canonical spelling of a type: `format_type`'s `character varying(n)`,
/// `character(n)`, `timestamp(n) with time zone`, `… without time zone` as the catalog
/// spells them (`varchar(n)`, `char(n)`, `timestamptz(n)`, `timestamp(n)`).
fn pg_type(s: &str) -> String {
    let s = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let (body, array) = match s.find('[') {
        Some(i) => (s[..i].trim_end().to_string(), s[i..].to_string()),
        None => (s, String::new()),
    };
    let body = if let Some(r) = body.strip_prefix("character varying") {
        format!("varchar{r}")
    } else if let Some(r) = body
        .strip_prefix("character")
        .filter(|r| r.is_empty() || r.starts_with('('))
    {
        format!("char{r}")
    } else {
        pg_zone(&body)
    };
    body + &array
}

fn pg_zone(b: &str) -> String {
    for name in ["timestamp", "time"] {
        let Some(rest) = b.strip_prefix(name) else {
            continue;
        };
        let (args, after) = if rest.starts_with('(') {
            match rest.find(')') {
                Some(e) => (&rest[..=e], &rest[e + 1..]),
                None => return b.into(),
            }
        } else {
            ("", rest)
        };
        return match after.trim() {
            "" | "without time zone" => format!("{name}{args}"),
            "with time zone" => format!("{name}tz{args}"),
            _ => b.into(),
        };
    }
    b.into()
}

/// A server default as compared: a literal without its cast, `NULL` as none.
fn pg_server_default(d: &str) -> Option<Vec<Tok>> {
    let d = d.trim();
    if let Some((lit, rest)) = unquote(d) {
        if rest.is_empty() || is_cast_suffix(rest) {
            return Some(unquote_numbers(vec![Tok::Str(lit)]));
        }
    }
    let lower = d.to_ascii_lowercase();
    if lower == "null" || (lower.starts_with("null::") && is_cast_suffix(&d[4..])) {
        return None;
    }
    Some(unquote_numbers(pg_norm(d)))
}

fn pg_column_matches(c: &ColumnDef, dc: &ColumnDetail, server: &ServerInfo, d: Dialect) -> bool {
    let Ok(full) = column_sql(c, server, d) else {
        return false;
    };
    if pg_type(&type_text(&c.data_type, server)) != pg_type(&dc.db_type) {
        return false;
    }
    let rendered = match &c.default {
        None => None,
        Some(_) => {
            let bare = ColumnDef {
                default: None,
                ..c.clone()
            };
            let Ok(bare) = column_sql(&bare, server, d) else {
                return false;
            };
            match full
                .strip_prefix(&bare)
                .and_then(|r| r.strip_prefix(" DEFAULT "))
            {
                Some(text) => Some(unquote_numbers(pg_norm(text))),
                None => return false,
            }
        }
    };
    rendered == dc.default.as_deref().and_then(pg_server_default)
}

fn pg_column_text(dc: &ColumnDetail) -> String {
    let mut out = format!(
        "{} {}",
        quote_ident(SqlKind::Postgres, &dc.name),
        dc.db_type
    );
    if let Some(g) = &dc.generated {
        out.push_str(&format!(" GENERATED ALWAYS AS ({g}) STORED"));
    }
    if !dc.nullable {
        out.push_str(" NOT NULL");
    }
    if let Some(def) = &dc.default {
        out.push_str(&format!(" DEFAULT {def}"));
    }
    out
}

/// `pg_get_indexdef` of a plain index from `USING` on (the part the fragment renders),
/// when its `CREATE [UNIQUE] INDEX n ON [ONLY] rel` head agrees with `unique`.
fn pg_index_tail(definition: &str, unique: bool) -> Option<Vec<Tok>> {
    let t = pg_norm(definition);
    if !is_word(t.first(), "create") {
        return None;
    }
    let mut i = 1;
    let def_unique = is_word(t.get(i), "unique");
    if def_unique {
        i += 1;
    }
    if def_unique != unique || !is_word(t.get(i), "index") {
        return None;
    }
    i += 2; // INDEX name
    if !is_word(t.get(i), "on") {
        return None;
    }
    i += 1;
    if is_word(t.get(i), "only") {
        i += 1;
    }
    i += 1; // relation
    if is_punct(t.get(i), '.') {
        i += 2;
    }
    if !is_word(t.get(i), "using") {
        return None;
    }
    Some(t[i..].to_vec())
}

/// The lock of an index (or primary key) stored outside the default tablespace
/// (ruling T9-1): re-creating it would move it, and its definitions do not say so.
fn in_tablespace(element: ElementRef, index: Option<&IndexDetail>) -> Option<Locked> {
    let index = index?;
    let ts = index.tablespace.as_deref()?;
    let text = format!(
        "{} TABLESPACE {}",
        index.definition.as_deref().unwrap_or(&index.name),
        quote_ident(SqlKind::Postgres, ts)
    );
    Some(lock(
        element,
        &format!(
            "This index is stored in tablespace \"{ts}\"; the designer would re-create it in \
             the default tablespace."
        ),
        text,
    ))
}

fn pg_fidelity(
    def: &TableDef,
    detail: &TableDetail,
    server: &ServerInfo,
    d: Dialect,
) -> Vec<Locked> {
    let mut out = Vec::new();
    for c in &def.columns {
        let k = key(&c.original, &c.name);
        let element = ElementRef::Column(k.to_string());
        let Some(dc) = detail.columns.iter().find(|x| x.name == k) else {
            out.push(lock(element, MISMATCH, ""));
            continue;
        };
        let text = pg_column_text(dc);
        if c.locked {
            out.push(lock(element, NOT_EDITABLE, text));
        } else if !pg_column_matches(c, dc, server, d) {
            out.push(lock(element, MISMATCH, text));
        }
    }

    // A constraint or index: its server definition against the rendered one.
    let compare = |out: &mut Vec<Locked>,
                   element: ElementRef,
                   locked: bool,
                   definition: Option<&str>,
                   same: &dyn Fn(&str) -> bool| {
        let Some(definition) = definition else {
            out.push(lock(element, MISMATCH, ""));
            return;
        };
        if locked {
            out.push(lock(element, NOT_EDITABLE, definition));
        } else if !same(definition) {
            out.push(lock(element, MISMATCH, definition));
        }
    };
    let rendered =
        |r: Result<String, RenderError>| r.ok().map(|r| without_constraint_name(pg_norm(&r)));

    if let Some(pk) = &def.primary_key {
        let server_pk = detail.primary_key.as_ref();
        if let Some(l) = in_tablespace(ElementRef::PrimaryKey, server_pk) {
            out.push(l);
        } else {
            let definition = server_pk.and_then(|i| i.definition.as_deref());
            let mine = rendered(primary_key_sql(pk, server, d));
            compare(
                &mut out,
                ElementRef::PrimaryKey,
                pk.locked,
                definition,
                &|s| mine.as_ref() == Some(&pg_norm(s)),
            );
        }
    }
    for i in &def.indexes {
        let k = key(&i.original, &i.name);
        let server_index = detail.indexes.iter().find(|x| x.name == k);
        if let Some(l) = in_tablespace(ElementRef::Index(k.to_string()), server_index) {
            out.push(l);
            continue;
        }
        let definition = server_index.and_then(|x| x.definition.as_deref());
        let r = index_sql(i, server, d);
        let same = |s: &str| {
            // EXCLUDE constraints (T5-6+): the designer has no exclusion constraints.
            if s.trim_start().to_ascii_uppercase().starts_with("EXCLUDE") {
                return false;
            }
            if i.constraint {
                rendered(r.clone()).is_some_and(|m| m == pg_norm(s))
            } else {
                match (&r, pg_index_tail(s, i.unique)) {
                    (Ok(m), Some(tail)) => pg_norm(m) == tail,
                    _ => false,
                }
            }
        };
        compare(
            &mut out,
            ElementRef::Index(k.to_string()),
            i.locked,
            definition,
            &same,
        );
    }
    for f in &def.foreign_keys {
        let k = key(&f.original, &f.name);
        let definition = detail
            .foreign_keys
            .iter()
            .find(|x| x.name == k)
            .and_then(|x| x.definition.as_deref());
        // The driver qualifies the referenced table; a draft leaves its own schema out.
        let qualified = FkDef {
            ref_schema: f
                .ref_schema
                .clone()
                .or_else(|| detail.object.schema.clone()),
            ..f.clone()
        };
        let mine = rendered(fk_sql(&qualified, server, d));
        compare(
            &mut out,
            ElementRef::ForeignKey(k.to_string()),
            f.locked,
            definition,
            &|s| mine.as_ref() == Some(&pg_norm(s)),
        );
    }
    for c in &def.checks {
        let k = key(&c.original, &c.name);
        let definition = detail
            .checks
            .iter()
            .find(|x| x.name == k)
            .and_then(|x| x.definition.as_deref());
        let mine = rendered(check_sql(c, server, d));
        compare(
            &mut out,
            ElementRef::Check(k.to_string()),
            c.locked,
            definition,
            &|s| mine.as_ref() == Some(&pg_norm(s)),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::from_detail;
    use std::path::PathBuf;

    const MYSQL: [&str; 4] = ["MySql57", "MySql8", "MariaDb106", "MariaDb114"];
    const PG: [&str; 2] = ["Pg12", "Pg17"];

    struct Fixture {
        server: ServerInfo,
        detail: TableDetail,
        ddl: Option<String>,
    }

    fn fixture(target: &str, table: &str) -> Fixture {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{target}_{table}"));
        let json = std::fs::read_to_string(base.with_extension("json"))
            .unwrap_or_else(|e| panic!("{target}_{table}.json: {e}"));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Built through the constructor: the recorded fixtures predate `capabilities`.
        let s = &v["server"];
        let field = |k: &str| s[k].clone();
        Fixture {
            server: ServerInfo::new(
                serde_json::from_value(field("kind")).unwrap(),
                serde_json::from_value(field("variant")).unwrap(),
                serde_json::from_value(field("version")).unwrap(),
                s["version_string"].as_str().unwrap(),
            ),
            detail: serde_json::from_value(v["detail"].clone()).unwrap(),
            ddl: std::fs::read_to_string(base.with_extension("sql")).ok(),
        }
    }

    impl Fixture {
        fn def(&self) -> TableDef {
            from_detail(&self.detail, &self.server).unwrap()
        }

        fn locks(&self) -> Vec<Locked> {
            self.locks_with(&self.def(), self.ddl.as_deref())
        }

        fn locks_with(&self, def: &TableDef, ddl: Option<&str>) -> Vec<Locked> {
            fidelity(def, &self.detail, ddl, &self.server).unwrap()
        }
    }

    fn elements(locks: &[Locked]) -> Vec<ElementRef> {
        locks.iter().map(|l| l.element.clone()).collect()
    }

    fn col(n: &str) -> ElementRef {
        ElementRef::Column(n.into())
    }

    fn idx(n: &str) -> ElementRef {
        ElementRef::Index(n.into())
    }

    fn check(n: &str) -> ElementRef {
        ElementRef::Check(n.into())
    }

    /// Backticks, `"`, backslash escapes and `/*!…*/` lex per kind.
    #[test]
    fn tokens_per_kind() {
        let s = r#"`a` "b" 'c\n' /*!50100 d */"#;
        assert_eq!(
            tokens(s, SqlKind::MySql),
            vec![
                Tok::Ident("a".into()),
                Tok::Str("b".into()),
                Tok::Str("c\n".into()),
                Tok::Word("d".into()),
            ]
        );
        assert_eq!(
            tokens(s, SqlKind::Postgres),
            vec![
                Tok::Punct('`'),
                Tok::Word("a".into()),
                Tok::Punct('`'),
                Tok::Ident("b".into()),
                Tok::Str(r"c\n".into()),
                Tok::Opaque("/*!50100 d */".into()),
            ]
        );
    }

    #[test]
    fn fixtures_have_no_locks_for_plain_tables() {
        for target in MYSQL.iter().chain(&PG) {
            for table in ["orders", "weird_names"] {
                let f = fixture(target, table);
                assert!(f.ddl.is_some() || f.server.kind == dnt_core::server::DbKind::Postgres);
                assert_eq!(f.locks(), vec![], "{target} {table}");
            }
        }
    }

    #[test]
    fn all_types_locks_only_expected() {
        for target in MYSQL.iter().chain(&PG) {
            let expected = match *target {
                // MariaDB's JSON is longtext + a column-level json_valid check.
                "MariaDb106" | "MariaDb114" => vec![col("c_json"), check("c_json")],
                _ => vec![],
            };
            let locks = fixture(target, "all_types").locks();
            assert_eq!(elements(&locks), expected, "{target}: {locks:#?}");
        }
        // A generated column is locked by the model; fidelity says so with its text.
        for target in PG {
            let locks = fixture(target, "invoices").locks();
            assert_eq!(elements(&locks), vec![col("amount_with_tax")], "{target}");
            assert!(locks[0].server_text.contains("numeric(12,2)"), "{locks:?}");
        }
    }

    #[test]
    fn mysql_unknown_column_attribute_locks_column() {
        for target in MYSQL {
            let f = fixture(target, "orders");
            let ddl = f.ddl.clone().unwrap();
            let edited = ddl
                .replace(
                    "`customer_id` int(11) NOT NULL,",
                    "`customer_id` int(11) NOT NULL COLUMN_FORMAT DYNAMIC,",
                )
                .replace(
                    "`customer_id` int NOT NULL,",
                    "`customer_id` int NOT NULL COLUMN_FORMAT DYNAMIC,",
                )
                .replace(
                    "`created_at` datetime(6) DEFAULT NULL,",
                    "`created_at` datetime(6) DEFAULT NULL /*!80003 SRID 4326 */,",
                );
            assert_ne!(edited, ddl, "{target}");
            let locks = f.locks_with(&f.def(), Some(&edited));
            assert_eq!(
                elements(&locks),
                vec![col("customer_id"), col("created_at")],
                "{target}"
            );
            assert_eq!(locks[0].reason, MISMATCH);
            assert!(
                locks[0].server_text.contains("COLUMN_FORMAT DYNAMIC"),
                "{locks:?}"
            );
            let mut def = f.def();
            apply_locks(&mut def, &locks);
            let locked: Vec<&str> = def
                .columns
                .iter()
                .filter(|c| c.locked)
                .map(|c| c.name.as_str())
                .collect();
            assert_eq!(locked, ["customer_id", "created_at"], "{target}");
        }
    }

    #[test]
    fn fulltext_index_shown_locked() {
        for target in MYSQL {
            let f = fixture(target, "fid_mysql_locks");
            let locks = f.locks();
            let mut expected = vec![idx("ft_body")];
            if target == "MySql8" {
                // NOT ENFORCED (T5-6): the designer would re-create it enforced.
                expected.push(check("ck_not_enforced"));
            }
            assert_eq!(elements(&locks), expected, "{target}: {locks:#?}");
            assert_eq!(locks[0].server_text, "FULLTEXT KEY `ft_body` (`body`)");
            let mut def = f.def();
            apply_locks(&mut def, &locks);
            let ft = def.indexes.iter().find(|i| i.name == "ft_body").unwrap();
            assert!(ft.locked);
            assert!(!def.indexes.iter().find(|i| i.name == "k_a").unwrap().locked);

            // A server index the introspection missed is added to the draft, locked.
            let mut detail = f.detail.clone();
            detail.indexes.retain(|i| i.name != "ft_body");
            let mut def = from_detail(&detail, &f.server).unwrap();
            let locks = fidelity(&def, &detail, f.ddl.as_deref(), &f.server).unwrap();
            assert!(elements(&locks).contains(&idx("ft_body")), "{target}");
            apply_locks(&mut def, &locks);
            let ft = def.indexes.iter().find(|i| i.name == "ft_body").unwrap();
            assert!(ft.locked && ft.original.as_deref() == Some("ft_body"));
            assert_eq!(ft.method.as_deref(), Some("FULLTEXT"));
            assert_eq!(ft.parts[0].column.as_deref(), Some("body"));
            assert_eq!(
                crate::validate(&def, &f.server).unwrap(),
                vec![],
                "{target}"
            );
        }
    }

    #[test]
    fn pg_index_options_and_exclusion_are_locked() {
        for target in PG {
            let locks = fixture(target, "fid_pg_locks").locks();
            assert_eq!(
                elements(&locks),
                vec![
                    idx("fid_excl"),
                    idx("fid_uq_ts"),
                    idx("idx_collate"),
                    idx("idx_nulls"),
                    idx("idx_opclass"),
                    idx("idx_ts"),
                    idx("idx_with"),
                ],
                "{target}: {locks:#?}"
            );
            // Tablespace (T9-1): neither pg_get_* function prints it; the lock does.
            for i in [1, 5] {
                assert!(locks[i].reason.contains("\"dnt_fid_ts\""), "{:?}", locks[i]);
                assert!(
                    locks[i].server_text.ends_with(" TABLESPACE \"dnt_fid_ts\""),
                    "{:?}",
                    locks[i]
                );
            }
            assert_eq!(
                locks[6].server_text,
                "CREATE INDEX idx_with ON fid_scratch.fid_pg_locks USING btree (name) \
                 WITH (fillfactor='70')"
            );
        }
    }

    #[test]
    fn pg_partitioned_measurements_is_read_only() {
        for target in PG {
            let f = fixture(target, "measurements");
            let mut def = f.def();
            let locks = f.locks();
            assert_eq!(locks, vec![], "{target}");
            apply_locks(&mut def, &locks);
            assert!(def.read_only_reason.is_some(), "{target}");
        }
    }

    #[test]
    fn real_differences_lock_only_their_element() {
        // MySQL: a changed default, type, index column and FK action on the server side.
        for target in MYSQL {
            let f = fixture(target, "orders");
            let ddl = f.ddl.clone().unwrap();
            for (from, to, element) in [
                ("DEFAULT 'new'", "DEFAULT 'paid'", col("status")),
                ("decimal(12,2)", "decimal(14,2)", col("total")),
                (
                    "(`customer_id`,`status`)",
                    "(`status`,`customer_id`)",
                    idx("idx_orders_customer_status"),
                ),
                (
                    "REFERENCES `customers` (`id`)",
                    "REFERENCES `customers` (`id`) ON DELETE CASCADE",
                    ElementRef::ForeignKey("fk_orders_customer".into()),
                ),
                ("ENGINE=InnoDB", "ENGINE=MyISAM", ElementRef::Options),
            ] {
                let edited = ddl.replace(from, to);
                assert_ne!(edited, ddl, "{target}: {from}");
                let locks = f.locks_with(&f.def(), Some(&edited));
                assert_eq!(elements(&locks), vec![element], "{target}: {from}");
            }
        }
        // PostgreSQL: the same through the detail's server texts.
        for target in PG {
            let f = fixture(target, "orders");
            let mut d = f.detail.clone();
            d.columns
                .iter_mut()
                .find(|c| c.name == "total")
                .unwrap()
                .default = Some("1".into());
            d.indexes[0].definition = d.indexes[0]
                .definition
                .as_ref()
                .map(|s| s.replace("(customer_id, status)", "(customer_id, status DESC)"));
            d.foreign_keys[0].definition = d.foreign_keys[0]
                .definition
                .as_ref()
                .map(|s| format!("{s} ON DELETE CASCADE"));
            d.checks[0].definition = Some("CHECK ((total > (0)::numeric))".into());
            let locks = fidelity(&f.def(), &d, None, &f.server).unwrap();
            assert_eq!(
                elements(&locks),
                vec![
                    col("total"),
                    idx("idx_orders_customer_status"),
                    ElementRef::ForeignKey(f.detail.foreign_keys[0].name.clone()),
                    check(&f.detail.checks[0].name),
                ],
                "{target}: {locks:#?}"
            );
            // A type the server spells differently from the catalog type it parsed to.
            let mut d = f.detail.clone();
            d.columns
                .iter_mut()
                .find(|c| c.name == "created_at")
                .unwrap()
                .db_type = "timestamp with time zone".into();
            let locks = fidelity(&f.def(), &d, None, &f.server).unwrap();
            assert_eq!(elements(&locks), vec![col("created_at")], "{target}");
        }
    }

    fn table_opts() -> TableOptionsDef {
        TableOptionsDef {
            engine: Some("InnoDB".into()),
            charset: Some("utf8mb4".into()),
            collation: Some("utf8mb4_unicode_ci".into()),
            row_format: None,
            locked: false,
        }
    }

    /// `ddl` with the element line containing `marker` replaced by `line`.
    fn replace_line(ddl: &str, marker: &str, line: &str) -> String {
        let out: Vec<String> = ddl
            .lines()
            .map(|l| {
                if l.contains(marker) {
                    line.to_string()
                } else {
                    l.to_string()
                }
            })
            .collect();
        assert!(out.iter().any(|l| l == line), "{marker} not in {ddl}");
        out.join("\n")
    }

    #[test]
    fn only_default_display_widths_are_dropped() {
        let t = table_opts();
        let n = |s: &str| mysql_norm(s, &t, true);
        assert_eq!(n("`a` int(11) DEFAULT NULL"), n("`a` int NULL"));
        assert_eq!(
            n("`a` int(10) unsigned NOT NULL"),
            n("`a` int unsigned NOT NULL")
        );
        assert_eq!(
            n("`a` bigint(20) unsigned NULL"),
            n("`a` bigint unsigned NULL")
        );
        assert_eq!(n("`a` tinyint(4) NULL"), n("`a` tinyint NULL"));
        assert_eq!(n("`y` year(4) NULL"), n("`y` year NULL"));
        // A width the user gave is part of the type.
        assert_ne!(n("`b` tinyint(1) DEFAULT NULL"), n("`b` tinyint NULL"));
        assert_ne!(n("`a` int(5) NULL"), n("`a` int NULL"));
        assert_ne!(n("`a` int(11) unsigned NULL"), n("`a` int unsigned NULL"));

        // Through fidelity: server `tinyint(1)` vs a draft `tinyint` locks the column.
        for target in MYSQL {
            let f = fixture(target, "weird_names");
            let ddl = replace_line(
                f.ddl.as_deref().unwrap(),
                "`id` int",
                "  `id` tinyint(1) NOT NULL,",
            );
            let mut def = f.def();
            def.columns[0].data_type = crate::parse_type("tinyint", &f.server).unwrap();
            assert_eq!(
                elements(&f.locks_with(&def, Some(&ddl))),
                vec![col("id")],
                "{target}"
            );
            // The same width in the draft matches.
            def.columns[0].data_type = crate::parse_type("tinyint(1)", &f.server).unwrap();
            assert_eq!(f.locks_with(&def, Some(&ddl)), vec![], "{target}");
        }
    }

    #[test]
    fn server_charset_or_collation_the_draft_lacks_locks() {
        for target in MYSQL {
            let f = fixture(target, "weird_names");
            for line in [
                "  `we``ird\"col` varchar(50) COLLATE utf8mb4_bin DEFAULT NULL,",
                "  `we``ird\"col` varchar(50) CHARACTER SET latin1 DEFAULT NULL,",
                "  `we``ird\"col` varchar(50) CHARACTER SET latin1 COLLATE latin1_bin DEFAULT NULL,",
            ] {
                let ddl = replace_line(f.ddl.as_deref().unwrap(), "ird\"col", line);
                assert_eq!(
                    elements(&f.locks_with(&f.def(), Some(&ddl))),
                    vec![col("we`ird\"col")],
                    "{target}: {line}"
                );
            }
        }
    }

    #[test]
    fn mysql_index_and_on_update_differences_lock() {
        for target in MYSQL {
            let f = fixture(target, "orders");
            let ddl = f.ddl.clone().unwrap();
            let index = idx("idx_orders_customer_status");
            for line in [
                "  KEY `idx_orders_customer_status` (`customer_id`,`status` DESC),",
                "  KEY `idx_orders_customer_status` (`customer_id`,`status`(3)),",
                "  KEY `idx_orders_customer_status` (`customer_id`,`status`) COMMENT 'x',",
            ] {
                let edited = replace_line(&ddl, "KEY `idx_orders_customer_status`", line);
                assert_eq!(
                    elements(&f.locks_with(&f.def(), Some(&edited))),
                    vec![index.clone()],
                    "{target}: {line}"
                );
            }
            // A changed index comment, from the draft side.
            let mut def = f.def();
            def.indexes[0].comment = Some("y".into());
            assert_eq!(
                elements(&f.locks_with(&def, Some(&ddl))),
                vec![index.clone()],
                "{target}"
            );
            // The server has no ON UPDATE where the draft has one.
            let mut def = f.def();
            let created = def
                .columns
                .iter_mut()
                .find(|c| c.name == "created_at")
                .unwrap();
            created.on_update = Some("CURRENT_TIMESTAMP(6)".into());
            assert_eq!(
                elements(&f.locks_with(&def, Some(&ddl))),
                vec![col("created_at")],
                "{target}"
            );
        }
    }

    #[test]
    fn leftover_column_pk_or_unknown_line_makes_the_table_read_only() {
        for target in MYSQL {
            let f = fixture(target, "weird_names");
            let ddl = f.ddl.clone().unwrap();
            let mut cases = vec![
                (
                    replace_line(
                        &ddl,
                        "PRIMARY KEY",
                        "  `ghost` int DEFAULT NULL,\n  PRIMARY KEY (`id`)",
                    ),
                    f.def(),
                ),
                (
                    replace_line(
                        &ddl,
                        "PRIMARY KEY",
                        "  PRIMARY KEY (`id`),\n  PERIOD FOR SYSTEM_TIME (`a`,`b`)",
                    ),
                    f.def(),
                ),
            ];
            let mut no_pk = f.def();
            no_pk.primary_key = None;
            cases.push((ddl.clone(), no_pk));
            for (edited, def) in cases {
                let locks = f.locks_with(&def, Some(&edited));
                assert_eq!(
                    elements(&locks),
                    vec![ElementRef::Table],
                    "{target}: {edited}"
                );
                assert_eq!(locks[0].reason, UNKNOWN);
                let mut def = def;
                apply_locks(&mut def, &locks);
                assert_eq!(def.read_only_reason.as_deref(), Some(UNKNOWN), "{target}");
            }
        }
    }

    #[test]
    fn unreadable_leftover_check_makes_the_table_read_only() {
        let f = fixture("MySql8", "orders");
        let mut def = f.def();
        let locks = vec![lock(
            ElementRef::Check("ghost".into()),
            MISMATCH,
            "CONSTRAINT `ghost` CHECK",
        )];
        apply_locks(&mut def, &locks);
        assert!(def.checks.iter().all(|c| c.name != "ghost"));
        assert_eq!(def.read_only_reason.as_deref(), Some(UNKNOWN));
        // A readable one is added, locked, and the table stays editable.
        let mut def = f.def();
        let locks = vec![lock(
            ElementRef::Check("ghost".into()),
            MISMATCH,
            "CONSTRAINT `ghost` CHECK ((`total` < 10)) /*!80016 NOT ENFORCED */",
        )];
        apply_locks(&mut def, &locks);
        let ghost = def.checks.iter().find(|c| c.name == "ghost").unwrap();
        assert!(ghost.locked);
        assert_eq!(ghost.expression, "(`total` < 10)");
        assert_eq!(def.read_only_reason, None);
    }

    #[test]
    fn apply_locks_never_unlocks() {
        let f = fixture("MySql8", "orders");
        let mut def = f.def();
        def.columns[1].locked = true;
        def.options.locked = true;
        def.read_only_reason = Some("earlier".into());
        let before = def.clone();
        apply_locks(&mut def, &[]);
        assert_eq!(def, before);
        apply_locks(
            &mut def,
            &[
                lock(col("id"), MISMATCH, ""),
                lock(ElementRef::Table, NO_DDL, ""),
                lock(
                    ElementRef::Check("ghost".into()),
                    MISMATCH,
                    "CONSTRAINT `ghost` CHECK",
                ),
            ],
        );
        assert!(def.columns[0].locked && def.columns[1].locked && def.options.locked);
        assert_eq!(def.read_only_reason.as_deref(), Some("earlier"));
    }

    #[test]
    fn missing_mysql_ddl_makes_the_table_read_only() {
        let f = fixture("MySql8", "orders");
        let mut def = f.def();
        let locks = f.locks_with(&def, None);
        assert_eq!(elements(&locks), vec![ElementRef::Table]);
        apply_locks(&mut def, &locks);
        assert_eq!(
            def.read_only_reason.as_deref(),
            Some(locks[0].reason.as_str())
        );
    }

    #[test]
    fn unparseable_table_options_lock_options() {
        let f = fixture("MySql8", "orders");
        let ddl = f.ddl.clone().unwrap().replace(
            "COLLATE=utf8mb4_unicode_ci",
            "COLLATE=utf8mb4_unicode_ci STATS_PERSISTENT=0",
        );
        let locks = f.locks_with(&f.def(), Some(&ddl));
        assert_eq!(elements(&locks), vec![ElementRef::Options]);
        let mut def = f.def();
        apply_locks(&mut def, &locks);
        assert!(def.options.locked);
    }

    #[test]
    fn normalise_mariadb_current_timestamp() {
        let server = crate::test_util::mariadb();
        let table = TableOptionsDef {
            engine: Some("InnoDB".into()),
            charset: Some("utf8mb4".into()),
            collation: Some("utf8mb4_unicode_ci".into()),
            row_format: None,
            locked: false,
        };
        let line = "  `t` timestamp NOT NULL DEFAULT current_timestamp() ON UPDATE \
                    current_timestamp(),";
        let c = ColumnDef {
            original: Some("t".into()),
            name: "t".into(),
            data_type: crate::parse_type("timestamp", &server).unwrap(),
            nullable: false,
            default: Some(DefaultSpec::Expression {
                text: "CURRENT_TIMESTAMP".into(),
            }),
            auto_increment: false,
            identity: None,
            on_update: Some("CURRENT_TIMESTAMP".into()),
            charset: None,
            collation: None,
            comment: None,
            using: None,
            generated: None,
            locked: false,
        };
        let rendered = crate::column_sql(&c, &server, dnt_core::sql::Dialect::MYSQL).unwrap();
        assert_eq!(
            mysql_norm(&rendered, &table, true),
            mysql_norm(line, &table, true)
        );
        assert_eq!(
            mysql_norm(line, &table, true),
            mysql_norm(
                "`t` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP",
                &table,
                true
            )
        );
        // MariaDB prints numbers unquoted where render quotes them (T7-3).
        assert_eq!(
            mysql_norm("`n` decimal(12,2) NOT NULL DEFAULT 0.00", &table, true),
            mysql_norm("`n` decimal(12,2) NOT NULL DEFAULT '0.00'", &table, true)
        );
        // Quotes inside literals: both escape styles read the same.
        assert_eq!(
            mysql_norm("`c` int COMMENT 'it''s'", &table, true),
            mysql_norm("`c` int COMMENT 'it\\'s'", &table, true)
        );
        assert_ne!(
            mysql_norm("`c` int COMMENT 'a'", &table, true),
            mysql_norm("`c` int COMMENT 'b'", &table, true)
        );
    }
}
