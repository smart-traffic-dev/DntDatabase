//! An in-memory, Redis-like Command driver (spec §9): proves the Command-family
//! contracts (language, guard, run, keyspace, key edits) work together without a server.
//!
//! The language, one command per non-empty line (`#` starts a comment line), tokens
//! separated by whitespace, `"…"` a quoted token with `\"` and `\\` escapes:
//!
//! | command                                 | class                        |
//! |-----------------------------------------|------------------------------|
//! | `GET k`, `HGETALL k`, `SCAN c [MATCH p]` | Read                        |
//! | `SET k v`, `DEL k…`, `HSET k f v`        | Write                       |
//! | `SELECT n` (0–15)                        | SessionChange               |
//! | `MULTI` / `EXEC` / `DISCARD`             | Transaction Begin / Commit / Rollback |
//! | `FLUSHDB`                                | Admin, drops the current db |
//! | `CONFIG …`                               | Admin                       |
//! | anything else                            | Unknown (the server refuses it) |

use async_trait::async_trait;
use dnt_core::command::{
    CommandClass, CommandDrop, CommandDropKind, CommandLanguage, CommandParseError, CommandSession,
    CommandSessionEffect, CommandSpan, CommandTarget, ParsedCommand,
};
use dnt_core::config::ResolvedConfig;
use dnt_core::data_access::{DataAccess, KeyEdit, PageSpec};
use dnt_core::driver::{
    CancelHandle, Canceller, Driver, Engine, ExecOptions, Introspector, ResultSummary, RowSink,
    Session, StmtOutcome,
};
use dnt_core::edit::EditSet;
use dnt_core::error::DbError;
use dnt_core::keyspace::{
    KeyName, KeyPage, KeySummary, KeyType, KeyValue, KeyValueView, Keyspace, ScanRequest,
    ValueRange,
};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use dnt_core::redact::Redacted;
use dnt_core::server::{
    Capabilities, DbKind, EngineFamily, NamespaceModel, ServerInfo, ServerVariant,
};
use dnt_core::sql::filter::DataFilter;
use dnt_core::sql::TxControl;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, LogicalType, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::Notify;

/// Number of databases, named `"0"` … `"15"`.
pub const DB_COUNT: usize = 16;

/// A value of the fake store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeValue {
    Str(String),
    Hash(BTreeMap<String, String>),
}

impl FakeValue {
    fn key_type(&self) -> KeyType {
        match self {
            FakeValue::Str(_) => KeyType::String,
            FakeValue::Hash(_) => KeyType::Hash,
        }
    }
}

type Db = BTreeMap<Vec<u8>, FakeValue>;

struct StoreState {
    dbs: Vec<Db>,
    /// The names of the commands executed, in order (every session).
    executed: Vec<String>,
}

struct StoreInner {
    state: Mutex<StoreState>,
    block_next: AtomicBool,
    blocked: AtomicBool,
    blocked_changed: Notify,
}

/// A shared handle on the data of a [`FakeCommandDriver`] (every session of the driver
/// sees the same data): to seed and inspect it, and to set hooks.
#[derive(Clone)]
pub struct FakeStore(Arc<StoreInner>);

