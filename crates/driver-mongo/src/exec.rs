//! Console execution (spec §5.3, §5.4, §5.6): the read-only allowlist, the server command
//! each console command becomes ([`plan`]), running it ([`run`]), the result shapes and the
//! single `result_start` ([`emit`]), and error mapping ([`map_error`]).
//!
//! Every command is sent as a raw server command (`find`, `insert`, …) through
//! `Database::run_command` / `run_cursor_command`: the comment, `maxTimeMS` and the user's
//! options land exactly where the server reads them, and nothing is retried.

use std::time::{Duration, Instant};

use dnt_core::command::{CommandClass, ParsedCommand};
use dnt_core::driver::{check_cell_size, CellMode, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::value::{FullValue, Value};
use mongodb::bson::oid::ObjectId;
use mongodb::bson::{doc, Bson, Document};
use mongodb::error::{ErrorKind, WriteFailure};
use mongodb::{Client, ClientSession, Cursor};

use crate::rows::{column_metas, columns_of, row_of};
use crate::shell::literal::integral_value;
use crate::shell::{
    has_out_or_merge, CollMethod, DbMethod, Modifier, Payload, ShellCommand, ShowWhat, Verbosity,
};

/// The refusal of everything outside the read-only allowlist (spec §5.3).
pub(crate) const READ_ONLY: &str = "This connection is read-only";

/// `runCommand` / `adminCommand` names of the Read group of spec §4.4.1 (without
/// `aggregate`, which is read only without `$out` / `$merge`, and `explain`).
const RUN_COMMAND_READS: &[&str] = &[
    "ping",
    "hello",
    "isMaster",
    "buildInfo",
    "serverStatus",
    "dbStats",
    "collStats",
    "listDatabases",
    "listCollections",
    "listIndexes",
    "find",
    "count",
    "distinct",
    "getMore",
    "connectionStatus",
    "hostInfo",
    "getParameter",
    "currentOp",
    "dataSize",
];

/// Session and wire fields an options document may not set (as in `runCommand`, §4.2).
const SESSION_FIELDS: &[&str] = &["lsid", "txnNumber", "startTransaction", "autocommit"];

/// Server error codes whose name a write error may lack.
const CODE_NAMES: &[(i32, &str)] = &[
    (13, "Unauthorized"),
    (50, "MaxTimeMSExpired"),
    (64, "WriteConcernFailed"),
    (79, "UnknownReplWriteConcern"),
    (100, "UnsatisfiableWriteConcern"),
    (121, "DocumentValidationFailure"),
    (10107, "NotWritablePrimary"),
    (11000, "DuplicateKey"),
    (INTERRUPTED, "Interrupted"),
];
const MAX_TIME_MS_EXPIRED: i32 = 50;
/// What a command stopped by `killOp` fails with.
const INTERRUPTED: i32 = 11601;
const DOCUMENT_VALIDATION_FAILURE: i32 = 121;

/// Whether a read-only connection may send `cmd` (spec §5.3, the second layer). Decided
/// from the command alone, never from the guard's classification: only the reads below,
/// explain with `queryPlanner` of them, and `use`.
pub(crate) fn allowed_read_only(cmd: &ShellCommand) -> bool {
    match cmd {
        ShellCommand::Use(_) | ShellCommand::Show(_) => true,
        ShellCommand::Db { method, args, .. } => match method {
            DbMethod::GetCollectionNames
            | DbMethod::Stats
            | DbMethod::ServerStatus
            | DbMethod::Version => true,
            DbMethod::CreateCollection | DbMethod::DropDatabase => false,
            DbMethod::RunCommand | DbMethod::AdminCommand => {
                matches!(args.first(), Some(Bson::Document(d)) if run_command_allowed(d, false))
            }
        },
        ShellCommand::Coll {
            method,
            args,
            explain,
            ..
        } => {
            let read = match method {
                CollMethod::Find
                | CollMethod::FindOne
                | CollMethod::CountDocuments
                | CollMethod::EstimatedDocumentCount
                | CollMethod::Distinct
                | CollMethod::GetIndexes
                | CollMethod::Stats => true,
                CollMethod::Aggregate => {
                    matches!(args.first(), Some(Bson::Array(p)) if has_out_or_merge(p).is_none())
                }
                CollMethod::InsertOne
                | CollMethod::InsertMany
                | CollMethod::UpdateOne
                | CollMethod::UpdateMany
                | CollMethod::ReplaceOne
                | CollMethod::DeleteOne
                | CollMethod::DeleteMany
                | CollMethod::FindOneAndUpdate
                | CollMethod::FindOneAndReplace
                | CollMethod::FindOneAndDelete
                | CollMethod::CreateIndex
                | CollMethod::CreateIndexes
                | CollMethod::DropIndex
                | CollMethod::DropIndexes
                | CollMethod::RenameCollection
                | CollMethod::Drop => false,
            };
            read && explain.is_none_or(|v| v == Verbosity::QueryPlanner)
        }
    }
}

/// A `runCommand` document a read-only connection may send: a Read name, an `aggregate`
/// whose pipeline has no `$out` / `$merge` (looked for again here), or an `explain` with
/// `verbosity: "queryPlanner"` of one of those.
fn run_command_allowed(cmd: &Document, nested: bool) -> bool {
    let Some((key, value)) = cmd.iter().next() else {
        return false;
    };
    match key.as_str() {
        "aggregate" => {
            matches!(cmd.get("pipeline"), Some(Bson::Array(p)) if has_out_or_merge(p).is_none())
        }
        "explain" => {
            !nested
                && cmd.get("verbosity").and_then(Bson::as_str) == Some("queryPlanner")
                && matches!(value, Bson::Document(inner) if run_command_allowed(inner, true))
        }
        k => RUN_COMMAND_READS.contains(&k),
    }
}

/// The driver's payload of `cmd`; another driver's is `Internal`.
pub(crate) fn payload_of(cmd: &ParsedCommand) -> Result<&Payload, DbError> {
    cmd.payload
        .downcast_ref::<Payload>()
        .ok_or_else(|| DbError::Internal {
            message: format!("`{}` was not parsed by the MongoDB driver", cmd.name),
        })
}

/// The read-only check of `execute_command`: the allowlist on the command itself, and no
/// drops or `unbounded`. `class` is not read.
pub(crate) fn check_read_only(cmd: &ParsedCommand) -> Result<(), DbError> {
    let payload = payload_of(cmd)?;
    if allowed_read_only(&payload.cmd) && cmd.drops.is_empty() && !cmd.unbounded {
        Ok(())
    } else {
        Err(DbError::Config {
            field: None,
            message: READ_ONLY.into(),
        })
    }
}

/// How the server's answer becomes a result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Shape {
    /// The documents of the cursor (or the reply), one row each.
    Rows,
    /// The `name` of each cursor document, sorted (`show collections`).
    Names,
    /// `countDocuments`: one `value`, the `n` of the one cursor document (0 without one).
    CountStage,
    /// The whole reply, one row.
    Reply,
    /// One `value`: this field of the reply (`n` of `count`, `version` of `buildInfo`).
    Field(&'static str),
    /// One `value` row per element of the reply's `values`.
    Distinct,
    /// `show dbs`: `name` and `sizeOnDisk` of each database.
    Databases,
    /// `insert`: the `_id`s sent, in order.
    Insert {
        ids: Vec<Bson>,
    },
    Update,
    Delete,
    /// `findAndModify`: the reply's `value`, one row when it is a document.
    FindAndModify,
    /// `use`: no rows; the session moves to the plan's database.
    Use,
}

/// The server command one console command becomes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    /// The database the command runs in.
    pub db: String,
    pub body: Document,
    /// The reply is a cursor, read further with `getMore`.
    pub cursor: bool,
    /// `getMore`'s batch size.
    pub batch_size: Option<u32>,
    /// `dnt:<uuid>`, also sent on `getMore`.
    pub comment: String,
    pub shape: Shape,
    /// A read: `opts.timeout` went into its `maxTimeMS`. Otherwise the session enforces
    /// `opts.timeout` itself, by cancelling the command (spec §5.4, §5.5).
    pub read: bool,
}

/// A command before the comment and `maxTimeMS`.
struct Built {
    /// `None`: the session's database.
    db: Option<String>,
    body: Document,
    cursor: bool,
    batch_size: Option<u32>,
    shape: Shape,
    /// A read: `opts.timeout` caps its `maxTimeMS`.
    read: bool,
    /// The user's `maxTimeMS` (a modifier or an option).
    max_time_ms: Option<i64>,
}

impl Built {
    fn new(db: &Option<String>, body: Document, shape: Shape, read: bool) -> Self {
        Built {
            db: db.clone(),
            body,
            cursor: false,
            batch_size: None,
            shape,
            read,
            max_time_ms: None,
        }
    }

    fn cursor(mut self, batch_size: u32) -> Self {
        self.cursor = true;
        self.batch_size = Some(batch_size);
        self
    }

    fn max_time(mut self, ms: Option<i64>) -> Self {
        self.max_time_ms = ms;
        self
    }
}

/// The server command for `cmd`, run in `current_db` unless it names its own database.
/// Every command carries `comment` (inside the document for `runCommand`); a read carries
/// `maxTimeMS = min(the user's, opts.timeout)`.
pub(crate) fn plan(
    cmd: &ShellCommand,
    current_db: &str,
    opts: &ExecOptions,
    comment: &str,
) -> Result<Plan, DbError> {
    let b = build(cmd, current_db, opts)?;
    let mut body = b.body;
    let name = body.keys().next().cloned();
    let cap = if b.read { opts.timeout } else { None };
    // `insert` sets an existing field in place; nothing is ever removed, which would move
    // the next field up into the command name.
    if let Some(ms) = max_time_ms(b.max_time_ms, cap) {
        body.insert("maxTimeMS", Bson::Int64(ms));
    }
    body.insert("comment", comment);
    // The server runs the command its first field names: it must be the one classified.
    if body.keys().next() != name.as_ref() {
        return Err(DbError::Internal {
            message: "the command name changed while building the command".into(),
        });
    }
    Ok(Plan {
        db: b.db.unwrap_or_else(|| current_db.to_string()),
        body,
        cursor: b.cursor,
        batch_size: b.batch_size,
        comment: comment.to_string(),
        shape: b.shape,
        read: b.read,
    })
}

/// The smaller of the user's limit (0: none, as on the server) and `cap`, in ms (at least
/// 1, since 0 would mean no limit).
fn max_time_ms(user: Option<i64>, cap: Option<Duration>) -> Option<i64> {
    let user = user.filter(|n| *n > 0);
    let cap = cap.map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX).max(1));
    match (user, cap) {
        (Some(u), Some(c)) => Some(u.min(c)),
        (u, c) => u.or(c),
    }
}

