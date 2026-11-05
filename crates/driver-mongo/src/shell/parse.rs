//! The console grammar (spec §4.2) and statement splitting (§4.1).
//!
//! Anything outside the grammar is refused with [`JS_REFUSED`]: the console never runs
//! JavaScript. Errors carry byte offsets into the command text.

use dnt_core::command::CommandSpan;
use mongodb::bson::{Bson, Document};

use super::literal::integral_value;
use super::{lex, lex_script, parse_literal, Piece, ShellError, Token, TokenKind};

/// The parse error of anything outside the grammar (spec §4.2).
pub const JS_REFUSED: &str =
    "Only MongoDB shell commands are supported; JavaScript is not executed";

/// One console command (spec §4.2).
#[derive(Debug, Clone, PartialEq)]
pub enum ShellCommand {
    /// `use <db>`; the name is valid ([`valid_db_name`]).
    Use(String),
    /// `show dbs` / `show databases`, `show collections` / `show tables`.
    Show(ShowWhat),
    /// A method of the database. `db`: the `getSiblingDB(…)` argument; `None` is the
    /// database in effect when the command runs. `modifiers` is always empty: modifiers
    /// are only valid after `find` and `aggregate`.
    Db {
        db: Option<String>,
        method: DbMethod,
        args: Vec<Bson>,
        modifiers: Vec<Modifier>,
    },
    /// A method of a collection (`coll` is the full dotted name: `system.users`). `db` as
    /// for [`ShellCommand::Db`]. `args` match the method's signature (checked at parse).
    /// `modifiers` follow `find` / `aggregate` in the order written (after `aggregate`,
    /// `sort` / `skip` / `limit` / `project` are stages appended to the pipeline as
    /// written, and `maxTimeMS` is its option). `find(…).count()` is parsed as
    /// `countDocuments` of the find's filter, keeping the modifiers written before it.
    /// `explain`: the command is explained (either position), not run.
    Coll {
        db: Option<String>,
        coll: String,
        method: CollMethod,
        args: Vec<Bson>,
        modifiers: Vec<Modifier>,
        explain: Option<Verbosity>,
    },
}

/// What `show` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShowWhat {
    Databases,
    Collections,
}

/// A cursor modifier kept for execution. `toArray()` and `pretty()` do nothing and are
/// not kept; `count()` and `explain()` change the command itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Modifier {
    Sort(Document),
    /// Any integer, as the server takes it.
    Limit(i64),
    /// Not negative.
    Skip(i64),
    /// `project(…)` or `projection(…)`.
    Project(Document),
    /// Not negative.
    MaxTimeMs(i64),
}

/// An enum of names as written in the console, with every value and a lookup.
macro_rules! names {
    ($(#[$meta:meta])* pub enum $name:ident { $($variant:ident = $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant,)+];

            /// The name as written in the console.
            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)+
                }
            }

            /// The value written as `text`, if any (case-sensitive).
            pub fn from_name(text: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.name() == text)
            }
        }
    };
}

names! {
    /// A method of the database (spec §4.2).
    pub enum DbMethod {
        GetCollectionNames = "getCollectionNames",
        Stats = "stats",
        ServerStatus = "serverStatus",
        Version = "version",
        CreateCollection = "createCollection",
        DropDatabase = "dropDatabase",
        RunCommand = "runCommand",
        AdminCommand = "adminCommand",
    }
}

names! {
    /// A method of a collection (spec §4.2).
    pub enum CollMethod {
        Find = "find",
        FindOne = "findOne",
        CountDocuments = "countDocuments",
        EstimatedDocumentCount = "estimatedDocumentCount",
        Distinct = "distinct",
        Aggregate = "aggregate",
        GetIndexes = "getIndexes",
        Stats = "stats",
        InsertOne = "insertOne",
        InsertMany = "insertMany",
        UpdateOne = "updateOne",
        UpdateMany = "updateMany",
        ReplaceOne = "replaceOne",
        DeleteOne = "deleteOne",
        DeleteMany = "deleteMany",
        FindOneAndUpdate = "findOneAndUpdate",
        FindOneAndReplace = "findOneAndReplace",
        FindOneAndDelete = "findOneAndDelete",
        CreateIndex = "createIndex",
        CreateIndexes = "createIndexes",
        DropIndex = "dropIndex",
        DropIndexes = "dropIndexes",
        RenameCollection = "renameCollection",
        Drop = "drop",
    }
}

names! {
    /// The verbosity of `explain` (spec §4.2); `queryPlanner` by default.
    pub enum Verbosity {
        QueryPlanner = "queryPlanner",
        ExecutionStats = "executionStats",
        AllPlansExecution = "allPlansExecution",
    }
}

/// A database name `use` and `getSiblingDB` accept: not empty, at most 64 bytes, none of
/// `/\. "$*<>:|?` (nor NUL).
pub fn valid_db_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.contains(|c| "/\\. \"$*<>:|?\0".contains(c))
}

const INVALID_DB: &str = "Invalid database name";
const INVALID_COLL: &str = "Invalid collection name";
const END: &str = "Unexpected end of command";
const SHOW_USAGE: &str = "Expected show dbs, show databases, show collections or show tables";
const EXPLAIN_USAGE: &str =
    r#"explain() expects "queryPlanner", "executionStats" or "allPlansExecution""#;

/// Words only JavaScript has here: a literal error on one is reported as [`JS_REFUSED`].
const JS_WORDS: &[&str] = &[
    "var",
    "let",
    "const",
    "function",
    "return",
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "class",
    "this",
    "typeof",
    "instanceof",
    "void",
    "delete",
    "async",
    "await",
    "yield",
    "try",
    "catch",
    "throw",
    "import",
    "export",
    "load",
    "eval",
    "print",
    "printjson",
];

/// Operators: a literal error on one is reported as [`JS_REFUSED`].
const JS_OPERATORS: &str = "=+-*/%<>!&|?~^";

/// Parses one command (one span of [`split`]; a trailing `;` is allowed). Returns it with
/// the offset a classification error ([`super::classify::classify`]) is reported at.
pub fn parse_command(text: &str) -> Result<(ShellCommand, usize), ShellError> {
    // `use` takes a raw name (`use 2024-logs`), which need not lex.
    if let Some(kw) = use_keyword(text) {
        return parse_use(text, kw + "use".len())
            .map(|(name, at, _)| (ShellCommand::Use(name), at));
    }
    let lead = text.len() - text.trim_start().len();
    let mut toks = lex(text)?;
    while matches!(
        toks.last(),
        Some(Token {
            kind: TokenKind::Punct(';'),
            ..
        })
    ) {
        toks.pop();
    }
    let mut p = Parser {
        toks: &toks,
        text,
        pos: 0,
    };
    let Some(first) = toks.first() else {
        return Err(ShellError::new("Expected a command", lead.min(text.len())));
    };
    match &first.kind {
        // `use` behind a comment is found by `use_keyword`; this is `use(…)`, `use.x`.
        TokenKind::Ident(w) if w == "use" => Err(ShellError::new(JS_REFUSED, first.start)),
        TokenKind::Ident(w) if w == "show" => p.show().map(|s| (s, first.start)),
        TokenKind::Ident(w) if w == "db" => p.db_stmt(),
        _ => Err(ShellError::new(JS_REFUSED, first.start)),
    }
}

/// `s` starts with the `use` keyword: `use` then whitespace, a quote or the end.
fn starts_with_use(s: &str) -> bool {
    s.strip_prefix("use").is_some_and(|rest| {
        rest.chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '`'))
    })
}

/// Where the `use` keyword of a `use` command starts: after leading whitespace, or behind
/// comments (the first token, when the text before the keyword lexes).
fn use_keyword(text: &str) -> Option<usize> {
    let lead = text.len() - text.trim_start().len();
    if starts_with_use(&text[lead..]) {
        return Some(lead);
    }
    // `lex` may fail after the keyword (`use 2024-logs`): lex only up to a `use`, which
    // must be the first token (not one in a comment: `/* use */ use x`, `// use⏎use x`).
    for (kw, _) in text.match_indices("use").take(16) {
        match lex(&text[..kw]) {
            Ok(before) if before.is_empty() => {}
            Ok(_) => return None,
            Err(_) => continue,
        }
        let end = kw + "use".len();
        if lex(&text[..end]).is_ok_and(|t| t.len() == 1 && t[0].start == kw) {
            return starts_with_use(&text[kw..]).then_some(kw);
        }
    }
    None
}