impl FakeStore {
    fn new() -> Self {
        FakeStore(Arc::new(StoreInner {
            state: Mutex::new(StoreState {
                dbs: vec![Db::new(); DB_COUNT],
                executed: Vec::new(),
            }),
            block_next: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            blocked_changed: Notify::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.0.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The value of `key` in database `db` (`"0"` … `"15"`).
    pub fn get(&self, db: &str, key: &[u8]) -> Option<FakeValue> {
        let index = db_index(db).ok()?;
        self.lock().dbs[index].get(key).cloned()
    }

    /// Stores `value` at `key` in database `db`. Panics on a db name outside 0–15.
    pub fn set(&self, db: &str, key: &[u8], value: FakeValue) {
        let index = db_index(db).expect("a database 0-15");
        self.lock().dbs[index].insert(key.to_vec(), value);
    }

    /// The names of the commands sessions executed, in order. A command refused before
    /// it started (stop) is not in it.
    pub fn executed(&self) -> Vec<String> {
        self.lock().executed.clone()
    }

    /// The next executed command waits, before doing anything, until its session's
    /// canceller fires, then fails `Cancelled { outcome_unknown: false, .. }`.
    pub fn block_next(&self) {
        self.0.block_next.store(true, Ordering::SeqCst);
    }

    /// Waits until a command is held by [`FakeStore::block_next`].
    pub async fn wait_blocked(&self) {
        loop {
            let mut changed = std::pin::pin!(self.0.blocked_changed.notified());
            changed.as_mut().enable();
            if self.0.blocked.load(Ordering::SeqCst) {
                return;
            }
            changed.await;
        }
    }

    fn set_blocked(&self, blocked: bool) {
        self.0.blocked.store(blocked, Ordering::SeqCst);
        self.0.blocked_changed.notify_waiters();
    }
}

/// The fake Command driver. `kind()` is `DbKind::Postgres` (there is no fake kind), so
/// that R2 app tests can store its connection; its sessions are Command sessions all
/// the same.
pub struct FakeCommandDriver {
    store: FakeStore,
}

impl Default for FakeCommandDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeCommandDriver {
    pub fn new() -> Self {
        FakeCommandDriver {
            store: FakeStore::new(),
        }
    }

    pub fn store(&self) -> FakeStore {
        self.store.clone()
    }
}

fn server_info() -> ServerInfo {
    let mut info = ServerInfo::new(
        DbKind::Postgres,
        ServerVariant::Postgres,
        (0, 0, 0),
        "fake-command",
    );
    info.capabilities = Capabilities {
        family: EngineFamily::Command,
        namespaces: NamespaceModel::NumberedDb,
        transactions: false,
        data_edit: true,
        designer: false,
        ddl_edit: false,
        object_actions: false,
        import: false,
        dump: false,
        seed: false,
        explain: false,
        paged_objects: true,
    };
    info
}

#[async_trait]
impl Driver for FakeCommandDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    /// The session starts in `cfg.database` when it names a db 0–15, else in db 0.
    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError> {
        let start = cfg
            .database
            .as_deref()
            .and_then(|d| db_index(d).ok())
            .unwrap_or(0);
        Ok(Box::new(FakeCommandSession {
            info: server_info(),
            store: self.store.clone(),
            language: FakeLanguage,
            db: AtomicUsize::new(start),
            queue: Mutex::new(None),
            cancel: Arc::new(CancelState::default()),
        }))
    }
}

fn db_index(name: &str) -> Result<usize, DbError> {
    name.parse::<usize>()
        .ok()
        .filter(|i| *i < DB_COUNT)
        .ok_or_else(|| server_err(format!("ERR invalid DB index '{name}'")))
}

fn server_err(message: impl Into<String>) -> DbError {
    DbError::Server {
        code: None,
        sqlstate: None,
        message: message.into(),
        position: None,
    }
}

fn unsupported(feature: &str) -> DbError {
    DbError::Unsupported {
        feature: feature.into(),
        version: String::new(),
    }
}

// ---------------------------------------------------------------------------------
// Language

/// What a parsed command does: the payload of its `ParsedCommand`.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    Get(Vec<u8>),
    HGetAll(Vec<u8>),
    Scan {
        cursor: usize,
        pattern: Option<String>,
    },
    Set(Vec<u8>, String),
    Del(Vec<Vec<u8>>),
    HSet(Vec<u8>, String, String),
    /// Only built by key edits; not in the console's language.
    HDel(Vec<u8>, String),
    Select(usize),
    Multi,
    Exec,
    Discard,
    FlushDb,
    Config,
    Unknown(String),
}

/// The payload: the op and, for a key edit, the database it targets (`None`: the
/// session's current db).
#[derive(Debug, Clone)]
struct Payload {
    db: Option<usize>,
    op: Op,
}

struct Token {
    text: String,
    /// Byte range of the token (quotes included) in the command.
    start: usize,
    end: usize,
}

/// The tokens of `text`; an unclosed quote is the last token (to the end) and its
/// opening offset is the error.
fn tokenize(text: &str) -> (Vec<Token>, Option<usize>) {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if bytes[i] == b'"' {
            let mut value = String::new();
            let mut chars = text[i + 1..].char_indices();
            let mut closed = None;
            while let Some((at, c)) = chars.next() {
                match c {
                    '"' => {
                        closed = Some(i + 1 + at + 1);
                        break;
                    }
                    '\\' => match chars.clone().next() {
                        Some((_, n @ ('"' | '\\'))) => {
                            value.push(n);
                            chars.next();
                        }
                        _ => value.push('\\'),
                    },
                    c => value.push(c),
                }
            }
            match closed {
                Some(end) => {
                    tokens.push(Token {
                        text: value,
                        start,
                        end,
                    });
                    i = end;
                }
                None => {
                    tokens.push(Token {
                        text: value,
                        start,
                        end: text.len(),
                    });
                    return (tokens, Some(start));
                }
            }
        } else {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(Token {
                text: text[start..i].to_string(),
                start,
                end: i,
            });
        }
    }
    (tokens, None)
}

/// The language. It names no database for a command's targets: they are in the db in
/// effect when the command runs (see `CommandTarget::database`).
struct FakeLanguage;

fn parse_error(message: impl Into<String>, offset: usize) -> CommandParseError {
    CommandParseError {
        message: message.into(),
        offset: Some(offset),
    }
}

/// The `ParsedCommand` of `op`. `db`: the db a key edit runs in, which its targets
/// name (`None`: the db in effect, so targets name no db and `FLUSHDB` drops the db
/// with an empty name).
fn build_command(db: Option<usize>, name: &str, class: CommandClass, op: Op) -> ParsedCommand {
    let target = |key: &[u8]| CommandTarget {
        database: db.map(|d| d.to_string()),
        name: KeyName::from_bytes(key.to_vec()).display,
    };
    let mut targets = Vec::new();
    let mut drops = Vec::new();
    let mut session_effect = None;
    match &op {
        Op::Get(k) | Op::HGetAll(k) | Op::Set(k, _) | Op::HSet(k, _, _) | Op::HDel(k, _) => {
            targets.push(target(k))
        }
        Op::Del(keys) => targets.extend(keys.iter().map(|k| target(k))),
        Op::Scan {
            pattern: Some(p), ..
        } => targets.push(target(p.as_bytes())),
        Op::Select(n) => session_effect = Some(CommandSessionEffect::UseDatabase(n.to_string())),
        Op::FlushDb => drops.push(CommandDrop {
            kind: CommandDropKind::Database,
            target: CommandTarget {
                database: None,
                name: String::new(),
            },
        }),
        _ => {}
    }
    ParsedCommand {
        name: name.to_string(),
        class,
        targets,
        unbounded: false,
        drops,
        session_effect,
        payload: Arc::new(Payload { db, op }),
    }
}

impl CommandLanguage for FakeLanguage {
    fn split(&self, text: &str) -> Vec<CommandSpan> {
        let mut out = Vec::new();
        let mut at = 0;
        for line in text.split_inclusive('\n') {
            let lead = line.len() - line.trim_start().len();
            let body = line.trim();
            if !body.is_empty() && !body.starts_with('#') {
                let start = at + lead;
                out.push(CommandSpan {
                    range: (start, start + body.len()),
                    text: body.to_string(),
                });
            }
            at += line.len();
        }
        out
    }

