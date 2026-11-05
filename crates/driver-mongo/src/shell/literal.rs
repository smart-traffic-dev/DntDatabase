//! JS-style literals, mongosh helpers and Extended JSON v2 -> BSON (spec §4.3).
//!
//! Objects take unquoted, quoted or `$op` keys and a trailing comma. Numbers follow
//! mongosh: an integer literal in the Int32 range is `Int32`, every other number is
//! `Double`. An object holding an Extended JSON wrapper key (`$oid`, `$date`, …) must be
//! exactly that wrapper and becomes the BSON value; a malformed one is an error, never a
//! sub-document.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{
    Binary, Bson, DateTime, Decimal128, Document, JavaScriptCodeWithScope, Regex, Timestamp, Uuid,
};

use super::{lex, ShellError, Token, TokenKind};

/// Arrays and objects nested deeper than this are refused (no stack overflow on hostile
/// input).
const MAX_DEPTH: usize = 200;

/// 2^63, exactly representable as `f64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// Option letters a BSON regex may carry.
const REGEX_FLAGS: &str = "ilmsux";

/// Reads one literal starting at `tokens[*at]`; on success `*at` is the index just past it.
/// `text` is the source the tokens were lexed from.
pub fn parse_literal(tokens: &[Token], text: &str, at: &mut usize) -> Result<Bson, ShellError> {
    let mut p = Parser {
        toks: tokens,
        text,
        pos: *at,
    };
    let v = p.value(0)?;
    *at = p.pos;
    Ok(v)
}

/// The whole text must be one document (a raw filter, a JSON cell).
pub fn parse_document(text: &str) -> Result<Document, ShellError> {
    let tokens = lex(text)?;
    let start = tokens.first().map_or(text.len(), |t| t.start);
    let expected = || ShellError::new("Expected a document", start);
    if !matches!(tokens.first().map(|t| &t.kind), Some(TokenKind::Punct('{'))) {
        return Err(expected());
    }
    let mut at = 0;
    let v = parse_literal(&tokens, text, &mut at)?;
    if let Some(t) = tokens.get(at) {
        return Err(ShellError::new(
            "Unexpected text after the document",
            t.start,
        ));
    }
    match v {
        Bson::Document(d) => Ok(d),
        _ => Err(expected()),
    }
}

struct Parser<'a> {
    toks: &'a [Token],
    text: &'a str,
    pos: usize,
}

/// One call argument: its value, where it starts, and the literal text when it is a
/// (possibly negated) number, so integer and decimal helpers keep full precision.
struct Arg {
    value: Bson,
    start: usize,
    number: Option<String>,
}

/// One object member, offsets kept for wrapper errors.
struct Member {
    key: String,
    value: Bson,
    value_start: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    /// Byte offset of the next token, or the end of the text.
    fn offset(&self) -> usize {
        self.peek().map_or(self.text.len(), |t| t.start)
    }

    fn err(&self, message: impl Into<String>) -> ShellError {
        ShellError::new(message, self.offset())
    }

