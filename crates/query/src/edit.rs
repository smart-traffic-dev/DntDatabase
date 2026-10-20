//! Data-view edits (spec §4.2a, §5.2): an [`EditSet`] staged in the UI becomes an
//! [`EditPlan`] of key-based DML, each statement with its row guard, plus the set's
//! atomicity and guard verdict. Pure decisions: no I/O.

use std::fmt;

use dnt_core::config::EnvTag;
use dnt_core::meta::{ColumnDetail, IndexDetail, ObjectKind, ObjectRef, TableDetail};
use dnt_core::server::DbKind;
use dnt_core::sql::build::{
    delete_sql, insert_sql, update_sql, BuildError, ColumnTarget, RowGuard, RowKey, TableRef,
};
use dnt_core::sql::{Dialect, StatementKind};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::guard::{strictest, GuardCtx};
use crate::plan::Verdict;

/// What the user typed or picked for one cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellInput {
    Null,
    /// The column's default (new rows only).
    Default,
    /// Text typed in the grid, parsed by the column's logical type.
    Text {
        text: String,
    },
    Value {
        value: Value,
    },
    /// Binary data (standard base64).
    Base64 {
        data: String,
    },
    /// A full value fetched earlier, by its token.
    Loaded {
        token: Uuid,
    },
}