/// For a `use` command: where its `use` keyword starts and where its name ends (the raw
/// text, or the string literal with its quotes). Before the keyword and after the name
/// there are only whitespace, comments and `;`. `None` for any other text, and for a
/// `use` that does not parse.
pub(crate) fn use_range(text: &str) -> Option<(usize, usize)> {
    let kw = use_keyword(text)?;
    let (_, _, end) = parse_use(text, kw + "use".len()).ok()?;
    Some((kw, end))
}

/// The database name of `use`, which starts after byte `after`: a string literal, or
/// the raw text up to whitespace or `;`. Only whitespace, comments and `;` may follow.
/// Returns the name, and where it starts and ends.
fn parse_use(text: &str, after: usize) -> Result<(String, usize, usize), ShellError> {
    let shift = |e: ShellError, by: usize| ShellError::new(e.message, e.offset + by);
    let rest = &text[after..];
    let at = after + (rest.len() - rest.trim_start().len());
    let body = &text[at..];
    let (name, tail_at) = if body.starts_with(['\'', '"', '`']) {
        let toks = lex(body).map_err(|e| shift(e, at))?;
        match toks.first() {
            Some(Token {
                kind: TokenKind::Str(s),
                end,
                ..
            }) => (s.clone(), at + end),
            _ => return Err(ShellError::new(INVALID_DB, at)),
        }
    } else {
        let end = body
            .find(|c: char| c.is_whitespace() || c == ';')
            .unwrap_or(body.len());
        (body[..end].to_string(), at + end)
    };
    let tail = lex(&text[tail_at..]).map_err(|e| shift(e, tail_at))?;
    if let Some(t) = tail.iter().find(|t| t.kind != TokenKind::Punct(';')) {
        return Err(ShellError::new(INVALID_DB, tail_at + t.start));
    }
    if !valid_db_name(&name) {
        return Err(ShellError::new(INVALID_DB, at));
    }
    Ok((name, at, tail_at))
}

/// One call argument and where it starts.
struct Arg {
    value: Bson,
    start: usize,
}

fn values(args: Vec<Arg>) -> Vec<Bson> {
    args.into_iter().map(|a| a.value).collect()
}

/// What an argument position takes.
#[derive(Clone, Copy)]
enum Slot {
    Doc,
    /// An array of documents (a pipeline, documents to insert, index keys).
    DocArray,
    Str,
    Bool,
    /// An update document or an update pipeline.
    Update,
    /// An index name or key document.
    IndexName,
    /// An index name, key document, or array of names.
    Indexes,
    /// A positive scale or an options document (`stats`).
    Scale,
}

fn fits(slot: Slot, v: &Bson) -> bool {
    let docs = |a: &[Bson]| a.iter().all(|x| matches!(x, Bson::Document(_)));
    match slot {
        Slot::Doc => matches!(v, Bson::Document(_)),
        Slot::DocArray => matches!(v, Bson::Array(a) if docs(a)),
        Slot::Str => matches!(v, Bson::String(_)),
        Slot::Bool => matches!(v, Bson::Boolean(_)),
        Slot::Update => matches!(v, Bson::Document(_)) || matches!(v, Bson::Array(a) if docs(a)),
        Slot::IndexName => matches!(v, Bson::String(_) | Bson::Document(_)),
        Slot::Indexes => {
            matches!(v, Bson::String(_) | Bson::Document(_))
                || matches!(v, Bson::Array(a) if a.iter().all(|x| matches!(x, Bson::String(_))))
        }
        Slot::Scale => matches!(v, Bson::Document(_)) || integral_value(v).is_some_and(|n| n > 0),
    }
}

/// A method's arguments: the first `required` of `slots` must be there. `usage` is the
/// error message after `<method>() `.
struct Sig {
    required: usize,
    slots: &'static [Slot],
    usage: &'static str,
}

fn sig(required: usize, slots: &'static [Slot], usage: &'static str) -> Sig {
    Sig {
        required,
        slots,
        usage,
    }
}

fn coll_sig(m: CollMethod) -> Sig {
    use CollMethod::*;
    use Slot::*;
    const OPTIONS: &str = "expects an optional options document";
    const FIND: &str = "expects an optional filter, projection and options document";
    const FILTER: &str = "expects a filter and an optional options document";
    const UPDATE: &str =
        "expects a filter, an update document or pipeline, and an optional options document";
    const REPLACE: &str =
        "expects a filter, a replacement document and an optional options document";
    match m {
        Find | FindOne => sig(0, &[Doc, Doc, Doc], FIND),
        CountDocuments => sig(
            0,
            &[Doc, Doc],
            "expects an optional filter and options document",
        ),
        EstimatedDocumentCount | Drop => sig(0, &[Doc], OPTIONS),
        Distinct => sig(
            1,
            &[Str, Doc, Doc],
            "expects a field name and an optional filter and options document",
        ),
        Aggregate => sig(
            0,
            &[DocArray, Doc],
            "expects a pipeline (an array of stage documents) and an optional options document",
        ),
        GetIndexes => sig(0, &[], "takes no arguments"),
        Stats => sig(0, &[Scale], "expects an optional scale or options document"),
        InsertOne => sig(
            1,
            &[Doc, Doc],
            "expects a document and an optional options document",
        ),
        InsertMany => sig(
            1,
            &[DocArray, Doc],
            "expects an array of documents and an optional options document",
        ),
        UpdateOne | UpdateMany | FindOneAndUpdate => sig(2, &[Doc, Update, Doc], UPDATE),
        ReplaceOne | FindOneAndReplace => sig(2, &[Doc, Doc, Doc], REPLACE),
        DeleteOne | DeleteMany | FindOneAndDelete => sig(1, &[Doc, Doc], FILTER),
        CreateIndex => sig(
            1,
            &[Doc, Doc],
            "expects a key document and an optional options document",
        ),
        CreateIndexes => sig(
            1,
            &[DocArray, Doc],
            "expects an array of key documents and an optional options document",
        ),
        DropIndex => sig(1, &[IndexName], "expects an index name or key document"),
        DropIndexes => sig(
            0,
            &[Indexes],
            "expects an optional index name, key document or array of names",
        ),
        RenameCollection => sig(
            1,
            &[Str, Bool],
            "expects a new name and an optional dropTarget boolean",
        ),
    }
}

fn db_sig(m: DbMethod) -> Sig {
    use DbMethod::*;
    use Slot::*;
    match m {
        GetCollectionNames => sig(0, &[Doc], "expects an optional filter document"),
        Stats => sig(0, &[Scale], "expects an optional scale or options document"),
        ServerStatus => sig(0, &[Doc], "expects an optional options document"),
        Version => sig(0, &[], "takes no arguments"),
        CreateCollection => sig(
            1,
            &[Str, Doc],
            "expects a collection name and an optional options document",
        ),
        DropDatabase => sig(0, &[Doc], "expects an optional write concern document"),
        RunCommand | AdminCommand => sig(1, &[Doc], "expects a command document"),
    }
}

/// The arguments of `method(…)` against `sig`; `at` is the method name (where a missing
/// argument is reported).
fn check(sig: &Sig, method: &str, args: &[Arg], at: usize) -> Result<(), ShellError> {
    let wrong = |at: usize| Err(ShellError::new(format!("{method}() {}", sig.usage), at));
    if args.len() < sig.required {
        return wrong(at);
    }
    if let Some(extra) = args.get(sig.slots.len()) {
        return wrong(extra.start);
    }
    match args
        .iter()
        .zip(sig.slots)
        .find(|(a, slot)| !fits(**slot, &a.value))
    {
        Some((a, _)) => wrong(a.start),
        None => Ok(()),
    }
}

/// A modifier name (spec §4.2).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModName {
    Sort,
    Limit,
    Skip,
    Project,
    MaxTimeMs,
    Count,
    ToArray,
    Pretty,
    Explain,
}