fn config(message: impl Into<String>) -> DbError {
    DbError::Config {
        field: None,
        message: message.into(),
    }
}

/// The batch size of cursors: `opts.batch_size`, but never more than one past `max_rows`.
fn default_batch(opts: &ExecOptions) -> u32 {
    let n = opts.batch_size.min(opts.max_rows.saturating_add(1)).max(1);
    u32::try_from(n).unwrap_or(u32::MAX).min(i32::MAX as u32)
}

/// A batch size the user gave, as the server takes it.
fn batch_of(v: &Bson) -> Result<u32, DbError> {
    integral_value(v)
        .filter(|n| *n >= 0)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| config("batchSize expects a non-negative integer"))
}

/// The user's options document (the argument at `i`), split into `maxTimeMS` and the
/// rest. `comment` is dropped (the driver's comment marks the command for cancel); `$`
/// fields and session fields are refused, as in `runCommand`.
struct UserOptions {
    rest: Document,
    max_time_ms: Option<i64>,
}

fn user_options(args: &[Bson], i: usize) -> Result<UserOptions, DbError> {
    let mut out = UserOptions {
        rest: Document::new(),
        max_time_ms: None,
    };
    let Some(Bson::Document(d)) = args.get(i) else {
        return Ok(out);
    };
    for (k, v) in d {
        if k.starts_with('$') || SESSION_FIELDS.contains(&k.as_str()) {
            return Err(config(format!("{k} is not allowed in a console command")));
        }
        match k.as_str() {
            "maxTimeMS" => {
                out.max_time_ms = Some(
                    integral_value(v)
                        .filter(|n| *n >= 0)
                        .ok_or_else(|| config("maxTimeMS expects a non-negative integer"))?,
                )
            }
            "comment" => {}
            _ => {
                out.rest.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(out)
}

/// Adds `extra` to `body`; a field `body` already has (the command's own) is refused.
fn extend(body: &mut Document, extra: Document, method: &str) -> Result<(), DbError> {
    for (k, v) in extra {
        if body.contains_key(&k) {
            return Err(config(format!(
                "{k} cannot be set in the options of {method}()"
            )));
        }
        body.insert(k, v);
    }
    Ok(())
}

/// Moves `keys` present in `from` to `to`.
fn move_keys(from: &mut Document, to: &mut Document, keys: &[&str]) {
    for k in keys {
        if let Some(v) = from.remove(*k) {
            to.insert(*k, v);
        }
    }
}

fn doc_at(args: &[Bson], i: usize) -> Document {
    match args.get(i) {
        Some(Bson::Document(d)) => d.clone(),
        _ => Document::new(),
    }
}

fn str_at(args: &[Bson], i: usize) -> String {
    args.get(i)
        .and_then(Bson::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `doc` with an `_id` (a new ObjectId first when it has none), and that `_id`.
fn with_id(doc: &Document) -> (Document, Bson) {
    if let Some(id) = doc.get("_id") {
        return (doc.clone(), id.clone());
    }
    let id = Bson::ObjectId(ObjectId::new());
    let mut out = doc! { "_id": id.clone() };
    out.extend(doc.clone());
    (out, id)
}

/// An update document must be all operators (`{$set: …}`), or a pipeline: a plain
/// document would replace the whole document. A replacement has no operators.
fn check_update(method: CollMethod, update: &Bson) -> Result<(), DbError> {
    let replace = matches!(
        method,
        CollMethod::ReplaceOne | CollMethod::FindOneAndReplace
    );
    let ok = match (replace, update) {
        (false, Bson::Document(d)) => !d.is_empty() && d.keys().all(|k| k.starts_with('$')),
        (false, Bson::Array(_)) => true,
        (true, Bson::Document(d)) => !d.keys().any(|k| k.starts_with('$')),
        _ => false,
    };
    match (ok, replace) {
        (true, _) => Ok(()),
        (false, false) => Err(config(format!(
            "{}() needs update operators such as $set, or a pipeline; use replaceOne() to replace a document",
            method.name()
        ))),
        (false, true) => Err(config(format!(
            "{}() takes a whole document, not update operators",
            method.name()
        ))),
    }
}

/// The default name of an index on `keys`: `a_1_b_-1`, as the shell names it.
fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::String(s) => s.clone(),
                other => match integral_value(other) {
                    Some(n) => n.to_string(),
                    None => other.to_string(),
                },
            };
            format!("{k}_{v}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// The index specs of `createIndex(es)`: one per key document, with the options that
/// belong to an index; `writeConcern` and `commitQuorum` go on the command.
fn create_indexes(coll: &str, keys: &[Document], options: Document) -> Result<Document, DbError> {
    let mut command_level = Document::new();
    let mut rest = options;
    move_keys(
        &mut rest,
        &mut command_level,
        &["writeConcern", "commitQuorum"],
    );
    let mut indexes = Vec::with_capacity(keys.len());
    for k in keys {
        let mut spec = doc! { "key": k.clone() };
        if !rest.contains_key("name") {
            spec.insert("name", index_name(k));
        }
        extend(&mut spec, rest.clone(), "createIndex")?;
        indexes.push(Bson::Document(spec));
    }
    let mut body = doc! { "createIndexes": coll, "indexes": indexes };
    extend(&mut body, command_level, "createIndex")?;
    Ok(body)
}

fn build(cmd: &ShellCommand, current_db: &str, opts: &ExecOptions) -> Result<Built, DbError> {
    match cmd {
        ShellCommand::Use(db) => Ok(Built::new(
            &Some(db.clone()),
            doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true },
            Shape::Use,
            true,
        )
        .cursor(1)),
        ShellCommand::Show(ShowWhat::Databases) => Ok(Built::new(
            &Some("admin".into()),
            doc! { "listDatabases": 1 },
            Shape::Databases,
            true,
        )),
        ShellCommand::Show(ShowWhat::Collections) => {
            Ok(list_collection_names(&None, Document::new(), opts))
        }
        ShellCommand::Db {
            db, method, args, ..
        } => db_method(db, *method, args, opts),
        ShellCommand::Coll {
            db,
            coll,
            method,
            args,
            modifiers,
            explain,
        } => {
            let b = coll_method(db, coll, *method, args, modifiers, current_db, opts)?;
            match explain {
                None => Ok(b),
                Some(v) => explained(b, *method, *v),
            }
        }
    }
}

fn list_collection_names(db: &Option<String>, filter: Document, opts: &ExecOptions) -> Built {
    let mut body = doc! { "listCollections": 1 };
    if !filter.is_empty() {
        body.insert("filter", filter);
    }
    body.insert("nameOnly", true);
    body.insert("authorizedCollections", true);
    body.insert("cursor", doc! { "batchSize": default_batch(opts) as i32 });
    Built::new(db, body, Shape::Names, true).cursor(default_batch(opts))
}

fn db_method(
    db: &Option<String>,
    method: DbMethod,
    args: &[Bson],
    opts: &ExecOptions,
) -> Result<Built, DbError> {
    let admin = Some("admin".to_string());
    Ok(match method {
        DbMethod::GetCollectionNames => list_collection_names(db, doc_at(args, 0), opts),
        DbMethod::Stats => {
            let mut body = doc! { "dbStats": 1 };
            match args.first() {
                Some(Bson::Document(_)) => {
                    let o = user_options(args, 0)?;
                    extend(&mut body, o.rest, "stats")?;
                    return Ok(Built::new(db, body, Shape::Reply, true).max_time(o.max_time_ms));
                }
                Some(scale) => {
                    body.insert("scale", scale.clone());
                }
                None => {}
            }
            Built::new(db, body, Shape::Reply, true)
        }
        DbMethod::ServerStatus => {
            let o = user_options(args, 0)?;
            let mut body = doc! { "serverStatus": 1 };
            extend(&mut body, o.rest, "serverStatus")?;
            Built::new(&admin, body, Shape::Reply, true).max_time(o.max_time_ms)
        }
        DbMethod::Version => Built::new(
            &admin,
            doc! { "buildInfo": 1 },
            Shape::Field("version"),
            true,
        ),
        DbMethod::CreateCollection => {
            let o = user_options(args, 1)?;
            let mut body = doc! { "create": str_at(args, 0) };
            extend(&mut body, o.rest, "createCollection")?;
            Built::new(db, body, Shape::Reply, false).max_time(o.max_time_ms)
        }
        DbMethod::DropDatabase => {
            let mut body = doc! { "dropDatabase": 1 };
            let wc = doc_at(args, 0);
            if !wc.is_empty() {
                body.insert("writeConcern", wc);
            }
            Built::new(db, body, Shape::Reply, false)
        }
        DbMethod::RunCommand | DbMethod::AdminCommand => {
            let db = match method {
                DbMethod::AdminCommand => admin,
                _ => db.clone(),
            };
            // The document is sent as written: the guard classified its first field as the
            // command name, and only fields after it may be changed (in place).
            let body = doc_at(args, 0);
            let first = body.keys().next().map(String::as_str);
            let user_max = match (first, body.get("maxTimeMS")) {
                // `{maxTimeMS: …, drop: …}` names no command the server knows (`Unknown`);
                // it stays that way.
                (Some("maxTimeMS"), _) | (_, None) => None,
                (_, Some(v)) => Some(
                    integral_value(v)
                        .filter(|n| *n >= 0)
                        .ok_or_else(|| config("maxTimeMS expects a non-negative integer"))?,
                ),
            };
            // `getMore` takes no `maxTimeMS` on an ordinary cursor.
            let read = first != Some("getMore") && run_command_allowed(&body, false);
            Built::new(&db, body, Shape::Reply, read).max_time(user_max)
        }
    })
}

fn coll_method(
    db: &Option<String>,
    coll: &str,
    method: CollMethod,
    args: &[Bson],
    modifiers: &[Modifier],
    current_db: &str,
    opts: &ExecOptions,
) -> Result<Built, DbError> {
    let name = method.name();
    Ok(match method {
        CollMethod::Find | CollMethod::FindOne => {
            let o = user_options(args, 2)?;
            let mut body = doc! { "find": coll, "filter": doc_at(args, 0) };
            extend(&mut body, o.rest, name)?;
            let projection = doc_at(args, 1);
            if !projection.is_empty() {
                body.insert("projection", projection);
            }
            let mut user_max = o.max_time_ms;
            for m in modifiers {
                match m {
                    Modifier::Sort(d) => {
                        body.insert("sort", d.clone());
                    }
                    Modifier::Limit(0) => {
                        body.remove("limit");
                        body.remove("singleBatch");
                    }
                    Modifier::Limit(n) => {
                        body.insert("limit", n.saturating_abs());
                        if *n < 0 {
                            body.insert("singleBatch", true);
                        }
                    }
                    Modifier::Skip(n) => {
                        body.insert("skip", *n);
                    }
                    Modifier::Project(d) => {
                        body.insert("projection", d.clone());
                    }
                    Modifier::MaxTimeMs(n) => user_max = Some(*n),
                }
            }
            if method == CollMethod::FindOne {
                body.insert("limit", 1i64);
                body.insert("singleBatch", true);
            }
            let batch = match body.get("batchSize") {
                Some(v) => batch_of(v)?,
                None => {
                    let b = default_batch(opts);
                    body.insert("batchSize", b as i32);
                    b
                }
            };
            Built::new(db, body, Shape::Rows, true)
                .cursor(batch)
                .max_time(user_max)
        }
        CollMethod::CountDocuments => {
            let o = user_options(args, 1)?;
            let mut rest = o.rest;
            let int = |v: Option<Bson>| -> Result<Option<i64>, DbError> {
                match v {
                    None => Ok(None),
                    Some(v) => integral_value(&v)
                        .map(Some)
                        .ok_or_else(|| config("countDocuments() expects integer skip and limit")),
                }
            };
            let mut skip = int(rest.remove("skip"))?;
            let mut limit = int(rest.remove("limit"))?;
            let mut user_max = o.max_time_ms;
            for m in modifiers {
                match m {
                    Modifier::Skip(n) => skip = Some(*n),
                    Modifier::Limit(n) => limit = Some(n.saturating_abs()),
                    Modifier::MaxTimeMs(n) => user_max = Some(*n),
                    Modifier::Sort(_) | Modifier::Project(_) => {}
                }
            }
            let mut pipeline = vec![Bson::Document(doc! { "$match": doc_at(args, 0) })];
            if let Some(n) = skip.filter(|n| *n > 0) {
                pipeline.push(Bson::Document(doc! { "$skip": n }));
            }
            if let Some(n) = limit.filter(|n| *n > 0) {
                pipeline.push(Bson::Document(doc! { "$limit": n }));
            }
            pipeline.push(Bson::Document(
                doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } },
            ));
            let mut body = doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} };
            extend(&mut body, rest, name)?;
            Built::new(db, body, Shape::CountStage, true)
                .cursor(1)
                .max_time(user_max)
        }
        CollMethod::EstimatedDocumentCount => {
            let o = user_options(args, 0)?;
            let mut body = doc! { "count": coll };
            extend(&mut body, o.rest, name)?;
            Built::new(db, body, Shape::Field("n"), true).max_time(o.max_time_ms)
        }
        CollMethod::Distinct => {
            let o = user_options(args, 2)?;
            let mut body =
                doc! { "distinct": coll, "key": str_at(args, 0), "query": doc_at(args, 1) };
            extend(&mut body, o.rest, name)?;
            Built::new(db, body, Shape::Distinct, true).max_time(o.max_time_ms)
        }
        CollMethod::Aggregate => {
            let given = match args.first() {
                Some(Bson::Array(p)) => p.clone(),
                _ => Vec::new(),
            };
            let read = has_out_or_merge(&given).is_none();
            let mut pipeline = given;
            let o = user_options(args, 1)?;
            let mut rest = o.rest;
            let batch = match rest.remove("batchSize") {
                Some(v) => batch_of(&v)?,
                None => default_batch(opts),
            };
            let mut user_max = o.max_time_ms;
            for m in modifiers {
                let stage = match m {
                    Modifier::Sort(d) => doc! { "$sort": d.clone() },
                    Modifier::Skip(n) => doc! { "$skip": *n },
                    Modifier::Limit(0) => continue,
                    Modifier::Limit(n) => doc! { "$limit": n.saturating_abs() },
                    Modifier::Project(d) => doc! { "$project": d.clone() },
                    Modifier::MaxTimeMs(n) => {
                        user_max = Some(*n);
                        continue;
                    }
                };
                pipeline.push(Bson::Document(stage));
            }
            let mut body = doc! {
                "aggregate": coll,
                "pipeline": pipeline,
                "cursor": { "batchSize": batch as i32 },
            };
            extend(&mut body, rest, name)?;
            Built::new(db, body, Shape::Rows, read)
                .cursor(batch)
                .max_time(user_max)
        }
        CollMethod::GetIndexes => {
            let batch = default_batch(opts);
            let body = doc! { "listIndexes": coll, "cursor": { "batchSize": batch as i32 } };
            Built::new(db, body, Shape::Rows, true).cursor(batch)
        }
        CollMethod::Stats => {
            let mut storage = Document::new();
            match args.first() {
                Some(Bson::Document(d)) => {
                    if let Some(scale) = d.get("scale") {
                        storage.insert("scale", scale.clone());
                    }
                }
                Some(scale) => {
                    storage.insert("scale", scale.clone());
                }
                None => {}
            }
            let pipeline = vec![Bson::Document(
                doc! { "$collStats": { "storageStats": storage } },
            )];
            let body = doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} };
            Built::new(db, body, Shape::Rows, true).cursor(default_batch(opts))
        }
        CollMethod::InsertOne | CollMethod::InsertMany => {
            let given: Vec<Document> = match (method, args.first()) {
                (CollMethod::InsertOne, Some(Bson::Document(d))) => vec![d.clone()],
                (_, Some(Bson::Array(a))) => {
                    a.iter().filter_map(|d| d.as_document().cloned()).collect()
                }
                _ => Vec::new(),
            };
            let (docs, ids): (Vec<Bson>, Vec<Bson>) = given
                .iter()
                .map(|d| {
                    let (d, id) = with_id(d);
                    (Bson::Document(d), id)
                })
                .unzip();
            let o = user_options(args, 1)?;
            let mut rest = o.rest;
            let ordered = rest.remove("ordered").unwrap_or(Bson::Boolean(true));
            let mut body = doc! { "insert": coll, "documents": docs, "ordered": ordered };
            extend(&mut body, rest, name)?;
            Built::new(db, body, Shape::Insert { ids }, false).max_time(o.max_time_ms)
        }
        CollMethod::UpdateOne | CollMethod::UpdateMany | CollMethod::ReplaceOne => {
            let update = args.get(1).cloned().unwrap_or(Bson::Null);
            check_update(method, &update)?;
            let o = user_options(args, 2)?;
            let mut rest = o.rest;
            let mut stmt = doc! {
                "q": doc_at(args, 0),
                "u": update,
                "multi": method == CollMethod::UpdateMany,
            };
            move_keys(
                &mut rest,
                &mut stmt,
                &["upsert", "arrayFilters", "collation", "hint", "sort"],
            );
            let mut body = doc! { "update": coll, "updates": [stmt], "ordered": true };
            extend(&mut body, rest, name)?;
            Built::new(db, body, Shape::Update, false).max_time(o.max_time_ms)
        }
        CollMethod::DeleteOne | CollMethod::DeleteMany => {
            let o = user_options(args, 1)?;
            let mut rest = o.rest;
            let limit = i32::from(method == CollMethod::DeleteOne);
            let mut stmt = doc! { "q": doc_at(args, 0), "limit": limit };
            move_keys(&mut rest, &mut stmt, &["collation", "hint"]);
            let mut body = doc! { "delete": coll, "deletes": [stmt], "ordered": true };
            extend(&mut body, rest, name)?;
            Built::new(db, body, Shape::Delete, false).max_time(o.max_time_ms)
        }
        CollMethod::FindOneAndUpdate
        | CollMethod::FindOneAndReplace
        | CollMethod::FindOneAndDelete => {
            let mut body = doc! { "findAndModify": coll, "query": doc_at(args, 0) };
            let options_at = if method == CollMethod::FindOneAndDelete {
                body.insert("remove", true);
                1
            } else {
                let update = args.get(1).cloned().unwrap_or(Bson::Null);
                check_update(method, &update)?;
                body.insert("update", update);
                2
            };
            let o = user_options(args, options_at)?;
            let mut rest = o.rest;
            if let Some(p) = rest.remove("projection") {
                body.insert("fields", p);
            }
            match rest.remove("returnDocument") {
                None => {}
                Some(Bson::String(s)) if s == "after" => {
                    body.insert("new", true);
                }
                Some(Bson::String(s)) if s == "before" => {
                    body.insert("new", false);
                }
                Some(_) => {
                    return Err(config(format!(
                        r#"{name}() expects returnDocument "before" or "after""#
                    )))
                }
            }
            if let Some(v) = rest.remove("returnNewDocument") {
                body.insert("new", v);
            }
            extend(&mut body, rest, name)?;
            Built::new(db, body, Shape::FindAndModify, false).max_time(o.max_time_ms)
        }
        CollMethod::CreateIndex | CollMethod::CreateIndexes => {
            let keys: Vec<Document> = match (method, args.first()) {
                (CollMethod::CreateIndex, Some(Bson::Document(d))) => vec![d.clone()],
                (_, Some(Bson::Array(a))) => {
                    a.iter().filter_map(|d| d.as_document().cloned()).collect()
                }
                _ => Vec::new(),
            };
            let o = user_options(args, 1)?;
            let body = create_indexes(coll, &keys, o.rest)?;
            Built::new(db, body, Shape::Reply, false).max_time(o.max_time_ms)
        }
        CollMethod::DropIndex | CollMethod::DropIndexes => {
            let index = args.first().cloned().unwrap_or_else(|| Bson::from("*"));
            Built::new(
                db,
                doc! { "dropIndexes": coll, "index": index },
                Shape::Reply,
                false,
            )
        }
        CollMethod::RenameCollection => {
            let in_db = db.clone().unwrap_or_else(|| current_db.to_string());
            let body = doc! {
                "renameCollection": format!("{in_db}.{coll}"),
                "to": format!("{in_db}.{}", str_at(args, 0)),
                "dropTarget": args.get(1).cloned().unwrap_or(Bson::Boolean(false)),
            };
            Built::new(&Some("admin".into()), body, Shape::Reply, false)
        }
        CollMethod::Drop => {
            let o = user_options(args, 0)?;
            let mut body = doc! { "drop": coll };
            extend(&mut body, o.rest, name)?;
            Built::new(db, body, Shape::Reply, false).max_time(o.max_time_ms)
        }
    })
}