/// One loaded row: its key values, every loaded cell as it was, and (PostgreSQL) its
/// `xmin::text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct RowRef {
    pub key: Vec<(String, Value)>,
    pub original: Vec<(String, Value)>,
    pub xmin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowEdit {
    Update {
        row: RowRef,
        set: Vec<(String, CellInput)>,
    },
    Insert {
        values: Vec<(String, CellInput)>,
    },
    Delete {
        row: RowRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct EditSet {
    pub object: ObjectRef,
    pub edits: Vec<RowEdit>,
    /// Drop every row guard (an explicit "Overwrite" after a conflict).
    pub overwrite: bool,
}

/// Whether a set of edits can be applied all-or-nothing (spec §4.2a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum Atomicity {
    Yes,
    No,
    Unknown,
}

/// How much of a row a statement's guard checks (spec §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Protection {
    /// Any change to the row since it was loaded is a conflict.
    Full,
    /// Changes only in `unchecked` columns go unnoticed.
    Columns { unchecked: Vec<String> },
    /// The key only.
    KeyOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEdit {
    /// Index of the edit in [`EditSet::edits`].
    pub index: u32,
    pub sql: String,
    pub expect_affected: u64,
    pub protection: Protection,
    /// Read the inserted row back (PG `RETURNING`, MySQL `LAST_INSERT_ID()`).
    pub returning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    pub stmts: Vec<PlannedEdit>,
    pub atomic: Atomicity,
    pub verdict: Verdict,
    /// Why the verdict is `Block` (UI copy); `None` otherwise.
    pub reason: Option<String>,
    pub fingerprint: u64,
    pub dialect: Dialect,
}

/// How a server reports a concurrent change to a row: PostgreSQL `xmin`, MySQL a
/// snapshot of the row's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum ProtectionKind {
    RowVersion,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editability {
    /// The columns that identify a row.
    pub key: Vec<String>,
    /// Key columns a `SELECT *` does not return (MySQL invisible columns).
    pub hidden_key: Vec<String>,
    /// Columns an UPDATE may set: not auto-increment, identity or generated.
    pub editable_columns: Vec<String>,
    pub atomic: Atomicity,
    pub protection_kind: ProtectionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotEditable {
    ReadOnlyConnection,
    NotATable,
    NoKey,
    Unsupported { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    NotEditable(NotEditable),
    /// A key value the data page may have cut.
    KeyNotComplete {
        column: String,
    },
    Invalid {
        column: String,
        reason: String,
    },
    /// The table is not atomic: apply one edit at a time.
    OneAtATime,
    Build(BuildError),
}

impl fmt::Display for NotEditable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NotEditable::ReadOnlyConnection => f.write_str("the connection is read-only"),
            NotEditable::NotATable => f.write_str("only tables can be edited"),
            NotEditable::NoKey => f.write_str(
                "the table has no primary key or unique NOT NULL index to identify rows",
            ),
            NotEditable::Unsupported { reason } => f.write_str(reason),
        }
    }
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            EditError::NotEditable(n) => write!(f, "not editable: {n}"),
            EditError::KeyNotComplete { column } => write!(
                f,
                "the key value in {column} may be cut; reload the row before editing it"
            ),
            EditError::Invalid { column, reason } if column.is_empty() => f.write_str(reason),
            EditError::Invalid { column, reason } => write!(f, "{column}: {reason}"),
            EditError::OneAtATime => {
                f.write_str("changes to this table cannot be undone as a unit: apply one at a time")
            }
            EditError::Build(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<BuildError> for EditError {
    fn from(e: BuildError) -> Self {
        EditError::Build(e)
    }
}

/// Why a Production edit of a table that is not [`Atomicity::Yes`] is blocked.
pub const NOT_ATOMIC_ON_PRODUCTION: &str = "Changes to this table cannot be rolled back as a \
     unit (non-transactional engine or triggers), so the data view does not edit it on a \
     Production connection. Write the SQL in a query tab instead.";

/// MySQL engines that roll back (spec §4.2a).
const TRANSACTIONAL_ENGINES: &[&str] = &["InnoDB", "MyRocks", "NDB", "NDBCLUSTER"];

/// Whether the data view may edit `detail`, and how (spec §5.2).
///
/// The key is the primary key, else the first unique index without an expression
/// whose every column exists and is `NOT NULL`.
pub fn editable(
    detail: &TableDetail,
    kind: DbKind,
    read_only: bool,
) -> Result<Editability, NotEditable> {
    if read_only {
        return Err(NotEditable::ReadOnlyConnection);
    }
    if detail.object.kind != ObjectKind::Table {
        return Err(NotEditable::NotATable);
    }
    let column = |name: &str| detail.columns.iter().find(|c| c.name == name);
    let usable = |ix: &IndexDetail| {
        ix.expression.is_none()
            && !ix.columns.is_empty()
            && ix
                .columns
                .iter()
                .all(|c| column(c).is_some_and(|c| !c.nullable))
    };
    let key = match &detail.primary_key {
        Some(pk) if usable(pk) => pk.columns.clone(),
        _ => detail
            .indexes
            .iter()
            .find(|ix| ix.unique && usable(ix))
            .map(|ix| ix.columns.clone())
            .ok_or(NotEditable::NoKey)?,
    };
    let hidden_key = key
        .iter()
        .filter(|k| column(k).is_some_and(|c| c.invisible))
        .cloned()
        .collect();
    let editable_columns = detail
        .columns
        .iter()
        .filter(|c| updatable(c))
        .map(|c| c.name.clone())
        .collect();
    Ok(Editability {
        key,
        hidden_key,
        editable_columns,
        atomic: atomicity(detail, kind),
        protection_kind: match kind {
            DbKind::Postgres => ProtectionKind::RowVersion,
            DbKind::MySql => ProtectionKind::Snapshot,
        },
    })
}

/// An UPDATE may set it: not auto-increment, identity or generated.
fn updatable(c: &ColumnDetail) -> bool {
    !c.auto_increment && c.identity.is_none() && c.generated.is_none()
}

/// A new row may give it a value: not generated, not `GENERATED ALWAYS AS IDENTITY`.
fn insertable(c: &ColumnDetail) -> bool {
    c.generated.is_none()
        && !c
            .identity
            .as_deref()
            .is_some_and(|i| i.eq_ignore_ascii_case("ALWAYS"))
}

/// Spec §4.2a: PostgreSQL DML is atomic. MySQL: a transactional engine and no trigger
/// → `Yes`; a transactional engine with a trigger (it may write elsewhere) or an engine
/// that could not be read → `Unknown`; any other engine → `No`.
pub fn atomicity(detail: &TableDetail, kind: DbKind) -> Atomicity {
    match kind {
        DbKind::Postgres => Atomicity::Yes,
        DbKind::MySql => match detail.options.engine.as_deref() {
            None => Atomicity::Unknown,
            Some(e)
                if !TRANSACTIONAL_ENGINES
                    .iter()
                    .any(|t| e.eq_ignore_ascii_case(t)) =>
            {
                Atomicity::No
            }
            Some(_) if !detail.triggers.is_empty() => Atomicity::Unknown,
            Some(_) => Atomicity::Yes,
        },
    }
}

/// A stable hash (FNV-1a, length-prefixed fields) of what a plan depends on: the
/// table's name, its columns (name, type, nullable, collation, and whether they are
/// auto-increment, identity, generated or invisible), its key and unique indexes, its
/// engine and its trigger names. A plan whose fingerprint no longer matches is stale.
pub fn fingerprint(detail: &TableDetail) -> u64 {
    let mut h = Fnv::new();
    let o = &detail.object;
    h.str(&o.database);
    h.opt(o.schema.as_deref());
    h.str(&o.name);
    h.len(detail.columns.len());
    for c in &detail.columns {
        h.str(&c.name);
        h.str(&c.db_type);
        h.flag(c.nullable);
        h.opt(c.collation.as_deref());
        h.flag(c.auto_increment);
        h.opt(c.identity.as_deref());
        h.opt(c.generated.as_deref());
        h.flag(c.invisible);
    }
    let index = |h: &mut Fnv, ix: &IndexDetail| {
        h.str(&ix.name);
        h.flag(ix.unique);
        h.flag(ix.primary);
        h.len(ix.columns.len());
        for c in &ix.columns {
            h.str(c);
        }
        h.opt(ix.expression.as_deref());
    };
    match &detail.primary_key {
        Some(pk) => {
            h.flag(true);
            index(&mut h, pk);
        }
        None => h.flag(false),
    }
    let unique: Vec<&IndexDetail> = detail.indexes.iter().filter(|i| i.unique).collect();
    h.len(unique.len());
    for ix in unique {
        index(&mut h, ix);
    }
    h.opt(detail.options.engine.as_deref());
    h.len(detail.triggers.len());
    for t in &detail.triggers {
        h.str(&t.name);
    }
    h.0
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn len(&mut self, n: usize) {
        self.bytes(&(n as u64).to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.bytes(s.as_bytes());
    }
    fn flag(&mut self, b: bool) {
        self.bytes(&[u8::from(b)]);
    }
    fn opt(&mut self, s: Option<&str>) {
        match s {
            Some(s) => {
                self.flag(true);
                self.str(s);
            }
            None => self.flag(false),
        }
    }
}

/// Builds the plan for `set` on `detail` (spec §5.2): one key-based statement per
/// edit, in order, each expecting one affected row.
///
/// - `metas`: the loaded columns' types (by name); a column the set touches, or a key
///   column, without one is [`NotEditable::Unsupported`].
/// - Row guard: PostgreSQL `xmin` when the row carries one ([`Protection::Full`]);
///   otherwise a snapshot of every comparable loaded column but the key
///   ([`Protection::Columns`]); no comparable column, or `set.overwrite`, the key only.
/// - Verdict: the guard's verdict for a write; Block on Production when the table is
///   not atomic; at least Confirm on Production for a key-only statement.
/// - A table that is not atomic takes one edit at a time ([`EditError::OneAtATime`]).
pub fn build_edit_plan(
    set: &EditSet,
    detail: &TableDetail,
    metas: &[ColumnMeta],
    dialect: Dialect,
    ctx: GuardCtx,
    loaded: &dyn Fn(Uuid) -> Option<FullValue>,
) -> Result<EditPlan, EditError> {
    let kind = match dialect {
        Dialect::MySql(_) => DbKind::MySql,
        Dialect::Postgres => DbKind::Postgres,
    };
    let ed = editable(detail, kind, ctx.read_only).map_err(EditError::NotEditable)?;
    let (o, t) = (&set.object, &detail.object);
    if (&o.database, &o.schema, &o.name) != (&t.database, &t.schema, &t.name) {
        return Err(invalid("", "the changes are for a different table"));
    }
    if set.edits.is_empty() {
        return Err(invalid("", "there are no changes to apply"));
    }
    if ed.atomic != Atomicity::Yes && set.edits.len() > 1 {
        return Err(EditError::OneAtATime);
    }
    let b = Builder {
        detail,
        metas,
        dialect,
        key: &ed.key,
        overwrite: set.overwrite,
        loaded,
        table: TableRef {
            kind,
            database: t.database.clone(),
            schema: t.schema.clone(),
            name: t.name.clone(),
        },
    };
    let mut stmts = Vec::with_capacity(set.edits.len());
    for (i, edit) in set.edits.iter().enumerate() {
        let index = u32::try_from(i).map_err(|_| invalid("", "too many changes"))?;
        stmts.push(b.edit(index, edit)?);
    }

    let production = ctx.env == Some(EnvTag::Production);
    let mut verdict = Verdict::Allow;
    for s in &stmts {
        verdict = strictest(
            verdict,
            crate::guard::verdict(
                &s.sql,
                StatementKind::Write,
                dialect,
                ctx.env,
                ctx.read_only,
            ),
        );
        if production && s.protection == Protection::KeyOnly {
            verdict = strictest(verdict, Verdict::Confirm);
        }
    }
    let mut reason = None;
    if production && ed.atomic != Atomicity::Yes {
        verdict = Verdict::Block;
        reason = Some(NOT_ATOMIC_ON_PRODUCTION.to_string());
    }
    Ok(EditPlan {
        stmts,
        atomic: ed.atomic,
        verdict,
        reason,
        fingerprint: fingerprint(detail),
        dialect,
    })
}

fn invalid(column: &str, reason: &str) -> EditError {
    EditError::Invalid {
        column: column.into(),
        reason: reason.into(),
    }
}

struct Builder<'a> {
    detail: &'a TableDetail,
    metas: &'a [ColumnMeta],
    dialect: Dialect,
    key: &'a [String],
    overwrite: bool,
    loaded: &'a dyn Fn(Uuid) -> Option<FullValue>,
    table: TableRef,
}

impl Builder<'_> {
    fn edit(&self, index: u32, edit: &RowEdit) -> Result<PlannedEdit, EditError> {
        let d = self.dialect;
        let (sql, protection, returning) = match edit {
            RowEdit::Update { row, set } => {
                let mut assignments: Vec<(ColumnTarget, FullValue)> = Vec::new();
                for (name, input) in set {
                    if assignments.iter().any(|(c, _)| &c.name == name) {
                        return Err(invalid(name, "the column is set twice"));
                    }
                    let (cd, target) = self.target(name)?;
                    if !updatable(cd) {
                        return Err(invalid(
                            name,
                            &format!("{} columns cannot be edited", what(cd)),
                        ));
                    }
                    let Some(v) = self.cell(cd, &target, input)? else {
                        return Err(invalid(name, "DEFAULT applies to new rows only"));
                    };
                    assignments.push((target, v));
                }
                let key = self.row_key(row)?;
                let (guard, protection) = self.row_guard(row)?;
                let sql = update_sql(&self.table, &key, &assignments, &guard, d)?;
                (sql, protection, false)
            }
            RowEdit::Delete { row } => {
                let key = self.row_key(row)?;
                let (guard, protection) = self.row_guard(row)?;
                (delete_sql(&self.table, &key, &guard, d)?, protection, false)
            }
            RowEdit::Insert { values } => {
                let mut cols: Vec<ColumnTarget> = Vec::new();
                let mut row = Vec::new();
                let mut seen: Vec<&str> = Vec::new();
                for (name, input) in values {
                    if seen.contains(&name.as_str()) {
                        return Err(invalid(name, "the column is set twice"));
                    }
                    seen.push(name);
                    if *input == CellInput::Default {
                        // Checked like any other column, then left to the server.
                        self.detail_column(name)?;
                        continue;
                    }
                    let (cd, target) = self.target(name)?;
                    if !insertable(cd) {
                        return Err(invalid(
                            name,
                            &format!("{} columns take no value", what(cd)),
                        ));
                    }
                    if let Some(v) = self.cell(cd, &target, input)? {
                        cols.push(target);
                        row.push(v);
                    }
                }
                // Every column, so the grid can show what the server assigned.
                let returning: Vec<ColumnTarget> = self
                    .detail
                    .columns
                    .iter()
                    .map(|c| self.loose_target(c))
                    .collect();
                let sql = insert_sql(&self.table, &cols, &[row], &returning, d)?;
                (sql, Protection::Full, true)
            }
        };
        Ok(PlannedEdit {
            index,
            sql,
            expect_affected: 1,
            protection,
            returning,
        })
    }

    fn detail_column(&self, name: &str) -> Result<&ColumnDetail, EditError> {
        self.detail
            .columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| invalid(name, "the table has no such column"))
    }

    fn meta(&self, name: &str) -> Option<&ColumnMeta> {
        self.metas.iter().find(|m| m.name == name)
    }

    /// The column's write target; one without type information cannot be written.
    fn target(&self, name: &str) -> Result<(&ColumnDetail, ColumnTarget), EditError> {
        let cd = self.detail_column(name)?;
        let meta = self.meta(name).ok_or_else(|| {
            EditError::NotEditable(NotEditable::Unsupported {
                reason: format!("the type of column {name} is unknown"),
            })
        })?;
        Ok((cd, column_target(cd, meta.logical)))
    }

    /// A target for `RETURNING` or a snapshot candidate: `Other` when its type is unknown.
    fn loose_target(&self, cd: &ColumnDetail) -> ColumnTarget {
        let logical = self
            .meta(&cd.name)
            .map_or(LogicalType::Other, |m| m.logical);
        column_target(cd, logical)
    }

    /// The value written for `input`; `None` for DEFAULT.
    fn cell(
        &self,
        cd: &ColumnDetail,
        t: &ColumnTarget,
        input: &CellInput,
    ) -> Result<Option<FullValue>, EditError> {
        let bad = |reason: String| invalid(&t.name, &reason);
        let v = match input {
            CellInput::Default => return Ok(None),
            CellInput::Null => {
                if !cd.nullable && !cd.auto_increment {
                    return Err(bad("the column cannot be NULL".into()));
                }
                return Ok(Some(null()));
            }
            CellInput::Text { text } => parse_text(text, t, self.dialect).map_err(bad)?,
            CellInput::Value { value } => {
                if value.may_be_page_truncated() || !value.is_complete() {
                    return Err(bad(
                        "the value may be cut; load it in full before editing it".into(),
                    ));
                }
                let v = FullValue::try_from_value(value.clone())
                    .map_err(|_| bad("the value is not complete".into()))?;
                normalise(v, t, self.dialect).map_err(bad)?
            }
            CellInput::Base64 { data } => {
                let bytes = base64_decode(data).ok_or_else(|| bad("invalid base64 data".into()))?;
                if t.logical == LogicalType::Binary {
                    FullValue::bytes(bytes)
                } else {
                    let s = String::from_utf8(bytes)
                        .map_err(|_| bad("the data is not UTF-8 text".into()))?;
                    parse_text(&s, t, self.dialect).map_err(bad)?
                }
            }
            CellInput::Loaded { token } => {
                let v = (self.loaded)(*token).ok_or_else(|| {
                    bad("the loaded value is no longer available; load it again".into())
                })?;
                normalise(v, t, self.dialect).map_err(bad)?
            }
        };
        if v.is_null() && !cd.nullable && !cd.auto_increment {
            return Err(bad("the column cannot be NULL".into()));
        }
        Ok(Some(v))
    }

    fn row_key(&self, row: &RowRef) -> Result<RowKey, EditError> {
        let mut parts = Vec::with_capacity(self.key.len());
        for name in self.key {
            let (_, t) = self.target(name)?;
            if t.logical == LogicalType::Array {
                return Err(array_key(name));
            }
            let v = row
                .key
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v)
                .ok_or_else(|| invalid(name, "the row's key value is missing"))?;
            if matches!(v, Value::Array(_)) {
                return Err(array_key(name));
            }
            if v.may_be_page_truncated() || !v.is_complete() {
                return Err(EditError::KeyNotComplete {
                    column: name.clone(),
                });
            }
            let fv =
                FullValue::try_from_value(v.clone()).map_err(|_| EditError::KeyNotComplete {
                    column: name.clone(),
                })?;
            let fv = normalise(fv, &t, self.dialect).map_err(|r| invalid(name, &r))?;
            parts.push((t, fv));
        }
        Ok(RowKey::new(parts)?)
    }

    fn row_guard(&self, row: &RowRef) -> Result<(RowGuard, Protection), EditError> {
        if self.overwrite {
            return Ok((RowGuard::KeyOnly, Protection::KeyOnly));
        }
        if let (Dialect::Postgres, Some(x)) = (self.dialect, &row.xmin) {
            return Ok((RowGuard::PgXmin(x.clone()), Protection::Full));
        }
        let mut snapshot = Vec::new();
        let mut unchecked = Vec::new();
        for cd in &self.detail.columns {
            if self.key.contains(&cd.name) {
                continue;
            }
            match self.snapshot_value(cd, row) {
                Some(entry) => snapshot.push(entry),
                None => unchecked.push(cd.name.clone()),
            }
        }
        Ok(if snapshot.is_empty() {
            (RowGuard::KeyOnly, Protection::KeyOnly)
        } else {
            (
                RowGuard::Snapshot(snapshot),
                Protection::Columns { unchecked },
            )
        })
    }

    /// The column's loaded value when it can be compared exactly: not a float, MySQL
    /// JSON, PostgreSQL `json`, array or unsupported type, and complete.
    fn snapshot_value(&self, cd: &ColumnDetail, row: &RowRef) -> Option<(ColumnTarget, FullValue)> {
        let meta = self.meta(&cd.name)?;
        let comparable = match meta.logical {
            LogicalType::Float | LogicalType::Array | LogicalType::Other => false,
            LogicalType::Json => {
                self.dialect == Dialect::Postgres && !cd.db_type.eq_ignore_ascii_case("json")
            }
            _ => true,
        };
        if !comparable {
            return None;
        }
        let v = row.original.iter().find(|(n, _)| n == &cd.name)?.1.clone();
        if v.may_be_page_truncated()
            || !v.is_complete()
            || matches!(v, Value::Float(_) | Value::Array(_) | Value::Other { .. })
        {
            return None;
        }
        let t = column_target(cd, meta.logical);
        let fv = FullValue::try_from_value(v).ok()?;
        let fv = normalise(fv, &t, self.dialect).ok()?;
        Some((t, fv))
    }
}