    fn parse(&self, text: &str) -> Result<ParsedCommand, CommandParseError> {
        let (tokens, unclosed) = tokenize(text);
        if let Some(offset) = unclosed {
            return Err(parse_error("unclosed quote", offset));
        }
        let Some((first, args)) = tokens.split_first() else {
            return Err(parse_error("empty command", 0));
        };
        let name = first.text.to_ascii_uppercase();
        // `args` must be between `min` and `max` tokens.
        let arity = |min: usize, max: Option<usize>| -> Result<(), CommandParseError> {
            if args.len() < min {
                return Err(parse_error(
                    format!("wrong number of arguments for '{name}'"),
                    text.len(),
                ));
            }
            match max {
                Some(max) if args.len() > max => Err(parse_error(
                    format!("wrong number of arguments for '{name}'"),
                    args[max].start,
                )),
                _ => Ok(()),
            }
        };
        let key = |i: usize| args[i].text.clone().into_bytes();
        let arg = |i: usize| args[i].text.clone();
        let (class, op) = match name.as_str() {
            "GET" => {
                arity(1, Some(1))?;
                (CommandClass::Read, Op::Get(key(0)))
            }
            "HGETALL" => {
                arity(1, Some(1))?;
                (CommandClass::Read, Op::HGetAll(key(0)))
            }
            "SCAN" => {
                arity(1, Some(3))?;
                let cursor = args[0]
                    .text
                    .parse::<usize>()
                    .map_err(|_| parse_error("invalid cursor", args[0].start))?;
                let pattern = match args.len() {
                    1 => None,
                    2 => return Err(parse_error("expected MATCH <pattern>", args[1].start)),
                    _ if args[1].text.eq_ignore_ascii_case("MATCH") => Some(arg(2)),
                    _ => return Err(parse_error("expected MATCH", args[1].start)),
                };
                (CommandClass::Read, Op::Scan { cursor, pattern })
            }
            "SET" => {
                arity(2, Some(2))?;
                (CommandClass::Write, Op::Set(key(0), arg(1)))
            }
            "DEL" => {
                arity(1, None)?;
                let keys = (0..args.len()).map(key).collect();
                (CommandClass::Write, Op::Del(keys))
            }
            "HSET" => {
                arity(3, Some(3))?;
                (CommandClass::Write, Op::HSet(key(0), arg(1), arg(2)))
            }
            "SELECT" => {
                arity(1, Some(1))?;
                let n = db_index(&args[0].text)
                    .map_err(|_| parse_error("invalid DB index", args[0].start))?;
                (CommandClass::SessionChange, Op::Select(n))
            }
            "MULTI" | "EXEC" | "DISCARD" => {
                arity(0, Some(0))?;
                match name.as_str() {
                    "MULTI" => (CommandClass::Transaction(TxControl::Begin), Op::Multi),
                    "EXEC" => (CommandClass::Transaction(TxControl::Commit), Op::Exec),
                    _ => (CommandClass::Transaction(TxControl::Rollback), Op::Discard),
                }
            }
            "FLUSHDB" => {
                arity(0, Some(0))?;
                (CommandClass::Admin, Op::FlushDb)
            }
            "CONFIG" => {
                arity(1, None)?;
                (CommandClass::Admin, Op::Config)
            }
            _ => (CommandClass::Unknown, Op::Unknown(name.clone())),
        };
        Ok(build_command(None, &name, class, op))
    }