fn modifier(name: &str) -> Option<ModName> {
    Some(match name {
        "sort" => ModName::Sort,
        "limit" => ModName::Limit,
        "skip" => ModName::Skip,
        "project" | "projection" => ModName::Project,
        "maxTimeMS" => ModName::MaxTimeMs,
        "count" => ModName::Count,
        "toArray" => ModName::ToArray,
        "pretty" => ModName::Pretty,
        "explain" => ModName::Explain,
        _ => return None,
    })
}

fn misplaced(m: &str, after: &str, at: usize) -> ShellError {
    ShellError::new(format!(".{m}() is not valid after {after}()"), at)
}

fn unknown_method(name: &str, at: usize) -> ShellError {
    ShellError::new(format!("Unknown method {name}()"), at)
}

/// Where an error about `args` points: the first argument, else the call.
fn args_at(args: &[Arg], at: usize) -> usize {
    args.first().map_or(at, |a| a.start)
}

fn one_doc(args: &[Arg], m: &str, at: usize) -> Result<Document, ShellError> {
    match args {
        [Arg {
            value: Bson::Document(d),
            ..
        }] => Ok(d.clone()),
        _ => Err(ShellError::new(
            format!("{m}() expects a document"),
            args_at(args, at),
        )),
    }
}

fn one_int(args: &[Arg], m: &str, at: usize, non_negative: bool) -> Result<i64, ShellError> {
    let n = match args {
        [a] => integral_value(&a.value).filter(|n| !non_negative || *n >= 0),
        _ => None,
    };
    n.ok_or_else(|| {
        let what = if non_negative {
            "a non-negative integer"
        } else {
            "an integer"
        };
        ShellError::new(format!("{m}() expects {what}"), args_at(args, at))
    })
}

fn no_args(args: &[Arg], m: &str) -> Result<(), ShellError> {
    match args.first() {
        None => Ok(()),
        Some(a) => Err(ShellError::new(
            format!("{m}() takes no arguments"),
            a.start,
        )),
    }
}

/// The argument of `explain(…)`.
fn verbosity(args: &[Arg]) -> Result<Verbosity, ShellError> {
    match args {
        [] => Ok(Verbosity::QueryPlanner),
        [Arg {
            value: Bson::String(s),
            start,
        }] => Verbosity::from_name(s).ok_or_else(|| ShellError::new(EXPLAIN_USAGE, *start)),
        [a] | [_, a, ..] => Err(ShellError::new(EXPLAIN_USAGE, a.start)),
    }
}