fn array_key(name: &str) -> EditError {
    EditError::NotEditable(NotEditable::Unsupported {
        reason: format!("the key column {name} is an array"),
    })
}

/// "Generated", "Identity" or "Auto-increment", for error messages.
fn what(c: &ColumnDetail) -> &'static str {
    if c.generated.is_some() {
        "generated"
    } else if c.identity.is_some() {
        "identity"
    } else {
        "auto-increment"
    }
}

fn column_target(cd: &ColumnDetail, logical: LogicalType) -> ColumnTarget {
    ColumnTarget {
        name: cd.name.clone(),
        db_type: cd.db_type.clone(),
        logical,
        collation: cd.collation.clone(),
    }
}

fn null() -> FullValue {
    full(Value::Null)
}

/// A value with no binary data is always complete.
fn full(v: Value) -> FullValue {
    FullValue::try_from_value(v).expect("a value without bytes is complete")
}

/// `v` in the shape [`literal`](dnt_core::sql::build::literal) expects for the column's
/// logical type (spec R16): e.g. a `Json` value for a text column (MariaDB reports JSON
/// columns as text) becomes `Text`, so it is never written as `CAST(… AS JSON)`; text
/// for a typed column is parsed. A value that does not fit is an error (the reason).
fn normalise(v: FullValue, t: &ColumnTarget, d: Dialect) -> Result<FullValue, String> {
    use LogicalType as L;
    if v.is_null() {
        return Ok(v);
    }
    let Some(val) = v.as_value() else {
        // Binary data.
        return match t.logical {
            L::Binary => Ok(v),
            _ => Err(format!("binary data does not fit a {} column", t.db_type)),
        };
    };
    let keep = || Ok(v.clone());
    match (t.logical, val) {
        (L::Array | L::Other, _) => Err(unsupported_type(t)),
        (L::Text | L::Enum, Value::Text(_)) => keep(),
        (
            L::Text | L::Enum,
            Value::Json(s)
            | Value::Uuid(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::TimestampTz(s)
            | Value::Decimal(s),
        ) => Ok(full(Value::Text(s.clone()))),
        (L::Text | L::Enum, Value::Int(i)) => Ok(full(Value::Text(i.to_string()))),
        (L::Text | L::Enum, Value::UInt(u)) => Ok(full(Value::Text(u.to_string()))),
        (L::Binary, Value::Text(s)) => Ok(FullValue::bytes(s.as_bytes().to_vec())),
        (_, Value::Text(s)) => parse_text(s, t, d),
        (L::Integer, Value::Int(_) | Value::UInt(_)) => keep(),
        (L::Integer, Value::Bool(b)) => Ok(full(Value::Int(i64::from(*b)))),
        (L::Integer, Value::Decimal(s)) => parse_text(s, t, d),
        (L::Decimal, Value::Decimal(_)) => keep(),
        (L::Decimal, Value::Int(i)) => Ok(full(Value::Decimal(i.to_string()))),
        (L::Decimal, Value::UInt(u)) => Ok(full(Value::Decimal(u.to_string()))),
        (L::Float, Value::Float(_) | Value::Int(_) | Value::UInt(_) | Value::Decimal(_)) => keep(),
        (L::Bool, Value::Bool(_)) => keep(),
        (L::Bool, Value::Int(0) | Value::UInt(0)) => Ok(full(Value::Bool(false))),
        (L::Bool, Value::Int(1) | Value::UInt(1)) => Ok(full(Value::Bool(true))),
        (L::Json, Value::Json(_))
        | (L::Uuid, Value::Uuid(_))
        | (L::Date, Value::Date(_))
        | (L::Time, Value::Time(_))
        | (L::DateTime, Value::DateTime(_) | Value::TimestampTz(_)) => keep(),
        (_, other) => Err(format!(
            "a {} value does not fit a {} column",
            variant_name(other),
            t.db_type
        )),
    }
}

fn unsupported_type(t: &ColumnTarget) -> String {
    format!("values of type {} cannot be edited here", t.db_type)
}

fn variant_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        Value::Int(_) | Value::UInt(_) => "integer",
        Value::Float(_) => "float",
        Value::Decimal(_) => "decimal",
        Value::Text(_) => "text",
        Value::Json(_) => "JSON",
        Value::Uuid(_) => "UUID",
        Value::Bytes { .. } => "binary",
        Value::Date(_) => "date",
        Value::Time(_) => "time",
        Value::DateTime(_) | Value::TimestampTz(_) => "date-time",
        Value::Array(_) => "array",
        Value::Other { .. } => "unsupported",
    }
}