/// `{explain: <the command>, verbosity}`: one reply, never executed as a write.
fn explained(b: Built, method: CollMethod, v: Verbosity) -> Result<Built, DbError> {
    match method {
        CollMethod::Find
        | CollMethod::FindOne
        | CollMethod::CountDocuments
        | CollMethod::EstimatedDocumentCount
        | CollMethod::Distinct
        | CollMethod::Aggregate
        | CollMethod::UpdateOne
        | CollMethod::UpdateMany
        | CollMethod::ReplaceOne
        | CollMethod::DeleteOne
        | CollMethod::DeleteMany
        | CollMethod::FindOneAndUpdate
        | CollMethod::FindOneAndReplace
        | CollMethod::FindOneAndDelete => {}
        CollMethod::GetIndexes
        | CollMethod::Stats
        | CollMethod::InsertOne
        | CollMethod::InsertMany
        | CollMethod::CreateIndex
        | CollMethod::CreateIndexes
        | CollMethod::DropIndex
        | CollMethod::DropIndexes
        | CollMethod::RenameCollection
        | CollMethod::Drop => {
            return Err(DbError::Unsupported {
                feature: format!("explain of {}()", method.name()),
                version: String::new(),
            })
        }
    }
    let body = doc! { "explain": b.body, "verbosity": v.name() };
    Ok(Built::new(&b.db, body, Shape::Reply, true).max_time(b.max_time_ms))
}