    /// `CONFIG SET requirepass <x>` (any case) becomes `CONFIG SET requirepass ***`.
    fn redact(&self, text: &str) -> Redacted {
        let mut out = String::with_capacity(text.len());
        let mut changed = false;
        for line in text.split_inclusive('\n') {
            let (tokens, _) = tokenize(line);
            let is = |i: usize, word: &str| {
                tokens
                    .get(i)
                    .is_some_and(|t| t.text.eq_ignore_ascii_case(word))
            };
            match tokens.get(3) {
                Some(secret) if is(0, "CONFIG") && is(1, "SET") && is(2, "requirepass") => {
                    out.push_str(&line[..secret.start]);
                    out.push_str("***");
                    out.push_str(&line[secret.end.min(line.trim_end().len())..]);
                    changed = true;
                }
                _ => out.push_str(line),
            }
        }
        Redacted { text: out, changed }
    }
}

// ---------------------------------------------------------------------------------
// Session

#[derive(Default)]
struct CancelState {
    /// How many times the canceller fired.
    fired: AtomicU64,
    notify: Notify,
}

struct FakeCanceller(Arc<CancelState>);

#[async_trait]
impl Canceller for FakeCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.fired.fetch_add(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
        Ok(())
    }
}

struct FakeCommandSession {
    info: ServerInfo,
    store: FakeStore,
    language: FakeLanguage,
    /// The current db.
    db: AtomicUsize,
    /// `Some` between `MULTI` and `EXEC` / `DISCARD`: the queued commands.
    queue: Mutex<Option<Vec<Payload>>>,
    cancel: Arc<CancelState>,
}

fn text_col(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        db_type: "string".into(),
        logical: LogicalType::Text,
        nullable: Some(true),
        origin_table: None,
        origin_column: None,
    }
}

fn wrong_type() -> DbError {
    server_err("WRONGTYPE Operation against a key holding the wrong kind of value")
}

/// Whether `key` matches the glob `pattern` (`*`, `?` and literal bytes).
fn glob_match(pattern: &[u8], key: &[u8]) -> bool {
    match pattern.split_first() {
        None => key.is_empty(),
        Some((b'*', rest)) => (0..=key.len()).any(|i| glob_match(rest, &key[i..])),
        Some((b'?', rest)) => !key.is_empty() && glob_match(rest, &key[1..]),
        Some((c, rest)) => key.first() == Some(c) && glob_match(rest, &key[1..]),
    }
}

/// Most keys one console `SCAN` returns.
const CONSOLE_SCAN_COUNT: usize = 10;

