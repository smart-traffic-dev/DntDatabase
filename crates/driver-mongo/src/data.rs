//! The data view's commands (spec §6.2, §6.3): `page`, `count`, and the edits
//! ([`build_edits`]).
//!
//! `page` is `find(filter).sort(sort + {_id: 1}).skip(skip).limit(limit)` and `count` is
//! `countDocuments(filter)`, both in the object's database and classified like the same
//! console commands (`Read`), so they pass `check_read` and run through the session's
//! own execution path.

//!
//! # Filters
//!
//! - A raw filter (`raw_json`) is a document in the console's literal syntax (spec §4.3:
//!   mongosh or Extended JSON), checked here: a document, with no key at any depth that
//!   runs JavaScript on the server (`$where`, `$function`, `$accumulator`).
//! - A structured filter maps each condition to its operator (spec §6.2). A value that is
//!   compared is coerced by the field's type inferred from the fixed sample (§6.1);
//!   patterns are text. `page` / `count` cannot read the sample (they build, they do not
//!   send), so such a command carries the filter in its payload (`Payload::typed`) and the
//!   session coerces it with the sample's types right before the read ([`resolve`]). Its
//!   `text` shows the values as typed, and ends with a comment that says so.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use dnt_core::command::ParsedCommand;
use dnt_core::data_access::PageSpec;
use dnt_core::edit::{CellInput, EditSet, RowEdit, RowRef};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, TableDetail};
use dnt_core::sql::filter::{
    DataFilter, FilterCond, FilterMode, FilterOp, SortDir, SortSpec, MAX_CONDITIONS,
    MAX_VALUE_BYTES,
};
use dnt_core::value::Value;
use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{doc, Binary, Bson, Decimal128, Document, Regex};

use crate::rows::{canonical_ejson, value_to_bson};
use crate::shell::classify::classify;
use crate::shell::literal::{parse_document, parse_iso_date, uuid_binary};
use crate::shell::{lex, parse_literal, CollMethod, Modifier, Payload, ShellCommand, ShellError};

/// Keys that run JavaScript on the server, refused anywhere in a raw filter (as core's
/// `check_raw_json` does for the JSON it reads).
const SERVER_SIDE_JS: [&str; 3] = ["$where", "$function", "$accumulator"];

const SERVER_JS: &str = "Server-side JavaScript is not allowed in a filter";
const NOT_A_DOCUMENT: &str = "The filter must be a JSON object";
/// Ends the `text` of a typed read, whose values are shown as typed (a line comment, so the
/// text still parses to the same command).
const TYPED_NOTE: &str = " // values are converted to the field types sampled from the collection";

fn config(field: &str, message: impl Into<String>) -> DbError {
    DbError::Config {
        field: Some(field.into()),
        message: message.into(),
    }
}

fn filter_error(message: impl Into<String>) -> DbError {
    config("filter", message)
}

/// One page of `obj`: `find(filter).sort(sort + {_id: 1}).skip(skip).limit(limit)`.
pub(crate) fn page(obj: &ObjectRef, spec: &PageSpec) -> Result<ParsedCommand, DbError> {
    let (filter, typed) = filter_of(spec.filter.as_ref())?;
    let skip = i64::try_from(spec.skip).map_err(|_| config("skip", "The page starts too far"))?;
    let modifiers = vec![
        Modifier::Sort(sort_document(&spec.sort)?),
        Modifier::Skip(skip),
        Modifier::Limit(i64::from(spec.limit)),
    ];
    build(
        obj,
        CollMethod::Find,
        vec![filter.into()],
        modifiers,
        typed,
        None,
    )
}

/// The number of documents of `obj` (matching `filter`): `countDocuments(filter)`.
pub(crate) fn count(
    obj: &ObjectRef,
    filter: Option<&DataFilter>,
) -> Result<ParsedCommand, DbError> {
    let (filter, typed) = filter_of(filter)?;
    build(
        obj,
        CollMethod::CountDocuments,
        vec![filter.into()],
        Vec::new(),
        typed,
        None,
    )
}

/// Collections and views have documents, in a named database.
fn check_object(obj: &ObjectRef) -> Result<(), DbError> {
    match obj.kind {
        ObjectKind::Collection | ObjectKind::View => {}
        ObjectKind::Table
        | ObjectKind::MaterializedView
        | ObjectKind::Function
        | ObjectKind::Procedure
        | ObjectKind::Trigger
        | ObjectKind::Event
        | ObjectKind::Sequence
        | ObjectKind::Type
        | ObjectKind::Key => {
            return Err(config("kind", "Only collections and views have documents"))
        }
    }
    if obj.database.is_empty() {
        return Err(config("database", "a database is required"));
    }
    if obj.name.is_empty() {
        return Err(config("name", "a collection name is required"));
    }
    Ok(())
}

/// `method(args)` with `modifiers` on `obj`, classified as the console classifies it.
/// `typed`, `document`: see [`Payload`].
fn build(
    obj: &ObjectRef,
    method: CollMethod,
    args: Vec<Bson>,
    modifiers: Vec<Modifier>,
    typed: Option<DataFilter>,
    document: Option<Bson>,
) -> Result<ParsedCommand, DbError> {
    check_object(obj)?;
    let mut text = coll_text(Some(&obj.database), &obj.name, method, &args, &modifiers);
    if typed.is_some() {
        text.push_str(TYPED_NOTE);
    }
    let cmd = ShellCommand::Coll {
        db: Some(obj.database.clone()),
        coll: obj.name.clone(),
        method,
        args,
        modifiers,
        explain: None,
    };
    let c = classify(&cmd).map_err(|message| DbError::Internal { message })?;
    Ok(ParsedCommand {
        name: c.name,
        text,
        class: c.class,
        targets: c.targets,
        unbounded: c.unbounded,
        drops: c.drops,
        session_effect: c.session_effect,
        payload: Arc::new(Payload {
            cmd,
            typed,
            document,
        }),
    })
}

/// The filter document of `filter`, and the structured filter to coerce before the read
/// when it compares values (see the module docs).
fn filter_of(filter: Option<&DataFilter>) -> Result<(Document, Option<DataFilter>), DbError> {
    let Some(f) = filter else {
        return Ok((Document::new(), None));
    };
    if f.raw_where.is_some() {
        return Err(filter_error(
            "This filter type is not available for this engine",
        ));
    }
    if let Some(text) = &f.raw_json {
        if !f.conditions.is_empty() {
            return Err(filter_error("a JSON filter takes no other conditions"));
        }
        return Ok((raw_filter(text)?, None));
    }
    // Every check runs now; without types every value stays text.
    let doc = structured_filter(f, &BTreeMap::new())?;
    let typed = f
        .conditions
        .iter()
        .any(|c| compares(c.op))
        .then(|| f.clone());
    Ok((doc, typed))
}

/// Whether `op` compares values, which are then coerced by the field's type.
fn compares(op: FilterOp) -> bool {
    match op {
        FilterOp::Eq
        | FilterOp::Ne
        | FilterOp::Lt
        | FilterOp::Le
        | FilterOp::Gt
        | FilterOp::Ge
        | FilterOp::In
        | FilterOp::Between => true,
        FilterOp::Like
        | FilterOp::NotLike
        | FilterOp::Contains
        | FilterOp::StartsWith
        | FilterOp::EndsWith
        | FilterOp::IsNull
        | FilterOp::IsNotNull => false,
    }
}

/// `cmd` (built by [`page`] / [`count`] with `filter`) with the filter coerced by `types`
/// (field -> inferred `db_type`).
pub(crate) fn resolve(
    cmd: &ShellCommand,
    filter: &DataFilter,
    types: &BTreeMap<String, String>,
) -> Result<ShellCommand, DbError> {
    let mut out = cmd.clone();
    match &mut out {
        ShellCommand::Coll { args, .. } if !args.is_empty() => {
            args[0] = Bson::Document(structured_filter(filter, types)?);
            Ok(out)
        }
        _ => Err(DbError::Internal {
            message: "a typed filter belongs to a collection read".into(),
        }),
    }
}