    fn eat(&mut self, c: char) -> bool {
        let hit = self.at_punct(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn value(&mut self, depth: usize) -> Result<Bson, ShellError> {
        let Some(tok) = self.peek().cloned() else {
            return Err(self.err("Expected a value"));
        };
        match tok.kind {
            TokenKind::Punct('{' | '[') if depth >= MAX_DEPTH => Err(self.err("Nested too deeply")),
            TokenKind::Punct('{') => self.object(depth),
            TokenKind::Punct('[') => self.array(depth),
            TokenKind::Punct('-') => {
                self.pos += 1;
                match self.peek().map(|t| &t.kind) {
                    Some(TokenKind::Num(n)) => {
                        let v = number(n, true, self.offset())?;
                        self.pos += 1;
                        Ok(v)
                    }
                    Some(TokenKind::Ident(i)) if i == "Infinity" => {
                        self.pos += 1;
                        Ok(Bson::Double(f64::NEG_INFINITY))
                    }
                    _ => Err(self.err("Expected a number after '-'")),
                }
            }
            TokenKind::Num(n) => {
                self.pos += 1;
                number(&n, false, tok.start)
            }
            TokenKind::Str(s) => {
                self.pos += 1;
                Ok(Bson::String(s))
            }
            TokenKind::Regex { pattern, flags } => {
                self.pos += 1;
                Ok(Bson::RegularExpression(Regex {
                    pattern,
                    options: regex_options(&flags, tok.start)?,
                }))
            }
            TokenKind::Ident(name) => {
                self.pos += 1;
                self.word(&name, tok.start, depth)
            }
            TokenKind::Punct(_) => Err(self.err("Expected a value")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Bson, ShellError> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            if self.eat(']') {
                return Ok(Bson::Array(items));
            }
            items.push(self.value(depth + 1)?);
            if !self.eat(',') && !self.at_punct(']') {
                return Err(self.err("Expected ',' or ']'"));
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Bson, ShellError> {
        let open = self.offset();
        self.pos += 1;
        let mut members = Vec::new();
        loop {
            if self.eat('}') {
                return wrapper_or_document(members, open);
            }
            let key = match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Ident(k) | TokenKind::Str(k)) => k.clone(),
                // Only plain digits: JS would rewrite `0x1F` or `1.0` as a key.
                Some(TokenKind::Num(k)) if k.bytes().all(|b| b.is_ascii_digit()) => k.clone(),
                _ => return Err(self.err("Expected a key")),
            };
            self.pos += 1;
            if !self.eat(':') {
                return Err(self.err("Expected ':' after the key"));
            }
            let value_start = self.offset();
            let value = self.value(depth + 1)?;
            members.push(Member {
                key,
                value,
                value_start,
            });
            if !self.eat(',') && !self.at_punct('}') {
                return Err(self.err("Expected ',' or '}'"));
            }
        }
    }

    /// A bare word in value position: a keyword, a constant or a helper.
    fn word(&mut self, name: &str, start: usize, depth: usize) -> Result<Bson, ShellError> {
        Ok(match name {
            "true" => Bson::Boolean(true),
            "false" => Bson::Boolean(false),
            // mongosh stores `undefined` as null.
            "null" | "undefined" => Bson::Null,
            "Infinity" => Bson::Double(f64::INFINITY),
            "NaN" => Bson::Double(f64::NAN),
            "MinKey" | "MaxKey" if !self.at_punct('(') => keyword_key(name),
            "new" => {
                let helper = match self.peek().map(|t| &t.kind) {
                    Some(TokenKind::Ident(h)) if is_helper(h) => h.clone(),
                    _ => return Err(self.err("Expected a helper name after new")),
                };
                let helper_start = self.offset();
                self.pos += 1;
                if !self.at_punct('(') {
                    return Err(self.err(format!("Expected '(' after {helper}")));
                }
                return self.helper(&helper, helper_start, true, depth);
            }
            h if is_helper(h) && self.at_punct('(') => return self.helper(h, start, false, depth),
            h if is_helper(h) => return Err(self.err(format!("Expected '(' after {h}"))),
            other => {
                return Err(ShellError::new(
                    format!("Unexpected identifier {other}"),
                    start,
                ))
            }
        })
    }

    fn at_punct(&self, c: char) -> bool {
        matches!(self.peek(), Some(Token { kind: TokenKind::Punct(p), .. }) if *p == c)
    }

    /// `(args)` after a helper name; the next token is `(`.
    fn args(&mut self, depth: usize) -> Result<Vec<Arg>, ShellError> {
        self.pos += 1;
        let mut args = Vec::new();
        loop {
            if self.eat(')') {
                return Ok(args);
            }
            let start = self.offset();
            let first = self.pos;
            let value = self.value(depth + 1)?;
            let number = match &self.toks[first..self.pos] {
                [Token {
                    kind: TokenKind::Num(n),
                    ..
                }] => Some(n.clone()),
                [Token {
                    kind: TokenKind::Punct('-'),
                    ..
                }, Token {
                    kind: TokenKind::Num(n),
                    ..
                }] => Some(format!("-{n}")),
                _ => None,
            };
            args.push(Arg {
                value,
                start,
                number,
            });
            if !self.eat(',') && !self.at_punct(')') {
                return Err(self.err("Expected ',' or ')'"));
            }
        }
    }

    /// A helper call (§4.3); the next token is `(`. `start` is the helper name's offset.
    fn helper(
        &mut self,
        name: &str,
        start: usize,
        newed: bool,
        depth: usize,
    ) -> Result<Bson, ShellError> {
        let args = self.args(depth)?;
        if name == "Date" && !newed {
            return Err(ShellError::new(
                "Use new Date(…); Date() without new is not supported",
                start,
            ));
        }
        // `Err(at)`: a wrong argument at `at`, or a wrong argument count (`at` = the helper).
        let result: Result<Bson, usize> = match (name, args.as_slice()) {
            ("ObjectId", []) => Ok(Bson::ObjectId(ObjectId::new())),
            ("ObjectId", [a]) => str_arg(a)
                .filter(|s| s.len() == 24)
                .and_then(|s| ObjectId::parse_str(s).ok())
                .map(Bson::ObjectId)
                .ok_or(a.start),
            ("ISODate" | "Date", []) => Ok(Bson::DateTime(DateTime::now())),
            (
                "ISODate" | "Date",
                [Arg {
                    value: Bson::String(s),
                    start: at,
                    ..
                }],
            ) => {
                return parse_iso_date(s)
                    .map(Bson::DateTime)
                    .ok_or_else(|| ShellError::new(format!("Invalid ISO-8601 date: {s}"), *at))
            }
            ("Date", [a]) => integral(a)
                .map(|ms| Bson::DateTime(DateTime::from_millis(ms)))
                .ok_or(a.start),
            ("ISODate", [a]) => Err(a.start),
            ("NumberLong", [a]) => integer_arg(a).map(Bson::Int64).ok_or(a.start),
            ("NumberInt", [a]) => integer_arg(a)
                .and_then(|n| i32::try_from(n).ok())
                .map(Bson::Int32)
                .ok_or(a.start),
            ("NumberDecimal", [a]) => match (&a.value, &a.number) {
                (Bson::String(s), _) => Some(s.as_str()),
                (_, Some(n)) => Some(n.as_str()),
                _ => None,
            }
            .and_then(|t| t.parse::<Decimal128>().ok())
            .map(Bson::Decimal128)
            .ok_or(a.start),
            ("UUID", []) => Ok(Bson::Binary(Uuid::new().into())),
            ("UUID", [a]) => str_arg(a).and_then(uuid_binary).ok_or(a.start),
            ("BinData", [t, data]) => bin_data(t, data),
            ("Timestamp", [t, i]) => {
                let u32_of = |a: &Arg| {
                    integral(a)
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or(a.start)
                };
                u32_of(t).and_then(|time| {
                    Ok(Bson::Timestamp(Timestamp {
                        time,
                        increment: u32_of(i)?,
                    }))
                })
            }
            ("MinKey" | "MaxKey", []) => Ok(keyword_key(name)),
            _ => Err(start),
        };
        result.map_err(|at| ShellError::new(usage(name), at))
    }
}

/// The error message of a helper called with wrong arguments.
fn usage(helper: &str) -> String {
    match helper {
        "ObjectId" => "ObjectId() expects no argument or a 24-digit hex string",
        "ISODate" => "ISODate() expects no argument or an ISO-8601 date string",
        "Date" => "new Date() expects no argument, an ISO-8601 date string or milliseconds",
        "NumberLong" => "NumberLong() expects an integer or an integer string",
        "NumberInt" => "NumberInt() expects a 32-bit integer",
        "NumberDecimal" => "NumberDecimal() expects a decimal string",
        "UUID" => "UUID() expects no argument or a 32-digit hex string",
        "BinData" => "BinData() expects a subtype (0-255) and a base64 string",
        "Timestamp" => "Timestamp() expects two 32-bit unsigned integers",
        _ => return format!("{helper}() takes no arguments"),
    }
    .into()
}

fn str_arg(a: &Arg) -> Option<&str> {
    match &a.value {
        Bson::String(s) => Some(s),
        _ => None,
    }
}

fn bin_data(t: &Arg, data: &Arg) -> Result<Bson, usize> {
    let sub = integral(t)
        .and_then(|n| u8::try_from(n).ok())
        .ok_or(t.start)?;
    let bytes = str_arg(data)
        .and_then(|s| B64.decode(s).ok())
        .ok_or(data.start)?;
    Ok(Bson::Binary(Binary {
        subtype: BinarySubtype::from(sub),
        bytes,
    }))
}

fn is_helper(name: &str) -> bool {
    matches!(
        name,
        "ObjectId"
            | "ISODate"
            | "Date"
            | "NumberLong"
            | "NumberInt"
            | "NumberDecimal"
            | "UUID"
            | "BinData"
            | "Timestamp"
            | "MinKey"
            | "MaxKey"
    )
}

fn keyword_key(name: &str) -> Bson {
    if name == "MinKey" {
        Bson::MinKey
    } else {
        Bson::MaxKey
    }
}

/// A number literal (`lit` without its sign). Integer literals in the Int32 range are
/// `Int32`; everything else is `Double`.
fn number(lit: &str, negative: bool, at: usize) -> Result<Bson, ShellError> {
    let bad = || ShellError::new("Invalid number", at);
    let sign = if negative { -1.0 } else { 1.0 };
    let (int, float) = if let Some(hex) = lit.strip_prefix("0x").or_else(|| lit.strip_prefix("0X"))
    {
        let magnitude = hex
            .chars()
            .try_fold(0f64, |acc, c| {
                c.to_digit(16).map(|d| acc * 16.0 + f64::from(d))
            })
            .ok_or_else(bad)?;
        (u64::from_str_radix(hex, 16).ok(), sign * magnitude)
    } else {
        let float: f64 = lit.parse().map_err(|_| bad())?;
        let int = lit.bytes().all(|b| b.is_ascii_digit());
        (int.then(|| lit.parse::<u64>().ok()).flatten(), sign * float)
    };
    // Only an integer literal (no `.`, no exponent) can be Int32.
    let int32 = int
        .and_then(|m| i64::try_from(m).ok())
        .map(|m| if negative { -m } else { m })
        .and_then(|n| i32::try_from(n).ok());
    Ok(match int32 {
        Some(n) => Bson::Int32(n),
        None => Bson::Double(float),
    })
}

/// An integer argument: an integer literal (exact, from its text), an integral number,
/// or a string holding an integer.
fn integer_arg(a: &Arg) -> Option<i64> {
    match &a.value {
        Bson::String(s) => s.parse().ok(),
        _ => integral(a),
    }
}

/// The argument as an exact integer, if it is an integral number.
fn integral(a: &Arg) -> Option<i64> {
    if let Some(n) = &a.number {
        if let Ok(i) = n.parse::<i64>() {
            return Some(i);
        }
    }
    integral_value(&a.value)
}

pub(crate) fn integral_value(v: &Bson) -> Option<i64> {
    match v {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        // Integral and inside [-2^63, 2^63), so the cast is exact.
        Bson::Double(f) if f.fract() == 0.0 && (-TWO_63..TWO_63).contains(f) => Some(*f as i64),
        _ => None,
    }
}

/// Regex flags -> BSON options, sorted; unknown or repeated flags are errors at `at`.
fn regex_options(flags: &str, at: usize) -> Result<String, ShellError> {
    let mut chars: Vec<char> = Vec::new();
    for c in flags.chars() {
        if !REGEX_FLAGS.contains(c) || chars.contains(&c) {
            return Err(ShellError::new(
                format!("Invalid regular expression flag '{c}'"),
                at,
            ));
        }
        chars.push(c);
    }
    chars.sort_unstable();
    Ok(chars.into_iter().collect())
}

/// 32 hex digits, or the canonical `8-4-4-4-12` layout.
fn uuid_binary(s: &str) -> Option<Bson> {
    let b = s.as_bytes();
    let digits: String = match b.len() {
        32 => s.to_string(),
        36 if [8, 13, 18, 23].iter().all(|&i| b[i] == b'-') => {
            s.chars().filter(|c| *c != '-').collect()
        }
        _ => return None,
    };
    match hex::decode(&digits) {
        Ok(bytes) if bytes.len() == 16 => Some(Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes,
        })),
        _ => None,
    }
}

/// RFC 3339, plus what mongosh's `ISODate` also takes: a date alone, a space for `T`,
/// no seconds, no offset (UTC), and `+HHMM` / `+HH` offsets.
fn parse_iso_date(s: &str) -> Option<DateTime> {
    // ISO-8601 text is ASCII; refusing anything else also keeps the slicing below on
    // char boundaries.
    if !s.is_ascii() {
        return None;
    }
    if let Ok(d) = DateTime::parse_rfc3339_str(s) {
        return Some(d);
    }
    DateTime::parse_rfc3339_str(normalize_iso(s)?).ok()
}

/// Only called on ASCII text; every slice still goes through `get` so it cannot panic.
fn normalize_iso(s: &str) -> Option<String> {
    let date = s.get(..10)?;
    let rest = s.get(10..)?;
    if rest.is_empty() {
        return Some(format!("{date}T00:00:00Z"));
    }
    let time = rest.strip_prefix(['T', 't', ' '])?;
    let zone_at = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(zone_at);
    let clock = match clock.len() {
        5 => format!("{clock}:00"),
        _ => clock.to_string(),
    };
    let zone = match zone.len() {
        0 => "Z".to_string(),
        3 => format!("{zone}:00"),
        5 => format!("{}:{}", zone.get(..3)?, zone.get(3..)?),
        _ => zone.to_string(),
    };
    Some(format!("{date}T{clock}{zone}"))
}

/// Keys that make an object an Extended JSON wrapper.
fn is_wrapper_key(key: &str) -> bool {
    matches!(
        key,
        "$oid"
            | "$date"
            | "$numberLong"
            | "$numberInt"
            | "$numberDouble"
            | "$numberDecimal"
            | "$binary"
            | "$uuid"
            | "$regularExpression"
            | "$timestamp"
            | "$minKey"
            | "$maxKey"
            | "$symbol"
            | "$code"
            | "$scope"
            | "$dbPointer"
            | "$undefined"
    )
}

/// A plain document, or the BSON value of the Extended JSON wrapper the object is.
fn wrapper_or_document(members: Vec<Member>, open: usize) -> Result<Bson, ShellError> {
    let Some(w) = members.iter().find(|m| is_wrapper_key(&m.key)) else {
        let mut d = Document::new();
        for m in members {
            d.insert(m.key, m.value);
        }
        return Ok(Bson::Document(d));
    };
    let has_code = members.iter().any(|m| m.key == "$code");
    if has_code {
        let shape_ok = match members.as_slice() {
            [_] => true,
            [a, b] => {
                (a.key.as_str(), b.key.as_str()) == ("$code", "$scope")
                    || (a.key.as_str(), b.key.as_str()) == ("$scope", "$code")
            }
            _ => false,
        };
        if !shape_ok {
            return Err(ShellError::new(
                "Extended JSON $code may only be joined by $scope",
                open,
            ));
        }
        return code_wrapper(&members);
    }
    if w.key == "$scope" {
        return Err(ShellError::new("Extended JSON $scope needs $code", open));
    }
    if members.len() != 1 {
        return Err(ShellError::new(
            format!("Extended JSON {} must be the only key in its object", w.key),
            open,
        ));
    }
    let Member {
        key,
        value,
        value_start: at,
    } = members.into_iter().next().expect("one member");
    wrapper(&key, value, at)
}

fn code_wrapper(members: &[Member]) -> Result<Bson, ShellError> {
    let mut code = None;
    let mut scope = None;
    for m in members {
        match (m.key.as_str(), &m.value) {
            ("$code", Bson::String(s)) => code = Some(s.clone()),
            ("$code", _) => {
                return Err(ShellError::new(
                    "Invalid $code: expected a string",
                    m.value_start,
                ))
            }
            (_, Bson::Document(d)) => scope = Some(d.clone()),
            _ => {
                return Err(ShellError::new(
                    "Invalid $scope: expected a document",
                    m.value_start,
                ))
            }
        }
    }
    let code = code.expect("shape checked");
    Ok(match scope {
        Some(scope) => Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope { code, scope }),
        None => Bson::JavaScriptCode(code),
    })
}

/// The exact keys of a wrapper's sub-document, in any order.
fn fields<'d>(v: &'d Bson, names: &[&str]) -> Option<Vec<&'d Bson>> {
    let Bson::Document(d) = v else { return None };
    if d.len() != names.len() {
        return None;
    }
    names.iter().map(|n| d.get(*n)).collect()
}