/// Why running a plan failed: a driver error (mapped with the command's class by
/// [`map_error`]) or one already mapped.
pub(crate) enum Fail {
    Mongo(mongodb::error::Error),
    Db(DbError),
}

impl From<mongodb::error::Error> for Fail {
    fn from(e: mongodb::error::Error) -> Self {
        Fail::Mongo(e)
    }
}

impl From<DbError> for Fail {
    fn from(e: DbError) -> Self {
        Fail::Db(e)
    }
}

impl Fail {
    /// The command was stopped by a `killOp` (`Interrupted`): as a command error, or as
    /// the write error of a write's reply.
    pub(crate) fn interrupted(&self) -> bool {
        match self {
            Fail::Mongo(e) => {
                matches!(e.kind.as_ref(), ErrorKind::Command(c) if c.code == INTERRUPTED)
            }
            Fail::Db(DbError::Server { code: Some(c), .. }) => c == "Interrupted",
            Fail::Db(_) => false,
        }
    }
}

/// A result, buffered whole before anything reaches the sink.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Output {
    /// `None`: the columns of `docs` ([`columns_of`]).
    pub cols: Option<Vec<String>>,
    pub docs: Vec<Document>,
    pub truncated: bool,
    pub affected: Option<u64>,
}

impl Output {
    fn rows(docs: Vec<Document>, truncated: bool) -> Self {
        Output {
            cols: None,
            docs,
            truncated,
            affected: None,
        }
    }

    /// One column `col`, one row per value.
    fn column(col: &str, values: Vec<Bson>, max_rows: usize) -> Self {
        let truncated = values.len() > max_rows;
        let docs = values
            .into_iter()
            .take(max_rows)
            .map(|v| doc! { col: v })
            .collect();
        Output {
            cols: Some(vec![col.to_string()]),
            docs,
            truncated,
            affected: None,
        }
    }
}

/// Sends `plan` and buffers its result: a cursor is read until `opts.max_rows + 1`
/// documents, its end, or `opts.stop`; a cursor left open is dropped (the crate kills it).
/// `class`: the command's class, for what a stop says about its outcome. With `session`
/// (a transaction of `execute_atomic`, whose `Client` `client` must be), the command runs
/// in it; only a command without a cursor can.
pub(crate) async fn run(
    client: &Client,
    plan: Plan,
    opts: &ExecOptions,
    class: &CommandClass,
    session: Option<&mut ClientSession>,
) -> Result<Output, Fail> {
    let db = client.database(&plan.db);
    if let Some(s) = session {
        if plan.cursor {
            return Err(Fail::Db(DbError::Internal {
                message: "a command with a cursor cannot run in a transaction".into(),
            }));
        }
        let reply = db.run_command(plan.body).session(s).await?;
        return reply_output(plan.shape, reply, opts).map_err(Fail::Db);
    }
    if plan.cursor {
        let mut action = db
            .run_cursor_command(plan.body)
            .comment(Bson::String(plan.comment));
        if let Some(n) = plan.batch_size {
            action = action.batch_size(n);
        }
        let cursor = action.await?;
        if plan.shape == Shape::Use {
            return Ok(Output {
                cols: Some(Vec::new()),
                docs: Vec::new(),
                truncated: false,
                affected: None,
            });
        }
        let (docs, truncated) = read_cursor(cursor, opts, class).await?;
        cursor_output(plan.shape, docs, truncated, opts)
    } else {
        let reply = db.run_command(plan.body).await?;
        reply_output(plan.shape, reply, opts).map_err(Fail::Db)
    }
}

async fn read_cursor(
    mut cursor: Cursor<Document>,
    opts: &ExecOptions,
    class: &CommandClass,
) -> Result<(Vec<Document>, bool), Fail> {
    let mut docs = Vec::new();
    while docs.len() <= opts.max_rows {
        if opts.stop.is_stopped() {
            return Err(Fail::Db(stopped(class)));
        }
        if !cursor.advance().await? {
            return Ok((docs, false));
        }
        docs.push(cursor.deserialize_current()?);
    }
    docs.truncate(opts.max_rows);
    Ok((docs, true))
}

/// A stop while the command's cursor is read: the command was sent, so a command that
/// may write (an `aggregate` with `$out` / `$merge` wrote before its first reply) may
/// have applied.
fn stopped(class: &CommandClass) -> DbError {
    DbError::Cancelled {
        outcome_unknown: *class != CommandClass::Read,
        was_in_tx: false,
    }
}

fn cursor_output(
    shape: Shape,
    docs: Vec<Document>,
    truncated: bool,
    opts: &ExecOptions,
) -> Result<Output, Fail> {
    Ok(match shape {
        Shape::Rows => Output::rows(docs, truncated),
        Shape::Names => {
            let mut names: Vec<Bson> = docs
                .into_iter()
                .filter_map(|mut d| d.remove("name"))
                .collect();
            names.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            let mut out = Output::column("name", names, opts.max_rows);
            out.truncated |= truncated;
            out
        }
        Shape::CountStage => {
            let n = docs
                .first()
                .and_then(|d| d.get("n").cloned())
                .unwrap_or(Bson::Int32(0));
            Output::column("value", vec![n], 1)
        }
        Shape::Reply
        | Shape::Field(_)
        | Shape::Distinct
        | Shape::Databases
        | Shape::Insert { .. }
        | Shape::Update
        | Shape::Delete
        | Shape::FindAndModify
        | Shape::Use => {
            return Err(Fail::Db(DbError::Internal {
                message: format!("{shape:?} is not the shape of a cursor"),
            }))
        }
    })
}

fn count_of(reply: &Document, key: &str) -> u64 {
    reply
        .get(key)
        .and_then(integral_value)
        .and_then(|n| u64::try_from(n).ok())
        .unwrap_or(0)
}

/// The `result` row of a write (spec §5.4).
fn write_result(
    affected: u64,
    matched: u64,
    modified: u64,
    deleted: u64,
    inserted: Vec<Bson>,
    upserted: Bson,
) -> Output {
    let n = |v: u64| Bson::Int64(i64::try_from(v).unwrap_or(i64::MAX));
    let result = doc! {
        "acknowledged": true,
        "matchedCount": n(matched),
        "modifiedCount": n(modified),
        "deletedCount": n(deleted),
        "insertedIds": inserted,
        "upsertedId": upserted,
    };
    Output {
        cols: Some(vec!["result".into()]),
        docs: vec![doc! { "result": result }],
        truncated: false,
        affected: Some(affected),
    }
}