/// `value` as `db_type` (spec §6.2): a number of that width, an ObjectId (24 hex digits),
/// a boolean, a date (ISO 8601, or the data view's `YYYY-MM-DD HH:MM:SS.mmm`, UTC), a
/// UUID. Every other type (`string`, `mixed(…)`, and types without a text form) keeps the
/// text as it is.
pub(crate) fn coerce(value: &str, db_type: &str) -> Result<Bson, DbError> {
    let bad = || filter_error(format!("Value \"{value}\" is not a valid {db_type}"));
    let t = value.trim();
    Ok(match db_type {
        "int" => Bson::Int32(t.parse().map_err(|_| bad())?),
        "long" => Bson::Int64(t.parse().map_err(|_| bad())?),
        "double" => Bson::Double(parse_double(t).ok_or_else(bad)?),
        "decimal" => Bson::Decimal128(t.parse::<Decimal128>().map_err(|_| bad())?),
        "objectId" if t.len() == 24 && t.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Bson::ObjectId(ObjectId::parse_str(t).map_err(|_| bad())?)
        }
        "objectId" => return Err(bad()),
        "bool" => match t.to_ascii_lowercase().as_str() {
            "true" => Bson::Boolean(true),
            "false" => Bson::Boolean(false),
            _ => return Err(bad()),
        },
        "date" => Bson::DateTime(parse_iso_date(t).ok_or_else(bad)?),
        "uuid" => uuid_binary(t).ok_or_else(bad)?,
        _ => Bson::String(value.to_string()),
    })
}

/// A decimal number, `NaN`, `Infinity` or `-Infinity` (Rust's own `inf` / `nan` spellings
/// are not numbers here).
fn parse_double(t: &str) -> Option<f64> {
    match t {
        "NaN" => Some(f64::NAN),
        "Infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ if !t.is_empty() && t.bytes().all(|b| b"0123456789+-.eE".contains(&b)) => t.parse().ok(),
        _ => None,
    }
}

/// The filter document of a structured filter: one document per condition, joined with
/// `$and` / `$or` when there are several. `types` gives a field's inferred `db_type`; a
/// field it does not know keeps its values as text.
///
/// Errors (`Config`, field `filter`): too many conditions, a wrong number of values, a
/// value too long, a field that would be read as an operator (empty or starting with `$`),
/// a value its field's type cannot take.
pub(crate) fn structured_filter(
    filter: &DataFilter,
    types: &BTreeMap<String, String>,
) -> Result<Document, DbError> {
    if filter.conditions.len() > MAX_CONDITIONS {
        return Err(filter_error(format!(
            "a filter has at most {MAX_CONDITIONS} conditions"
        )));
    }
    let mut parts = filter
        .conditions
        .iter()
        .map(|c| condition(c, types.get(&c.column).map(String::as_str)))
        .collect::<Result<Vec<_>, _>>()?;
    let joiner = match filter.mode {
        FilterMode::And => "$and",
        FilterMode::Or => "$or",
    };
    Ok(match parts.len() {
        0 => Document::new(),
        1 => parts.remove(0),
        _ => doc! { joiner: parts },
    })
}

/// `{field: {<operator>: …}}` of one condition (spec §6.2).
fn condition(c: &FilterCond, db_type: Option<&str>) -> Result<Document, DbError> {
    if c.column.is_empty() || c.column.starts_with('$') {
        return Err(filter_error(format!(
            "\"{}\" cannot be filtered here; use a JSON filter",
            c.column
        )));
    }
    let (min, max) = c.op.arity();
    let n = c.values.len();
    if n < min || n > max {
        let want = match (min, max) {
            (0, 0) => "no value".to_string(),
            (a, b) if a == b => format!("{a} value{}", if a == 1 { "" } else { "s" }),
            (a, b) => format!("{a} to {b} values"),
        };
        return Err(filter_error(format!(
            "the condition on \"{}\" takes {want}, not {n}",
            c.column
        )));
    }
    if let Some(v) = c.values.iter().find(|v| v.len() > MAX_VALUE_BYTES) {
        return Err(filter_error(format!(
            "a filter value is too long ({} bytes; at most {MAX_VALUE_BYTES})",
            v.len()
        )));
    }
    let ty = db_type.unwrap_or("string");
    let value = |i: usize| coerce(&c.values[i], ty);
    let pattern = |p: String, options: &str| {
        Bson::RegularExpression(Regex {
            pattern: p,
            options: options.into(),
        })
    };
    let first = || c.values[0].as_str();
    let ops = match c.op {
        FilterOp::Eq => doc! { "$eq": value(0)? },
        FilterOp::Ne => doc! { "$ne": value(0)? },
        FilterOp::Lt => doc! { "$lt": value(0)? },
        FilterOp::Le => doc! { "$lte": value(0)? },
        FilterOp::Gt => doc! { "$gt": value(0)? },
        FilterOp::Ge => doc! { "$gte": value(0)? },
        // `s`: the wildcards match line breaks too, as SQL's LIKE does.
        FilterOp::Like => doc! { "$regex": pattern(like_pattern(first()), "s") },
        FilterOp::NotLike => doc! { "$not": pattern(like_pattern(first()), "s") },
        FilterOp::Contains => doc! { "$regex": pattern(escape_regex(first()), "") },
        FilterOp::StartsWith => {
            doc! { "$regex": pattern(format!("^{}", escape_regex(first())), "") }
        }
        FilterOp::EndsWith => {
            doc! { "$regex": pattern(format!("{}{END}", escape_regex(first())), "") }
        }
        FilterOp::In => doc! {
            "$in": (0..n).map(value).collect::<Result<Vec<_>, _>>()?,
        },
        FilterOp::Between => doc! { "$gte": value(0)?, "$lte": value(1)? },
        FilterOp::IsNull => doc! { "$eq": Bson::Null },
        FilterOp::IsNotNull => doc! { "$ne": Bson::Null },
    };
    Ok(doc! { c.column.clone(): ops })
}

/// The end of the subject (`$` would also match before a final line break).
const END: &str = r"\z";

/// A LIKE pattern as an anchored regex: `%` (one or several in a row) is `.*`, `_` is `.`,
/// the rest is literal.
fn like_pattern(like: &str) -> String {
    let mut out = String::from("^");
    let mut after_percent = false;
    for ch in like.chars() {
        match ch {
            '%' if after_percent => {}
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            c => push_literal(&mut out, c),
        }
        after_percent = ch == '%';
    }
    out.push_str(END);
    out
}

/// `text` matched literally by a regex.
fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_literal(&mut out, c);
    }
    out
}

fn push_literal(out: &mut String, c: char) {
    if r"\^$.|?*+()[]{}".contains(c) {
        out.push('\\');
    }
    out.push(c);
}

/// A raw filter (`DataFilter::raw_json`): one document in the console's literal syntax,
/// without server-side JavaScript at any depth.
pub(crate) fn raw_filter(text: &str) -> Result<Document, DbError> {
    match parse_document(text) {
        Ok(doc) if runs_js(&doc) => Err(filter_error(SERVER_JS)),
        Ok(doc) => Ok(doc),
        // Any text that is not one document, as core's `check_raw_json` says it.
        Err(_) => Err(filter_error(NOT_A_DOCUMENT)),
    }
}

/// Whether a key of `doc`, at any depth (arrays included), runs JavaScript on the server.
fn runs_js(doc: &Document) -> bool {
    doc.iter()
        .any(|(k, v)| SERVER_SIDE_JS.contains(&k.as_str()) || value_runs_js(v))
}

fn value_runs_js(v: &Bson) -> bool {
    match v {
        Bson::Document(d) => runs_js(d),
        Bson::Array(items) => items.iter().any(value_runs_js),
        _ => false,
    }
}