/// Text typed in the grid, read by the column's logical type. The server still checks
/// ranges and calendars; this refuses what is plainly not a value of the type.
fn parse_text(s: &str, t: &ColumnTarget, d: Dialect) -> Result<FullValue, String> {
    use LogicalType as L;
    let pg = d == Dialect::Postgres;
    let ok = |v: Value| Ok(full(v));
    let not = |what: &str| Err(format!("{s:?} is not {what}"));
    match t.logical {
        L::Text | L::Enum => ok(Value::Text(s.into())),
        L::Binary => Ok(FullValue::bytes(s.as_bytes().to_vec())),
        L::Integer => match (s.parse::<i64>(), s.parse::<u64>()) {
            (Ok(i), _) if is_integer(s) => ok(Value::Int(i)),
            (_, Ok(u)) if is_integer(s) => ok(Value::UInt(u)),
            _ => not("an integer"),
        },
        L::Decimal => {
            if is_decimal(s) || (pg && matches!(s, "NaN" | "Infinity" | "-Infinity")) {
                ok(Value::Decimal(s.into()))
            } else {
                not("a decimal number")
            }
        }
        L::Float => match s {
            "NaN" => ok(Value::Float(f64::NAN)),
            "Infinity" => ok(Value::Float(f64::INFINITY)),
            "-Infinity" => ok(Value::Float(f64::NEG_INFINITY)),
            _ if is_decimal(s) => s
                .parse::<f64>()
                .map(|f| full(Value::Float(f)))
                .or_else(|_| not("a number")),
            _ => not("a number"),
        },
        L::Bool => match s.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => ok(Value::Bool(true)),
            "false" | "f" | "no" | "n" | "off" | "0" => ok(Value::Bool(false)),
            _ => not("true or false"),
        },
        L::Date if is_date(s) || (pg && is_pg_infinity(s)) => ok(Value::Date(s.into())),
        L::Date => not("a date (YYYY-MM-DD)"),
        L::Time if is_time(s, true) => ok(Value::Time(s.into())),
        L::Time => not("a time (HH:MM:SS)"),
        L::DateTime if is_datetime(s) || (pg && is_pg_infinity(s)) => ok(Value::DateTime(s.into())),
        L::DateTime => not("a date and time (YYYY-MM-DD HH:MM:SS)"),
        L::Uuid if is_uuid(s) => ok(Value::Uuid(s.into())),
        L::Uuid => not("a UUID"),
        L::Json if serde_json::from_str::<serde_json::Value>(s).is_ok() => {
            ok(Value::Json(s.into()))
        }
        L::Json => not("valid JSON"),
        L::Array | L::Other => Err(unsupported_type(t)),
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `-?\d+`
fn is_integer(s: &str) -> bool {
    digits(s.strip_prefix('-').unwrap_or(s))
}

/// `-?\d+(\.\d+)?([eE][-+]?\d+)?`
fn is_decimal(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
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

fn is_pg_infinity(s: &str) -> bool {
    matches!(s, "infinity" | "-infinity")
}

/// `YYYY-M[M]-D[D]` (year of four or more digits).
fn is_date(s: &str) -> bool {
    let mut it = s.split('-');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(y), Some(m), Some(d), None) => {
            digits(y) && y.len() >= 4 && digits(m) && m.len() <= 2 && digits(d) && d.len() <= 2
        }
        _ => false,
    }
}