impl FakeCommandSession {
    fn queue(&self) -> MutexGuard<'_, Option<Vec<Payload>>> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn in_multi(&self) -> bool {
        self.queue().is_some()
    }

    /// Waits until the canceller has fired more than `start` times.
    async fn wait_cancelled(&self, start: u64) {
        loop {
            let mut fired = std::pin::pin!(self.cancel.notify.notified());
            fired.as_mut().enable();
            if self.cancel.fired.load(Ordering::SeqCst) != start {
                return;
            }
            fired.await;
        }
    }

    /// Runs one op (not a transaction step) into `sink`.
    fn apply(
        &self,
        payload: &Payload,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<ResultSummary, DbError> {
        let db = payload.db.unwrap_or_else(|| self.db.load(Ordering::SeqCst));
        let mut state = self.store.lock();
        let data = &mut state.dbs[db];
        let mut rows: Option<(Vec<ColumnMeta>, Vec<Vec<Value>>)> = None;
        let mut affected = None;
        match &payload.op {
            Op::Get(k) => {
                let value = match data.get(k) {
                    None => Value::Null,
                    Some(FakeValue::Str(s)) => Value::Text(s.clone()),
                    Some(FakeValue::Hash(_)) => return Err(wrong_type()),
                };
                rows = Some((vec![text_col("value")], vec![vec![value]]));
            }
            Op::HGetAll(k) => {
                let fields = match data.get(k) {
                    None => vec![],
                    Some(FakeValue::Hash(h)) => h
                        .iter()
                        .map(|(f, v)| vec![Value::Text(f.clone()), Value::Text(v.clone())])
                        .collect(),
                    Some(FakeValue::Str(_)) => return Err(wrong_type()),
                };
                rows = Some((vec![text_col("field"), text_col("value")], fields));
            }
            Op::Scan { cursor, pattern } => {
                let keys = data
                    .keys()
                    .filter(|k| pattern.as_ref().is_none_or(|p| glob_match(p.as_bytes(), k)))
                    .skip(*cursor)
                    .take(CONSOLE_SCAN_COUNT)
                    .map(|k| vec![Value::Text(KeyName::from_bytes(k.clone()).display)])
                    .collect();
                rows = Some((vec![text_col("key")], keys));
            }
            Op::Set(k, v) => {
                data.insert(k.clone(), FakeValue::Str(v.clone()));
                affected = Some(1);
            }
            Op::Del(keys) => {
                let removed = keys.iter().filter(|k| data.remove(*k).is_some()).count();
                affected = Some(removed as u64);
            }
            Op::HSet(k, f, v) => {
                let entry = data
                    .entry(k.clone())
                    .or_insert_with(|| FakeValue::Hash(BTreeMap::new()));
                let FakeValue::Hash(h) = entry else {
                    return Err(wrong_type());
                };
                affected = Some(u64::from(h.insert(f.clone(), v.clone()).is_none()));
            }
            Op::HDel(k, f) => {
                affected = Some(match data.get_mut(k) {
                    None => 0,
                    Some(FakeValue::Hash(h)) => u64::from(h.remove(f).is_some()),
                    Some(FakeValue::Str(_)) => return Err(wrong_type()),
                });
            }
            Op::Select(n) => self.db.store(*n, Ordering::SeqCst),
            Op::FlushDb => data.clear(),
            Op::Config => {}
            Op::Unknown(name) => return Err(server_err(format!("ERR unknown command '{name}'"))),
            Op::Multi | Op::Exec | Op::Discard => {
                return Err(DbError::Internal {
                    message: "a transaction step is not an op".into(),
                })
            }
        }
        drop(state);
        let (cols, mut rows) = rows.unwrap_or_default();
        let truncated = rows.len() > opts.max_rows;
        rows.truncate(opts.max_rows);
        let summary = ResultSummary {
            affected,
            row_count: rows.len() as u64,
            truncated,
            elapsed_ms: 0,
        };
        sink.result_start(&cols);
        if !rows.is_empty() {
            sink.rows(rows);
        }
        sink.result_end(&summary);
        Ok(summary)
    }
}

#[async_trait]
impl Session for FakeCommandSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }
    fn engine(&self) -> Engine<'_> {
        Engine::Command(self)
    }
    fn introspector(&self) -> &dyn Introspector {
        self
    }
    fn keyspace(&self) -> Option<&dyn Keyspace> {
        Some(self)
    }
    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(FakeCanceller(self.cancel.clone()))
    }
    async fn ping(&self) -> Result<(), DbError> {
        Ok(())
    }
    async fn close(self: Box<Self>) {}
}