/// The sort document: the user's keys in order (a repeated one keeps its first place and
/// direction), then `_id` ascending unless already there, so pages are stable.
pub(crate) fn sort_document(sort: &[SortSpec]) -> Result<Document, DbError> {
    let mut d = Document::new();
    for s in sort {
        if s.column.is_empty() || s.column.starts_with('$') {
            return Err(config(
                "sort",
                format!("\"{}\" cannot be sorted here", s.column),
            ));
        }
        if !d.contains_key(&s.column) {
            let dir = match s.dir {
                SortDir::Asc => 1,
                SortDir::Desc => -1,
            };
            d.insert(s.column.clone(), dir);
        }
    }
    if !d.contains_key("_id") {
        d.insert("_id", 1);
    }
    Ok(d)
}

/// A collection command in the console's syntax, which parses back to the same command:
/// `db.getSiblingDB("<db>").<coll>.<method>(<args>)<modifiers>`, the collection through
/// `getCollection("…")` unless it is a plain identifier, values as canonical Extended
/// JSON (every BSON type kept).
pub(crate) fn coll_text(
    db: Option<&str>,
    coll: &str,
    method: CollMethod,
    args: &[Bson],
    modifiers: &[Modifier],
) -> String {
    let mut s = String::from("db");
    if let Some(db) = db {
        s.push_str(&format!(".getSiblingDB({})", quoted(db)));
    }
    if plain_name(coll) {
        s.push('.');
        s.push_str(coll);
    } else {
        s.push_str(&format!(".getCollection({})", quoted(coll)));
    }
    let args: Vec<String> = args.iter().map(canonical_ejson).collect();
    s.push_str(&format!(".{}({})", method.name(), args.join(", ")));
    for m in modifiers {
        let (name, arg) = match m {
            Modifier::Sort(d) => ("sort", canonical_ejson(&Bson::Document(d.clone()))),
            Modifier::Limit(n) => ("limit", n.to_string()),
            Modifier::Skip(n) => ("skip", n.to_string()),
            Modifier::Project(d) => ("project", canonical_ejson(&Bson::Document(d.clone()))),
            Modifier::MaxTimeMs(n) => ("maxTimeMS", n.to_string()),
        };
        s.push_str(&format!(".{name}({arg})"));
    }
    s
}

/// A JSON (and so JavaScript) string literal.
fn quoted(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// `[A-Za-z_][A-Za-z0-9_]*`: written as `db.<name>`.
fn plain_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---- Edits (spec §6.3) ----

pub(crate) const ID_CANNOT_CHANGE: &str = "_id cannot be changed";
pub(crate) const FIELD_REFUSED: &str =
    "Fields whose name contains \".\" or starts with \"$\" cannot be edited here; use the console";
pub(crate) const NO_DEFAULT: &str = "Default values are not available for MongoDB";
pub(crate) const NO_LOADED: &str = "Cut cells cannot be edited for MongoDB";

/// A refused edit: `Config` on field `edit`.
fn edit_error(message: impl Into<String>) -> DbError {
    config("edit", message)
}

/// A `Config` error of a cell's value (from §6.2's coercion or §7's mapping) as an edit's.
fn as_edit(e: DbError) -> DbError {
    match e {
        DbError::Config { message, .. } => edit_error(message),
        other => other,
    }
}

/// The commands that apply `edits` to `obj`, one per edit, in order (spec §6.3):
///
/// - `Update`: `updateOne({_id}, {$set: {…}, $unset: {…}})` (an update without changes
///   sends nothing);
/// - `Insert`: `insertOne({…})` (without `_id`, one is made when it is sent);
/// - `Delete`: `deleteOne({_id})`.
///
/// `_id` is the row's key value rebuilt by §7 (a binary `_id` keeps its subtype and
/// bytes). A cell becomes BSON by its `CellInput` and the field's sampled type
/// (`detail`): see [`cell_value`]. Refused (`Config`, field `edit`), and then nothing is
/// built: changing `_id`, a field whose name holds `.` or starts with `$`, `Default`,
/// `Loaded`, and a value its field's type cannot take.
///
/// Each command's `text` is in the console's syntax and parses back to the same command
/// ([`coll_text`]); an update or delete carries its `_id` ([`Payload::document`]), so
/// matching no document is `DocumentNotFound` ([`document_not_found`]).
pub(crate) fn build_edits(
    obj: &ObjectRef,
    detail: &TableDetail,
    edits: &EditSet,
) -> Result<Vec<ParsedCommand>, DbError> {
    let types: BTreeMap<&str, &str> = detail
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.db_type.as_str()))
        .collect();
    let ty = |field: &str| types.get(field).copied();
    let mut out = Vec::with_capacity(edits.edits.len());
    for edit in &edits.edits {
        match edit {
            RowEdit::Update { row, set } => {
                let id = row_id(row, ty("_id"))?;
                let mut fields = Document::new();
                let mut unset = Document::new();
                for (field, input) in set {
                    if field == "_id" {
                        return Err(edit_error(ID_CANNOT_CHANGE));
                    }
                    check_field(field)?;
                    match cell_value(input, ty(field))? {
                        Some(v) => fields.insert(field.clone(), v),
                        None => unset.insert(field.clone(), ""),
                    };
                }
                let mut update = Document::new();
                if !fields.is_empty() {
                    update.insert("$set", fields);
                }
                if !unset.is_empty() {
                    update.insert("$unset", unset);
                }
                if update.is_empty() {
                    continue;
                }
                let filter = doc! { "_id": id.clone() };
                out.push(build(
                    obj,
                    CollMethod::UpdateOne,
                    vec![filter.into(), update.into()],
                    Vec::new(),
                    None,
                    Some(id),
                )?);
            }
            RowEdit::Insert { values } => {
                let mut new = Document::new();
                for (field, input) in values {
                    check_field(field)?;
                    if let Some(v) = cell_value(input, ty(field))? {
                        new.insert(field.clone(), v);
                    }
                }
                out.push(build(
                    obj,
                    CollMethod::InsertOne,
                    vec![new.into()],
                    Vec::new(),
                    None,
                    None,
                )?);
            }
            RowEdit::Delete { row } => {
                let id = row_id(row, ty("_id"))?;
                let filter = doc! { "_id": id.clone() };
                out.push(build(
                    obj,
                    CollMethod::DeleteOne,
                    vec![filter.into()],
                    Vec::new(),
                    None,
                    Some(id),
                )?);
            }
        }
    }
    Ok(out)
}

/// The `_id` of a loaded row, rebuilt from its key value by §7.
fn row_id(row: &RowRef, db_type: Option<&str>) -> Result<Bson, DbError> {
    let (_, id) = row
        .key
        .iter()
        .find(|(k, _)| k == "_id")
        .ok_or_else(|| edit_error("The document has no _id to find it by"))?;
    value_to_bson(id, db_type).map_err(as_edit)
}

/// A field this view can set: no `.` (a path) and no leading `$` (an operator).
fn check_field(field: &str) -> Result<(), DbError> {
    if field.contains('.') || field.starts_with('$') {
        return Err(edit_error(FIELD_REFUSED));
    }
    Ok(())
}

/// The BSON value of one cell (spec §6.3), `None` to remove the field (`Unset`):
///
/// - `Null`: null;
/// - `Text`: by the field's sampled type as a filter value is (§6.2); a `mixed(…)` field
///   takes the text as a §4.3 literal when it is one, else as text; an `object` / `array`
///   field takes a §4.3 literal; a field the sample did not see keeps the text;
/// - `Value`: by §7, a JSON value (a document or array) read back by §4.3;
/// - `Base64`: binary data, subtype 0;
/// - `Default` and `Loaded` are refused.
fn cell_value(input: &CellInput, db_type: Option<&str>) -> Result<Option<Bson>, DbError> {
    Ok(Some(match input {
        CellInput::Null => Bson::Null,
        CellInput::Unset => return Ok(None),
        CellInput::Default => return Err(edit_error(NO_DEFAULT)),
        CellInput::Loaded { .. } => return Err(edit_error(NO_LOADED)),
        CellInput::Text { text } => typed_text(text, db_type)?,
        CellInput::Value {
            value: Value::Json(json),
        } => whole_literal(json)
            .map_err(|e| edit_error(format!("invalid JSON value: {}", e.message)))?,
        CellInput::Value { value } => value_to_bson(value, db_type).map_err(as_edit)?,
        CellInput::Base64 { data } => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: B64
                .decode(data)
                .map_err(|_| edit_error("The binary value is not valid base64"))?,
        }),
    }))
}