/// `[-]H[HH]:MM[:SS[.f…]]` and, when `zone`, an optional `Z` / `±HH[:MM]` suffix.
fn is_time(s: &str, zone: bool) -> bool {
    let s = if zone { strip_zone(s) } else { s };
    let s = s.strip_prefix('-').unwrap_or(s);
    let (hms, frac) = match s.split_once('.') {
        Some((a, f)) => (a, Some(f)),
        None => (s, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let two = |p: &str| digits(p) && p.len() == 2;
    (parts.len() == 2 || parts.len() == 3)
        && digits(parts[0])
        && parts[0].len() <= 3
        && parts[1..].iter().all(|p| two(p))
        && (frac.is_none() || parts.len() == 3)
        && frac.is_none_or(digits)
}

/// `s` without a trailing `Z` or `±HH[:MM]` zone.
fn strip_zone(s: &str) -> &str {
    if let Some(r) = s.strip_suffix('Z') {
        return r;
    }
    if let Some(i) = s.rfind(['+', '-']) {
        let zone = &s[i + 1..];
        let ok = match zone.split_once(':') {
            Some((h, m)) => digits(h) && h.len() == 2 && digits(m) && m.len() == 2,
            None => digits(zone) && (zone.len() == 2 || zone.len() == 4),
        };
        // A leading `-` is a negative time, not a zone.
        if ok && i > 0 && s[..i].contains(':') {
            return &s[..i];
        }
    }
    s
}

/// A date, optionally followed by `T` or a space and a time with an optional zone.
fn is_datetime(s: &str) -> bool {
    match s.split_once(['T', ' ']) {
        Some((date, time)) => is_date(date) && !time.starts_with('-') && is_time(time, true),
        None => is_date(s),
    }
}

/// `8-4-4-4-12` hex digits.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Standard base64 (`+/`), padding optional; anything else is `None`.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let body = s.trim_end_matches('=');
    if s.len() - body.len() > 2 || body.len() % 4 == 1 {
        return None;
    }
    if s.len() != body.len() && !s.len().is_multiple_of(4) {
        return None;
    }
    let sextet = |c: u8| -> Option<u32> {
        Some(u32::from(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    for chunk in body.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= sextet(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::config::EnvTag;
    use dnt_core::meta::{IndexDetail, ObjectKind, TableOptions, TriggerDetail};
    use dnt_core::value::{LogicalType, PAGE_CELL_CAP};

    // ---- fixtures -------------------------------------------------------------------

    fn coldet(name: &str, db_type: &str, nullable: bool, collation: Option<&str>) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 0,
            db_type: db_type.into(),
            nullable,
            default: None,
            auto_increment: false,
            identity: None,
            generated: None,
            invisible: false,
            collation: collation.map(Into::into),
            comment: None,
        }
    }

    fn index(name: &str, cols: &[&str], unique: bool, primary: bool) -> IndexDetail {
        IndexDetail {
            name: name.into(),
            unique,
            primary,
            columns: cols.iter().map(|c| c.to_string()).collect(),
            expression: None,
            method: None,
            definition: None,
        }
    }

    fn object(kind: ObjectKind, schema: Option<&str>) -> ObjectRef {
        ObjectRef {
            database: "shop".into(),
            schema: schema.map(Into::into),
            name: "items".into(),
            kind,
        }
    }

    fn table(
        schema: Option<&str>,
        columns: Vec<ColumnDetail>,
        pk: Option<&[&str]>,
        engine: Option<&str>,
    ) -> TableDetail {
        TableDetail {
            object: object(ObjectKind::Table, schema),
            columns,
            primary_key: pk.map(|c| index("PRIMARY", c, true, true)),
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            options: TableOptions {
                engine: engine.map(Into::into),
                collation: None,
                tablespace: None,
                partitioned_by: None,
                system_versioned: false,
            },
            row_estimate: None,
            size_bytes: None,
            comment: None,
        }
    }

    fn meta(name: &str, db_type: &str, logical: LogicalType) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            db_type: db_type.into(),
            logical,
            nullable: None,
            origin_table: None,
            origin_column: None,
        }
    }

    /// MySQL `shop.items`: id bigint unsigned PK, name varchar (ci collation), qty int,
    /// price double, payload longtext.
    fn my_items(engine: Option<&str>) -> (TableDetail, Vec<ColumnMeta>) {
        let detail = table(
            None,
            vec![
                coldet("id", "bigint unsigned", false, None),
                coldet("name", "varchar(50)", true, Some("utf8mb4_0900_ai_ci")),
                coldet("qty", "int", true, None),
                coldet("price", "double", true, None),
                coldet("payload", "longtext", true, Some("utf8mb4_bin")),
            ],
            Some(&["id"]),
            engine,
        );
        let metas = vec![
            meta("id", "bigint unsigned", LogicalType::Integer),
            meta("name", "varchar", LogicalType::Text),
            meta("qty", "int", LogicalType::Integer),
            meta("price", "double", LogicalType::Float),
            meta("payload", "longtext", LogicalType::Text),
        ];
        (detail, metas)
    }

    /// PostgreSQL `shop.public.items`: id int4 PK, name text, tags int4[].
    fn pg_items() -> (TableDetail, Vec<ColumnMeta>) {
        let detail = table(
            Some("public"),
            vec![
                coldet("id", "integer", false, None),
                coldet("name", "text", true, None),
                coldet("tags", "integer[]", true, None),
                coldet("ref", "uuid", true, None),
                coldet("doc", "jsonb", true, None),
            ],
            Some(&["id"]),
            None,
        );
        let metas = vec![
            meta("id", "int4", LogicalType::Integer),
            meta("name", "text", LogicalType::Text),
            meta("tags", "_int4", LogicalType::Array),
            meta("ref", "uuid", LogicalType::Uuid),
            meta("doc", "jsonb", LogicalType::Json),
        ];
        (detail, metas)
    }

    fn ctx(env: Option<EnvTag>) -> GuardCtx {
        GuardCtx {
            env,
            read_only: false,
            analyze: false,
            confirm_unbounded: true,
        }
    }

    fn none(_: Uuid) -> Option<FullValue> {
        None
    }

    fn pairs<T: Clone>(items: &[(&str, T)]) -> Vec<(String, T)> {
        items
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect()
    }

    fn row(key: &[(&str, Value)], original: &[(&str, Value)]) -> RowRef {
        RowRef {
            key: pairs(key),
            original: pairs(original),
            xmin: None,
        }
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    /// A full original row of [`my_items`] with id 1.
    fn my_row() -> RowRef {
        row(
            &[("id", Value::UInt(1))],
            &[
                ("id", Value::UInt(1)),
                ("name", text("Anna")),
                ("qty", Value::Int(3)),
                ("price", Value::Float(9.5)),
                ("payload", text("{}")),
            ],
        )
    }

    fn set(edits: Vec<RowEdit>) -> EditSet {
        EditSet {
            object: object(ObjectKind::Table, None),
            edits,
            overwrite: false,
        }
    }

    fn pg_set(edits: Vec<RowEdit>) -> EditSet {
        EditSet {
            object: object(ObjectKind::Table, Some("public")),
            edits,
            overwrite: false,
        }
    }

    fn plan_my(
        engine: Option<&str>,
        s: &EditSet,
        env: Option<EnvTag>,
    ) -> Result<EditPlan, EditError> {
        let (d, m) = my_items(engine);
        build_edit_plan(s, &d, &m, Dialect::MYSQL, ctx(env), &none)
    }

    fn plan_pg(s: &EditSet) -> Result<EditPlan, EditError> {
        let (d, m) = pg_items();
        build_edit_plan(s, &d, &m, Dialect::Postgres, ctx(None), &none)
    }

    fn update(r: RowRef, cells: &[(&str, CellInput)]) -> RowEdit {
        RowEdit::Update {
            row: r,
            set: pairs(cells),
        }
    }

    fn typed(s: &str) -> CellInput {
        CellInput::Text { text: s.into() }
    }

    const INNODB: Option<&str> = Some("InnoDB");

    // ---- editability and atomicity ---------------------------------------------------

    #[test]
    fn editable_uses_pk_then_not_null_unique() {
        let (d, _) = my_items(INNODB);
        let e = editable(&d, DbKind::MySql, false).unwrap();
        assert_eq!(e.key, ["id"]);
        assert_eq!(e.atomic, Atomicity::Yes);
        assert_eq!(e.protection_kind, ProtectionKind::Snapshot);
        assert!(e.hidden_key.is_empty());

        // No PK: an expression index, a unique index over a nullable column and a
        // non-unique index are skipped; the first unique all-NOT-NULL index wins.
        let mut d = table(
            None,
            vec![
                coldet("code", "varchar(10)", false, None),
                coldet("email", "varchar(99)", true, None),
                coldet("sku", "varchar(10)", false, None),
                coldet("seq", "int", false, None),
            ],
            None,
            INNODB,
        );
        let mut expr = index("by_lower", &[], true, false);
        expr.expression = Some("lower(code)".into());
        d.indexes = vec![
            expr,
            index("by_email", &["email"], true, false),
            index("by_seq", &["seq"], false, false),
            index("by_sku_code", &["sku", "code"], true, false),
            index("by_code", &["code"], true, false),
        ];
        d.columns[2].invisible = true;
        let e = editable(&d, DbKind::MySql, false).unwrap();
        assert_eq!(e.key, ["sku", "code"]);
        assert_eq!(e.hidden_key, ["sku"]);

        // Auto-increment, identity and generated columns are not editable.
        let (mut d, _) = pg_items();
        d.columns[0].auto_increment = true;
        d.columns[0].identity = Some("ALWAYS".into());
        d.columns[1].generated = Some("upper(x)".into());
        let e = editable(&d, DbKind::Postgres, false).unwrap();
        assert_eq!(e.editable_columns, ["tags", "ref", "doc"]);
        assert_eq!(e.protection_kind, ProtectionKind::RowVersion);
    }

    #[test]
    fn no_key_table_is_read_only() {
        let (mut d, m) = my_items(INNODB);
        d.primary_key = None;
        d.indexes = vec![index("by_name", &["name"], true, false)];
        assert_eq!(editable(&d, DbKind::MySql, false), Err(NotEditable::NoKey));
        assert_eq!(
            build_edit_plan(
                &set(vec![RowEdit::Delete { row: my_row() }]),
                &d,
                &m,
                Dialect::MYSQL,
                ctx(None),
                &none
            ),
            Err(EditError::NotEditable(NotEditable::NoKey))
        );

        let (mut d, _) = my_items(INNODB);
        for kind in [ObjectKind::View, ObjectKind::MaterializedView] {
            d.object.kind = kind;
            assert_eq!(
                editable(&d, DbKind::MySql, false),
                Err(NotEditable::NotATable)
            );
        }
        let (d, _) = my_items(INNODB);
        assert_eq!(
            editable(&d, DbKind::MySql, true),
            Err(NotEditable::ReadOnlyConnection)
        );
    }

    #[test]
    fn myisam_is_not_atomic() {
        for engine in ["MyISAM", "MEMORY", "Aria", "CSV", "ARCHIVE"] {
            let (d, _) = my_items(Some(engine));
            assert_eq!(atomicity(&d, DbKind::MySql), Atomicity::No, "{engine}");
        }
        // An engine that could not be read.
        let (d, _) = my_items(None);
        assert_eq!(atomicity(&d, DbKind::MySql), Atomicity::Unknown);
    }

    #[test]
    fn innodb_with_trigger_is_unknown() {
        for engine in ["InnoDB", "innodb", "MyRocks", "NDB", "ndbcluster"] {
            let (mut d, _) = my_items(Some(engine));
            assert_eq!(atomicity(&d, DbKind::MySql), Atomicity::Yes, "{engine}");
            d.triggers = vec![TriggerDetail {
                name: "audit".into(),
                timing: "AFTER".into(),
                event: "UPDATE".into(),
            }];
            assert_eq!(atomicity(&d, DbKind::MySql), Atomicity::Unknown, "{engine}");
        }
    }

    #[test]
    fn pg_is_atomic() {
        let (mut d, _) = pg_items();
        assert_eq!(atomicity(&d, DbKind::Postgres), Atomicity::Yes);
        d.triggers = vec![TriggerDetail {
            name: "audit".into(),
            timing: "AFTER".into(),
            event: "UPDATE".into(),
        }];
        assert_eq!(atomicity(&d, DbKind::Postgres), Atomicity::Yes);
    }

    // ---- verdict ------------------------------------------------------------------------

    #[test]
    fn production_blocks_non_atomic_edits() {
        let one = set(vec![update(my_row(), &[("qty", typed("4"))])]);
        let p = plan_my(Some("MyISAM"), &one, Some(EnvTag::Production)).unwrap();
        assert_eq!(p.atomic, Atomicity::No);
        assert_eq!(p.verdict, Verdict::Block);
        assert!(p.reason.as_deref().is_some_and(|r| !r.is_empty()));
        // Unknown atomicity blocks too.
        let p = plan_my(None, &one, Some(EnvTag::Production)).unwrap();
        assert_eq!(p.verdict, Verdict::Block);
        // Elsewhere it is allowed, one edit at a time.
        let p = plan_my(Some("MyISAM"), &one, Some(EnvTag::Local)).unwrap();
        assert_eq!((p.verdict, p.reason), (Verdict::Allow, None));
        let two = set(vec![
            update(my_row(), &[("qty", typed("4"))]),
            RowEdit::Delete { row: my_row() },
        ]);
        assert_eq!(
            plan_my(Some("MyISAM"), &two, None),
            Err(EditError::OneAtATime)
        );
        // An atomic table on Production: a write to confirm.
        let p = plan_my(INNODB, &two, Some(EnvTag::Production)).unwrap();
        assert_eq!((p.verdict, p.reason), (Verdict::Confirm, None));
        assert_eq!(p.stmts.len(), 2);
        assert_eq!(p.stmts[1].index, 1);
        assert!(p.stmts.iter().all(|s| s.expect_affected == 1));
        // A read-only connection never gets a plan.
        let (d, m) = my_items(INNODB);
        let mut ro = ctx(None);
        ro.read_only = true;
        assert_eq!(
            build_edit_plan(&one, &d, &m, Dialect::MYSQL, ro, &none),
            Err(EditError::NotEditable(NotEditable::ReadOnlyConnection))
        );
    }

    // ---- row guard ----------------------------------------------------------------------

    #[test]
    fn delete_uses_full_snapshot_not_only_edited_columns() {
        let r = row(
            &[("id", Value::UInt(1))],
            &[
                ("id", Value::UInt(1)),
                ("name", text("Anna")),
                ("qty", Value::Int(3)),
                ("payload", Value::Null),
            ],
        );
        let p = plan_my(INNODB, &set(vec![RowEdit::Delete { row: r.clone() }]), None).unwrap();
        let s = &p.stmts[0];
        assert_eq!(
            s.sql,
            "DELETE FROM `shop`.`items` WHERE `id` = 1 \
             AND `name` <=> CONVERT(_utf8mb4'Anna' USING utf8mb4) COLLATE utf8mb4_bin \
             AND `qty` <=> 3 AND `payload` <=> NULL"
        );
        assert_eq!(s.expect_affected, 1);
        assert!(!s.returning);
        // `price` is a float and was not loaded: listed, never compared.
        assert_eq!(
            s.protection,
            Protection::Columns {
                unchecked: vec!["price".into()]
            }
        );
        // An UPDATE of one column checks every comparable column too.
        let p = plan_my(INNODB, &set(vec![update(r, &[("qty", typed("4"))])]), None).unwrap();
        assert_eq!(
            p.stmts[0].sql,
            "UPDATE `shop`.`items` SET `qty` = 4 WHERE `id` = 1 \
             AND `name` <=> CONVERT(_utf8mb4'Anna' USING utf8mb4) COLLATE utf8mb4_bin \
             AND `qty` <=> 3 AND `payload` <=> NULL"
        );
    }

    #[test]
    fn float_and_truncated_cells_are_unchecked() {
        let long = "x".repeat(PAGE_CELL_CAP - 3);
        let mut r = my_row();
        r.original[4].1 = text(&long);
        let p = plan_my(INNODB, &set(vec![update(r, &[("qty", typed("4"))])]), None).unwrap();
        let s = &p.stmts[0];
        assert_eq!(
            s.protection,
            Protection::Columns {
                unchecked: vec!["price".into(), "payload".into()]
            }
        );
        assert!(!s.sql.contains("`price` <=>"), "{}", s.sql);
        assert!(!s.sql.contains("`payload` <=>"), "{}", s.sql);
        assert!(s.sql.contains("`qty` <=> 3"), "{}", s.sql);

        // Cut binary data is not compared either; JSON on MySQL compares semantically.
        let mut d = table(
            None,
            vec![
                coldet("id", "int", false, None),
                coldet("blob", "blob", true, None),
                coldet("doc", "json", true, None),
            ],
            Some(&["id"]),
            INNODB,
        );
        d.object = object(ObjectKind::Table, None);
        let m = vec![
            meta("id", "int", LogicalType::Integer),
            meta("blob", "blob", LogicalType::Binary),
            meta("doc", "json", LogicalType::Json),
        ];
        let r = row(
            &[("id", Value::Int(1))],
            &[
                ("id", Value::Int(1)),
                ("blob", Value::bytes(&[7; 100])),
                ("doc", Value::Json("{}".into())),
            ],
        );
        let p = build_edit_plan(
            &set(vec![RowEdit::Delete { row: r }]),
            &d,
            &m,
            Dialect::MYSQL,
            ctx(None),
            &none,
        )
        .unwrap();
        assert_eq!(p.stmts[0].protection, Protection::KeyOnly);
        assert_eq!(p.stmts[0].sql, "DELETE FROM `shop`.`items` WHERE `id` = 1");
    }

    #[test]
    fn array_cells_are_unchecked_and_cannot_be_keys() {
        let r = RowRef {
            key: pairs(&[("id", Value::Int(1))]),
            original: pairs(&[
                ("id", Value::Int(1)),
                ("name", text("a")),
                ("tags", Value::Array(vec![Value::Int(1)])),
                ("ref", Value::Null),
                ("doc", Value::Json("{}".into())),
            ]),
            xmin: None,
        };
        let p = plan_pg(&pg_set(vec![RowEdit::Delete { row: r }])).unwrap();
        assert_eq!(
            p.stmts[0].sql,
            "DELETE FROM \"public\".\"items\" WHERE \"id\" = 1 \
             AND \"name\" IS NOT DISTINCT FROM 'a' AND \"ref\" IS NOT DISTINCT FROM NULL \
             AND \"doc\" IS NOT DISTINCT FROM '{}'::jsonb"
        );
        assert_eq!(
            p.stmts[0].protection,
            Protection::Columns {
                unchecked: vec!["tags".into()]
            }
        );
        // An array key column cannot identify a row.
        let (mut d, m) = pg_items();
        d.primary_key = Some(index("pk", &["tags"], true, true));
        d.columns[2].nullable = false;
        let r = row(&[("tags", Value::Array(vec![Value::Int(1)]))], &[]);
        assert!(matches!(
            build_edit_plan(
                &pg_set(vec![RowEdit::Delete { row: r }]),
                &d,
                &m,
                Dialect::Postgres,
                ctx(None),
                &none
            ),
            Err(EditError::NotEditable(NotEditable::Unsupported { .. }))
        ));
    }

    #[test]
    fn pg_xmin_guards_the_whole_row() {
        let mut r = row(&[("id", Value::Int(1))], &[("name", text("a"))]);
        r.xmin = Some("42".into());
        let p = plan_pg(&pg_set(vec![update(r, &[("name", typed("b"))])])).unwrap();
        assert_eq!(
            p.stmts[0].sql,
            "UPDATE \"public\".\"items\" SET \"name\" = 'b' WHERE \"id\" = 1 AND xmin = '42'::xid"
        );
        assert_eq!(p.stmts[0].protection, Protection::Full);
        assert_eq!(p.atomic, Atomicity::Yes);
    }

    #[test]
    fn bigint_key_beyond_2_53_round_trips() {
        let r = row(&[("id", Value::UInt(18446744073709551615))], &[]);
        let p = plan_my(INNODB, &set(vec![RowEdit::Delete { row: r }]), None).unwrap();
        assert!(
            p.stmts[0].sql.contains("`id` = 18446744073709551615"),
            "{}",
            p.stmts[0].sql
        );
    }

    #[test]
    fn key_that_may_be_truncated_is_rejected() {
        let (mut d, m) = my_items(INNODB);
        d.primary_key = Some(index("PRIMARY", &["payload"], true, true));
        d.columns[4].nullable = false;
        let r = row(&[("payload", text(&"k".repeat(PAGE_CELL_CAP)))], &[]);
        assert_eq!(
            build_edit_plan(
                &set(vec![RowEdit::Delete { row: r }]),
                &d,
                &m,
                Dialect::MYSQL,
                ctx(None),
                &none
            ),
            Err(EditError::KeyNotComplete {
                column: "payload".into()
            })
        );
        // Cut binary data too.
        let r = row(&[("payload", Value::bytes(&[1; 100]))], &[]);
        assert!(matches!(
            build_edit_plan(
                &set(vec![RowEdit::Delete { row: r }]),
                &d,
                &m,
                Dialect::MYSQL,
                ctx(None),
                &none
            ),
            Err(EditError::KeyNotComplete { .. })
        ));
        // A key value that is missing is invalid.
        let r = row(&[], &[]);
        assert!(matches!(
            plan_my(INNODB, &set(vec![RowEdit::Delete { row: r }]), None),
            Err(EditError::Invalid { ref column, .. }) if column == "id"
        ));
    }

    #[test]
    fn overwrite_drops_guard() {
        let mut s = set(vec![RowEdit::Delete { row: my_row() }]);
        s.overwrite = true;
        let p = plan_my(INNODB, &s, None).unwrap();
        assert_eq!(p.stmts[0].sql, "DELETE FROM `shop`.`items` WHERE `id` = 1");
        assert_eq!(p.stmts[0].protection, Protection::KeyOnly);
        assert_eq!(p.verdict, Verdict::Allow);
        // Key-only on Production must be confirmed.
        let p = plan_my(INNODB, &s, Some(EnvTag::Production)).unwrap();
        assert_eq!(p.verdict, Verdict::Confirm);
        // PG: the xmin is dropped too.
        let mut r = row(&[("id", Value::Int(1))], &[]);
        r.xmin = Some("42".into());
        let mut s = pg_set(vec![RowEdit::Delete { row: r }]);
        s.overwrite = true;
        let p = plan_pg(&s).unwrap();
        assert_eq!(
            p.stmts[0].sql,
            "DELETE FROM \"public\".\"items\" WHERE \"id\" = 1"
        );
        assert_eq!(p.stmts[0].protection, Protection::KeyOnly);
    }

    #[test]
    fn fingerprint_changes_with_collation_or_trigger() {
        let (d, _) = my_items(INNODB);
        let base = fingerprint(&d);
        assert_eq!(base, fingerprint(&d.clone()));
        let mut c = d.clone();
        c.columns[1].collation = Some("utf8mb4_bin".into());
        assert_ne!(fingerprint(&c), base);
        let mut t = d.clone();
        t.triggers.push(TriggerDetail {
            name: "audit".into(),
            timing: "AFTER".into(),
            event: "UPDATE".into(),
        });
        assert_ne!(fingerprint(&t), base);
        let mut e = d.clone();
        e.options.engine = Some("MyISAM".into());
        assert_ne!(fingerprint(&e), base);
        let mut k = d.clone();
        k.primary_key = Some(index("PRIMARY", &["id", "name"], true, true));
        assert_ne!(fingerprint(&k), base);
        let mut ty = d.clone();
        ty.columns[2].db_type = "bigint".into();
        assert_ne!(fingerprint(&ty), base);
        let mut n = d.clone();
        n.columns[2].nullable = false;
        assert_ne!(fingerprint(&n), base);
        // The plan carries it.
        let p = plan_my(INNODB, &set(vec![RowEdit::Delete { row: my_row() }]), None).unwrap();
        assert_eq!(p.fingerprint, base);
        assert_eq!(p.dialect, Dialect::MYSQL);
    }

    // ---- values -------------------------------------------------------------------------

    #[test]
    fn text_input_parses_by_column_type() {
        let up = |cells: &[(&str, CellInput)]| {
            plan_my(INNODB, &set(vec![update(my_row(), cells)]), None)
        };
        assert!(matches!(
            up(&[("qty", typed("12a"))]),
            Err(EditError::Invalid { ref column, .. }) if column == "qty"
        ));
        assert!(up(&[("qty", typed("12"))]).unwrap().stmts[0]
            .sql
            .starts_with("UPDATE `shop`.`items` SET `qty` = 12 WHERE"));
        assert!(matches!(
            up(&[("price", typed("1.5.1"))]),
            Err(EditError::Invalid { ref column, .. }) if column == "price"
        ));
        assert!(up(&[("price", typed("-1.5e3"))]).unwrap().stmts[0]
            .sql
            .starts_with("UPDATE `shop`.`items` SET `price` = -1500.0 WHERE"));
        // Text stays text; the empty string is not NULL.
        assert!(up(&[("name", typed("")), ("payload", CellInput::Null)])
            .unwrap()
            .stmts[0]
            .sql
            .starts_with("UPDATE `shop`.`items` SET `name` = _utf8mb4'', `payload` = NULL WHERE"));
        // A NOT NULL column refuses NULL; DEFAULT is for new rows.
        assert!(matches!(
            up(&[("id", CellInput::Null)]),
            Err(EditError::Invalid { ref column, .. }) if column == "id"
        ));
        assert!(matches!(
            up(&[("qty", CellInput::Default)]),
            Err(EditError::Invalid { ref column, .. }) if column == "qty"
        ));
        // Unknown column.
        assert!(matches!(
            up(&[("nope", typed("1"))]),
            Err(EditError::Invalid { ref column, .. }) if column == "nope"
        ));

        // PG: uuid, json and integer parse; bad input is refused per column.
        let pg_up = |cells: &[(&str, CellInput)]| {
            let mut r = row(&[("id", Value::Int(1))], &[]);
            r.xmin = Some("7".into());
            plan_pg(&pg_set(vec![update(r, cells)]))
        };
        let u = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(
            pg_up(&[("ref", typed(u)), ("doc", typed(r#"{"a": 1}"#))]).unwrap().stmts[0].sql,
            format!(
                "UPDATE \"public\".\"items\" SET \"ref\" = '{u}'::uuid, \"doc\" = '{{\"a\": 1}}'::jsonb \
                 WHERE \"id\" = 1 AND xmin = '7'::xid"
            )
        );
        for (col, bad) in [("ref", "not-a-uuid"), ("doc", "{a:"), ("tags", "{1,2}")] {
            assert!(
                matches!(
                    pg_up(&[(col, typed(bad))]),
                    Err(EditError::Invalid { ref column, .. }) if column == col
                ),
                "{col} {bad}"
            );
        }
    }

    #[test]
    fn date_bool_and_binary_inputs() {
        let mut d = table(
            None,
            vec![
                coldet("id", "int", false, None),
                coldet("day", "date", true, None),
                coldet("at", "datetime", true, None),
                coldet("on", "bool", true, None),
                coldet("bin", "varbinary(10)", true, None),
            ],
            Some(&["id"]),
            INNODB,
        );
        d.object = object(ObjectKind::Table, None);
        let m = vec![
            meta("id", "int", LogicalType::Integer),
            meta("day", "date", LogicalType::Date),
            meta("at", "datetime", LogicalType::DateTime),
            meta("on", "bool", LogicalType::Bool),
            meta("bin", "varbinary", LogicalType::Binary),
        ];
        let go = |cells: &[(&str, CellInput)]| {
            let mut s = set(vec![update(row(&[("id", Value::Int(1))], &[]), cells)]);
            s.overwrite = true;
            build_edit_plan(&s, &d, &m, Dialect::MYSQL, ctx(None), &none)
        };
        assert_eq!(
            go(&[
                ("day", typed("2024-02-29")),
                ("at", typed("2024-02-29 13:45:00.5")),
                ("on", typed("true")),
                (
                    "bin",
                    CellInput::Base64 {
                        data: "AP8=".into()
                    }
                ),
            ])
            .unwrap()
            .stmts[0]
                .sql,
            "UPDATE `shop`.`items` SET `day` = _utf8mb4'2024-02-29', \
             `at` = _utf8mb4'2024-02-29 13:45:00.5', `on` = 1, `bin` = X'00ff' WHERE `id` = 1"
        );
        for (col, bad) in [("day", "29/02/2024"), ("at", "tomorrow"), ("on", "maybe")] {
            assert!(
                matches!(go(&[(col, typed(bad))]), Err(EditError::Invalid { ref column, .. }) if column == col),
                "{col} {bad}"
            );
        }
        assert!(matches!(
            go(&[("bin", CellInput::Base64 { data: "A*==".into() })]),
            Err(EditError::Invalid { ref column, .. }) if column == "bin"
        ));
    }

    /// MariaDB reports JSON columns as text: a JSON value is written as a string, never
    /// `CAST(… AS JSON)`.
    #[test]
    fn json_value_for_a_text_column_is_written_as_text() {
        let p = plan_my(
            INNODB,
            &set(vec![update(
                my_row(),
                &[(
                    "payload",
                    CellInput::Value {
                        value: Value::Json(r#"{"a":1}"#.into()),
                    },
                )],
            )]),
            None,
        )
        .unwrap();
        assert!(
            p.stmts[0]
                .sql
                .starts_with(r#"UPDATE `shop`.`items` SET `payload` = _utf8mb4'{"a":1}' WHERE"#),
            "{}",
            p.stmts[0].sql
        );
        assert!(!p.stmts[0].sql.contains("AS JSON"));
        // A boolean fits an integer column (MySQL `tinyint(1)`); a UUID does not.
        assert!(plan_my(
            INNODB,
            &set(vec![update(
                my_row(),
                &[(
                    "qty",
                    CellInput::Value {
                        value: Value::Bool(true)
                    }
                )]
            )]),
            None
        )
        .unwrap()
        .stmts[0]
            .sql
            .starts_with("UPDATE `shop`.`items` SET `qty` = 1 WHERE"));
        assert!(matches!(
            plan_my(
                INNODB,
                &set(vec![update(
                    my_row(),
                    &[("qty", CellInput::Value { value: Value::Uuid("x".into()) })]
                )]),
                None
            ),
            Err(EditError::Invalid { ref column, .. }) if column == "qty"
        ));
        // A value cut by the page is never written.
        assert!(matches!(
            plan_my(
                INNODB,
                &set(vec![update(
                    my_row(),
                    &[("payload", CellInput::Value { value: text(&"x".repeat(PAGE_CELL_CAP)) })]
                )]),
                None
            ),
            Err(EditError::Invalid { ref column, .. }) if column == "payload"
        ));
    }

    #[test]
    fn loaded_token_supplies_the_full_value() {
        let token = Uuid::from_u128(7);
        let big = "y".repeat(PAGE_CELL_CAP + 10);
        let full = FullValue::try_from_value(text(&big)).unwrap();
        let loaded = move |t: Uuid| (t == token).then(|| full.clone());
        let (d, m) = my_items(INNODB);
        let s = set(vec![update(
            my_row(),
            &[("payload", CellInput::Loaded { token })],
        )]);
        let p = build_edit_plan(&s, &d, &m, Dialect::MYSQL, ctx(None), &loaded).unwrap();
        assert!(p.stmts[0].sql.contains(&big));
        let s = set(vec![update(
            my_row(),
            &[(
                "payload",
                CellInput::Loaded {
                    token: Uuid::from_u128(8),
                },
            )],
        )]);
        assert!(matches!(
            build_edit_plan(&s, &d, &m, Dialect::MYSQL, ctx(None), &loaded),
            Err(EditError::Invalid { ref column, .. }) if column == "payload"
        ));
    }

    #[test]
    fn column_missing_from_metas_is_unsupported_when_touched() {
        let (d, mut m) = my_items(INNODB);
        m.retain(|c| c.name != "qty");
        let go = |s: &EditSet| build_edit_plan(s, &d, &m, Dialect::MYSQL, ctx(None), &none);
        assert!(matches!(
            go(&set(vec![update(my_row(), &[("qty", typed("1"))])])),
            Err(EditError::NotEditable(NotEditable::Unsupported { .. }))
        ));
        // Not touched: only left out of the snapshot.
        let p = go(&set(vec![update(my_row(), &[("name", typed("b"))])])).unwrap();
        assert_eq!(
            p.stmts[0].protection,
            Protection::Columns {
                unchecked: vec!["qty".into(), "price".into()]
            }
        );
        // A key column without type information.
        let (d, mut m) = my_items(INNODB);
        m.retain(|c| c.name != "id");
        assert!(matches!(
            build_edit_plan(
                &set(vec![RowEdit::Delete { row: my_row() }]),
                &d,
                &m,
                Dialect::MYSQL,
                ctx(None),
                &none
            ),
            Err(EditError::NotEditable(NotEditable::Unsupported { .. }))
        ));
    }

    #[test]
    fn insert_skips_defaults_and_reads_back() {
        let (mut d, m) = pg_items();
        d.columns[0].auto_increment = true;
        let s = pg_set(vec![RowEdit::Insert {
            values: pairs(&[
                ("id", CellInput::Default),
                ("name", typed("n")),
                ("ref", CellInput::Null),
            ]),
        }]);
        let p = build_edit_plan(&s, &d, &m, Dialect::Postgres, ctx(None), &none).unwrap();
        let st = &p.stmts[0];
        assert_eq!(
            st.sql,
            "INSERT INTO \"public\".\"items\" (\"name\", \"ref\") VALUES ('n', NULL) \
             RETURNING \"id\", \"name\", \"tags\", \"ref\", \"doc\""
        );
        assert!(st.returning);
        assert_eq!(st.expect_affected, 1);
        // An auto-increment column takes a value the user typed on a new row ...
        let s = pg_set(vec![RowEdit::Insert {
            values: pairs(&[("id", typed("5"))]),
        }]);
        assert!(
            build_edit_plan(&s, &d, &m, Dialect::Postgres, ctx(None), &none)
                .unwrap()
                .stmts[0]
                .sql
                .starts_with("INSERT INTO \"public\".\"items\" (\"id\") VALUES (5)")
        );
        // ... but not in an UPDATE.
        let mut r = row(&[("id", Value::Int(1))], &[]);
        r.xmin = Some("1".into());
        assert!(matches!(
            build_edit_plan(
                &pg_set(vec![update(r, &[("id", typed("5"))])]),
                &d,
                &m,
                Dialect::Postgres,
                ctx(None),
                &none
            ),
            Err(EditError::Invalid { ref column, .. }) if column == "id"
        ));
        // Identity ALWAYS and generated columns take no value at all.
        d.columns[0].identity = Some("ALWAYS".into());
        d.columns[1].generated = Some("upper(x)".into());
        for col in ["id", "name"] {
            let s = pg_set(vec![RowEdit::Insert {
                values: pairs(&[(col, typed("5"))]),
            }]);
            assert!(
                matches!(
                    build_edit_plan(&s, &d, &m, Dialect::Postgres, ctx(None), &none),
                    Err(EditError::Invalid { ref column, .. }) if column == col
                ),
                "{col}"
            );
        }
        // All defaults.
        let s = pg_set(vec![RowEdit::Insert { values: vec![] }]);
        assert!(
            build_edit_plan(&s, &d, &m, Dialect::Postgres, ctx(None), &none)
                .unwrap()
                .stmts[0]
                .sql
                .starts_with("INSERT INTO \"public\".\"items\" DEFAULT VALUES RETURNING")
        );
    }

    #[test]
    fn set_must_match_the_table() {
        let mut s = set(vec![RowEdit::Delete { row: my_row() }]);
        s.object.name = "other".into();
        assert!(matches!(
            plan_my(INNODB, &s, None),
            Err(EditError::Invalid { .. })
        ));
        assert!(matches!(
            plan_my(INNODB, &set(vec![]), None),
            Err(EditError::Invalid { .. })
        ));
    }

    #[test]
    fn edit_set_wire_format() {
        let s: EditSet = serde_json::from_value(serde_json::json!({
            "object": {"database": "shop", "schema": null, "name": "items", "kind": "Table"},
            "edits": [
                {"op": "update",
                 "row": {"key": [["id", {"t": "UInt", "v": "18446744073709551615"}]],
                         "original": [], "xmin": null},
                 "set": [["name", {"kind": "text", "text": "x"}],
                         ["qty", {"kind": "null"}]]},
                {"op": "insert", "values": [["name", {"kind": "default"}]]},
                {"op": "delete", "row": {"key": [], "original": [], "xmin": "5"}}
            ],
            "overwrite": false
        }))
        .unwrap();
        assert_eq!(s.edits.len(), 3);
        assert!(matches!(
            &s.edits[0],
            RowEdit::Update { row, .. } if row.key[0].1 == Value::UInt(u64::MAX)
        ));
        assert_eq!(
            serde_json::to_value(Protection::Columns {
                unchecked: vec!["a".into()]
            })
            .unwrap(),
            serde_json::json!({"level": "columns", "unchecked": ["a"]})
        );
    }
}