#[async_trait]
impl CommandSession for FakeCommandSession {
    fn language(&self) -> &dyn CommandLanguage {
        &self.language
    }

    /// `Active` between `MULTI` and `EXEC` / `DISCARD`.
    fn tx_state(&self) -> TxState {
        if self.in_multi() {
            TxState::Active
        } else {
            TxState::Idle
        }
    }

    fn data_access(&self) -> &dyn DataAccess {
        self
    }

    /// Inside `MULTI` every command but `EXEC` / `DISCARD` is queued (an empty
    /// result) and runs on `EXEC`, one result each.
    async fn execute_command(
        &self,
        cmd: &ParsedCommand,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let payload = cmd
            .payload
            .downcast_ref::<Payload>()
            .ok_or_else(|| DbError::Internal {
                message: format!("`{}` was not parsed by the fake Command driver", cmd.name),
            })?;
        if opts.stop.is_stopped() {
            return Err(DbError::cancelled());
        }
        let was_in_tx = self.in_multi();
        if self.store.0.block_next.swap(false, Ordering::SeqCst) {
            // Read before telling the test we are blocked, so its cancel is counted.
            let start = self.cancel.fired.load(Ordering::SeqCst);
            self.store.set_blocked(true);
            self.wait_cancelled(start).await;
            self.store.set_blocked(false);
            return Err(DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx,
            });
        }
        self.store.lock().executed.push(cmd.name.clone());

        let mut results = Vec::new();
        match &payload.op {
            Op::Multi => {
                let mut queue = self.queue();
                if queue.is_some() {
                    return Err(server_err("ERR MULTI calls can not be nested"));
                }
                *queue = Some(Vec::new());
            }
            Op::Exec => {
                let queued = self
                    .queue()
                    .take()
                    .ok_or_else(|| server_err("ERR EXEC without MULTI"))?;
                for p in &queued {
                    results.push(self.apply(p, opts, sink)?);
                }
            }
            Op::Discard => {
                self.queue()
                    .take()
                    .ok_or_else(|| server_err("ERR DISCARD without MULTI"))?;
            }
            _ => {
                let queued = match self.queue().as_mut() {
                    Some(queue) => {
                        queue.push(payload.clone());
                        true
                    }
                    None => false,
                };
                if queued {
                    let summary = ResultSummary {
                        affected: None,
                        row_count: 0,
                        truncated: false,
                        elapsed_ms: 0,
                    };
                    sink.result_start(&[]);
                    sink.result_end(&summary);
                    results.push(summary);
                } else {
                    results.push(self.apply(payload, opts, sink)?);
                }
            }
        }
        Ok(StmtOutcome {
            results,
            late_completion: false,
        })
    }
}

#[async_trait]
impl Keyspace for FakeCommandSession {
    /// `cursor` is the numeric index, in the sorted keys that match, of the first key of
    /// the page.
    async fn scan(&self, db: &str, req: &ScanRequest) -> Result<KeyPage, DbError> {
        let index = db_index(db)?;
        let from = match &req.cursor {
            None => 0,
            Some(c) => c
                .parse::<usize>()
                .map_err(|_| server_err(format!("ERR invalid cursor '{c}'")))?,
        };
        let state = self.store.lock();
        let matching: Vec<KeySummary> = state.dbs[index]
            .iter()
            .filter(|(k, _)| {
                req.pattern
                    .as_ref()
                    .is_none_or(|p| glob_match(p.as_bytes(), k))
            })
            .filter(|(_, v)| req.key_type.as_ref().is_none_or(|t| *t == v.key_type()))
            .map(|(k, v)| KeySummary {
                name: KeyName::from_bytes(k.clone()),
                key_type: v.key_type(),
                ttl_ms: None,
            })
            .collect();
        let count = req.count.max(1) as usize;
        let keys: Vec<KeySummary> = matching.iter().skip(from).take(count).cloned().collect();
        let end = from + keys.len();
        Ok(KeyPage {
            keys,
            next_cursor: (end < matching.len()).then(|| end.to_string()),
        })
    }