/// Typed text by the field's sampled type (see [`cell_value`]).
fn typed_text(text: &str, db_type: Option<&str>) -> Result<Bson, DbError> {
    match db_type {
        Some(t) if t.starts_with("mixed(") => {
            Ok(whole_literal(text).unwrap_or_else(|_| Bson::String(text.to_string())))
        }
        Some(t @ ("object" | "array")) => whole_literal(text)
            .map_err(|_| edit_error(format!("Value \"{text}\" is not a valid {t}"))),
        Some(t) => coerce(text, t).map_err(as_edit),
        None => Ok(Bson::String(text.to_string())),
    }
}

/// `text` as exactly one §4.3 literal.
fn whole_literal(text: &str) -> Result<Bson, ShellError> {
    let tokens = lex(text)?;
    if tokens.is_empty() {
        return Err(ShellError::new("Expected a value", 0));
    }
    let mut at = 0;
    let v = parse_literal(&tokens, text, &mut at)?;
    match tokens.get(at) {
        Some(t) => Err(ShellError::new("Unexpected text after the value", t.start)),
        None => Ok(v),
    }
}

/// An edit's document no longer exists: its `updateOne` / `deleteOne` matched nothing
/// (spec §6.4). `id` is shown as canonical Extended JSON, as in the command's text.
pub(crate) fn document_not_found(id: &Bson) -> DbError {
    DbError::Server {
        code: Some("DocumentNotFound".into()),
        sqlstate: None,
        message: format!("Document {} no longer exists", canonical_ejson(id)),
        position: None,
    }
}