struct Parser<'a> {
    toks: &'a [Token],
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn kind(&self, at: usize) -> Option<&'a TokenKind> {
        self.toks.get(at).map(|t| &t.kind)
    }

    /// Byte offset of the current token, or the end of the text.
    fn offset(&self) -> usize {
        self.toks.get(self.pos).map_or(self.text.len(), |t| t.start)
    }

    fn punct_at(&self, at: usize, c: char) -> bool {
        matches!(self.kind(at), Some(TokenKind::Punct(p)) if *p == c)
    }

    fn at_punct(&self, c: char) -> bool {
        self.punct_at(self.pos, c)
    }

    fn eat(&mut self, c: char) -> bool {
        let hit = self.at_punct(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn ident(&self) -> Option<&'a str> {
        match self.kind(self.pos) {
            Some(TokenKind::Ident(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// `name(` at the current token.
    fn at_call(&self, name: &str) -> bool {
        self.ident() == Some(name) && self.punct_at(self.pos + 1, '(')
    }

    /// The current token is outside the grammar.
    fn unexpected(&self) -> ShellError {
        match self.toks.get(self.pos) {
            None => ShellError::new(END, self.text.len()),
            Some(t) => ShellError::new(JS_REFUSED, t.start),
        }
    }

    fn expect(&mut self, c: char) -> Result<(), ShellError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    /// A literal's error, as [`JS_REFUSED`] when it is on an operator or a JavaScript word.
    fn literal_error(&self, e: ShellError) -> ShellError {
        let Some(k) = self.toks.iter().position(|t| t.start == e.offset) else {
            return e;
        };
        let js = match &self.toks[k].kind {
            TokenKind::Punct(c) => JS_OPERATORS.contains(*c),
            TokenKind::Ident(w) => JS_WORDS.contains(&w.as_str()) || self.punct_at(k + 1, '='),
            _ => false,
        };
        if js {
            ShellError::new(JS_REFUSED, e.offset)
        } else {
            e
        }
    }

    /// `(args)`; the current token is `(`. A trailing comma is allowed.
    fn args(&mut self) -> Result<Vec<Arg>, ShellError> {
        self.pos += 1;
        let mut args = Vec::new();
        loop {
            if self.eat(')') {
                return Ok(args);
            }
            if self.pos >= self.toks.len() {
                return Err(self.unexpected());
            }
            let start = self.offset();
            let value = parse_literal(self.toks, self.text, &mut self.pos)
                .map_err(|e| self.literal_error(e))?;
            args.push(Arg { value, start });
            if !self.at_punct(')') && !self.eat(',') {
                return Err(self.unexpected());
            }
        }
    }

    /// The end of a command whose last call was `after()`: a modifier here is
    /// misplaced, anything else is outside the grammar.
    fn finish(&self, after: &str) -> Result<(), ShellError> {
        if self.at_punct('.') {
            if let Some(TokenKind::Ident(m)) = self.kind(self.pos + 1) {
                if modifier(m).is_some() && self.punct_at(self.pos + 2, '(') {
                    return Err(misplaced(m, after, self.offset()));
                }
            }
        }
        match self.toks.get(self.pos) {
            None => Ok(()),
            Some(_) => Err(self.unexpected()),
        }
    }

    fn show(&self) -> Result<ShellCommand, ShellError> {
        let what = match self.kind(1) {
            Some(TokenKind::Ident(w)) => match w.as_str() {
                "dbs" | "databases" => Some(ShowWhat::Databases),
                "collections" | "tables" => Some(ShowWhat::Collections),
                _ => None,
            },
            _ => None,
        };
        let at = |i: usize| self.toks.get(i).map_or(self.text.len(), |t| t.start);
        match (what, self.toks.get(2)) {
            (Some(w), None) => Ok(ShellCommand::Show(w)),
            (Some(_), Some(_)) => Err(ShellError::new(SHOW_USAGE, at(2))),
            (None, _) => Err(ShellError::new(SHOW_USAGE, at(1))),
        }
    }

    /// The offset of the top-level `key` of the document literal at byte `open`, or
    /// `open`.
    fn key_offset(&self, open: usize, key: &str) -> usize {
        let Some(first) = self.toks.iter().position(|t| t.start == open) else {
            return open;
        };
        let mut depth = 0usize;
        for (i, t) in self.toks.iter().enumerate().skip(first) {
            match &t.kind {
                TokenKind::Punct('(' | '[' | '{') => depth += 1,
                TokenKind::Punct(')' | ']' | '}') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::Ident(k) | TokenKind::Str(k)
                    if depth == 1 && k == key && self.punct_at(i + 1, ':') =>
                {
                    return t.start;
                }
                _ => {}
            }
        }
        open
    }

    /// A string argument naming something (`getSiblingDB`, `getCollection`).
    fn name_arg(&mut self, usage: &str) -> Result<(String, usize), ShellError> {
        let at = self.offset();
        self.pos += 1;
        let args = self.args()?;
        match args.as_slice() {
            [Arg {
                value: Bson::String(s),
                start,
            }] => Ok((s.clone(), *start)),
            [a, ..] => Err(ShellError::new(usage, a.start)),
            [] => Err(ShellError::new(usage, at)),
        }
    }

    /// `db…`; the first token is `db`.
    fn db_stmt(&mut self) -> Result<(ShellCommand, usize), ShellError> {
        self.pos = 1;
        let mut db = None;
        if !self.at_punct('[') {
            self.expect('.')?;
            if self.at_call("getSiblingDB") {
                let (name, at) = self.name_arg("getSiblingDB() expects a database name")?;
                if !valid_db_name(&name) {
                    return Err(ShellError::new(INVALID_DB, at));
                }
                db = Some(name);
                if !self.at_punct('[') {
                    self.expect('.')?;
                }
            }
        }
        // collRef: `[name]`, `getCollection(name)`, or names before the method.
        let mut coll = None;
        if self.at_punct('[') {
            let at = self.offset();
            self.pos += 1;
            let Some(TokenKind::Str(name)) = self.kind(self.pos) else {
                return Err(self.unexpected());
            };
            self.pos += 1;
            self.expect(']')?;
            self.expect('.')?;
            coll = Some((name.clone(), at));
        } else if self.at_call("getCollection") {
            let at = self.offset();
            let (name, _) = self.name_arg("getCollection() expects a collection name")?;
            self.expect('.')?;
            coll = Some((name, at));
        }
        // `a.b.c(`: the name before `(` is the method, the names before it the collection.
        let mut chain: Vec<(&'a str, usize)> = Vec::new();
        loop {
            let Some(name) = self.ident() else {
                return Err(self.unexpected());
            };
            chain.push((name, self.offset()));
            self.pos += 1;
            if self.at_punct('(') {
                break;
            }
            self.expect('.')?;
        }
        let (method, method_at) = chain.pop().expect("one name was read");
        let names = chain.iter().map(|(n, _)| *n);
        let coll = match (coll, chain.first()) {
            (Some((c, at)), _) => Some((
                std::iter::once(c.as_str())
                    .chain(names)
                    .collect::<Vec<_>>()
                    .join("."),
                at,
            )),
            (None, Some(&(_, at))) => Some((names.collect::<Vec<_>>().join("."), at)),
            (None, None) => None,
        };
        match coll {
            None => self.db_method(db, method, method_at),
            Some((c, at)) => {
                if c.is_empty() || c.contains('\0') {
                    return Err(ShellError::new(INVALID_COLL, at));
                }
                if method == "explain" {
                    self.explain_prefix(db, c, method_at)
                } else {
                    self.coll_method(db, c, method, method_at)
                }
            }
        }
    }

    fn db_method(
        &mut self,
        db: Option<String>,
        name: &str,
        at: usize,
    ) -> Result<(ShellCommand, usize), ShellError> {
        let method = DbMethod::from_name(name).ok_or_else(|| unknown_method(name, at))?;
        let args = self.args()?;
        check(&db_sig(method), name, &args, at)?;
        if method == DbMethod::CreateCollection {
            if let [Arg {
                value: Bson::String(c),
                start,
            }, ..] = args.as_slice()
            {
                if c.is_empty() || c.contains('\0') {
                    return Err(ShellError::new(INVALID_COLL, *start));
                }
            }
        }
        if matches!(method, DbMethod::RunCommand | DbMethod::AdminCommand) {
            if let [Arg {
                value: Bson::Document(d),
                start,
            }, ..] = args.as_slice()
            {
                // Session and transaction fields are the driver's; `$` fields are wire
                // protocol (`$db` would retarget the command).
                if let Some(k) = d.keys().find(|k| {
                    k.starts_with('$')
                        || matches!(
                            k.as_str(),
                            "lsid" | "txnNumber" | "startTransaction" | "autocommit"
                        )
                }) {
                    return Err(ShellError::new(
                        format!("{k} is not allowed in a console command"),
                        self.key_offset(*start, k),
                    ));
                }
            }
        }
        self.finish(name)?;
        let classify_at = args_at(&args, at);
        Ok((
            ShellCommand::Db {
                db,
                method,
                args: values(args),
                modifiers: Vec::new(),
            },
            classify_at,
        ))
    }

    /// `coll.method(…)` and its modifiers; the current token is `(`.
    fn coll_method(
        &mut self,
        db: Option<String>,
        coll: String,
        name: &'a str,
        at: usize,
    ) -> Result<(ShellCommand, usize), ShellError> {
        let mut method = CollMethod::from_name(name).ok_or_else(|| unknown_method(name, at))?;
        let args = self.args()?;
        check(&coll_sig(method), name, &args, at)?;
        let mut args = values(args);
        let mut modifiers = Vec::new();
        let mut explain = None;
        let mut classify_at = at;
        // The last call, for a misplaced modifier; whether modifiers may follow it.
        let mut after: &str = name;
        let mut open = matches!(method, CollMethod::Find | CollMethod::Aggregate);
        while self.at_punct('.') {
            let dot = self.offset();
            let (Some(TokenKind::Ident(m)), true) =
                (self.kind(self.pos + 1), self.punct_at(self.pos + 2, '('))
            else {
                return Err(self.unexpected());
            };
            let Some(kind) = modifier(m) else {
                return Err(self.unexpected());
            };
            if !open || (kind == ModName::Count && method != CollMethod::Find) {
                return Err(misplaced(m, after, dot));
            }
            let m_at = self.toks[self.pos + 1].start;
            self.pos += 2;
            let margs = self.args()?;
            match kind {
                ModName::Sort => modifiers.push(Modifier::Sort(one_doc(&margs, m, m_at)?)),
                ModName::Project => modifiers.push(Modifier::Project(one_doc(&margs, m, m_at)?)),
                ModName::Limit => modifiers.push(Modifier::Limit(one_int(&margs, m, m_at, false)?)),
                ModName::Skip => modifiers.push(Modifier::Skip(one_int(&margs, m, m_at, true)?)),
                ModName::MaxTimeMs => {
                    modifiers.push(Modifier::MaxTimeMs(one_int(&margs, m, m_at, true)?))
                }
                ModName::ToArray | ModName::Pretty => no_args(&margs, m)?,
                ModName::Count => {
                    no_args(&margs, m)?;
                    method = CollMethod::CountDocuments;
                    args.truncate(1);
                    after = "count";
                    open = false;
                }
                ModName::Explain => {
                    explain = Some(verbosity(&margs)?);
                    classify_at = args_at(&margs, m_at);
                    after = "explain";
                    open = false;
                }
            }
        }
        if self.pos < self.toks.len() {
            return Err(self.unexpected());
        }
        Ok((
            ShellCommand::Coll {
                db,
                coll,
                method,
                args,
                modifiers,
                explain,
            },
            classify_at,
        ))
    }

    /// `coll.explain(verbosity?).method(…)`; the current token is the `(` of `explain`.
    fn explain_prefix(
        &mut self,
        db: Option<String>,
        coll: String,
        at: usize,
    ) -> Result<(ShellCommand, usize), ShellError> {
        let vargs = self.args()?;
        let v = verbosity(&vargs)?;
        self.expect('.')?;
        let Some(name) = self.ident() else {
            return Err(self.unexpected());
        };
        let name_at = self.offset();
        self.pos += 1;
        if !self.at_punct('(') {
            return Err(self.unexpected());
        }
        let method = CollMethod::from_name(name).ok_or_else(|| unknown_method(name, name_at))?;
        let args = self.args()?;
        check(&coll_sig(method), name, &args, name_at)?;
        // The grammar takes no modifiers after this form.
        self.finish("explain")?;
        Ok((
            ShellCommand::Coll {
                db,
                coll,
                method,
                args: values(args),
                modifiers: Vec::new(),
                explain: Some(v),
            },
            args_at(&vargs, at),
        ))
    }
}

/// The commands of a script (spec §4.1): a command ends at `;`, or at the end of a line
/// once its brackets are closed, unless the next line (past blank lines and comments)
/// starts with `.`. Strings, regexes and comments never split. Never panics; text that
/// does not lex (a bad line, with the lines back to where an unfinished string, template
/// or comment began) is one command, and the rules resume after it.
pub fn split(text: &str) -> Vec<CommandSpan> {
    let mut out = Vec::new();
    for piece in lex_script(text) {
        match piece {
            Piece::Tokens(tokens) => token_spans(text, &tokens, &mut out),
            // Text that does not lex is one command, however many lines it has: read a
            // line at a time, what JavaScript reads as a comment, string or template
            // could become a command of its own (`/* off:⏎db.c.drop()⏎*/ 'oops`).
            Piece::Raw(from, to) | Piece::Unclosed(from, to) => {
                let body = text.get(from..to).unwrap_or_default();
                let start = from + (body.len() - body.trim_start().len());
                let end = from + body.trim_end().len();
                if start < end {
                    out.push(span(text, start, end));
                }
            }
        }
    }
    out
}

fn span(text: &str, start: usize, end: usize) -> CommandSpan {
    CommandSpan {
        range: (start, end),
        text: text.get(start..end).unwrap_or_default().to_string(),
    }
}

/// The commands of `tokens` (§4.1).
fn token_spans(text: &str, tokens: &[Token], out: &mut Vec<CommandSpan>) {
    let span = |start: usize, end: usize| span(text, start, end);
    let mut first: Option<usize> = None;
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        if t.kind == TokenKind::Punct(';') {
            if let Some(f) = first.take() {
                out.push(span(tokens[f].start, tokens[i - 1].end));
            }
            depth = 0;
            continue;
        }
        first.get_or_insert(i);
        match t.kind {
            TokenKind::Punct('(' | '[' | '{') => depth += 1,
            TokenKind::Punct(')' | ']' | '}') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth > 0 {
            continue;
        }
        let ends = match tokens.get(i + 1) {
            None => true,
            Some(next) => {
                let gap = text.get(t.end..next.start).unwrap_or_default();
                gap.contains(['\n', '\r'])
                    && !text.get(next.start..).is_some_and(|s| s.starts_with('.'))
            }
        };
        if ends {
            if let Some(f) = first.take() {
                out.push(span(tokens[f].start, t.end));
            }
        }
    }
    // Brackets still open at the end: the rest is one command (it will not parse).
    if let (Some(f), Some(last)) = (first, tokens.last()) {
        out.push(span(tokens[f].start, last.end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{MongoLanguage, Payload};
    use dnt_core::command::{CommandClass, CommandLanguage, ParsedCommand};
    use mongodb::bson::doc;

    fn spans(text: &str) -> Vec<String> {
        let out = split(text);
        for s in &out {
            assert_eq!(&text[s.range.0..s.range.1], s.text, "{text:?}");
        }
        out.into_iter().map(|s| s.text).collect()
    }

    fn parsed(text: &str) -> ParsedCommand {
        MongoLanguage
            .parse(text)
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    fn cmd(text: &str) -> ShellCommand {
        parsed(text)
            .payload
            .downcast_ref::<Payload>()
            .expect("the driver's payload")
            .cmd
            .clone()
    }

    fn err(text: &str) -> (String, Option<usize>) {
        match MongoLanguage.parse(text) {
            Ok(c) => panic!("{text}: parsed as {c:?}"),
            Err(e) => (e.message, e.offset),
        }
    }

    fn coll(c: &str, method: CollMethod, args: Vec<Bson>) -> ShellCommand {
        ShellCommand::Coll {
            db: None,
            coll: c.into(),
            method,
            args,
            modifiers: vec![],
            explain: None,
        }
    }

    #[test]
    fn split_multiline_and_chained_lines() {
        let text = "db.c.find({\n  a: 1\n})\n  .sort({a: -1})\n\n  // keep going\n  .limit(5)\n\
                    db.d.insertOne({x: 1}); show dbs\nuse 2024_logs\n\n/* note */ db.e.find() // trailing\n";
        assert_eq!(
            spans(text),
            [
                "db.c.find({\n  a: 1\n})\n  .sort({a: -1})\n\n  // keep going\n  .limit(5)",
                "db.d.insertOne({x: 1})",
                "show dbs",
                "use 2024_logs",
                "db.e.find()",
            ]
        );
        // Open brackets carry a command over lines.
        assert_eq!(
            spans("db.c.insertMany([\n{a: 1},\n{a: 2}\n])\ndb.c.find()"),
            ["db.c.insertMany([\n{a: 1},\n{a: 2}\n])", "db.c.find()"]
        );
        assert_eq!(
            spans("show dbs\r\nshow collections\r\n"),
            ["show dbs", "show collections"]
        );
        // Ranges are bytes.
        let s = split("é;db.c.find()");
        assert_eq!(
            s.iter().map(|s| s.range).collect::<Vec<_>>(),
            [(0, 2), (3, 14)]
        );
        assert!(split(";; \n // only a comment\n").is_empty());
        // Text that does not lex: one command per line from the bad line on.
        let text = "db.a.find({\n x: 1\n})\ndb.b.find({x: 'open)\ndb.c.find()";
        assert_eq!(
            spans(text),
            [
                "db.a.find({\n x: 1\n})",
                "db.b.find({x: 'open)",
                "db.c.find()"
            ]
        );
        assert_eq!(
            err("db.b.find({x: 'open)"),
            ("Unterminated string".into(), Some(14))
        );
        // After a bad line the rules of §4.1 resume: a chained command stays whole and
        // `;` still splits.
        let text = "db.x.find({a: 'bad)\ndb.c.aggregate([{$out: 'x'}])\n  .explain()\n\
                    db.a.find(); db.b.find()";
        assert_eq!(
            spans(text),
            [
                "db.x.find({a: 'bad)",
                "db.c.aggregate([{$out: 'x'}])\n  .explain()",
                "db.a.find()",
                "db.b.find()",
            ]
        );
        // Brackets still open where the text stops lexing, or at the end: one command.
        assert_eq!(
            spans("db.c.insertOne({\n  a: 'bad\n})\ndb.d.find()"),
            ["db.c.insertOne({", "a: 'bad", "})", "db.d.find()"]
        );
        assert_eq!(spans("db.c.find({"), ["db.c.find({"]);
        assert_eq!(
            spans("db.c.insertOne({a: 1}); db.d.insertOne({\n b: 'bad\n})"),
            [
                "db.c.insertOne({a: 1})",
                "db.d.insertOne({",
                "b: 'bad",
                "})"
            ]
        );
        // An unterminated template or comment runs to the end: nothing inside it is
        // lexed as a command of its own.
        for (text, want) in [
            (
                "db.e.find({\n  b: `bad\n}); db.c.drop()",
                vec!["db.e.find({\n  b:", "`bad\n}); db.c.drop()"],
            ),
            (
                "db.c.find({a: 'x'}); db.e.find({\n  b: `bad\n}); db.c.drop()",
                vec![
                    "db.c.find({a: 'x'})",
                    "db.e.find({\n  b:",
                    "`bad\n}); db.c.drop()",
                ],
            ),
            (
                "db.a.find() /* note\ndb.c.drop()\n",
                vec!["db.a.find()", "/* note\ndb.c.drop()"],
            ),
            (
                "db.a.find({t: `${x}\n`}); db.c.drop()",
                vec!["db.a.find({t:", "`${x}\n`}); db.c.drop()"],
            ),
            (
                "db.a.find({t: `\\1\n`})\ndb.c.drop()",
                vec!["db.a.find({t:", "`\\1\n`})\ndb.c.drop()"],
            ),
        ] {
            let got = spans(text);
            assert_eq!(got, want, "{text:?}");
            assert!(got.iter().all(|s| s != "db.c.drop()"), "{text:?}");
        }
        // A string continued over a line (`\` at the line end) that does not end, or
        // meets a plain line break later: from its quote, the rest is one command.
        for (text, want) in [
            (
                "db.c.insertOne({note: 'do not run \\\ndb.c.drop()",
                vec!["db.c.insertOne({note:", "'do not run \\\ndb.c.drop()"],
            ),
            (
                "db.c.find({a: \"x\\\ndb.c.drop()\ny\"})",
                vec!["db.c.find({a:", "\"x\\\ndb.c.drop()\ny\"})"],
            ),
            (
                "db.c.find({a: 'x\\\r\ny\\x'})\ndb.c.drop()",
                vec!["db.c.find({a:", "'x\\\r\ny\\x'})\ndb.c.drop()"],
            ),
        ] {
            let got = spans(text);
            assert_eq!(got, want, "{text:?}");
            assert!(got.iter().all(|s| s != "db.c.drop()"), "{text:?}");
        }
        // A string continued over a line that does end is fine.
        assert_eq!(
            spans("db.c.find({a: 'x\\\ny'})\ndb.d.find()"),
            ["db.c.find({a: 'x\\\ny'})", "db.d.find()"]
        );
        // A lone CR ends a line as LF does.
        assert_eq!(
            spans("db.a.find({x: 'bad)\rdb.c.find()\rshow dbs"),
            ["db.a.find({x: 'bad)", "db.c.find()", "show dbs"]
        );
        // A bad escape in a string is one bad line; the next command is its own.
        assert_eq!(
            spans("db.a.find({t: '\\1'})\ndb.c.find()"),
            ["db.a.find({t: '\\1'})", "db.c.find()"]
        );
        // `use` names the lexer refuses, after `;` too.
        assert_eq!(
            spans("db.a.find(); use 2024_logs\ndb.b.find()"),
            ["db.a.find()", "use 2024_logs", "db.b.find()"]
        );
    }

    #[test]
    fn split_never_runs_what_javascript_reads_as_text() {
        for text in [
            // A bad region over lines is one command (a construct that ended before the
            // bad line, read a line at a time, would yield its content as commands).
            "/* disabled:\ndb.c.drop()\n*/ x = 'oops",
            "db.x.insertOne({a: `line one\ndb.c.drop()\nend`, b: 'oops",
            "/* note\ndb.c.drop() // */ x 'oops",
            "db.x.insertOne({a: `note\ndb.c.drop()//`, b: 'oops",
            "db.x.insertOne({a: 'note \\\ndb.c.drop()//', b: 'oops",
            "/*)\r/*/*db.c.insertOne({pwd: 'HUNTER9'})\":/*/db.c.drop()",
            "'}\\\ndb.c.drop()//db.c.insertOne({pwd: 'HUNTER9'})})\n\"",
            // After an error JavaScript may go on into a template, a comment or a
            // continued string on the same line (`\1` and `1_0` are JavaScript).
            "'\\1' + `\ndb.c.drop()\n`",
            "x = 1_0 /*\ndb.c.drop()\n*/",
            "x = 1_0 + 'a\\\ndb.c.drop()'",
            // `/` that JavaScript reads the other way.
            "a++ /x/*\ndb.c.drop()\n*/",
            "typeof /`/ + `\ndb.c.drop()\n`",
            "if (a) /`/ + `\ndb.c.drop()\n`",
            "x)\n./ db.c.find()//;db.c.drop()",
            "await)\n./***/ /mongodb://;db.c.drop()",
            "a\u{301}/x//;db.c.drop()",
            "f(1_0)\n/x//;db.c.drop()",
            "db.  --> ('pwd'db['c'].find()\r/x//* c */;db.c.drop()",
            // Comments JavaScript has and the lexer did not, or ended elsewhere.
            "// a\r/*\ndb.c.drop()\n*/",
            "// a\u{2028}/*\ndb.c.drop()\n*/",
            "<!-- x; db.c.drop()",
            "x\n--> ; db.c.drop()",
            "#! x; db.c.drop()",
        ] {
            for s in spans(text) {
                if let Ok(p) = MongoLanguage.parse(&s) {
                    assert_ne!(p.name, "drop", "{text:?} runs {s:?}");
                }
            }
        }
        // What JavaScript reads the same way still splits as before.
        for (text, want) in [
            (
                "db.c.find({a: /x/})\ndb.d.find({b: [/y/i, /z/]})",
                vec!["db.c.find({a: /x/})", "db.d.find({b: [/y/i, /z/]})"],
            ),
            (
                "db.c.find({a: 1}) // note\rdb.d.find()",
                vec!["db.c.find({a: 1})", "db.d.find()"],
            ),
            ("\u{feff}show dbs", vec!["show dbs"]),
            (
                "db.c.find({x: 'bad)\ndb.d.find()",
                vec!["db.c.find({x: 'bad)", "db.d.find()"],
            ),
        ] {
            assert_eq!(spans(text), want, "{text:?}");
        }
    }

    #[test]
    fn use_behind_a_comment() {
        assert_eq!(cmd("/* use */ use x"), ShellCommand::Use("x".into()));
        assert_eq!(cmd("// use\nuse x"), ShellCommand::Use("x".into()));
        assert_eq!(
            cmd("// use\r\n/* use */use 01-x"),
            ShellCommand::Use("01-x".into())
        );
        assert_eq!(parsed("db.c.find({a: 'use x'})").name, "find");
        assert_eq!(
            cmd("/* note */ use 2024-logs"),
            ShellCommand::Use("2024-logs".into())
        );
        assert_eq!(err("/* c */ use(1)").0, JS_REFUSED);
        assert_eq!(use_range("  use x // c"), Some((2, 7)));
        assert_eq!(use_range("/* c */ use 'a-b';"), Some((8, 17)));
        assert_eq!(use_range("use 'a b'"), None);
        assert_eq!(use_range("db.c.find()"), None);
    }

    #[test]
    fn run_command_refuses_session_fields() {
        for (text, key, offset) in [
            ("db.runCommand({find: 'c', $db: 'other'})", "$db", 26),
            ("db.adminCommand({ping: 1, lsid: {id: 1}})", "lsid", 26),
            (
                "db.runCommand({insert: 'c', documents: [], txnNumber: 1})",
                "txnNumber",
                43,
            ),
            (
                "db.runCommand({find: 'c', startTransaction: true})",
                "startTransaction",
                26,
            ),
            (
                "db.runCommand({find: 'c', 'autocommit': false})",
                "autocommit",
                26,
            ),
            (
                "db.runCommand({$readPreference: {mode: 'primary'}, ping: 1})",
                "$readPreference",
                15,
            ),
            (
                "db.getSiblingDB('x').runCommand({ping: 1, \"$clusterTime\": {}})",
                "$clusterTime",
                42,
            ),
        ] {
            assert_eq!(
                err(text),
                (
                    format!("{key} is not allowed in a console command"),
                    Some(offset)
                ),
                "{text}"
            );
        }
        // Nested documents and other methods are not checked.
        parsed("db.runCommand({explain: {find: 'c', filter: {$db: 1}}})");
        parsed("db.c.find({lsid: 1, $and: [{a: 1}]})");
    }

    #[test]
    fn split_ignores_semicolons_in_strings_and_regex() {
        let text = "db.c.find({a: \"x;y\", b: 'p;q', c: `t;\nu`, d: /;/i}); db.c.find({e: 1}) // no; split\n\
                    /* a;\n b */ show dbs";
        assert_eq!(
            spans(text),
            [
                "db.c.find({a: \"x;y\", b: 'p;q', c: `t;\nu`, d: /;/i})",
                "db.c.find({e: 1})",
                "show dbs",
            ]
        );
    }

    #[test]
    fn every_stmt_form_parses() {
        use CollMethod::*;
        use ShellCommand::{Db, Show, Use};
        assert_eq!(cmd("use test"), Use("test".into()));
        assert_eq!(cmd("use 'my_db';"), Use("my_db".into()));
        assert_eq!(cmd("use 2024-logs // switch"), Use("2024-logs".into()));
        assert_eq!(cmd("use é"), Use("é".into()));
        for (text, what) in [
            ("show dbs", ShowWhat::Databases),
            ("show databases", ShowWhat::Databases),
            ("show collections", ShowWhat::Collections),
            ("show tables;", ShowWhat::Collections),
        ] {
            assert_eq!(cmd(text), Show(what), "{text}");
        }

        let db_forms = [
            ("db.getCollectionNames()", DbMethod::GetCollectionNames),
            ("db.stats(1024)", DbMethod::Stats),
            ("db.serverStatus()", DbMethod::ServerStatus),
            ("db.version()", DbMethod::Version),
            (
                "db.createCollection('c', {capped: true, size: 4096})",
                DbMethod::CreateCollection,
            ),
            ("db.dropDatabase()", DbMethod::DropDatabase),
            ("db.runCommand({ping: 1})", DbMethod::RunCommand),
            (
                "db.adminCommand({listDatabases: 1})",
                DbMethod::AdminCommand,
            ),
        ];
        assert_eq!(db_forms.len(), DbMethod::ALL.len());
        for (text, m) in db_forms {
            assert!(
                matches!(cmd(text), Db { db: None, method, ref modifiers, .. } if method == m && modifiers.is_empty()),
                "{text}"
            );
        }
        assert_eq!(
            cmd("db.runCommand({ping: 1})"),
            Db {
                db: None,
                method: DbMethod::RunCommand,
                args: vec![doc! {"ping": 1}.into()],
                modifiers: vec![],
            }
        );

        let coll_forms = [
            ("db.c.find({a: 1}, {b: 1})", Find),
            (
                "db.c.findOne({_id: ObjectId('507f1f77bcf86cd799439011')})",
                FindOne,
            ),
            ("db.c.countDocuments({a: {$gt: 1}})", CountDocuments),
            ("db.c.estimatedDocumentCount()", EstimatedDocumentCount),
            ("db.c.distinct('a', {b: 1})", Distinct),
            (
                "db.c.aggregate([{$match: {}}, {$group: {_id: '$a'}}])",
                Aggregate,
            ),
            ("db.c.getIndexes()", GetIndexes),
            ("db.c.stats()", Stats),
            ("db.c.insertOne({a: 1})", InsertOne),
            (
                "db.c.insertMany([{a: 1}, {a: 2}], {ordered: false})",
                InsertMany,
            ),
            (
                "db.c.updateOne({a: 1}, {$set: {b: 2}}, {upsert: true})",
                UpdateOne,
            ),
            ("db.c.updateMany({a: 1}, [{$set: {b: 2}}])", UpdateMany),
            ("db.c.replaceOne({a: 1}, {a: 2})", ReplaceOne),
            ("db.c.deleteOne({a: 1})", DeleteOne),
            ("db.c.deleteMany({a: 1})", DeleteMany),
            (
                "db.c.findOneAndUpdate({a: 1}, {$inc: {n: 1}})",
                FindOneAndUpdate,
            ),
            ("db.c.findOneAndReplace({a: 1}, {a: 3})", FindOneAndReplace),
            ("db.c.findOneAndDelete({a: 1})", FindOneAndDelete),
            ("db.c.createIndex({a: 1}, {unique: true})", CreateIndex),
            ("db.c.createIndexes([{a: 1}, {b: -1}])", CreateIndexes),
            ("db.c.dropIndex('a_1')", DropIndex),
            ("db.c.dropIndexes()", DropIndexes),
            ("db.c.renameCollection('d')", RenameCollection),
            ("db.c.drop()", Drop),
        ];
        assert_eq!(coll_forms.len(), CollMethod::ALL.len());
        for (text, m) in coll_forms {
            match cmd(text) {
                ShellCommand::Coll {
                    db: None,
                    coll,
                    method,
                    explain: None,
                    ..
                } => assert_eq!((coll.as_str(), method), ("c", m), "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }

        // Modifiers in the order written; toArray() and pretty() are not kept.
        assert_eq!(
            cmd(
                "db.c.find({a: 1}).sort({a: -1}).skip(5).limit(10).project({a: 1})\
                 .projection({b: 0}).maxTimeMS(500).pretty().toArray()"
            ),
            ShellCommand::Coll {
                db: None,
                coll: "c".into(),
                method: Find,
                args: vec![doc! {"a": 1}.into()],
                modifiers: vec![
                    Modifier::Sort(doc! {"a": -1}),
                    Modifier::Skip(5),
                    Modifier::Limit(10),
                    Modifier::Project(doc! {"a": 1}),
                    Modifier::Project(doc! {"b": 0}),
                    Modifier::MaxTimeMs(500),
                ],
                explain: None,
            }
        );
        assert_eq!(
            cmd("db.c.aggregate([]).limit(2).toArray()"),
            ShellCommand::Coll {
                db: None,
                coll: "c".into(),
                method: Aggregate,
                args: vec![Bson::Array(vec![])],
                modifiers: vec![Modifier::Limit(2)],
                explain: None,
            }
        );
        assert_eq!(cmd("db.c.aggregate()"), coll("c", Aggregate, vec![]));
        // count() makes the find a countDocuments of its filter.
        assert_eq!(
            cmd("db.c.find({a: 1}, {b: 1}).limit(3).count()"),
            ShellCommand::Coll {
                db: None,
                coll: "c".into(),
                method: CountDocuments,
                args: vec![doc! {"a": 1}.into()],
                modifiers: vec![Modifier::Limit(3)],
                explain: None,
            }
        );
        assert_eq!(parsed("db.c.find().count()").name, "countDocuments");

        // dbRef and collRef forms.
        let sibling = ShellCommand::Coll {
            db: Some("other".into()),
            coll: "my coll".into(),
            method: FindOne,
            args: vec![],
            modifiers: vec![],
            explain: None,
        };
        for text in [
            "db.getSiblingDB('other')['my coll'].findOne()",
            "db.getSiblingDB(\"other\").getCollection('my coll').findOne()",
            "db.getSiblingDB('other').[\"my coll\"].findOne()",
        ] {
            assert_eq!(cmd(text), sibling, "{text}");
        }
        assert_eq!(cmd("db['c'].find()"), coll("c", Find, vec![]));
        assert_eq!(cmd("db.getCollection('c').find()"), coll("c", Find, vec![]));
        // Whitespace, comments, a trailing comma and a trailing semicolon.
        assert_eq!(
            cmd("db . c /* x */ . find ( { } , ) ;"),
            coll("c", Find, vec![doc! {}.into()])
        );
        // The command keeps its text as written.
        assert_eq!(parsed(" show dbs ").text, " show dbs ");
        assert_eq!(parsed("db.c.find()").class, CommandClass::Read);
    }

    #[test]
    fn dotted_collection_names() {
        use CollMethod::*;
        assert_eq!(
            cmd("db.system.users.find()"),
            coll("system.users", Find, vec![])
        );
        assert_eq!(
            cmd("db.fs.files.countDocuments({})"),
            coll("fs.files", CountDocuments, vec![doc! {}.into()])
        );
        assert_eq!(
            cmd("db.getCollection('x').y.find()"),
            coll("x.y", Find, vec![])
        );
        assert_eq!(
            cmd("db.a.b.c.explain().find()"),
            ShellCommand::Coll {
                db: None,
                coll: "a.b.c".into(),
                method: Find,
                args: vec![],
                modifiers: vec![],
                explain: Some(Verbosity::QueryPlanner),
            }
        );
        assert_eq!(parsed("db.fs.chunks.drop()").targets[0].name, "fs.chunks");
        for (text, offset) in [("db[''].find()", 2), ("db.getCollection('').find()", 3)] {
            assert_eq!(
                err(text),
                ("Invalid collection name".into(), Some(offset)),
                "{text}"
            );
        }
    }

    #[test]
    fn db_method_vs_collection() {
        assert!(matches!(
            cmd("db.stats()"),
            ShellCommand::Db {
                method: DbMethod::Stats,
                ..
            }
        ));
        assert!(matches!(
            cmd("db.c.stats()"),
            ShellCommand::Coll {
                method: CollMethod::Stats,
                ..
            }
        ));
        // A collection may be named like a db method.
        assert_eq!(
            cmd("db.getCollectionNames.find()"),
            coll("getCollectionNames", CollMethod::Find, vec![])
        );
        let cases = [
            ("db.foo()", "Unknown method foo()", 3),
            ("db.c.foo()", "Unknown method foo()", 5),
            ("db.c.version()", "Unknown method version()", 5),
            ("db.explain()", "Unknown method explain()", 3),
            ("db.c.explain().foo()", "Unknown method foo()", 15),
            // User management is not in the grammar.
            (
                "db.createUser({user: 'u', pwd: 'p', roles: []})",
                "Unknown method createUser()",
                3,
            ),
            ("db.auth('u', 'p')", "Unknown method auth()", 3),
            (
                "db.changeUserPassword('u', 'p')",
                "Unknown method changeUserPassword()",
                3,
            ),
            ("db.getSiblingDB('x').foo()", "Unknown method foo()", 21),
        ];
        for (text, message, offset) in cases {
            assert_eq!(err(text), (message.into(), Some(offset)), "{text}");
        }
    }

    #[test]
    fn explain_both_positions() {
        let want = ShellCommand::Coll {
            db: None,
            coll: "c".into(),
            method: CollMethod::Find,
            args: vec![doc! {"a": 1}.into()],
            modifiers: vec![],
            explain: Some(Verbosity::ExecutionStats),
        };
        assert_eq!(cmd("db.c.find({a: 1}).explain('executionStats')"), want);
        assert_eq!(cmd("db.c.explain('executionStats').find({a: 1})"), want);
        for text in ["db.c.find().explain()", "db.c.explain().find()"] {
            assert!(
                matches!(
                    cmd(text),
                    ShellCommand::Coll {
                        explain: Some(Verbosity::QueryPlanner),
                        ..
                    }
                ),
                "{text}"
            );
        }
        assert!(matches!(
            cmd("db.c.aggregate([]).explain(\"allPlansExecution\")"),
            ShellCommand::Coll {
                explain: Some(Verbosity::AllPlansExecution),
                ..
            }
        ));
        assert_eq!(
            parsed("db.c.find().explain('allPlansExecution')").class,
            CommandClass::Read
        );
        let usage = r#"explain() expects "queryPlanner", "executionStats" or "allPlansExecution""#;
        for (text, message, offset) in [
            ("db.c.find().explain('verbose')", usage, 20),
            ("db.c.find().explain(true)", usage, 20),
            ("db.c.explain('verbose').find()", usage, 13),
            (
                "db.c.find().explain().limit(1)",
                ".limit() is not valid after explain()",
                21,
            ),
            (
                "db.c.explain().find().sort({a: 1})",
                ".sort() is not valid after explain()",
                21,
            ),
        ] {
            assert_eq!(err(text), (message.into(), Some(offset)), "{text}");
        }
    }

    #[test]
    fn misplaced_modifier_is_error() {
        let cases = [
            (
                "db.c.findOne().limit(1)",
                ".limit() is not valid after findOne()",
                14,
            ),
            (
                "db.c.insertOne({a: 1}).sort({a: 1})",
                ".sort() is not valid after insertOne()",
                22,
            ),
            (
                "db.getCollectionNames().sort()",
                ".sort() is not valid after getCollectionNames()",
                23,
            ),
            (
                "db.c.aggregate([]).count()",
                ".count() is not valid after aggregate()",
                18,
            ),
            (
                "db.c.find().count().limit(1)",
                ".limit() is not valid after count()",
                19,
            ),
            (
                "db.c.countDocuments({}).explain()",
                ".explain() is not valid after countDocuments()",
                23,
            ),
            (
                "db.version().pretty()",
                ".pretty() is not valid after version()",
                12,
            ),
            // Modifier arguments.
            ("db.c.find().limit('x')", "limit() expects an integer", 18),
            (
                "db.c.find().skip(-1)",
                "skip() expects a non-negative integer",
                17,
            ),
            ("db.c.find().sort(1)", "sort() expects a document", 17),
            ("db.c.find().toArray(1)", "toArray() takes no arguments", 20),
            // Method arguments.
            (
                "db.c.insertOne()",
                "insertOne() expects a document and an optional options document",
                5,
            ),
            (
                "db.c.updateMany({})",
                "updateMany() expects a filter, an update document or pipeline, and an optional options document",
                5,
            ),
            (
                "db.c.aggregate({$match: {}})",
                "aggregate() expects a pipeline (an array of stage documents) and an optional options document",
                15,
            ),
            (
                "db.c.renameCollection('d', 1)",
                "renameCollection() expects a new name and an optional dropTarget boolean",
                27,
            ),
            (
                "db.c.find({}, {}, {}, {})",
                "find() expects an optional filter, projection and options document",
                22,
            ),
            (
                "db.runCommand('ping')",
                "runCommand() expects a command document",
                14,
            ),
            ("db.c.find(", "Unexpected end of command", 10),
            ("db.c", "Unexpected end of command", 4),
            ("", "Expected a command", 0),
            (
                "show users",
                "Expected show dbs, show databases, show collections or show tables",
                5,
            ),
        ];
        for (text, message, offset) in cases {
            assert_eq!(err(text), (message.into(), Some(offset)), "{text}");
        }
    }

    #[test]
    fn javascript_is_refused() {
        let cases = [
            ("var x = 1", 0),
            ("let x = db.c.find()", 0),
            ("const c = db.c", 0),
            ("x => x", 0),
            ("function f() {}", 0),
            ("load('script.js')", 0),
            ("eval('db.c.drop()')", 0),
            ("db.c.find().forEach(printjson)", 11),
            ("db.c.find().map(d => d._id)", 11),
            ("x = 1", 0),
            ("db.c = 1", 5),
            ("db.c.find({a: 1 + 1})", 16),
            ("db.c.find() + 1", 12),
            ("db.c.insertOne({a: 1 - 2})", 21),
            ("db.c.find({a: x => 1})", 14),
            ("db.c.find({$where: function() { return true }})", 19),
            ("db.c.find({}).length", 13),
            ("1 + 1", 0),
            ("rs.status()", 0),
            ("db.c.find(); db.d.find()", 11),
            ("use=5", 0),
        ];
        for (text, offset) in cases {
            assert_eq!(err(text), (JS_REFUSED.into(), Some(offset)), "{text}");
        }
    }

    #[test]
    fn invalid_db_name() {
        let long = format!("use {}", "a".repeat(65));
        let cases = [
            ("use", 3),
            ("use ;", 4),
            ("use ''", 4),
            ("use a/b", 4),
            ("use a\\b", 4),
            ("use a.b", 4),
            ("use a b", 6),
            ("use 'a b'", 4),
            ("use a\"b", 4),
            ("use $x", 4),
            ("use a*", 4),
            ("use a<b", 4),
            ("use a>b", 4),
            ("use a:b", 4),
            ("use a|b", 4),
            ("use a?b", 4),
            (long.as_str(), 4),
            ("db.getSiblingDB('a.b').c.find()", 16),
            ("db.getSiblingDB('').dropDatabase()", 16),
        ];
        for (text, offset) in cases {
            assert_eq!(
                err(text),
                ("Invalid database name".into(), Some(offset)),
                "{text}"
            );
        }
        assert_eq!(
            err("db.getSiblingDB(1).c.find()"),
            ("getSiblingDB() expects a database name".into(), Some(16))
        );
        let max = format!("use {}", "a".repeat(64));
        assert_eq!(cmd(&max), ShellCommand::Use("a".repeat(64)));
        assert!(valid_db_name("my-db_2") && !valid_db_name("a\0b"));
    }

    /// Splitting, parsing and redacting any text never panics, and spans are in order
    /// and slice the text exactly.
    fn never_panics(text: &str) {
        let spans = MongoLanguage.split(text);
        let mut last = 0;
        for s in &spans {
            assert!(s.range.0 >= last && s.range.0 <= s.range.1, "{text:?}");
            assert_eq!(
                text.get(s.range.0..s.range.1),
                Some(s.text.as_str()),
                "{text:?}"
            );
            last = s.range.1;
            let _ = MongoLanguage.parse(&s.text);
            let _ = MongoLanguage.redact(&s.text);
        }
        let _ = MongoLanguage.parse(text);
        let r = MongoLanguage.redact(text);
        if !r.changed {
            assert_eq!(r.text, text);
        }
    }

    #[test]
    fn split_never_panics() {
        let inputs = [
            "",
            ";",
            ";;;",
            "(((",
            ")))",
            "}{",
            "'",
            "\"",
            "`",
            "/",
            "/*",
            "*/",
            "//",
            "\\",
            "é",
            "😀;😀",
            "db.",
            "db.c.find(\"",
            "db.c.find({a: /x",
            "use",
            "use ",
            "use \n",
            "use 'a",
            "use é;",
            "use 😀x\n'",
            "\n\n",
            "\r\n\r",
            ".\n.\n.",
            "db.c.find()\n.",
            "\u{2028}",
            "a\u{0}b",
            "${",
            "`${x}`",
            "0x",
            "1e",
            "db.c.find({a: '\\",
            "/**/;/**/",
            "db.c.insertOne({a: `x\ny`})\n'",
            "pwd:",
            "pwd: '",
            "password = ",
            ".auth(",
            ".auth('a',",
            "mongodb://",
            "mongodb://:@",
            "mongodb+srv://u:é@😀",
            "{secret: {a: [}",
            "db.auth('é', 'é",
        ];
        for text in inputs {
            never_panics(text);
        }
        // A deterministic fuzz over the characters that matter.
        let alphabet: Vec<char> = "db.c(){}[];\n '\"`/*\\éuse1x:,$\r-=pwd@m😀"
            .chars()
            .collect();
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..4000 {
            let len = (next() % 28) as usize;
            let text: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            never_panics(&text);
        }
    }
}