    async fn key_value(
        &self,
        db: &str,
        key: &KeyName,
        range: &ValueRange,
    ) -> Result<KeyValueView, DbError> {
        let index = db_index(db)?;
        let state = self.store.lock();
        let value = match state.dbs[index].get(&key.raw) {
            None => return Err(server_err(format!("ERR no such key '{}'", key.display))),
            Some(FakeValue::Str(s)) => KeyValue::String {
                value: Value::Text(s.clone()),
            },
            Some(FakeValue::Hash(h)) => {
                let from = usize::try_from(range.offset).unwrap_or(usize::MAX);
                let fields: Vec<(Value, Value)> = h
                    .iter()
                    .skip(from)
                    .take(range.count as usize)
                    .map(|(f, v)| (Value::Text(f.clone()), Value::Text(v.clone())))
                    .collect();
                let end = from.saturating_add(fields.len());
                KeyValue::Hash {
                    fields,
                    total: h.len() as u64,
                    next: (end < h.len()).then(|| end.to_string()),
                }
            }
        };
        Ok(KeyValueView {
            value,
            ttl_ms: None,
            memory_bytes: None,
        })
    }
}

impl DataAccess for FakeCommandSession {
    fn page(&self, _obj: &ObjectRef, _spec: &PageSpec) -> Result<ParsedCommand, DbError> {
        Err(unsupported("collections"))
    }

    fn count(
        &self,
        _obj: &ObjectRef,
        _filter: Option<&DataFilter>,
    ) -> Result<ParsedCommand, DbError> {
        Err(unsupported("collections"))
    }

    fn build_edits(
        &self,
        _obj: &ObjectRef,
        _detail: &TableDetail,
        _edits: &EditSet,
    ) -> Result<Vec<ParsedCommand>, DbError> {
        Err(unsupported("collections"))
    }

    /// `SetString`, `HashSet`, `HashDelete` and `Delete`; the others are `Unsupported`.
    fn build_key_edit(&self, db: &str, edit: &KeyEdit) -> Result<Vec<ParsedCommand>, DbError> {
        let index = db_index(db)?;
        let write =
            |name: &str, op: Op| vec![build_command(Some(index), name, CommandClass::Write, op)];
        Ok(match edit {
            KeyEdit::SetString { key, value } => {
                write("SET", Op::Set(key.raw.clone(), value.clone()))
            }
            KeyEdit::HashSet { key, field, value } => write(
                "HSET",
                Op::HSet(key.raw.clone(), field.clone(), value.clone()),
            ),
            KeyEdit::HashDelete { key, field } => {
                write("HDEL", Op::HDel(key.raw.clone(), field.clone()))
            }
            KeyEdit::Delete { key } => write("DEL", Op::Del(vec![key.raw.clone()])),
            _ => return Err(unsupported("this key edit")),
        })
    }
}

#[async_trait]
impl Introspector for FakeCommandSession {
    /// The databases that have keys, plus the current one, in numeric order.
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        let current = self.db.load(Ordering::SeqCst);
        let state = self.store.lock();
        Ok((0..DB_COUNT)
            .filter(|i| *i == current || !state.dbs[*i].is_empty())
            .map(|i| DatabaseInfo {
                name: i.to_string(),
            })
            .collect())
    }

    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![])
    }

    async fn list_objects(
        &self,
        _scope: &Scope,
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Ok(vec![])
    }

    async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        Ok(vec![])
    }

    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Err(unsupported("table detail"))
    }

    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Err(unsupported("object DDL"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_unquote_and_report_unclosed_quotes() {
        let (tokens, err) = tokenize(r#"SET  k "a \"b\" \\ \n""#);
        assert_eq!(err, None);
        let texts: Vec<_> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["SET", "k", r#"a "b" \ \n"#]);
        assert_eq!((tokens[1].start, tokens[1].end), (5, 6));
        let (tokens, err) = tokenize("GET \"open");
        assert_eq!(err, Some(4));
        assert_eq!(tokens[1].text, "open");
    }

    #[test]
    fn glob_matches_stars_and_marks() {
        assert!(glob_match(b"user:*", b"user:1"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"ac"));
        assert!(!glob_match(b"user:*", b"order:1"));
    }

    #[test]
    fn split_skips_blank_and_comment_lines() {
        let lang = FakeLanguage;
        let spans = lang.split("GET a\n\n  # note\r\n  SET b 1\r\n");
        let got: Vec<_> = spans.iter().map(|s| (s.range, s.text.as_str())).collect();
        assert_eq!(got, [((0, 5), "GET a"), ((19, 26), "SET b 1")]);
    }
}