/// `Err(DocumentNotFound)` when the edit of `id` matched no document.
pub(crate) fn check_found(id: &Bson, affected: Option<u64>) -> Result<(), DbError> {
    match affected {
        Some(0) => Err(document_not_found(id)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::payload_of;
    use crate::shell::MongoLanguage;
    use dnt_core::command::{CommandClass, CommandLanguage, CommandTarget};
    use dnt_core::data_access::check_read;
    use dnt_core::meta::{ColumnDetail, ObjectKind, TableOptions};
    use dnt_core::sql::filter::{FilterCond, FilterMode, FilterOp, SortDir};
    use mongodb::bson::oid::ObjectId;
    use mongodb::bson::spec::BinarySubtype;
    use mongodb::bson::{doc, Binary, DateTime, Decimal128, Regex};

    const OID: &str = "65a1b2c3d4e5f60718293a01";

    fn types() -> BTreeMap<String, String> {
        [
            ("_id", "objectId"),
            ("qty", "int"),
            ("total", "long"),
            ("price", "double"),
            ("amount", "decimal"),
            ("gift", "bool"),
            ("at", "date"),
            ("uid", "uuid"),
            ("name", "string"),
            ("mixed", "mixed(int|string)"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn cond(column: &str, op: FilterOp, values: &[&str]) -> FilterCond {
        FilterCond {
            column: column.into(),
            op,
            values: values.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn filter(mode: FilterMode, conditions: Vec<FilterCond>) -> DataFilter {
        DataFilter {
            mode,
            conditions,
            raw_where: None,
            raw_json: None,
        }
    }

    fn raw(text: &str) -> DataFilter {
        DataFilter {
            raw_json: Some(text.into()),
            ..filter(FilterMode::And, vec![])
        }
    }

    fn one(c: FilterCond) -> Document {
        structured_filter(&filter(FilterMode::And, vec![c]), &types()).unwrap()
    }

    fn regex(pattern: &str, options: &str) -> Bson {
        Bson::RegularExpression(Regex {
            pattern: pattern.into(),
            options: options.into(),
        })
    }

    fn filter_message(r: Result<impl std::fmt::Debug, DbError>) -> String {
        match r {
            Err(DbError::Config { field, message }) => {
                assert_eq!(field.as_deref(), Some("filter"), "{message}");
                message
            }
            other => panic!("expected a filter error, got {other:?}"),
        }
    }

    fn orders() -> ObjectRef {
        ObjectRef {
            database: "dnt_fixture".into(),
            schema: None,
            name: "orders".into(),
            kind: ObjectKind::Collection,
        }
    }

    fn spec(sort: Vec<SortSpec>, filter: Option<DataFilter>) -> PageSpec {
        PageSpec {
            sort,
            filter,
            limit: 51,
            skip: 100,
        }
    }

    /// The filter (first argument) of a built command's payload.
    fn filter_of(cmd: &ParsedCommand) -> Document {
        match &payload_of(cmd).unwrap().cmd {
            ShellCommand::Coll { args, .. } => args[0].as_document().unwrap().clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn filter_operator_table() {
        use FilterOp::*;
        let cases: Vec<(FilterOp, &[&str], Document)> = vec![
            (Eq, &["5"], doc! { "qty": { "$eq": 5 } }),
            (Ne, &["5"], doc! { "qty": { "$ne": 5 } }),
            (Lt, &["5"], doc! { "qty": { "$lt": 5 } }),
            (Le, &["5"], doc! { "qty": { "$lte": 5 } }),
            (Gt, &["5"], doc! { "qty": { "$gt": 5 } }),
            (Ge, &["5"], doc! { "qty": { "$gte": 5 } }),
            (In, &["1", "2", "3"], doc! { "qty": { "$in": [1, 2, 3] } }),
            (
                Between,
                &["1", "9"],
                doc! { "qty": { "$gte": 1, "$lte": 9 } },
            ),
            (IsNull, &[], doc! { "qty": { "$eq": Bson::Null } }),
            (IsNotNull, &[], doc! { "qty": { "$ne": Bson::Null } }),
            (
                Like,
                &["5%"],
                doc! { "qty": { "$regex": regex(r"^5.*\z", "s") } },
            ),
            (
                NotLike,
                &["5%"],
                doc! { "qty": { "$not": regex(r"^5.*\z", "s") } },
            ),
            (
                Contains,
                &["5"],
                doc! { "qty": { "$regex": regex("5", "") } },
            ),
            (
                StartsWith,
                &["5"],
                doc! { "qty": { "$regex": regex("^5", "") } },
            ),
            (
                EndsWith,
                &["5"],
                doc! { "qty": { "$regex": regex(r"5\z", "") } },
            ),
        ];
        for (op, values, want) in cases {
            assert_eq!(one(cond("qty", op, values)), want, "{op:?}");
        }
        // No conditions: everything. Several: `$and` / `$or`, in order.
        assert_eq!(
            structured_filter(&filter(FilterMode::And, vec![]), &types()).unwrap(),
            doc! {}
        );
        let two = vec![
            cond("qty", FilterOp::Gt, &["1"]),
            cond("name", FilterOp::Eq, &["Bob"]),
        ];
        assert_eq!(
            structured_filter(&filter(FilterMode::And, two.clone()), &types()).unwrap(),
            doc! { "$and": [ { "qty": { "$gt": 1 } }, { "name": { "$eq": "Bob" } } ] }
        );
        assert_eq!(
            structured_filter(&filter(FilterMode::Or, two), &types()).unwrap(),
            doc! { "$or": [ { "qty": { "$gt": 1 } }, { "name": { "$eq": "Bob" } } ] }
        );
        // Arity, and a field that would be read as an operator.
        for (c, want) in [
            (cond("qty", FilterOp::Eq, &[]), "takes 1 value, not 0"),
            (
                cond("qty", FilterOp::Between, &["1"]),
                "takes 2 values, not 1",
            ),
            (
                cond("qty", FilterOp::IsNull, &["1"]),
                "takes no value, not 1",
            ),
            (
                cond("qty", FilterOp::In, &[]),
                "takes 1 to 1000 values, not 0",
            ),
        ] {
            let m = filter_message(structured_filter(
                &filter(FilterMode::And, vec![c]),
                &types(),
            ));
            assert!(m.ends_with(want), "{m}");
        }
        for bad in ["$where", "", "$expr"] {
            let m = filter_message(structured_filter(
                &filter(FilterMode::And, vec![cond(bad, FilterOp::IsNull, &[])]),
                &types(),
            ));
            assert!(m.contains("cannot be filtered here"), "{bad}: {m}");
        }
        // A raw WHERE is not a MongoDB filter; a JSON filter takes no conditions.
        let mut where_ = filter(FilterMode::And, vec![]);
        where_.raw_where = Some("1 = 1".into());
        assert_eq!(
            filter_message(page(&orders(), &spec(vec![], Some(where_)))),
            "This filter type is not available for this engine"
        );
        let mut mixed = raw("{}");
        mixed.conditions = vec![cond("qty", FilterOp::IsNull, &[])];
        assert_eq!(
            filter_message(count(&orders(), Some(&mixed))),
            "a JSON filter takes no other conditions"
        );
    }

    #[test]
    fn like_is_anchored() {
        // `%` and `_` are wildcards (matching line breaks too); everything else is literal.
        assert_eq!(
            one(cond("name", FilterOp::Like, &["a%b_c.d(e)*"])),
            doc! { "name": { "$regex": regex(r"^a.*b.c\.d\(e\)\*\z", "s") } }
        );
        assert_eq!(
            one(cond("name", FilterOp::NotLike, &["x_"])),
            doc! { "name": { "$not": regex(r"^x.\z", "s") } }
        );
        // Consecutive `%` are one `.*`; `_` still counts one character each. The end
        // anchor is the end of the text (`$` would match before a final line break).
        assert_eq!(like_pattern("%%a%%%_%"), r"^.*a.*..*\z");
        assert_eq!(like_pattern("__"), r"^..\z");
        assert_eq!(like_pattern(""), r"^\z");
        // The literal operators escape every metacharacter, `%` and `_` included.
        let meta = r"a.b*c+d?e^f$g|h(i)j[k]l{m}n\o%p_q";
        let escaped = r"a\.b\*c\+d\?e\^f\$g\|h\(i\)j\[k\]l\{m\}n\\o%p_q";
        assert_eq!(
            one(cond("name", FilterOp::Contains, &[meta])),
            doc! { "name": { "$regex": regex(escaped, "") } }
        );
        assert_eq!(
            one(cond("name", FilterOp::StartsWith, &[meta])),
            doc! { "name": { "$regex": regex(&format!("^{escaped}"), "") } }
        );
        assert_eq!(
            one(cond("name", FilterOp::EndsWith, &[meta])),
            doc! { "name": { "$regex": regex(&format!(r"{escaped}\z"), "") } }
        );
        // Patterns are text whatever the field's type.
        assert_eq!(
            one(cond("qty", FilterOp::StartsWith, &["x"])),
            doc! { "qty": { "$regex": regex("^x", "") } }
        );
    }

    #[test]
    fn coercion_by_db_type() {
        let c = |v: &str, t: &str| coerce(v, t).unwrap();
        assert_eq!(c("5", "int"), Bson::Int32(5));
        assert_eq!(c("-7", "int"), Bson::Int32(-7));
        assert_eq!(
            c("9007199254740993", "long"),
            Bson::Int64(9_007_199_254_740_993)
        );
        assert_eq!(c("5", "long"), Bson::Int64(5));
        assert_eq!(c("19.99", "double"), Bson::Double(19.99));
        assert_eq!(c("3", "double"), Bson::Double(3.0));
        assert_eq!(c("-1e3", "double"), Bson::Double(-1000.0));
        assert!(matches!(c("NaN", "double"), Bson::Double(d) if d.is_nan()));
        assert_eq!(c("-Infinity", "double"), Bson::Double(f64::NEG_INFINITY));
        assert_eq!(
            c("1234.5678", "decimal"),
            Bson::Decimal128("1234.5678".parse::<Decimal128>().unwrap())
        );
        assert_eq!(
            c(OID, "objectId"),
            Bson::ObjectId(ObjectId::parse_str(OID).unwrap())
        );
        assert_eq!(c("true", "bool"), Bson::Boolean(true));
        assert_eq!(c("FALSE", "bool"), Bson::Boolean(false));
        // ISO 8601, and the data view's own display form.
        let at = Bson::DateTime(DateTime::from_millis(1_730_764_800_123));
        assert_eq!(c("2024-11-05T00:00:00.123Z", "date"), at);
        assert_eq!(c("2024-11-05 00:00:00.123", "date"), at);
        assert_eq!(
            c("2024-11-05", "date"),
            Bson::DateTime(DateTime::from_millis(1_730_764_800_000))
        );
        let uuid = Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: (0u8..16).collect(),
        });
        assert_eq!(c("00010203-0405-0607-0809-0a0b0c0d0e0f", "uuid"), uuid);
        assert_eq!(c("000102030405060708090a0b0c0d0e0f", "uuid"), uuid);
        // Text stays text: strings, mixed fields, and types without a text form.
        for t in [
            "string",
            "mixed(int|string)",
            "binData",
            "object",
            "null",
            "regex",
        ] {
            assert_eq!(c("5", t), Bson::String("5".into()), "{t}");
        }
        // In a filter: by the field's inferred type; an unknown field keeps the text.
        assert_eq!(
            one(cond("_id", FilterOp::Eq, &[OID])),
            doc! { "_id": { "$eq": ObjectId::parse_str(OID).unwrap() } }
        );
        assert_eq!(
            one(cond("mixed", FilterOp::In, &["1", "x"])),
            doc! { "mixed": { "$in": ["1", "x"] } }
        );
        assert_eq!(
            one(cond("other", FilterOp::Eq, &["5"])),
            doc! { "other": { "$eq": "5" } }
        );
        assert_eq!(
            one(cond("total", FilterOp::Between, &["1", "2"])),
            doc! { "total": { "$gte": 1_i64, "$lte": 2_i64 } }
        );
    }

    #[test]
    fn coercion_error_text() {
        for (v, t) in [
            ("abc", "int"),
            ("3000000000", "int"),
            ("1.5", "int"),
            ("", "int"),
            ("x", "long"),
            ("inf", "double"),
            ("1,5", "double"),
            ("one", "decimal"),
            ("65a1b2c3d4e5f60718293a0", "objectId"),
            ("65a1b2c3d4e5f60718293a0z", "objectId"),
            ("yes", "bool"),
            ("2024-13-01", "date"),
            ("not-a-uuid", "uuid"),
        ] {
            assert_eq!(
                filter_message(coerce(v, t)),
                format!("Value \"{v}\" is not a valid {t}"),
            );
        }
        // Every value of the condition is coerced.
        assert_eq!(
            filter_message(structured_filter(
                &filter(
                    FilterMode::And,
                    vec![cond("qty", FilterOp::In, &["1", "x"])]
                ),
                &types(),
            )),
            "Value \"x\" is not a valid int"
        );
    }

    #[test]
    fn raw_filter_mongosh_and_ejson() {
        let want = doc! { "_id": ObjectId::parse_str(OID).unwrap(), "qty": { "$gt": 2 } };
        let mongosh = format!("{{_id: ObjectId(\"{OID}\"), qty: {{$gt: 2}}}}");
        let ejson = format!("{{\"_id\": {{\"$oid\": \"{OID}\"}}, \"qty\": {{\"$gt\": 2}}}}");
        assert_eq!(raw_filter(&mongosh).unwrap(), want);
        assert_eq!(raw_filter(&ejson).unwrap(), want);
        // The page runs it as written, without the sample.
        let p = page(&orders(), &spec(vec![], Some(raw(&mongosh)))).unwrap();
        assert_eq!(filter_of(&p), want);
        assert!(payload_of(&p).unwrap().typed.is_none());
        let c = count(&orders(), Some(&raw(&ejson))).unwrap();
        assert_eq!(filter_of(&c), want);
        // A syntax error is core's message too.
        for bad in [
            "{a: ObjectId(\"zz\")}",
            "{",
            "{a: }",
            "{} {}",
            "{a: 1,, b: 2}",
        ] {
            assert_eq!(
                filter_message(raw_filter(bad)),
                "The filter must be a JSON object",
                "{bad}"
            );
        }
    }

    #[test]
    fn raw_filter_blocks_server_js() {
        for bad in [
            "{$where: 'sleep(100)'}",
            "{a: {$where: '1'}}",
            "{$or: [{x: 1}, {$expr: {$function: {body: 'f', args: [], lang: 'js'}}}]}",
            r#"{"g": {"$accumulator": {}}}"#,
            "{a: [[{b: {$where: 'x'}}]]}",
        ] {
            assert_eq!(
                filter_message(raw_filter(bad)),
                "Server-side JavaScript is not allowed in a filter",
                "{bad}"
            );
        }
        // Only keys run code: a string value is inert.
        assert!(raw_filter("{note: '$where'}").is_ok());
        for bad in ["[1]", "\"x\"", "1", "null", "", "  "] {
            assert_eq!(
                filter_message(raw_filter(bad)),
                "The filter must be a JSON object",
                "{bad}"
            );
        }
        // Nothing is built from a refused filter.
        assert!(page(&orders(), &spec(vec![], Some(raw("{$where: '1'}")))).is_err());
        assert!(count(&orders(), Some(&raw("[]"))).is_err());
    }

    #[test]
    fn sort_tiebreak_id() {
        let s = |column: &str, dir| SortSpec {
            column: column.into(),
            dir,
        };
        assert_eq!(sort_document(&[]).unwrap(), doc! { "_id": 1 });
        assert_eq!(
            sort_document(&[s("qty", SortDir::Desc), s("name", SortDir::Asc)]).unwrap(),
            doc! { "qty": -1, "name": 1, "_id": 1 }
        );
        // `_id` sorted by the user stays where it is, in its direction.
        assert_eq!(
            sort_document(&[s("_id", SortDir::Desc), s("qty", SortDir::Asc)]).unwrap(),
            doc! { "_id": -1, "qty": 1 }
        );
        match sort_document(&[s("$natural", SortDir::Asc)]) {
            Err(DbError::Config { field, .. }) => assert_eq!(field.as_deref(), Some("sort")),
            other => panic!("{other:?}"),
        }
        // The page: sort, then skip and limit.
        let p = page(&orders(), &spec(vec![s("qty", SortDir::Desc)], None)).unwrap();
        match &payload_of(&p).unwrap().cmd {
            ShellCommand::Coll {
                method, modifiers, ..
            } => {
                assert_eq!(*method, CollMethod::Find);
                assert_eq!(
                    modifiers,
                    &[
                        Modifier::Sort(doc! { "qty": -1, "_id": 1 }),
                        Modifier::Skip(100),
                        Modifier::Limit(51),
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn page_and_count_are_reads_whose_text_parses_back() {
        let typed = filter(
            FilterMode::And,
            vec![
                cond("qty", FilterOp::Ge, &["2"]),
                cond("name", FilterOp::StartsWith, &["A"]),
            ],
        );
        let untyped = filter(FilterMode::Or, vec![cond("note", FilterOp::IsNull, &[])]);
        for obj in [
            orders(),
            ObjectRef {
                name: "my coll.v2".into(),
                ..orders()
            },
        ] {
            let built = [
                page(&obj, &spec(vec![], Some(typed.clone()))).unwrap(),
                page(&obj, &spec(vec![], Some(untyped.clone()))).unwrap(),
                page(&obj, &spec(vec![], None)).unwrap(),
                count(&obj, Some(&typed)).unwrap(),
                count(&obj, None).unwrap(),
            ];
            for cmd in &built {
                check_read(cmd).unwrap();
                assert_eq!(cmd.class, CommandClass::Read);
                assert_eq!(
                    cmd.targets,
                    [CommandTarget {
                        database: Some("dnt_fixture".into()),
                        name: obj.name.clone(),
                    }],
                    "{}",
                    cmd.text
                );
                // A typed read's text ends with its note, which `parse` reads as a comment.
                assert_eq!(
                    cmd.text.ends_with(TYPED_NOTE),
                    payload_of(cmd).unwrap().typed.is_some(),
                    "{}",
                    cmd.text
                );
                let back = MongoLanguage.parse(&cmd.text).unwrap();
                assert_eq!(back.name, cmd.name, "{}", cmd.text);
                assert_eq!(
                    payload_of(&back).unwrap().cmd,
                    payload_of(cmd).unwrap().cmd,
                    "{}",
                    cmd.text
                );
            }
            assert_eq!(built[0].name, "find");
            assert_eq!(built[3].name, "countDocuments");
            // Values are coerced once the sample's types are known; the rest is ready.
            assert_eq!(payload_of(&built[0]).unwrap().typed.as_ref(), Some(&typed));
            assert_eq!(payload_of(&built[3]).unwrap().typed.as_ref(), Some(&typed));
            for i in [1, 2, 4] {
                assert!(payload_of(&built[i]).unwrap().typed.is_none());
            }
            assert_eq!(filter_of(&built[2]), doc! {});
        }
        let p = page(&orders(), &spec(vec![], Some(typed.clone()))).unwrap();
        assert_eq!(
            p.text,
            "db.getSiblingDB(\"dnt_fixture\").orders.find({\"$and\":[{\"qty\":{\"$gte\":\"2\"}},\
             {\"name\":{\"$regex\":{\"$regularExpression\":{\"pattern\":\"^A\",\"options\":\"\"}}}}]})\
             .sort({\"_id\":{\"$numberInt\":\"1\"}}).skip(100).limit(51) \
             // values are converted to the field types sampled from the collection"
        );
        // Only typed reads say so.
        let plain = page(&orders(), &spec(vec![], None)).unwrap();
        assert_eq!(
            plain.text,
            "db.getSiblingDB(\"dnt_fixture\").orders.find({})\
             .sort({\"_id\":{\"$numberInt\":\"1\"}}).skip(100).limit(51)"
        );
        let untyped_count = count(&orders(), Some(&untyped)).unwrap();
        assert!(!untyped_count.text.contains("//"), "{}", untyped_count.text);
        let typed_count = count(&orders(), Some(&typed)).unwrap();
        assert!(
            typed_count.text.ends_with(
                ") // values are converted to the field types sampled from the collection"
            ),
            "{}",
            typed_count.text
        );
        // Resolving with the sample's types coerces the values in place.
        let resolved = resolve(&payload_of(&p).unwrap().cmd, &typed, &types()).unwrap();
        match resolved {
            ShellCommand::Coll { args, .. } => assert_eq!(
                args[0],
                Bson::Document(doc! { "$and": [
                    { "qty": { "$gte": 2 } },
                    { "name": { "$regex": regex("^A", "") } },
                ] })
            ),
            other => panic!("{other:?}"),
        }
        // Only collections and views have documents; a database is required.
        let table = ObjectRef {
            kind: ObjectKind::Table,
            ..orders()
        };
        assert!(matches!(
            page(&table, &spec(vec![], None)),
            Err(DbError::Config { .. })
        ));
        let nodb = ObjectRef {
            database: String::new(),
            ..orders()
        };
        assert!(matches!(count(&nodb, None), Err(DbError::Config { .. })));
    }

    // ---- build_edits (spec §6.3) ----

    const OID2: &str = "65a1b2c3d4e5f60718293a02";

    fn detail(cols: &[(&str, &str)]) -> TableDetail {
        TableDetail {
            object: orders(),
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, (name, db_type))| ColumnDetail {
                    name: name.to_string(),
                    ordinal: i as u32 + 1,
                    db_type: db_type.to_string(),
                    nullable: *name != "_id",
                    ..ColumnDetail::default()
                })
                .collect(),
            primary_key: None,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            checks: Vec::new(),
            triggers: Vec::new(),
            options: TableOptions::default(),
            row_estimate: None,
            size_bytes: None,
            comment: None,
        }
    }

    /// The fields of `orders`, by their sampled type.
    fn edit_detail() -> TableDetail {
        detail(&[
            ("_id", "objectId"),
            ("name", "string"),
            ("qty", "int"),
            ("total", "long"),
            ("count", "long"),
            ("price", "double"),
            ("amount", "decimal"),
            ("gift", "bool"),
            ("at", "date"),
            ("uid", "uuid"),
            ("mixed", "mixed(int|string)"),
            ("alt", "mixed(int|string)"),
            ("shipping", "object"),
            ("items", "array"),
            ("token", "binData"),
            ("note", "null"),
        ])
    }

    fn oid(hex: &str) -> Bson {
        Bson::ObjectId(ObjectId::parse_str(hex).unwrap())
    }

    fn other(type_name: &str, text: &str) -> Value {
        Value::Other {
            type_name: type_name.into(),
            text: text.into(),
        }
    }

    fn row(id: Value) -> RowRef {
        RowRef {
            key: vec![("_id".into(), id)],
            original: Vec::new(),
            xmin: None,
            loaded: Vec::new(),
        }
    }

    fn text(s: &str) -> CellInput {
        CellInput::Text { text: s.into() }
    }

    fn value(v: Value) -> CellInput {
        CellInput::Value { value: v }
    }

    fn loaded() -> CellInput {
        serde_json::from_value(serde_json::json!({
            "kind": "loaded",
            "token": "00000000-0000-0000-0000-000000000001",
        }))
        .unwrap()
    }

    fn cells(list: &[(&str, CellInput)]) -> Vec<(String, CellInput)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn update(id: Value, set: &[(&str, CellInput)]) -> RowEdit {
        RowEdit::Update {
            row: row(id),
            set: cells(set),
        }
    }

    fn insert(values: &[(&str, CellInput)]) -> RowEdit {
        RowEdit::Insert {
            values: cells(values),
        }
    }

    fn delete(id: Value) -> RowEdit {
        RowEdit::Delete { row: row(id) }
    }

    fn edits_on(
        obj: &ObjectRef,
        d: &TableDetail,
        edits: Vec<RowEdit>,
    ) -> Result<Vec<ParsedCommand>, DbError> {
        build_edits(
            obj,
            d,
            &EditSet {
                object: obj.clone(),
                edits,
                overwrite: false,
            },
        )
    }

    fn edits(edits: Vec<RowEdit>) -> Result<Vec<ParsedCommand>, DbError> {
        edits_on(&orders(), &edit_detail(), edits)
    }

    /// The method and arguments of a built command.
    fn call(cmd: &ParsedCommand) -> (CollMethod, Vec<Bson>) {
        match &payload_of(cmd).unwrap().cmd {
            ShellCommand::Coll { method, args, .. } => (*method, args.clone()),
            other => panic!("{other:?}"),
        }
    }

    fn edit_message(r: Result<impl std::fmt::Debug, DbError>) -> String {
        match r {
            Err(DbError::Config { field, message }) => {
                assert_eq!(field.as_deref(), Some("edit"), "{message}");
                message
            }
            other => panic!("expected an edit error, got {other:?}"),
        }
    }

    #[test]
    fn cell_input_table() {
        let at = Bson::DateTime(DateTime::from_millis(1_730_764_800_123));
        let uuid = Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: (0u8..16).collect(),
        });
        let input = [
            // Text: by the sampled type (§6.2).
            ("name", text("Zoe")),
            ("qty", text("5")),
            ("total", text("5")),
            ("price", text("2.5")),
            ("amount", text("1.10")),
            ("gift", text("true")),
            ("at", text("2024-11-05 00:00:00.123")),
            ("uid", text("00010203-0405-0607-0809-0a0b0c0d0e0f")),
            // A mixed field: a literal when the text is one, else the text.
            ("mixed", text("42")),
            ("alt", text("forty-two")),
            // An object or array field: the literal replaces the whole field.
            (
                "shipping",
                text(&format!(
                    "{{carrier: 'UPS', weight_g: NumberLong(2000), ref: ObjectId('{OID2}')}}"
                )),
            ),
            ("items", text("[{sku: 'A-1', qty: 2}]")),
            // A field the sample did not see keeps the text.
            ("extra", text("free")),
            ("note", CellInput::Null),
            // Value: by §7, the integer width by the sampled type.
            ("count", value(Value::Int(7))),
            (
                "when",
                value(Value::DateTime("2024-11-05 00:00:00.123".into())),
            ),
            ("ref", value(other("objectId", OID2))),
            (
                "json",
                value(Value::Json(r#"{"a": {"$numberLong": "5"}}"#.into())),
            ),
            (
                "token",
                CellInput::Base64 {
                    data: "AQID".into(),
                },
            ),
            ("legacy", CellInput::Unset),
        ];
        let set = doc! {
            "name": "Zoe",
            "qty": 5,
            "total": 5_i64,
            "price": 2.5,
            "amount": "1.10".parse::<Decimal128>().unwrap(),
            "gift": true,
            "at": at.clone(),
            "uid": uuid.clone(),
            "mixed": 42,
            "alt": "forty-two",
            "shipping": { "carrier": "UPS", "weight_g": 2000_i64, "ref": oid(OID2) },
            "items": [{ "sku": "A-1", "qty": 2 }],
            "extra": "free",
            "note": Bson::Null,
            "count": 7_i64,
            "when": at.clone(),
            "ref": oid(OID2),
            "json": { "a": 5_i64 },
            "token": Binary { subtype: BinarySubtype::Generic, bytes: vec![1, 2, 3] },
        };
        let built = edits(vec![
            update(other("objectId", OID), &input),
            insert(&input),
            delete(other("objectId", OID)),
        ])
        .unwrap();
        assert_eq!(built.len(), 3);
        assert_eq!(
            call(&built[0]),
            (
                CollMethod::UpdateOne,
                vec![
                    doc! { "_id": oid(OID) }.into(),
                    doc! { "$set": set.clone(), "$unset": { "legacy": "" } }.into(),
                ]
            )
        );
        // Insert: `Unset` leaves the field out.
        assert_eq!(
            call(&built[1]),
            (CollMethod::InsertOne, vec![set.clone().into()])
        );
        assert_eq!(
            call(&built[2]),
            (CollMethod::DeleteOne, vec![doc! { "_id": oid(OID) }.into()])
        );
        // An update or delete names its document; an insert does not.
        let docs: Vec<Option<Bson>> = built
            .iter()
            .map(|c| payload_of(c).unwrap().document.clone())
            .collect();
        assert_eq!(docs, [Some(oid(OID)), None, Some(oid(OID))]);
        for cmd in &built {
            assert_eq!(cmd.class, CommandClass::Write, "{}", cmd.text);
            assert!(!cmd.unbounded && cmd.drops.is_empty(), "{}", cmd.text);
            assert_eq!(
                cmd.targets,
                [CommandTarget {
                    database: Some("dnt_fixture".into()),
                    name: "orders".into(),
                }]
            );
        }
        assert_eq!(
            built.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["updateOne", "insertOne", "deleteOne"]
        );

        // Only removals: `$unset` alone. Nothing to set: no command.
        let only = edits(vec![
            update(other("objectId", OID), &[("note", CellInput::Unset)]),
            update(other("objectId", OID), &[]),
        ])
        .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(
            call(&only[0]).1[1],
            Bson::Document(doc! { "$unset": { "note": "" } })
        );

        // Text the field's type cannot take, as §6.2 says it.
        let id = || other("objectId", OID);
        for (field, typed, want) in [
            ("qty", "x", "Value \"x\" is not a valid int"),
            ("gift", "yes", "Value \"yes\" is not a valid bool"),
            (
                "shipping",
                "{carrier: ",
                "Value \"{carrier: \" is not a valid object",
            ),
            ("items", "[1,", "Value \"[1,\" is not a valid array"),
        ] {
            assert_eq!(
                edit_message(edits(vec![update(id(), &[(field, text(typed))])])),
                want
            );
            assert_eq!(
                edit_message(edits(vec![insert(&[(field, text(typed))])])),
                want
            );
        }
        assert!(edit_message(edits(vec![update(
            id(),
            &[("token", CellInput::Base64 { data: "**".into() })]
        )]))
        .contains("base64"));
        // A value §7 cannot write.
        assert_eq!(
            edit_message(edits(vec![update(
                id(),
                &[("js", value(other("javascript", "1")))]
            )])),
            "javascript values are read-only"
        );
    }

    #[test]
    fn id_cannot_change() {
        for set in [
            vec![("_id", text(OID2))],
            vec![("qty", text("1")), ("_id", value(other("objectId", OID2)))],
            vec![("_id", CellInput::Unset)],
            vec![("_id", CellInput::Null)],
        ] {
            assert_eq!(
                edit_message(edits(vec![update(other("objectId", OID), &set)])),
                ID_CANNOT_CHANGE
            );
        }
        // A new document may name its `_id`.
        let built = edits(vec![insert(&[("_id", text(OID2)), ("qty", text("1"))])]).unwrap();
        assert_eq!(
            call(&built[0]).1,
            [doc! { "_id": oid(OID2), "qty": 1 }.into()]
        );
        // A row without `_id` cannot be found again.
        let no_id = RowEdit::Delete {
            row: RowRef {
                key: Vec::new(),
                ..row(Value::Null)
            },
        };
        edit_message(edits(vec![no_id]));
    }

    #[test]
    fn binary_and_other_ids_are_rebuilt() {
        let d = detail(&[("_id", "mixed(binData|objectId)"), ("qty", "int")]);
        let cases = [
            (
                other("binData", "00:AQID"),
                Bson::Binary(Binary {
                    subtype: BinarySubtype::Generic,
                    bytes: vec![1, 2, 3],
                }),
            ),
            (
                other("binData", "05:AAECAwQFBgcICQoLDA0ODw=="),
                Bson::Binary(Binary {
                    subtype: BinarySubtype::Md5,
                    bytes: (0u8..16).collect(),
                }),
            ),
            (
                Value::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into()),
                Bson::Binary(Binary {
                    subtype: BinarySubtype::Uuid,
                    bytes: (0u8..16).collect(),
                }),
            ),
            (Value::Text("sku-1".into()), Bson::String("sku-1".into())),
            (Value::Int(7), Bson::Int32(7)),
            (other("objectId", OID), oid(OID)),
        ];
        for (id, want) in cases {
            let built = edits_on(
                &orders(),
                &d,
                vec![update(id.clone(), &[("qty", text("3"))]), delete(id)],
            )
            .unwrap();
            assert_eq!(
                call(&built[0]).1[0],
                Bson::Document(doc! { "_id": want.clone() })
            );
            assert_eq!(
                call(&built[1]).1[0],
                Bson::Document(doc! { "_id": want.clone() })
            );
            assert_eq!(payload_of(&built[1]).unwrap().document, Some(want));
        }
        // A long `_id` stays a long.
        let d = detail(&[("_id", "long")]);
        let built = edits_on(&orders(), &d, vec![delete(Value::Int(5))]).unwrap();
        assert_eq!(call(&built[0]).1[0], Bson::Document(doc! { "_id": 5_i64 }));
    }

    #[test]
    fn dotted_or_dollar_fields_refused() {
        for field in ["a.b", "shipping.carrier", "$set", "$", ".x"] {
            assert_eq!(
                edit_message(edits(vec![update(
                    other("objectId", OID),
                    &[(field, text("1"))]
                )])),
                FIELD_REFUSED,
                "{field}"
            );
            assert_eq!(
                edit_message(edits(vec![insert(&[(field, text("1"))])])),
                FIELD_REFUSED,
                "{field}"
            );
            assert_eq!(
                edit_message(edits(vec![update(
                    other("objectId", OID),
                    &[(field, CellInput::Unset)]
                )])),
                FIELD_REFUSED,
                "{field}"
            );
        }
        // Only a leading `$` is an operator.
        assert!(edits(vec![insert(&[("price$", text("1"))])]).is_ok());
    }

    #[test]
    fn default_and_loaded_refused() {
        for (input, want) in [(CellInput::Default, NO_DEFAULT), (loaded(), NO_LOADED)] {
            assert_eq!(
                edit_message(edits(vec![update(
                    other("objectId", OID),
                    &[("qty", input.clone())]
                )])),
                want
            );
            assert_eq!(
                edit_message(edits(vec![insert(&[("qty", input.clone())])])),
                want
            );
        }
        // One refused cell refuses the whole set: nothing is built.
        assert!(edits(vec![
            delete(other("objectId", OID)),
            insert(&[("qty", CellInput::Default)]),
        ])
        .is_err());
    }

    #[test]
    fn edit_text_parses_back() {
        let set = [
            ("qty", text("5")),
            ("total", text("9007199254740993")),
            ("at", text("2024-11-05T00:00:00Z")),
            (
                "shipping",
                text("{carrier: 'UPS', ref: ObjectId('65a1b2c3d4e5f60718293a02')}"),
            ),
            (
                "token",
                CellInput::Base64 {
                    data: "3q2+7w==".into(),
                },
            ),
            ("amount", text("1234.5678")),
            ("note", CellInput::Null),
            ("legacy", CellInput::Unset),
        ];
        let ids = [
            other("objectId", OID),
            other("binData", "00:AQID"),
            Value::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into()),
            Value::Text("a \"quoted\" id".into()),
        ];
        for obj in [
            orders(),
            ObjectRef {
                name: "my coll.v2".into(),
                ..orders()
            },
        ] {
            for id in &ids {
                let built = edits_on(
                    &obj,
                    &edit_detail(),
                    vec![update(id.clone(), &set), insert(&set), delete(id.clone())],
                )
                .unwrap();
                for cmd in &built {
                    let back = MongoLanguage
                        .parse(&cmd.text)
                        .unwrap_or_else(|e| panic!("{}: {e:?}", cmd.text));
                    assert_eq!(back.name, cmd.name, "{}", cmd.text);
                    assert_eq!(back.class, cmd.class, "{}", cmd.text);
                    assert_eq!(back.targets, cmd.targets, "{}", cmd.text);
                    assert_eq!(
                        payload_of(&back).unwrap().cmd,
                        payload_of(cmd).unwrap().cmd,
                        "{}",
                        cmd.text
                    );
                }
            }
        }
        // The console's syntax, values as canonical Extended JSON.
        let built = edits(vec![
            update(
                other("objectId", OID),
                &[("qty", text("5")), ("note", CellInput::Unset)],
            ),
            insert(&[("name", text("Zoe"))]),
            delete(other("objectId", OID)),
        ])
        .unwrap();
        let texts: Vec<&str> = built.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "db.getSiblingDB(\"dnt_fixture\").orders.updateOne(\
                 {\"_id\":{\"$oid\":\"65a1b2c3d4e5f60718293a01\"}}, \
                 {\"$set\":{\"qty\":{\"$numberInt\":\"5\"}},\"$unset\":{\"note\":\"\"}})",
                "db.getSiblingDB(\"dnt_fixture\").orders.insertOne({\"name\":\"Zoe\"})",
                "db.getSiblingDB(\"dnt_fixture\").orders.deleteOne(\
                 {\"_id\":{\"$oid\":\"65a1b2c3d4e5f60718293a01\"}})",
            ]
        );
    }

    #[test]
    fn document_not_found_names_the_id() {
        match document_not_found(&oid(OID)) {
            DbError::Server { code, message, .. } => {
                assert_eq!(code.as_deref(), Some("DocumentNotFound"));
                assert_eq!(
                    message,
                    "Document {\"$oid\":\"65a1b2c3d4e5f60718293a01\"} no longer exists"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