fn reply_output(shape: Shape, reply: Document, opts: &ExecOptions) -> Result<Output, DbError> {
    check_write_reply(&shape, &reply)?;
    Ok(match shape {
        Shape::Reply => Output::rows(vec![reply], false),
        Shape::Field(f) => {
            let v = reply.get(f).cloned().unwrap_or(Bson::Null);
            Output::column("value", vec![v], 1)
        }
        Shape::Distinct => {
            let values = match reply.get("values") {
                Some(Bson::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            Output::column("value", values, opts.max_rows)
        }
        Shape::Databases => {
            let dbs: Vec<Document> = match reply.get("databases") {
                Some(Bson::Array(a)) => a
                    .iter()
                    .filter_map(Bson::as_document)
                    .map(|d| {
                        doc! {
                            "name": d.get("name").cloned().unwrap_or(Bson::Null),
                            "sizeOnDisk": d.get("sizeOnDisk").cloned().unwrap_or(Bson::Null),
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let truncated = dbs.len() > opts.max_rows;
            Output {
                cols: Some(vec!["name".into(), "sizeOnDisk".into()]),
                docs: dbs.into_iter().take(opts.max_rows).collect(),
                truncated,
                affected: None,
            }
        }
        Shape::Insert { ids } => {
            let n = count_of(&reply, "n");
            write_result(n, 0, 0, 0, ids, Bson::Null)
        }
        Shape::Update => {
            let upserted = match reply.get("upserted") {
                Some(Bson::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let matched = count_of(&reply, "n").saturating_sub(upserted.len() as u64);
            let modified = count_of(&reply, "nModified");
            let upserted_id = upserted
                .first()
                .and_then(Bson::as_document)
                .and_then(|d| d.get("_id").cloned())
                .unwrap_or(Bson::Null);
            write_result(matched, matched, modified, 0, Vec::new(), upserted_id)
        }
        Shape::Delete => {
            let n = count_of(&reply, "n");
            write_result(n, 0, 0, n, Vec::new(), Bson::Null)
        }
        Shape::FindAndModify => match reply.get("value") {
            Some(Bson::Document(d)) => Output {
                affected: Some(1),
                ..Output::rows(vec![d.clone()], false)
            },
            _ => Output {
                affected: Some(0),
                ..Output::rows(Vec::new(), false)
            },
        },
        Shape::Rows | Shape::Names | Shape::CountStage | Shape::Use => {
            return Err(DbError::Internal {
                message: format!("{shape:?} is not the shape of a reply"),
            })
        }
    })
}

/// A write's reply carries its errors (`ok: 1` with `writeErrors`): the first is the
/// error, and for `insertMany` it says how many documents went in before it. Any reply
/// (`findAndModify`, `createIndexes`, `drop`, `dropDatabase`, `runCommand`…) may carry a
/// `writeConcernError`: the command was applied but its write concern was not met, which
/// is an error too ([`write_concern_error`]). `run_command` does not check either.
fn check_write_reply(shape: &Shape, reply: &Document) -> Result<(), DbError> {
    let writes = matches!(shape, Shape::Insert { .. } | Shape::Update | Shape::Delete);
    if let Some(e) = reply
        .get_array("writeErrors")
        .ok()
        .filter(|_| writes)
        .and_then(|a| a.first())
        .and_then(Bson::as_document)
    {
        let (code, message) = write_error(e);
        let message = match shape {
            Shape::Insert { ids } if ids.len() > 1 => format!(
                "Inserted {} of {} before the error: {message}",
                count_of(reply, "n"),
                ids.len()
            ),
            _ => message,
        };
        return Err(server(code, message));
    }
    if let Ok(e) = reply.get_document("writeConcernError") {
        let (code, message) = write_error(e);
        return Err(write_concern_error(code, &message));
    }
    Ok(())
}

/// The start of a write concern error's message ([`write_concern_error`]).
const WRITE_CONCERN_ERROR: &str = "Write concern error: ";

/// A write concern error: the command was applied, but its write concern was not met. The
/// server's message is redacted after the prefix, so the prefix always stays
/// ([`is_write_concern_error`]).
fn write_concern_error(code: String, message: &str) -> DbError {
    DbError::Server {
        code: Some(code),
        sqlstate: None,
        message: format!("{WRITE_CONCERN_ERROR}{}", redact_message(message)),
        position: None,
    }
}

/// Whether `e` is a [`write_concern_error`].
pub(crate) fn is_write_concern_error(e: &DbError) -> bool {
    matches!(e, DbError::Server { message, .. } if message.starts_with(WRITE_CONCERN_ERROR))
}

/// The code name and message of a write error document.
fn write_error(e: &Document) -> (String, String) {
    let code = e.get("code").and_then(integral_value).unwrap_or_default();
    let code = i32::try_from(code).unwrap_or_default();
    let name = match e.get_str("codeName") {
        Ok(n) if !n.is_empty() => n.to_string(),
        _ => code_name(code),
    };
    let mut message = e.get_str("errmsg").unwrap_or_default().to_string();
    if code == DOCUMENT_VALIDATION_FAILURE {
        if let Ok(info) = e.get_document("errInfo") {
            message = with_err_info(message, info);
        }
    }
    (name, message)
}

fn code_name(code: i32) -> String {
    CODE_NAMES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or_else(|| code.to_string(), |(_, n)| (*n).to_string())
}

/// `message` with the validation failure's `errInfo`, without any `consideredValue`: that is
/// the rejected value itself, which may be a secret under a name the redactor does not know
/// (`propertyName: "password"`).
fn with_err_info(message: String, info: &Document) -> String {
    let mut info = Bson::Document(info.clone());
    drop_considered_values(&mut info);
    format!("{message}; errInfo: {}", info.into_relaxed_extjson())
}

/// Removes every `consideredValue` field, at any depth.
fn drop_considered_values(b: &mut Bson) {
    match b {
        Bson::Document(d) => {
            d.remove("consideredValue");
            for (_, v) in d.iter_mut() {
                drop_considered_values(v);
            }
        }
        Bson::Array(a) => a.iter_mut().for_each(drop_considered_values),
        _ => {}
    }
}

/// A server error. The message is redacted: the server quotes the refused command in
/// some errors (`not authorized … to execute command { …, ldapQueryPassword: "…" }`).
fn server(code: String, message: String) -> DbError {
    DbError::Server {
        code: Some(code),
        sqlstate: None,
        message: redact_message(&message),
        position: None,
    }
}

/// `message` unchanged unless it holds a secret. Then the console's redaction masks it
/// (a message does not parse, so all of it), but the server's words before the quoted
/// command are kept when they hold none.
fn redact_message(message: &str) -> String {
    use crate::shell::redact::redact;
    let r = redact(message);
    if !r.changed {
        return message.to_string();
    }
    match message.find('{') {
        Some(i) if !redact(&message[..i]).changed => format!("{}***", &message[..i]),
        _ => r.text,
    }
}

/// The `DbError` of a driver error for a command of `class` (spec §5.6), and whether it is
/// a network error (the session counts it in `reconnects()`). Nothing is retried.
pub(crate) fn map_error(e: &mongodb::error::Error, class: &CommandClass) -> (DbError, bool) {
    let may_have_applied = *class != CommandClass::Read;
    match e.kind.as_ref() {
        ErrorKind::Command(c) if c.code == MAX_TIME_MS_EXPIRED => (
            DbError::Timeout {
                stage: ConnectStage::Execute,
                outcome_unknown: may_have_applied,
                was_in_tx: false,
            },
            false,
        ),
        ErrorKind::Command(c) => {
            let name = if c.code_name.is_empty() {
                code_name(c.code)
            } else {
                c.code_name.clone()
            };
            let mut message = c.message.clone();
            if c.code == DOCUMENT_VALIDATION_FAILURE {
                let info = e
                    .server_response()
                    .and_then(|r| Document::try_from(r.as_ref()).ok())
                    .and_then(|r| r.get_document("errInfo").ok().cloned());
                if let Some(info) = info {
                    message = with_err_info(message, &info);
                }
            }
            (server(name, message), false)
        }
        // The crate's own writes (`commitTransaction`): applied, write concern not met.
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => {
            let name = if w.code_name.is_empty() {
                code_name(w.code)
            } else {
                w.code_name.clone()
            };
            (write_concern_error(name, &w.message), false)
        }
        ErrorKind::Io(_) | ErrorKind::ConnectionPoolCleared { .. } => (
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: may_have_applied,
                message: redact_message(&e.kind.to_string()),
            },
            true,
        ),
        // No server to send it to: nothing was sent.
        ErrorKind::ServerSelection { message, .. } => (
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                message: message.clone(),
            },
            true,
        ),
        ErrorKind::Authentication { message, .. } => (
            DbError::Auth {
                message: message.clone(),
            },
            false,
        ),
        // The kind's text may quote the server's reply.
        _ => (
            DbError::Internal {
                message: redact_message(&e.kind.to_string()),
            },
            false,
        ),
    }
}

/// Reports `out` to `sink`: `result_start` exactly once, the rows in batches of
/// `opts.batch_size`, then `result_end`. With [`CellMode::Full`] every cell is checked
/// against `max_cell_bytes` first, so an oversized cell is an error and nothing is sent.
pub(crate) fn emit(
    sink: &mut dyn RowSink,
    out: Output,
    opts: &ExecOptions,
    started: Instant,
) -> Result<ResultSummary, DbError> {
    let cols = out.cols.unwrap_or_else(|| columns_of(&out.docs));
    let metas = column_metas(&cols, &out.docs);
    let rows: Vec<Vec<Value>> = out
        .docs
        .iter()
        .map(|d| row_of(d, &cols, &opts.cells))
        .collect();
    let batch = opts.batch_size.max(1);
    let row_count = rows.len() as u64;
    match opts.cells {
        CellMode::Preview => {
            sink.result_start(&metas);
            let mut rows = rows.into_iter().peekable();
            while rows.peek().is_some() {
                sink.rows(rows.by_ref().take(batch).collect());
            }
        }
        CellMode::Full { max_cell_bytes } => {
            let mut full = Vec::with_capacity(rows.len());
            for row in rows {
                let cells = row
                    .into_iter()
                    .map(|v| {
                        let f = FullValue::try_from_value(v).map_err(|_| DbError::Internal {
                            message: "a MongoDB cell lost data".into(),
                        })?;
                        check_cell_size(&f, max_cell_bytes)?;
                        Ok(f)
                    })
                    .collect::<Result<Vec<FullValue>, DbError>>()?;
                full.push(cells);
            }
            sink.result_start(&metas);
            let mut full = full.into_iter().peekable();
            while full.peek().is_some() {
                sink.full_rows(full.by_ref().take(batch).collect());
            }
        }
    }
    let summary = ResultSummary {
        affected: out.affected,
        row_count,
        truncated: out.truncated,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    sink.result_end(&summary);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::MongoLanguage;
    use dnt_core::command::CommandLanguage;
    use dnt_core::value::ColumnMeta;

    fn parsed(text: &str) -> ParsedCommand {
        MongoLanguage
            .parse(text)
            .unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    fn shell(text: &str) -> ShellCommand {
        payload_of(&parsed(text)).unwrap().cmd.clone()
    }

    fn plan_of(text: &str, opts: &ExecOptions) -> Plan {
        plan(&shell(text), "app", opts, "dnt:u1").unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    fn plan_err(text: &str) -> DbError {
        match plan(&shell(text), "app", &ExecOptions::default(), "dnt:u1") {
            Ok(p) => panic!("{text}: planned {p:?}"),
            Err(e) => e,
        }
    }

    const REFUSED: &[&str] = &[
        "db.c.insertOne({a: 1})",
        "db.c.insertMany([{a: 1}])",
        "db.c.updateOne({}, {$set: {a: 1}})",
        "db.c.updateMany({a: 1}, {$set: {a: 1}})",
        "db.c.replaceOne({}, {a: 1})",
        "db.c.deleteOne({})",
        "db.c.deleteMany({a: 1})",
        "db.c.findOneAndUpdate({}, {$set: {a: 1}})",
        "db.c.findOneAndReplace({}, {a: 1})",
        "db.c.findOneAndDelete({})",
        "db.c.createIndex({a: 1})",
        "db.c.createIndexes([{a: 1}])",
        "db.c.dropIndex('a_1')",
        "db.c.dropIndexes()",
        "db.c.renameCollection('d')",
        "db.c.drop()",
        "db.createCollection('n')",
        "db.dropDatabase()",
        "db.c.aggregate([{$out: 'd'}])",
        "db.c.aggregate([{$merge: 'd'}])",
        "db.c.aggregate([{$facet: {a: [{$match: {}}], b: [{$out: 'd'}]}}])",
        "db.c.aggregate([{$lookup: {from: 'x', as: 'y', pipeline: [{$merge: {into: 'd'}}]}}])",
        "db.c.aggregate([{$unionWith: {coll: 'x', pipeline: [{$out: 'd'}]}}])",
        "db.c.explain().deleteMany({})",
        "db.c.explain().aggregate([{$out: 'd'}])",
        "db.c.find().explain('executionStats')",
        "db.c.explain('allPlansExecution').aggregate([])",
        "db.runCommand({insert: 'c', documents: [{}]})",
        "db.runCommand({update: 'c', updates: []})",
        "db.runCommand({delete: 'c', deletes: []})",
        "db.runCommand({findAndModify: 'c', remove: true})",
        "db.runCommand({drop: 'c'})",
        "db.runCommand({create: 'c'})",
        "db.runCommand({aggregate: 'c', pipeline: [{$out: 'd'}], cursor: {}})",
        "db.runCommand({aggregate: 'c', pipeline: [{$facet: {a: [{$merge: 'd'}]}}], cursor: {}})",
        "db.runCommand({aggregate: 'c', pipeline: {}, cursor: {}})",
        "db.runCommand({explain: {find: 'c'}})",
        "db.runCommand({explain: {find: 'c'}, verbosity: 'executionStats'})",
        "db.runCommand({explain: {delete: 'c', deletes: []}, verbosity: 'queryPlanner'})",
        "db.runCommand({explain: {explain: {find: 'c'}}, verbosity: 'queryPlanner'})",
        "db.runCommand({killOp: 1, op: 5})",
        "db.runCommand({setParameter: 1, x: 1})",
        "db.runCommand({createUser: 'u', pwd: 'p', roles: []})",
        "db.runCommand({foo: 1})",
        "db.runCommand({PING: 1})",
        "db.runCommand({})",
        "db.adminCommand({dropDatabase: 1})",
        "db.adminCommand({shutdown: 1})",
        "db.getSiblingDB('x').dropDatabase()",
    ];

    const ALLOWED: &[&str] = &[
        "use other",
        "show dbs",
        "show collections",
        "db.getCollectionNames()",
        "db.stats()",
        "db.serverStatus()",
        "db.version()",
        "db.c.find({a: 1}).sort({a: 1}).limit(2)",
        "db.c.findOne()",
        "db.c.countDocuments({})",
        "db.c.find().count()",
        "db.c.estimatedDocumentCount()",
        "db.c.distinct('a')",
        "db.c.getIndexes()",
        "db.c.stats()",
        "db.c.aggregate([{$match: {}}, {$facet: {a: [{$match: {}}]}}])",
        "db.c.find().explain()",
        "db.c.explain('queryPlanner').aggregate([])",
        "db.runCommand({ping: 1})",
        "db.runCommand({find: 'c', filter: {}})",
        "db.runCommand({aggregate: 'c', pipeline: [], cursor: {}})",
        "db.runCommand({explain: {count: 'c'}, verbosity: 'queryPlanner'})",
        "db.adminCommand({listDatabases: 1})",
        "db.adminCommand({currentOp: 1})",
    ];

    #[test]
    fn read_only_allowlist_ignores_class() {
        for text in REFUSED {
            assert!(!allowed_read_only(&shell(text)), "{text}");
            // Even when the guard's class is wrong (says Read), it is refused.
            let mut cmd = parsed(text);
            cmd.class = CommandClass::Read;
            match check_read_only(&cmd) {
                Err(DbError::Config {
                    field: None,
                    message,
                }) => assert_eq!(message, READ_ONLY, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        for text in ALLOWED {
            assert!(allowed_read_only(&shell(text)), "{text}");
            let mut cmd = parsed(text);
            // A class that says the opposite does not matter either.
            cmd.class = CommandClass::Write;
            assert!(check_read_only(&cmd).is_ok(), "{text}");
        }
        // Drops and `unbounded` on the payload are refused whatever the command.
        let mut cmd = parsed("db.c.find()");
        cmd.unbounded = true;
        assert!(check_read_only(&cmd).is_err());
        // Another driver's payload is an internal error.
        let mut cmd = parsed("db.c.find()");
        cmd.payload = std::sync::Arc::new(1u8);
        assert!(matches!(
            check_read_only(&cmd),
            Err(DbError::Internal { .. })
        ));
    }

    #[derive(Default)]
    struct Log {
        events: Vec<String>,
        cols: Vec<Vec<ColumnMeta>>,
        rows: Vec<Vec<Value>>,
    }

    impl RowSink for Log {
        fn result_start(&mut self, cols: &[ColumnMeta]) {
            self.events.push("start".into());
            self.cols.push(cols.to_vec());
        }
        fn rows(&mut self, batch: Vec<Vec<Value>>) {
            self.events.push(format!("rows {}", batch.len()));
            self.rows.extend(batch);
        }
        fn full_rows(&mut self, batch: Vec<Vec<FullValue>>) {
            self.events.push(format!("full {}", batch.len()));
        }
        fn result_end(&mut self, summary: &ResultSummary) {
            self.events
                .push(format!("end {} {}", summary.row_count, summary.truncated));
        }
    }

    #[test]
    fn buffer_emits_one_header() {
        // Fields first seen in a later batch: one header with every column.
        let docs = vec![
            doc! { "_id": 1, "a": 1 },
            doc! { "_id": 2, "a": "x" },
            doc! { "b": 2, "_id": 3 },
            doc! { "_id": 4, "c": { "d": 1 } },
            doc! { "_id": 5 },
        ];
        let opts = ExecOptions {
            batch_size: 2,
            ..ExecOptions::default()
        };
        let mut log = Log::default();
        let out = Output::rows(docs, true);
        let s = emit(&mut log, out, &opts, Instant::now()).unwrap();
        assert_eq!(
            log.events,
            ["start", "rows 2", "rows 2", "rows 1", "end 5 true"]
        );
        let cols: Vec<(&str, &str)> = log.cols[0]
            .iter()
            .map(|c| (c.name.as_str(), c.db_type.as_str()))
            .collect();
        assert_eq!(
            cols,
            [
                ("_id", "int"),
                ("a", "mixed(int|string)"),
                ("b", "int"),
                ("c", "object")
            ]
        );
        assert_eq!(
            log.rows[2],
            [Value::Int(3), Value::Null, Value::Int(2), Value::Null]
        );
        assert_eq!((s.row_count, s.truncated, s.affected), (5, true, None));

        // No rows: a header, no `rows` call.
        let mut log = Log::default();
        emit(&mut log, Output::rows(vec![], false), &opts, Instant::now()).unwrap();
        assert_eq!(log.events, ["start", "end 0 false"]);

        // Full cells: checked before anything is sent.
        let full = ExecOptions {
            cells: CellMode::Full { max_cell_bytes: 3 },
            ..ExecOptions::default()
        };
        let mut log = Log::default();
        let out = Output::rows(vec![doc! { "s": "abc" }, doc! { "s": "abcd" }], false);
        assert!(matches!(
            emit(&mut log, out, &full, Instant::now()),
            Err(DbError::Unsupported { .. })
        ));
        assert!(log.events.is_empty());
        let mut log = Log::default();
        let out = Output::rows(vec![doc! { "s": "abc" }], false);
        emit(&mut log, out, &full, Instant::now()).unwrap();
        assert_eq!(log.events, ["start", "full 1", "end 1 false"]);
    }

    #[test]
    fn every_command_carries_the_comment() {
        let opts = ExecOptions::default();
        for text in ALLOWED.iter().chain(REFUSED) {
            let Ok(p) = plan(&shell(text), "app", &opts, "dnt:u1") else {
                continue;
            };
            assert_eq!(p.body.get_str("comment"), Ok("dnt:u1"), "{text}");
            assert_eq!(p.comment, "dnt:u1");
        }
        // Inside the user's runCommand document, replacing theirs.
        let p = plan_of("db.runCommand({ping: 1, comment: 'mine'})", &opts);
        assert_eq!(p.body, doc! { "ping": 1, "comment": "dnt:u1" });
        assert_eq!(p.db, "app");
        assert_eq!(plan_of("db.adminCommand({ping: 1})", &opts).db, "admin");
        assert_eq!(
            plan_of("db.getSiblingDB('x').runCommand({ping: 1})", &opts).db,
            "x"
        );
    }

    #[test]
    fn reads_take_the_smaller_max_time() {
        let none = ExecOptions::default();
        let t = |ms: u64| ExecOptions {
            timeout: Some(Duration::from_millis(ms)),
            ..ExecOptions::default()
        };
        let max =
            |text: &str, opts: &ExecOptions| plan_of(text, opts).body.get("maxTimeMS").cloned();
        assert_eq!(max("db.c.find()", &none), None);
        assert_eq!(max("db.c.find()", &t(500)), Some(Bson::Int64(500)));
        assert_eq!(
            max("db.c.find().maxTimeMS(200)", &t(500)),
            Some(Bson::Int64(200))
        );
        assert_eq!(
            max("db.c.find().maxTimeMS(900)", &t(500)),
            Some(Bson::Int64(500))
        );
        assert_eq!(
            max("db.c.find().maxTimeMS(900)", &none),
            Some(Bson::Int64(900))
        );
        // 0 is no limit on the server, so it is no limit here.
        assert_eq!(
            max("db.c.find().maxTimeMS(0)", &t(500)),
            Some(Bson::Int64(500))
        );
        assert_eq!(
            max("db.c.find({}, {}, {maxTimeMS: 50})", &t(500)),
            Some(Bson::Int64(50))
        );
        assert_eq!(
            max("db.c.aggregate([], {maxTimeMS: 50})", &none),
            Some(Bson::Int64(50))
        );
        assert_eq!(
            max("db.c.countDocuments({}, {maxTimeMS: 70})", &t(60)),
            Some(Bson::Int64(60))
        );
        assert_eq!(
            max("db.runCommand({count: 'c', maxTimeMS: 70})", &t(60)),
            Some(Bson::Int64(60))
        );
        assert_eq!(max("db.c.explain().find()", &t(60)), Some(Bson::Int64(60)));
        // A write keeps only the user's own limit.
        assert_eq!(max("db.c.insertOne({a: 1})", &t(60)), None);
        assert_eq!(max("db.c.aggregate([{$out: 'd'}])", &t(60)), None);
        assert_eq!(
            max(
                "db.c.updateMany({a: 1}, {$set: {a: 2}}, {maxTimeMS: 70})",
                &t(60)
            ),
            Some(Bson::Int64(70))
        );
        assert_eq!(
            max("db.runCommand({insert: 'c', documents: []})", &t(60)),
            None
        );
        assert_eq!(
            max("db.runCommand({getMore: 1, collection: 'c'})", &t(60)),
            None
        );
    }

    #[test]
    fn modifiers_become_fields_and_stages() {
        let opts = ExecOptions {
            batch_size: 50,
            ..ExecOptions::default()
        };
        let p = plan_of(
            "db.c.find({a: 1}, {a: 1}).sort({a: -1}).skip(2).limit(-3)",
            &opts,
        );
        assert!(p.cursor);
        assert_eq!(p.batch_size, Some(50));
        let mut body = p.body;
        body.remove("comment");
        assert_eq!(
            body,
            doc! {
                "find": "c", "filter": {"a": 1}, "projection": {"a": 1}, "sort": {"a": -1},
                "skip": 2i64, "limit": 3i64, "singleBatch": true, "batchSize": 50,
            }
        );
        // After aggregate: stages after the pipeline; batchSize in `cursor`.
        let p = plan_of(
            "db.c.aggregate([{$match: {}}], {batchSize: 7, allowDiskUse: true}).sort({a: 1}).skip(1).limit(2).project({a: 1}).maxTimeMS(9)",
            &opts,
        );
        assert_eq!(p.batch_size, Some(7));
        assert_eq!(
            p.body.get_array("pipeline").unwrap(),
            &vec![
                Bson::Document(doc! { "$match": {} }),
                Bson::Document(doc! { "$sort": {"a": 1} }),
                Bson::Document(doc! { "$skip": 1i64 }),
                Bson::Document(doc! { "$limit": 2i64 }),
                Bson::Document(doc! { "$project": {"a": 1} }),
            ]
        );
        assert_eq!(
            p.body.get_document("cursor").unwrap(),
            &doc! { "batchSize": 7 }
        );
        assert_eq!(p.body.get_bool("allowDiskUse"), Ok(true));
        assert_eq!(p.body.get("maxTimeMS"), Some(&Bson::Int64(9)));
        // find().count(): countDocuments with the skip and limit written before it.
        let p = plan_of("db.c.find({a: 1}).skip(1).limit(2).count()", &opts);
        assert_eq!(p.shape, Shape::CountStage);
        assert_eq!(
            p.body.get_array("pipeline").unwrap(),
            &vec![
                Bson::Document(doc! { "$match": {"a": 1} }),
                Bson::Document(doc! { "$skip": 1i64 }),
                Bson::Document(doc! { "$limit": 2i64 }),
                Bson::Document(doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } }),
            ]
        );
        // stats(): the $collStats stage, not the deprecated collStats command.
        let p = plan_of("db.c.stats(1024)", &opts);
        assert_eq!(
            p.body.get_array("pipeline").unwrap(),
            &vec![Bson::Document(
                doc! { "$collStats": { "storageStats": { "scale": 1024 } } }
            )]
        );
        // explain wraps the command; a sibling db and the verbosity are kept.
        let p = plan_of(
            "db.getSiblingDB('x').c.explain('executionStats').find({a: 1})",
            &opts,
        );
        assert_eq!(p.db, "x");
        assert_eq!(p.body.get_str("verbosity"), Ok("executionStats"));
        assert_eq!(
            p.body.get_document("explain").unwrap().get_str("find"),
            Ok("c")
        );
        assert!(!p.cursor);
        // A rename names the database on both sides.
        let p = plan_of("db.c.renameCollection('d', true)", &opts);
        assert_eq!(
            (p.db.as_str(), p.body.get_str("to")),
            ("admin", Ok("app.d"))
        );
        // Writes: one statement, the user's statement-level options on it.
        let p = plan_of(
            "db.c.updateMany({a: 1}, {$set: {b: 1}}, {upsert: true, writeConcern: {w: 1}})",
            &opts,
        );
        assert_eq!(
            p.body.get_array("updates").unwrap(),
            &vec![Bson::Document(
                doc! { "q": {"a": 1}, "u": {"$set": {"b": 1}}, "multi": true, "upsert": true }
            )]
        );
        assert_eq!(
            p.body.get_document("writeConcern").unwrap(),
            &doc! { "w": 1 }
        );
        let p = plan_of("db.c.deleteMany({})", &opts);
        assert_eq!(
            p.body.get_array("deletes").unwrap(),
            &vec![Bson::Document(doc! { "q": {}, "limit": 0 })]
        );
        let p = plan_of(
            "db.c.insertMany([{_id: 1}, {a: 2}], {ordered: false})",
            &opts,
        );
        assert_eq!(p.body.get_bool("ordered"), Ok(false));
        match &p.shape {
            Shape::Insert { ids } => {
                assert_eq!(ids[0], Bson::Int32(1));
                assert!(matches!(ids[1], Bson::ObjectId(_)));
            }
            other => panic!("{other:?}"),
        }
        let p = plan_of("db.c.createIndex({a: 1, b: -1}, {unique: true})", &opts);
        assert_eq!(
            p.body.get_array("indexes").unwrap(),
            &vec![Bson::Document(
                doc! { "key": {"a": 1, "b": -1}, "name": "a_1_b_-1", "unique": true }
            )]
        );
    }

    #[test]
    fn options_cannot_retarget_or_carry_session_fields() {
        for text in [
            "db.c.find({}, {}, {find: 'other'})",
            "db.c.find({}, {}, {filter: {}})",
            "db.c.aggregate([], {pipeline: []})",
            "db.c.aggregate([], {cursor: {}})",
            "db.c.countDocuments({}, {aggregate: 'other'})",
            "db.c.insertOne({}, {documents: []})",
            "db.c.deleteOne({}, {deletes: []})",
            "db.c.find({}, {}, {lsid: {}})",
            "db.c.find({}, {}, {$db: 'other'})",
            "db.c.find({}, {}, {maxTimeMS: 'x'})",
            "db.c.updateOne({}, {a: 1})",
            "db.c.updateOne({}, {})",
            "db.c.replaceOne({}, {$set: {a: 1}})",
            "db.c.findOneAndReplace({}, {$set: {a: 1}})",
            "db.c.findOneAndUpdate({}, {a: 1}, {returnDocument: 'later'})",
        ] {
            assert!(
                matches!(plan_err(text), DbError::Config { field: None, .. }),
                "{text}"
            );
        }
        // The user's comment is replaced by the driver's.
        let p = plan_of(
            "db.c.find({}, {}, {comment: 'mine'})",
            &ExecOptions::default(),
        );
        assert_eq!(p.body.get_str("comment"), Ok("dnt:u1"));
        assert!(matches!(
            plan_err("db.c.explain().drop()"),
            DbError::Unsupported { .. }
        ));
    }

    fn reply(shape: Shape, r: Document) -> Result<Output, DbError> {
        reply_output(shape, r, &ExecOptions::default())
    }

    #[test]
    fn write_replies() {
        let ids = vec![Bson::Int32(1), Bson::Int32(2), Bson::Int32(3)];
        let out = reply(
            Shape::Insert { ids: ids.clone() },
            doc! { "n": 3, "ok": 1.0 },
        )
        .unwrap();
        assert_eq!(out.affected, Some(3));
        assert_eq!(out.cols, Some(vec!["result".to_string()]));
        let result = out.docs[0].get_document("result").unwrap();
        assert_eq!(result.get_array("insertedIds").unwrap(), &ids);
        let dup = doc! {
            "n": 1, "ok": 1.0,
            "writeErrors": [{ "index": 1, "code": 11000, "errmsg": "E11000 duplicate key error" }],
        };
        match reply(Shape::Insert { ids }, dup.clone()) {
            Err(DbError::Server { code, message, .. }) => {
                assert_eq!(code.as_deref(), Some("DuplicateKey"));
                assert_eq!(
                    message,
                    "Inserted 1 of 3 before the error: E11000 duplicate key error"
                );
            }
            other => panic!("{other:?}"),
        }
        // insertOne: the error as is.
        match reply(
            Shape::Insert {
                ids: vec![Bson::Int32(1)],
            },
            dup,
        ) {
            Err(DbError::Server { message, .. }) => {
                assert_eq!(message, "E11000 duplicate key error")
            }
            other => panic!("{other:?}"),
        }
        let out = reply(
            Shape::Update,
            doc! { "n": 3, "nModified": 1, "upserted": [{ "index": 0, "_id": 9 }], "ok": 1.0 },
        )
        .unwrap();
        assert_eq!(out.affected, Some(2));
        let result = out.docs[0].get_document("result").unwrap();
        assert_eq!(result.get_i64("matchedCount"), Ok(2));
        assert_eq!(result.get_i64("modifiedCount"), Ok(1));
        assert_eq!(result.get("upsertedId"), Some(&Bson::Int32(9)));
        let out = reply(Shape::Delete, doc! { "n": 4, "ok": 1.0 }).unwrap();
        assert_eq!(out.affected, Some(4));
        let out = reply(Shape::FindAndModify, doc! { "value": null, "ok": 1.0 }).unwrap();
        assert_eq!((out.affected, out.docs.len()), (Some(0), 0));
        let out = reply(
            Shape::FindAndModify,
            doc! { "value": { "_id": 1 }, "ok": 1.0 },
        )
        .unwrap();
        assert_eq!((out.affected, out.docs.len()), (Some(1), 1));
        // Validation failures say why.
        let invalid = doc! {
            "n": 0, "ok": 1.0,
            "writeErrors": [{
                "index": 0, "code": 121, "errmsg": "Document failed validation",
                "errInfo": { "failingDocumentId": 1 },
            }],
        };
        match reply(Shape::Update, invalid) {
            Err(DbError::Server { code, message, .. }) => {
                assert_eq!(code.as_deref(), Some("DocumentValidationFailure"));
                assert!(
                    message.contains("errInfo: {\"failingDocumentId\":1}"),
                    "{message}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// Every reply's `writeConcernError` is an error, not only an insert's, update's or
    /// delete's: `findAndModify`, `createIndexes`, `drop`, `dropDatabase`, `runCommand`.
    #[test]
    fn every_reply_reports_its_write_concern_error() {
        let wce = doc! { "code": 100, "codeName": "UnsatisfiableWriteConcern", "errmsg": "Not enough data-bearing nodes" };
        for (shape, mut r) in [
            (Shape::Update, doc! { "n": 1, "nModified": 1, "ok": 1.0 }),
            (
                Shape::Insert {
                    ids: vec![Bson::Int32(1)],
                },
                doc! { "n": 1, "ok": 1.0 },
            ),
            (Shape::Delete, doc! { "n": 1, "ok": 1.0 }),
            // findAndModify
            (
                Shape::FindAndModify,
                doc! { "value": { "_id": 1 }, "ok": 1.0 },
            ),
            // createIndexes, then drop, then dropDatabase
            (
                Shape::Reply,
                doc! { "numIndexesBefore": 1, "numIndexesAfter": 2, "ok": 1.0 },
            ),
            (
                Shape::Reply,
                doc! { "nIndexesWas": 1, "ns": "d.c", "ok": 1.0 },
            ),
            (Shape::Reply, doc! { "dropped": "d", "ok": 1.0 }),
            (Shape::Field("ok"), doc! { "ok": 1.0 }),
        ] {
            r.insert("writeConcernError", wce.clone());
            let what = format!("{shape:?}");
            match reply(shape, r) {
                Err(e @ DbError::Server { .. }) => {
                    assert!(is_write_concern_error(&e), "{what}: {e:?}");
                    let DbError::Server { code, message, .. } = e else {
                        unreachable!()
                    };
                    assert_eq!(code.as_deref(), Some("UnsatisfiableWriteConcern"), "{what}");
                    assert_eq!(
                        message, "Write concern error: Not enough data-bearing nodes",
                        "{what}"
                    );
                }
                other => panic!("{what}: {other:?}"),
            }
        }
        // Without a code name, the known codes are named.
        match reply(
            Shape::Reply,
            doc! { "ok": 1.0, "writeConcernError": { "code": 79, "errmsg": "x" } },
        ) {
            Err(DbError::Server { code, .. }) => {
                assert_eq!(code.as_deref(), Some("UnknownReplWriteConcern"))
            }
            other => panic!("{other:?}"),
        }
        // A write error is not one; a write error on a plain reply is not looked at.
        let dup = doc! { "n": 0, "ok": 1.0, "writeErrors": [{ "index": 0, "code": 11000, "errmsg": "dup" }] };
        assert!(!is_write_concern_error(
            &reply(Shape::Update, dup.clone()).unwrap_err()
        ));
        assert!(reply(Shape::Reply, dup).is_ok());
        // The prefix stays when the server's message holds a secret.
        let e = write_concern_error("Unknown".into(), r#"pwd: "hunter2""#);
        assert!(is_write_concern_error(&e), "{e:?}");
        assert!(!format!("{e:?}").contains("hunter2"), "{e:?}");
    }

    /// The fallback messages of `map_error` (the kind's own text) are redacted too.
    #[test]
    fn fallback_error_messages_are_redacted() {
        let secret = r#"refused { setParameter: 1, ldapQueryPassword: "hunter2" }"#;
        let io = mongodb::error::Error::from(std::io::Error::other(secret));
        let other = mongodb::error::Error::from(mongodb::bson::ser::Error::from(
            std::io::Error::other(secret),
        ));
        for e in [io, other] {
            let (mapped, _) = map_error(&e, &CommandClass::Read);
            let text = format!("{mapped:?}");
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("refused "), "{text}");
            assert!(
                matches!(
                    mapped,
                    DbError::ConnectionLost { .. } | DbError::Internal { .. }
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn the_sent_command_is_the_classified_one() {
        let none = ExecOptions::default();
        let timeout = ExecOptions {
            timeout: Some(Duration::from_millis(60)),
            ..ExecOptions::default()
        };
        let first = |d: &Document| d.keys().next().cloned();
        // runCommand / adminCommand: the user's document as written, in its order; only
        // `maxTimeMS` (in place) and the trailing `comment` change.
        for text in [
            "db.runCommand({maxTimeMS: 0, dropDatabase: 1})",
            "db.runCommand({maxTimeMS: 5, drop: 'users'})",
            "db.adminCommand({maxTimeMS: 5, drop: 'users'})",
            "db.runCommand({comment: 'x', drop: 'users'})",
            "db.runCommand({drop: 'users', maxTimeMS: 5})",
            "db.runCommand({count: 'c', maxTimeMS: 70, query: {}})",
            "db.runCommand({find: 'c', maxTimeMS: 0})",
            "db.runCommand({ping: 1})",
            "db.getSiblingDB('x').runCommand({dropDatabase: 1, maxTimeMS: 9})",
            "db.adminCommand({explain: {find: 'c'}, verbosity: 'queryPlanner', maxTimeMS: 9})",
        ] {
            let sent_doc = match &shell(text) {
                ShellCommand::Db { args, .. } => doc_at(args, 0),
                other => panic!("{other:?}"),
            };
            for opts in [&none, &timeout] {
                let p = plan_of(text, opts);
                assert_eq!(first(&p.body), first(&sent_doc), "{text}");
                let mut keys: Vec<&String> = p.body.keys().collect();
                if !sent_doc.contains_key("comment") {
                    assert_eq!(keys.pop().map(String::as_str), Some("comment"), "{text}");
                }
                // A read without its own limit gets `opts.timeout`, after its fields.
                if !sent_doc.contains_key("maxTimeMS")
                    && keys.last().map(|k| k.as_str()) == Some("maxTimeMS")
                {
                    keys.pop();
                }
                assert_eq!(keys, sent_doc.keys().collect::<Vec<_>>(), "{text}");
            }
        }
        // A leading `maxTimeMS` is not a command: classified Unknown with no drop, and sent
        // untouched (the server refuses it), never as `dropDatabase`.
        let cmd = parsed("db.runCommand({maxTimeMS: 0, dropDatabase: 1})");
        assert_eq!((cmd.class, cmd.drops.len()), (CommandClass::Unknown, 0));
        let p = plan_of("db.runCommand({maxTimeMS: 5, drop: 'users'})", &timeout);
        assert_eq!(
            p.body,
            doc! { "maxTimeMS": 5, "drop": "users", "comment": "dnt:u1" }
        );
        // A later `maxTimeMS` of a read is capped in place.
        let p = plan_of(
            "db.runCommand({count: 'c', maxTimeMS: 70, query: {}})",
            &timeout,
        );
        assert_eq!(
            p.body,
            doc! { "count": "c", "maxTimeMS": 60i64, "query": {}, "comment": "dnt:u1" }
        );

        // Console methods: options (merged with `extend`) never come before the name.
        for (text, name) in [
            (
                "db.c.find({}, {}, {maxTimeMS: 5, hint: 'x', sort: {a: 1}})",
                "find",
            ),
            ("db.c.findOne({}, {}, {collation: {locale: 'en'}})", "find"),
            ("db.c.countDocuments({}, {hint: 'x', skip: 1})", "aggregate"),
            ("db.c.estimatedDocumentCount({maxTimeMS: 5})", "count"),
            (
                "db.c.distinct('a', {}, {collation: {locale: 'en'}})",
                "distinct",
            ),
            (
                "db.c.aggregate([], {allowDiskUse: true, batchSize: 2})",
                "aggregate",
            ),
            ("db.c.insertOne({a: 1}, {writeConcern: {w: 1}})", "insert"),
            ("db.c.insertMany([{a: 1}], {ordered: false})", "insert"),
            (
                "db.c.updateOne({}, {$set: {a: 1}}, {upsert: true, let: {}})",
                "update",
            ),
            ("db.c.replaceOne({}, {a: 1}, {hint: 'x'})", "update"),
            (
                "db.c.deleteMany({a: 1}, {collation: {locale: 'en'}})",
                "delete",
            ),
            (
                "db.c.findOneAndUpdate({}, {$set: {a: 1}}, {projection: {a: 1}})",
                "findAndModify",
            ),
            ("db.c.findOneAndDelete({}, {sort: {a: 1}})", "findAndModify"),
            (
                "db.c.createIndex({a: 1}, {unique: true, writeConcern: {w: 1}})",
                "createIndexes",
            ),
            ("db.c.drop({writeConcern: {w: 1}})", "drop"),
            (
                "db.createCollection('n', {capped: true, size: 1})",
                "create",
            ),
            ("db.stats({scale: 2})", "dbStats"),
            ("db.serverStatus({repl: 0})", "serverStatus"),
            ("db.c.explain().find({}, {}, {hint: 'x'})", "explain"),
        ] {
            for opts in [&none, &timeout] {
                assert_eq!(
                    first(&plan_of(text, opts).body).as_deref(),
                    Some(name),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn validation_errors_never_show_the_rejected_value() {
        let info = doc! {
            "failingDocumentId": 1,
            "details": {
                "operatorName": "$jsonSchema",
                "schemaRulesNotSatisfied": [{
                    "operatorName": "properties",
                    "propertiesNotSatisfied": [{
                        "propertyName": "password",
                        "details": [{
                            "operatorName": "maxLength",
                            "specifiedAs": { "maxLength": 4 },
                            "reason": "specified string length was not satisfied",
                            "consideredValue": "hunter2-secret",
                        }],
                    }],
                }, {
                    "operatorName": "required",
                    "consideredValue": { "nested": ["hunter2-secret"] },
                }],
            },
        };
        let message = with_err_info("Document failed validation".into(), &info);
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("consideredValue"), "{message}");
        assert!(
            message.contains("\"propertyName\":\"password\""),
            "{message}"
        );
        assert!(message.contains("maxLength"), "{message}");
        // The same through a write reply.
        let r = reply(
            Shape::Insert {
                ids: vec![Bson::Int32(1)],
            },
            doc! {
                "n": 0, "ok": 1.0,
                "writeErrors": [{
                    "index": 0, "code": 121, "errmsg": "Document failed validation",
                    "errInfo": info,
                }],
            },
        );
        match r {
            Err(DbError::Server { message, .. }) => {
                assert!(!message.contains("hunter2"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_stop_while_reading_a_write_cursor_may_have_applied() {
        for (text, unknown) in [
            ("db.c.find()", false),
            ("db.c.aggregate([{$match: {}}])", false),
            ("db.c.aggregate([{$match: {}}, {$out: 'd'}])", true),
            ("db.c.aggregate([{$merge: 'd'}])", true),
        ] {
            assert!(
                matches!(
                    stopped(&parsed(text).class),
                    DbError::Cancelled { outcome_unknown, was_in_tx: false } if outcome_unknown == unknown
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn server_messages_are_redacted() {
        let e = server(
            "Unauthorized".into(),
            r#"not authorized on admin to execute command { setParameter: 1, ldapQueryPassword: "hunter2", $db: "admin" }"#.into(),
        );
        match e {
            DbError::Server { message, .. } => {
                assert!(!message.contains("hunter2"), "{message}");
                assert!(message.starts_with("not authorized on admin"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn single_value_shapes() {
        let opts = ExecOptions {
            max_rows: 2,
            ..ExecOptions::default()
        };
        let out = reply_output(Shape::Distinct, doc! { "values": [1, 2, 3] }, &opts).unwrap();
        assert_eq!(out.cols, Some(vec!["value".to_string()]));
        assert_eq!((out.docs.len(), out.truncated), (2, true));
        let out = reply(Shape::Field("version"), doc! { "version": "8.0.1" }).unwrap();
        assert_eq!(out.docs, vec![doc! { "value": "8.0.1" }]);
        let out = cursor_output(Shape::CountStage, vec![], false, &opts)
            .ok()
            .unwrap();
        assert_eq!(out.docs, vec![doc! { "value": 0 }]);
        let out = cursor_output(
            Shape::Names,
            vec![
                doc! { "name": "b", "type": "collection" },
                doc! { "name": "a" },
            ],
            false,
            &opts,
        )
        .ok()
        .unwrap();
        assert_eq!(out.docs, vec![doc! { "name": "a" }, doc! { "name": "b" }]);
        let out = reply(
            Shape::Databases,
            doc! { "databases": [{ "name": "x", "sizeOnDisk": 10, "empty": false }], "totalSize": 10 },
        )
        .unwrap();
        assert_eq!(out.docs, vec![doc! { "name": "x", "sizeOnDisk": 10 }]);
    }
}