fn wrapper(key: &str, v: Bson, at: usize) -> Result<Bson, ShellError> {
    let expected = |what: &str| ShellError::new(format!("Invalid {key}: expected {what}"), at);
    let string = |v: &Bson| match v {
        Bson::String(s) => Some(s.clone()),
        _ => None,
    };
    match key {
        "$oid" => string(&v)
            .filter(|s| s.len() == 24)
            .and_then(|s| ObjectId::parse_str(s).ok())
            .map(Bson::ObjectId)
            .ok_or_else(|| expected("a 24-digit hex string")),
        "$date" => match &v {
            Bson::String(s) => {
                return parse_iso_date(s)
                    .map(Bson::DateTime)
                    .ok_or_else(|| ShellError::new(format!("Invalid ISO-8601 date: {s}"), at))
            }
            other => integral_value(other).map(DateTime::from_millis),
        }
        .map(Bson::DateTime)
        .ok_or_else(|| expected(r#"an ISO-8601 string, milliseconds or {"$numberLong": "…"}"#)),
        "$numberLong" => string(&v)
            .and_then(|s| s.parse::<i64>().ok())
            .map(Bson::Int64)
            .ok_or_else(|| expected("a string holding a 64-bit integer")),
        "$numberInt" => string(&v)
            .and_then(|s| s.parse::<i32>().ok())
            .map(Bson::Int32)
            .ok_or_else(|| expected("a string holding a 32-bit integer")),
        "$numberDouble" => string(&v)
            .and_then(|s| parse_double(&s))
            .map(Bson::Double)
            .ok_or_else(|| expected(r#"a number string, "NaN", "Infinity" or "-Infinity""#)),
        "$numberDecimal" => string(&v)
            .and_then(|s| s.parse::<Decimal128>().ok())
            .map(Bson::Decimal128)
            .ok_or_else(|| expected("a decimal string")),
        "$binary" => fields(&v, &["base64", "subType"])
            .and_then(|f| {
                let bytes = B64.decode(string(f[0])?).ok()?;
                let sub = string(f[1])?;
                if sub.is_empty() || sub.len() > 2 {
                    return None;
                }
                let sub = u8::from_str_radix(&sub, 16).ok()?;
                Some(Bson::Binary(Binary {
                    subtype: BinarySubtype::from(sub),
                    bytes,
                }))
            })
            .ok_or_else(|| expected(r#"{"base64": "…", "subType": "<hex>"}"#)),
        "$uuid" => string(&v)
            .and_then(|s| uuid_binary(&s))
            .ok_or_else(|| expected("a UUID string")),
        "$regularExpression" => {
            let f = fields(&v, &["pattern", "options"])
                .and_then(|f| Some((string(f[0])?, string(f[1])?)))
                .ok_or_else(|| expected(r#"{"pattern": "…", "options": "…"}"#))?;
            Ok(Bson::RegularExpression(Regex {
                pattern: f.0,
                options: regex_options(&f.1, at)?,
            }))
        }
        "$timestamp" => fields(&v, &["t", "i"])
            .and_then(|f| {
                let u32_of = |b: &Bson| integral_value(b).and_then(|n| u32::try_from(n).ok());
                Some(Bson::Timestamp(Timestamp {
                    time: u32_of(f[0])?,
                    increment: u32_of(f[1])?,
                }))
            })
            .ok_or_else(|| expected(r#"{"t": <uint32>, "i": <uint32>}"#)),
        "$minKey" | "$maxKey" => match integral_value(&v) {
            Some(1) if key == "$minKey" => Ok(Bson::MinKey),
            Some(1) => Ok(Bson::MaxKey),
            _ => Err(expected("1")),
        },
        "$symbol" => string(&v)
            .map(Bson::Symbol)
            .ok_or_else(|| expected("a string")),
        "$undefined" => match v {
            Bson::Boolean(true) => Ok(Bson::Undefined),
            _ => Err(expected("true")),
        },
        // `$dbPointer`: its fields are private, so build it through bson's own Extended
        // JSON reader.
        _ => fields(&v, &["$ref", "$id"])
            .and_then(|f| match (f[0], f[1]) {
                (Bson::String(r), Bson::ObjectId(id)) => Bson::try_from(serde_json::json!(
                    {"$dbPointer": {"$ref": r, "$id": {"$oid": id.to_hex()}}}
                ))
                .ok(),
                _ => None,
            })
            .ok_or_else(|| expected(r#"{"$ref": "…", "$id": {"$oid": "…"}}"#)),
    }
}

/// `$numberDouble` text: a decimal number, `NaN`, `Infinity` or `-Infinity` (Rust's own
/// `inf` / `nan` spellings are refused).
fn parse_double(s: &str) -> Option<f64> {
    match s {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ if s.bytes().any(|b| b.is_ascii_digit())
            && s.bytes()
                .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) =>
        {
            s.parse::<f64>().ok().filter(|f| f.is_finite())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rows::canonical_ejson;
    use crate::shell::lex;
    use mongodb::bson::oid::ObjectId;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{
        doc, Binary, DateTime, Decimal128, JavaScriptCodeWithScope, Regex, Timestamp,
    };

    const OID: &str = "507f1f77bcf86cd799439011";
    const MILLIS: i64 = 1_700_000_000_123;

    /// One literal; every token must be used.
    fn lit(text: &str) -> Bson {
        let toks = lex(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        let mut at = 0;
        let v = parse_literal(&toks, text, &mut at).unwrap_or_else(|e| panic!("{text}: {e:?}"));
        assert_eq!(at, toks.len(), "{text}: tokens left over");
        v
    }

    /// The lex or parse error of one literal.
    fn lit_err(text: &str) -> ShellError {
        let toks = match lex(text) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mut at = 0;
        match parse_literal(&toks, text, &mut at) {
            Ok(v) => panic!("{text}: parsed as {v:?}"),
            Err(e) => e,
        }
    }

    fn date(ms: i64) -> Bson {
        Bson::DateTime(DateTime::from_millis(ms))
    }

    fn oid() -> Bson {
        Bson::ObjectId(ObjectId::parse_str(OID).unwrap())
    }

    fn uuid_bin() -> Bson {
        Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: (0u8..16).collect(),
        })
    }

    fn regex(p: &str, o: &str) -> Bson {
        Bson::RegularExpression(Regex {
            pattern: p.into(),
            options: o.into(),
        })
    }

    #[test]
    fn js_object_literals() {
        assert_eq!(
            lit(r#"{a: 1, 'b': "x", "c": 'y', $gt: true, _id: null, u: undefined, f: false,}"#),
            Bson::Document(doc! {
                "a": 1, "b": "x", "c": "y", "$gt": true, "_id": Bson::Null,
                "u": Bson::Null, "f": false,
            })
        );
        assert_eq!(
            lit("[1, [2, {x: []}], {}, `t`,]"),
            Bson::Array(vec![
                Bson::Int32(1),
                Bson::Array(vec![Bson::Int32(2), Bson::Document(doc! {"x": []})]),
                Bson::Document(doc! {}),
                Bson::String("t".into()),
            ])
        );
        // Key order is kept; a repeated key keeps its first position and takes the last value.
        let d = lit("{z: 1, a: 2, z: 3}");
        assert_eq!(d, Bson::Document(doc! {"z": 3, "a": 2}));
        let Bson::Document(d) = d else { unreachable!() };
        assert_eq!(d.keys().collect::<Vec<_>>(), ["z", "a"]);
        // Unicode identifiers, `$`-keys, quoted dotted keys, comments between tokens.
        assert_eq!(
            lit("{café: 1, 'a.b': {$in: [1, 2]}, // note\n /* c */ 7: 'n'}"),
            Bson::Document(doc! {"café": 1, "a.b": {"$in": [1, 2]}, "7": "n"})
        );
        // `parse_literal` reads one literal and leaves `at` after it.
        let text = "{a: 1}, [2]";
        let toks = lex(text).unwrap();
        let mut at = 0;
        assert_eq!(
            parse_literal(&toks, text, &mut at).unwrap(),
            Bson::Document(doc! {"a": 1})
        );
        assert_eq!(toks[at].kind, crate::shell::TokenKind::Punct(','));
        at += 1;
        assert_eq!(
            parse_literal(&toks, text, &mut at).unwrap(),
            Bson::Array(vec![Bson::Int32(2)])
        );
        assert_eq!(at, toks.len());
        // `parse_document`: the whole text is one document.
        assert_eq!(
            parse_document("  {name: 'x', n: NumberLong(5)}  ").unwrap(),
            doc! {"name": "x", "n": 5_i64}
        );
        for bad in [
            "",
            "   ",
            "[1]",
            "5",
            r#"{"$oid": "507f1f77bcf86cd799439011"}"#,
        ] {
            assert!(parse_document(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn numbers_follow_mongosh() {
        assert_eq!(lit("1"), Bson::Int32(1));
        assert_eq!(lit("0"), Bson::Int32(0));
        assert_eq!(lit("-5"), Bson::Int32(-5));
        assert_eq!(lit("- 5"), Bson::Int32(-5));
        assert_eq!(lit("2147483647"), Bson::Int32(i32::MAX));
        assert_eq!(lit("-2147483648"), Bson::Int32(i32::MIN));
        assert_eq!(lit("0x1F"), Bson::Int32(31));
        // Integers outside Int32 are Double (never Int64), like mongosh.
        assert_eq!(lit("2147483648"), Bson::Double(2_147_483_648.0));
        assert_eq!(lit("-2147483649"), Bson::Double(-2_147_483_649.0));
        assert_eq!(
            lit("12345678901234567890"),
            Bson::Double(12_345_678_901_234_567_890.0)
        );
        assert_eq!(lit("0x100000000"), Bson::Double(4_294_967_296.0));
        // Fractions and exponents are Double, even when integral.
        assert_eq!(lit("1.5"), Bson::Double(1.5));
        assert_eq!(lit("3.0"), Bson::Double(3.0));
        assert_eq!(lit("-0.25"), Bson::Double(-0.25));
        assert_eq!(lit(".5"), Bson::Double(0.5));
        assert_eq!(lit("1e3"), Bson::Double(1000.0));
        assert_eq!(lit("2E-2"), Bson::Double(0.02));
        assert_eq!(lit("Infinity"), Bson::Double(f64::INFINITY));
        assert_eq!(lit("-Infinity"), Bson::Double(f64::NEG_INFINITY));
        assert!(matches!(lit("NaN"), Bson::Double(f) if f.is_nan()));
        assert_eq!(
            lit("{a: -1, b: [-2.5]}"),
            Bson::Document(doc! {"a": -1, "b": [-2.5]})
        );
        let e = lit_err("-x");
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Expected a number after '-'", 1)
        );
    }

    #[test]
    fn helpers_with_and_without_args() {
        assert_eq!(lit(&format!("ObjectId('{OID}')")), oid());
        assert_eq!(lit(&format!("new ObjectId(\"{OID}\")")), oid());
        let (a, b) = (lit("ObjectId()"), lit("new ObjectId()"));
        assert!(matches!(a, Bson::ObjectId(_)) && matches!(b, Bson::ObjectId(_)));
        assert_ne!(a, b);

        assert_eq!(lit("ISODate('2023-11-14T22:13:20.123Z')"), date(MILLIS));
        assert_eq!(lit("new Date('2023-11-14T22:13:20.123Z')"), date(MILLIS));
        assert_eq!(
            lit("ISODate('2023-11-14T23:13:20.123+01:00')"),
            date(MILLIS)
        );
        assert_eq!(lit("ISODate('2023-11-14T22:13:20.123')"), date(MILLIS));
        assert_eq!(lit("ISODate('2023-11-14 22:13:20.123Z')"), date(MILLIS));
        assert_eq!(lit("ISODate('2023-11-14T22:13Z')"), date(1_699_999_980_000));
        assert_eq!(lit("ISODate('2023-11-14')"), date(1_699_920_000_000));
        assert_eq!(lit("new Date(1700000000123)"), date(MILLIS));
        let before = DateTime::now().timestamp_millis();
        let now = [lit("ISODate()"), lit("new Date()")];
        let after = DateTime::now().timestamp_millis();
        for n in now {
            let Bson::DateTime(d) = n else {
                panic!("{n:?}")
            };
            assert!((before..=after).contains(&d.timestamp_millis()));
        }

        assert_eq!(lit("NumberLong(5)"), Bson::Int64(5));
        assert_eq!(lit("NumberLong(-3)"), Bson::Int64(-3));
        assert_eq!(
            lit("NumberLong('9223372036854775807')"),
            Bson::Int64(i64::MAX)
        );
        assert_eq!(
            lit("NumberLong(\"-9223372036854775808\")"),
            Bson::Int64(i64::MIN)
        );
        // Exact even past 2^53 (read from the literal text, not through a Double).
        assert_eq!(
            lit("NumberLong(9007199254740993)"),
            Bson::Int64(9_007_199_254_740_993)
        );
        assert_eq!(lit("NumberInt(7)"), Bson::Int32(7));
        assert_eq!(lit("NumberInt('8')"), Bson::Int32(8));
        assert_eq!(lit("NumberInt(-2147483648)"), Bson::Int32(i32::MIN));
        assert_eq!(
            lit("NumberDecimal('1.50')"),
            Bson::Decimal128("1.50".parse().unwrap())
        );
        assert_eq!(
            lit("NumberDecimal(0.1)"),
            Bson::Decimal128("0.1".parse().unwrap())
        );
        assert_eq!(
            lit("UUID('00010203-0405-0607-0809-0a0b0c0d0e0f')"),
            uuid_bin()
        );
        assert_eq!(
            lit("new UUID('000102030405060708090A0B0C0D0E0F')"),
            uuid_bin()
        );
        let Bson::Binary(fresh) = lit("UUID()") else {
            panic!()
        };
        assert_eq!(
            (fresh.subtype, fresh.bytes.len()),
            (BinarySubtype::Uuid, 16)
        );
        assert_eq!(
            lit("BinData(0, 'AQI=')"),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2]
            })
        );
        assert_eq!(
            lit("BinData(128, \"AQI=\")"),
            Bson::Binary(Binary {
                subtype: BinarySubtype::UserDefined(0x80),
                bytes: vec![1, 2]
            })
        );
        assert_eq!(
            lit("Timestamp(7, 3)"),
            Bson::Timestamp(Timestamp {
                time: 7,
                increment: 3
            })
        );
        assert_eq!(
            lit("new Timestamp(4294967295, 0)"),
            Bson::Timestamp(Timestamp {
                time: u32::MAX,
                increment: 0
            })
        );
        assert_eq!(lit("MinKey"), Bson::MinKey);
        assert_eq!(lit("MaxKey"), Bson::MaxKey);
        assert_eq!(lit("MinKey()"), Bson::MinKey);
        assert_eq!(lit("MaxKey()"), Bson::MaxKey);
        assert_eq!(
            lit("{a: MinKey, b: [MaxKey], c: NumberInt(1)}"),
            Bson::Document(doc! {"a": Bson::MinKey, "b": [Bson::MaxKey], "c": 1})
        );

        // Wrong arguments are errors at the argument (or the helper when the count is wrong).
        let cases = [
            (
                "ObjectId('xyz')",
                "ObjectId() expects no argument or a 24-digit hex string",
                9,
            ),
            (
                "ObjectId(1, 2)",
                "ObjectId() expects no argument or a 24-digit hex string",
                0,
            ),
            (
                "ISODate('yesterday')",
                "Invalid ISO-8601 date: yesterday",
                8,
            ),
            (
                "ISODate(5)",
                "ISODate() expects no argument or an ISO-8601 date string",
                8,
            ),
            (
                "Date('2023-11-14')",
                "Use new Date(…); Date() without new is not supported",
                0,
            ),
            (
                "NumberLong('x')",
                "NumberLong() expects an integer or an integer string",
                11,
            ),
            (
                "NumberLong(1.5)",
                "NumberLong() expects an integer or an integer string",
                11,
            ),
            (
                "NumberLong()",
                "NumberLong() expects an integer or an integer string",
                0,
            ),
            (
                "NumberLong('9223372036854775808')",
                "NumberLong() expects an integer or an integer string",
                11,
            ),
            (
                "NumberInt(2147483648)",
                "NumberInt() expects a 32-bit integer",
                10,
            ),
            (
                "NumberDecimal('abc')",
                "NumberDecimal() expects a decimal string",
                14,
            ),
            (
                "UUID('123')",
                "UUID() expects no argument or a 32-digit hex string",
                5,
            ),
            // Dashes only in the canonical 8-4-4-4-12 places.
            (
                "UUID('0001020-30405-0607-0809-0a0b0c0d0e0f')",
                "UUID() expects no argument or a 32-digit hex string",
                5,
            ),
            (
                "UUID('00010203-04050607-0809-0a0b-0c0d0e0f')",
                "UUID() expects no argument or a 32-digit hex string",
                5,
            ),
            (
                "UUID('0-0010203040506070809-0a0b0c0d0e0f')",
                "UUID() expects no argument or a 32-digit hex string",
                5,
            ),
            // Non-ASCII date text is refused, never sliced mid-character.
            (
                "ISODate('2020-01-01T00:00+1é1')",
                "Invalid ISO-8601 date: 2020-01-01T00:00+1é1",
                8,
            ),
            (
                "new Date('2020-01-01T00:00+1é1')",
                "Invalid ISO-8601 date: 2020-01-01T00:00+1é1",
                9,
            ),
            (
                "BinData(256, 'AQI=')",
                "BinData() expects a subtype (0-255) and a base64 string",
                8,
            ),
            (
                "BinData(0, '!!')",
                "BinData() expects a subtype (0-255) and a base64 string",
                11,
            ),
            (
                "BinData(0)",
                "BinData() expects a subtype (0-255) and a base64 string",
                0,
            ),
            (
                "Timestamp(-1, 0)",
                "Timestamp() expects two 32-bit unsigned integers",
                10,
            ),
            (
                "Timestamp(1)",
                "Timestamp() expects two 32-bit unsigned integers",
                0,
            ),
            ("MinKey(1)", "MinKey() takes no arguments", 0),
            ("new NumberLong", "Expected '(' after NumberLong", 14),
            ("new foo()", "Expected a helper name after new", 4),
        ];
        for (text, message, offset) in cases {
            let e = lit_err(text);
            assert_eq!((e.message.as_str(), e.offset), (message, offset), "{text}");
        }
    }

    /// Every BSON kind, as the data view writes it (canonical Extended JSON).
    fn every_kind() -> Document {
        let db_pointer = Bson::try_from(serde_json::json!(
            {"$dbPointer": {"$ref": "users", "$id": {"$oid": OID}}}
        ))
        .unwrap();
        doc! {
            "oid": oid(),
            "date": date(MILLIS),
            "farDate": date(i64::MAX),
            "long": 5_i64,
            "int": -7,
            "double": 1.5,
            "inf": f64::INFINITY,
            "negInf": f64::NEG_INFINITY,
            "decimal": "1.50".parse::<Decimal128>().unwrap(),
            "bin": Binary { subtype: BinarySubtype::UserDefined(0x80), bytes: vec![1, 2] },
            "uuid": uuid_bin(),
            "re": regex("^a", "im"),
            "ts": Timestamp { time: 7, increment: 3 },
            "min": Bson::MinKey,
            "max": Bson::MaxKey,
            "symbol": Bson::Symbol("s".into()),
            "code": Bson::JavaScriptCode("f()".into()),
            "scoped": JavaScriptCodeWithScope { code: "g()".into(), scope: doc! {"x": 1} },
            "ptr": db_pointer,
            "undef": Bson::Undefined,
            "nested": {"arr": [1_i64, {"$in": [oid()]}], "s": "t", "b": true, "n": Bson::Null},
        }
    }

    #[test]
    fn extended_json_canonical_and_relaxed() {
        // Canonical: the data view's own output parses back to the same document.
        let d = every_kind();
        let canonical = canonical_ejson(&Bson::Document(d.clone()));
        assert_eq!(parse_document(&canonical).unwrap(), d);

        // Every wrapper, one at a time, canonical and relaxed.
        let cases: Vec<(&str, Bson)> = vec![
            (r#"{"$oid": "507f1f77bcf86cd799439011"}"#, oid()),
            (
                r#"{"$date": {"$numberLong": "1700000000123"}}"#,
                date(MILLIS),
            ),
            (r#"{"$date": "2023-11-14T22:13:20.123Z"}"#, date(MILLIS)),
            (r#"{"$date": 1700000000123}"#, date(MILLIS)),
            (r#"{"$date": {"$numberLong": "-1"}}"#, date(-1)),
            (r#"{"$numberLong": "5"}"#, Bson::Int64(5)),
            (
                r#"{"$numberLong": "-9223372036854775808"}"#,
                Bson::Int64(i64::MIN),
            ),
            (r#"{"$numberInt": "-7"}"#, Bson::Int32(-7)),
            (r#"{"$numberDouble": "1.5"}"#, Bson::Double(1.5)),
            (r#"{"$numberDouble": "-1.0E-3"}"#, Bson::Double(-0.001)),
            (
                r#"{"$numberDouble": "Infinity"}"#,
                Bson::Double(f64::INFINITY),
            ),
            (
                r#"{"$numberDouble": "-Infinity"}"#,
                Bson::Double(f64::NEG_INFINITY),
            ),
            (
                r#"{"$numberDecimal": "1.50"}"#,
                Bson::Decimal128("1.50".parse().unwrap()),
            ),
            (
                r#"{"$binary": {"base64": "AQI=", "subType": "80"}}"#,
                Bson::Binary(Binary {
                    subtype: BinarySubtype::UserDefined(0x80),
                    bytes: vec![1, 2],
                }),
            ),
            (
                r#"{"$binary": {"subType": "0", "base64": ""}}"#,
                Bson::Binary(Binary {
                    subtype: BinarySubtype::Generic,
                    bytes: vec![],
                }),
            ),
            (
                r#"{"$uuid": "00010203-0405-0607-0809-0a0b0c0d0e0f"}"#,
                uuid_bin(),
            ),
            (
                r#"{"$regularExpression": {"pattern": "^a", "options": "mi"}}"#,
                regex("^a", "im"),
            ),
            (
                r#"{"$timestamp": {"t": 4294967295, "i": 3}}"#,
                Bson::Timestamp(Timestamp {
                    time: u32::MAX,
                    increment: 3,
                }),
            ),
            (r#"{"$minKey": 1}"#, Bson::MinKey),
            (r#"{"$maxKey": 1}"#, Bson::MaxKey),
            (r#"{"$symbol": "s"}"#, Bson::Symbol("s".into())),
            (r#"{"$code": "f()"}"#, Bson::JavaScriptCode("f()".into())),
            (r#"{"$undefined": true}"#, Bson::Undefined),
            // mongosh style around a wrapper: unquoted key, single quotes.
            ("{$oid: '507f1f77bcf86cd799439011'}", oid()),
        ];
        for (text, want) in cases {
            assert_eq!(lit(text), want, "{text}");
        }
        assert!(matches!(lit(r#"{"$numberDouble": "NaN"}"#), Bson::Double(f) if f.is_nan()));

        // Relaxed output of a document without Int64 parses back the same.
        let relaxed_doc = doc! {
            "oid": oid(), "date": date(MILLIS), "int": 3, "double": 2.5,
            "re": regex("x", "i"), "ts": Timestamp { time: 1, increment: 2 },
        };
        let relaxed = Bson::Document(relaxed_doc.clone())
            .into_relaxed_extjson()
            .to_string();
        assert_eq!(parse_document(&relaxed).unwrap(), relaxed_doc);

        // Wrappers inside queries; query operators that look similar stay documents.
        assert_eq!(
            parse_document(r#"{at: {$gte: {"$date": "2023-11-14T22:13:20.123Z"}}, n: {$regex: "a", $options: "i"}}"#)
                .unwrap(),
            doc! {"at": {"$gte": date(MILLIS)}, "n": {"$regex": "a", "$options": "i"}}
        );
    }

    #[test]
    fn bad_wrapper_is_error() {
        let bad = [
            r#"{"$oid": 5}"#,
            r#"{"$oid": "xyz"}"#,
            r#"{"$oid": "507f1f77bcf86cd799439011", "x": 1}"#,
            r#"{"x": 1, "$oid": "507f1f77bcf86cd799439011"}"#,
            r#"{"$date": "not a date"}"#,
            r#"{"$date": 1.5}"#,
            r#"{"$date": {"$numberLong": "x"}}"#,
            r#"{"$date": true}"#,
            r#"{"$numberLong": 5}"#,
            r#"{"$numberLong": "abc"}"#,
            r#"{"$numberLong": "9223372036854775808"}"#,
            r#"{"$numberInt": "2147483648"}"#,
            r#"{"$numberInt": 1}"#,
            r#"{"$numberDouble": "abc"}"#,
            r#"{"$numberDouble": "inf"}"#,
            r#"{"$numberDouble": 1.5}"#,
            r#"{"$numberDecimal": "x"}"#,
            r#"{"$binary": "AQI="}"#,
            r#"{"$binary": {"base64": "AQI="}}"#,
            r#"{"$binary": {"base64": "!!", "subType": "00"}}"#,
            r#"{"$binary": {"base64": "AQI=", "subType": "zz"}}"#,
            r#"{"$binary": {"base64": "AQI=", "subType": "00", "x": 1}}"#,
            r#"{"$uuid": "123"}"#,
            r#"{"$uuid": "0001020-30405-0607-0809-0a0b0c0d0e0f"}"#,
            r#"{"$date": "2020-01-01T00:00+1é1"}"#,
            r#"{"$regularExpression": {"pattern": "a"}}"#,
            r#"{"$regularExpression": {"pattern": "a", "options": "g"}}"#,
            r#"{"$timestamp": {"t": -1, "i": 0}}"#,
            r#"{"$timestamp": {"t": 1}}"#,
            r#"{"$timestamp": {"t": 4294967296, "i": 0}}"#,
            r#"{"$minKey": 2}"#,
            r#"{"$maxKey": "1"}"#,
            r#"{"$symbol": 1}"#,
            r#"{"$code": 1}"#,
            r#"{"$code": "f()", "$scope": 1}"#,
            r#"{"$code": "f()", "x": 1}"#,
            r#"{"$scope": {}}"#,
            r#"{"$undefined": false}"#,
            r#"{"$dbPointer": {"$ref": "c"}}"#,
        ];
        for text in bad {
            let e = lit_err(text);
            assert!(e.offset < text.len(), "{text}: {e:?}");
            // The wrapper check failed, not the lexer or the object syntax.
            let m = e.message.as_str();
            assert!(
                m.starts_with("Invalid $")
                    || m.starts_with("Extended JSON $")
                    || m.starts_with("Invalid ISO-8601 date: ")
                    || m.starts_with("Invalid regular expression flag "),
                "{text}: {e:?}"
            );
            // Never silently a sub-document, at any depth.
            assert!(
                parse_document(&format!("{{a: [{text}]}}")).is_err(),
                "{text}"
            );
        }
        let e = lit_err(r#"{"$oid": 5}"#);
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Invalid $oid: expected a 24-digit hex string", 9)
        );
        let e = lit_err(r#"{"$oid": "507f1f77bcf86cd799439011", "x": 1}"#);
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Extended JSON $oid must be the only key in its object", 0)
        );
        let e = lit_err(r#"{"$numberInt": "2147483648"}"#);
        assert_eq!(
            (e.message.as_str(), e.offset),
            (
                "Invalid $numberInt: expected a string holding a 32-bit integer",
                15
            )
        );
        // Non-ASCII date text (a multi-byte char where the zone is sliced) is an error, not a panic.
        for text in [
            r#"{"$date": "2020-01-01T00:00+1é1"}"#,
            r#"{"$date": "2020-01-01T00:00é"}"#,
            r#"{"$date": "2020-01-0é"}"#,
        ] {
            let e = lit_err(text);
            assert!(
                e.message.starts_with("Invalid ISO-8601 date: "),
                "{text}: {e:?}"
            );
            assert_eq!(e.offset, 10, "{text}");
        }
        // UUID: 32 hex digits or 8-4-4-4-12, nothing in between.
        assert_eq!(
            lit(r#"{"$uuid": "000102030405060708090a0b0c0d0e0f"}"#),
            uuid_bin()
        );
    }

    #[test]
    fn regex_literal() {
        assert_eq!(lit("/^a.b$/i"), regex("^a.b$", "i"));
        assert_eq!(lit(r"/a\/b[/]c/"), regex(r"a\/b[/]c", ""));
        // Options are stored sorted, as BSON requires.
        assert_eq!(lit("/x/smi"), regex("x", "ims"));
        assert_eq!(
            lit("{name: /^J/, tags: {$in: [/a/, /b/u]}}"),
            Bson::Document(
                doc! {"name": regex("^J", ""), "tags": {"$in": [regex("a", ""), regex("b", "u")]}}
            )
        );
        for (text, offset) in [("{a: /x/g}", 4), ("/x/ii", 0), ("/x/q", 0)] {
            let e = lit_err(text);
            assert!(
                e.message.starts_with("Invalid regular expression flag"),
                "{text}: {e:?}"
            );
            assert_eq!(e.offset, offset, "{text}");
        }
        let e = lit_err("{a: /open}");
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Unterminated regular expression", 4)
        );
    }

    #[test]
    fn errors_have_offsets() {
        let cases = [
            ("{a: 1, b: }", "Expected a value", 10),
            ("{a 1}", "Expected ':' after the key", 3),
            ("{a: 1 b: 2}", "Expected ',' or '}'", 6),
            ("[1 2]", "Expected ',' or ']'", 3),
            ("[1,,2]", "Expected a value", 3),
            ("{a: 'abc", "Unterminated string", 4),
            ("[1, 2", "Expected ',' or ']'", 5),
            ("{a: ", "Expected a value", 4),
            ("{: 1}", "Expected a key", 1),
            ("{0x1F: 1}", "Expected a key", 1),
            ("{a.b: 1}", "Expected ':' after the key", 2),
            ("{x: foo}", "Unexpected identifier foo", 4),
            (
                "{a: NumberInt(3000000000)}",
                "NumberInt() expects a 32-bit integer",
                14,
            ),
            (
                "{a: `x${y}`}",
                "Template literal substitutions (${…}) are not supported",
                6,
            ),
            ("{a: 1} /* open", "Unterminated comment", 7),
            // Offsets count bytes: `é` is two bytes.
            ("{'é': 1, b: }", "Expected a value", 13),
            ("{a: =>}", "Expected a value", 4),
        ];
        for (text, message, offset) in cases {
            let e = lit_err(text);
            assert_eq!((e.message.as_str(), e.offset), (message, offset), "{text}");
        }
        let e = parse_document("{a: 1} trailing").unwrap_err();
        assert_eq!(
            (e.message.as_str(), e.offset),
            ("Unexpected text after the document", 7)
        );
        let e = parse_document("  [1]").unwrap_err();
        assert_eq!((e.message.as_str(), e.offset), ("Expected a document", 2));
        let e = parse_document("").unwrap_err();
        assert_eq!((e.message.as_str(), e.offset), ("Expected a document", 0));
        // Deep nesting is an error, not a stack overflow.
        let deep = "[".repeat(100_000);
        let e = lit_err(&deep);
        assert_eq!(e.message, "Nested too deeply");
        assert!(e.offset < deep.len());
        assert_eq!(e.to_string(), "Nested too deeply");
    }
}
