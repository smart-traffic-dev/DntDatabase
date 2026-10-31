//! The table data view: pages of a table / view / materialized view, and its exact row
//! count.
//!
//! # Where the statements run
//!
//! On the connection's data sessions (see [`crate::data_sessions`]): never on a query
//! tab's session (so the view neither waits for a tab's run nor reads inside a tab's
//! open transaction) and never under the metadata `link` (so a slow page or count does
//! not hold up the tree, completion or Info). Every request runs under a caller-chosen
//! `request_key` (the UI uses one per tab and purpose): a new request under the same key
//! cancels the previous one, and `table_data_cancel` cancels it on demand.
//!
//! The statements are plain `SELECT`s built here (identifiers quoted per engine, numbers
//! formatted from integers), so they need no Production confirmation and run on read-only
//! connections. Pages are bounded: long text cells are cut and a page stops taking rows
//! at a byte budget (see [`crate::data_sessions::BoundedPage`]).
//!
//! The filter and sort arrive as a structured AST ([`DataFilter`], [`SortSpec`]), never
//! as SQL: columns are checked against the object's real columns and quoted, values are
//! bound parameters (see [`dnt_core::sql::filter`]).
//!
//! The one exception is a raw `WHERE` filter ([`DataFilter::raw_where`]): the user's own
//! condition text, checked with [`dnt_core::sql::build::check_raw_where`] under the
//! data session's dialect and run (page and count alike) inside a read-only
//! transaction that is always rolled back (see [`crate::data_sessions::DataRequest`]).
//!
//! # Extra page columns
//!
//! A page of a table the data view may edit ([`dnt_query::edit::editable`]) also reads
//! what an edit needs to find and guard its rows, before `*`, and returns it apart from
//! `columns` / `rows`: PostgreSQL `xmin::text` ([`TableDataPage::row_versions`]) and key
//! columns a `SELECT *` does not return, such as MySQL `INVISIBLE` columns
//! ([`TableDataPage::hidden_key`]).

use super::browse::table_detail_impl;
use super::transfer::path_error;
use super::workspace::write_atomic;
use super::{blocking, guard, require_cap, require_sql};
use crate::data_sessions::{BoundedPage, DataRequest, RawRead};
use crate::edit_plans::size_text;
pub use crate::edit_plans::{
    ApplyOutcome, ApplyReport, CellPayload, EditPreview, LoadedCell, StmtApply,
};
use crate::file_picks::FileAccess;
use crate::session_manager::config_error;
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ColumnDetail, ObjectKind, ObjectRef, TableDetail};
use dnt_core::server::{Cap, SqlKind};
use dnt_core::sql::build::{
    insert_sql, raw_where_clause, update_sql, BuildError, ColumnTarget, RowGuard, RowKey, TableRef,
};
use dnt_core::sql::filter::{
    build_where, pg_unordered, WhereClause, MAX_CONDITIONS, MAX_IN_VALUES, MAX_SORT_KEYS,
    MAX_VALUE_BYTES,
};
pub use dnt_core::sql::filter::{DataFilter, FilterCond, FilterMode, FilterOp, SortDir, SortSpec};
use dnt_core::sql::ident::{count_where_sql, qualified_name, quote_ident, select_sql};
use dnt_core::sql::Dialect;
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
pub use dnt_query::edit::EditSet;
use dnt_query::edit::{
    atomicity, column_target, editable, insertable, normalise, updatable, NotEditable, RowRef,
};
pub use dnt_query::edit::{Atomicity, ProtectionKind};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tauri::State;
use uuid::Uuid;

/// Largest page the data view may ask for.
pub const MAX_PAGE_SIZE: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TableDataPage {
    pub columns: Vec<ColumnMeta>,
    /// At most `limit` rows, starting at `offset` (fewer when `size_capped`).
    pub rows: Vec<Vec<Value>>,
    /// Whether a row follows this page (the next page starts at `offset + rows.len()`).
    pub has_more: bool,
    /// Text / JSON cells cut to 64 KB.
    pub truncated_cells: u32,
    /// Where those cells are: `[row, column]` indexes into `rows`.
    #[specta(type = Vec<[f64; 2]>)]
    pub truncated: Vec<(u32, u32)>,
    /// The page reached its byte budget: rows were left for the next page.
    pub size_capped: bool,
    /// Without an explicit sort: the primary key columns the page is ordered by (stable
    /// paging); empty when there is no primary key or a sort was given.
    pub key_order: Vec<String>,
    /// PostgreSQL tables the data view may edit: each row's `xmin`, in row order (the
    /// row version an edit is guarded with). Null otherwise.
    pub row_versions: Option<Vec<String>>,
    /// Key columns a `SELECT *` does not return (MySQL `INVISIBLE`), read explicitly;
    /// not part of `columns`.
    pub hidden_key: Vec<ColumnMeta>,
    /// Their values, one entry per row of `rows` (empty without `hidden_key`).
    pub hidden_key_values: Vec<Vec<Value>>,
    #[specta(type = f64)]
    pub elapsed_ms: u64,
}

/// Whether the data view may edit an object, and how (`data_editable`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct EditableInfo {
    pub editable: bool,
    /// Why not, when not editable: `read_only_connection`, `not_a_table`, `no_key` or
    /// `unsupported:<detail>` (machine-readable; the UI words it).
    pub reason: Option<String>,
    /// The columns that identify a row: set whenever the table has a usable key, even
    /// when it is not editable (e.g. a read-only connection), so complete cells can be
    /// fetched by key (`data_cell_fetch`); empty otherwise.
    pub key: Vec<String>,
    /// Key columns a `SELECT *` does not return (MySQL `INVISIBLE`); a page returns them
    /// apart (`TableDataPage::hidden_key`).
    pub hidden_key: Vec<String>,
    /// Columns an UPDATE may set (empty when not editable).
    pub editable_columns: Vec<String>,
    pub atomic: Atomicity,
    pub protection: ProtectionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TableRowCount {
    /// Exact `COUNT(*)`.
    #[specta(type = f64)]
    pub rows: u64,
    #[specta(type = f64)]
    pub elapsed_ms: u64,
}

/// The object must be a table, view or materialized view, with a schema on PostgreSQL
/// (and none on MySQL).
pub fn check_object(kind: SqlKind, obj: &ObjectRef) -> Result<(), DbError> {
    let allowed = match (kind, obj.kind) {
        (SqlKind::MySql, ObjectKind::Table | ObjectKind::View) => true,
        (
            SqlKind::Postgres,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView,
        ) => true,
        (SqlKind::MySql | SqlKind::Postgres, _) => false,
    };
    if !allowed {
        return Err(config_error(
            "kind",
            "only tables and views have data to show",
        ));
    }
    if obj.database.is_empty() {
        return Err(config_error("database", "a database is required"));
    }
    match (kind, &obj.schema) {
        (SqlKind::Postgres, None) => {
            Err(config_error("schema", "a schema is required on PostgreSQL"))
        }
        (SqlKind::MySql, Some(_)) => Err(config_error(
            "schema",
            "MySQL has no schemas inside a database",
        )),
        (SqlKind::MySql, None) | (SqlKind::Postgres, Some(_)) => Ok(()),
    }
}

pub fn check_limit(limit: u32) -> Result<(), DbError> {
    if (1..=MAX_PAGE_SIZE).contains(&limit) {
        Ok(())
    } else {
        Err(config_error(
            "limit",
            &format!("the page size must be between 1 and {MAX_PAGE_SIZE}"),
        ))
    }
}

/// Whether a column of type `db_type` cannot be sorted on: PostgreSQL types without a
/// default ordering (`json`, `xml`, the geometric types, and arrays of them). MySQL sorts
/// every type.
pub fn unsortable(kind: SqlKind, db_type: &str) -> bool {
    match kind {
        SqlKind::MySql => false,
        SqlKind::Postgres => pg_unordered(db_type),
    }
}

/// `order` checked against the object's columns (exact name, sortable type):
/// `(column, ascending)`.
pub fn resolve_order(
    kind: SqlKind,
    columns: &[ColumnDetail],
    order: Option<SortSpec>,
) -> Result<Option<(String, bool)>, DbError> {
    let Some(order) = order else {
        return Ok(None);
    };
    let Some(column) = columns.iter().find(|c| c.name == order.column) else {
        return Err(config_error(
            "order",
            &format!("\"{}\" is not a column of this object", order.column),
        ));
    };
    if unsortable(kind, &column.db_type) {
        return Err(config_error(
            "order",
            &format!(
                "Can't sort by \"{}\": values of type {} have no order",
                column.name, column.db_type
            ),
        ));
    }
    Ok(Some((order.column, order.dir == SortDir::Asc)))
}

/// The primary key columns to order by when no sort is given (none when the object has
/// no plain-column primary key).
pub fn key_order(detail: &TableDetail) -> Vec<String> {
    match &detail.primary_key {
        Some(pk)
            if !pk.columns.is_empty()
                && pk
                    .columns
                    .iter()
                    .all(|c| detail.columns.iter().any(|d| &d.name == c)) =>
        {
            pk.columns.clone()
        }
        _ => Vec::new(),
    }
}

/// `ORDER BY` columns with their direction (`true`: ascending).
pub type OrderBy = Vec<(String, bool)>;

/// The `ORDER BY` of a page and its implicit key order (`TableDataPage::key_order`): the
/// requested sort keys in order (a column named twice counts once, first wins) followed
/// by the primary key columns as a tie-breaker (stable paging over equal values), or,
/// without a sort, the primary key alone.
pub fn page_order(
    kind: SqlKind,
    detail: &TableDetail,
    sort: &[SortSpec],
) -> Result<(OrderBy, Vec<String>), DbError> {
    if sort.len() > MAX_SORT_KEYS {
        return Err(config_error(
            "order",
            &format!("sort by at most {MAX_SORT_KEYS} columns"),
        ));
    }
    let keys = key_order(detail);
    if sort.is_empty() {
        return Ok((keys.iter().map(|k| (k.clone(), true)).collect(), keys));
    }
    let mut by: OrderBy = Vec::with_capacity(sort.len() + keys.len());
    for spec in sort {
        if let Some((column, asc)) = resolve_order(kind, &detail.columns, Some(spec.clone()))? {
            if !by.iter().any(|(c, _)| *c == column) {
                by.push((column, asc));
            }
        }
    }
    for k in keys {
        if !by.iter().any(|(c, _)| *c == k) {
            by.push((k, true));
        }
    }
    Ok((by, Vec::new()))
}

/// Whether `filter` has any structured condition.
fn filtered(filter: Option<&DataFilter>) -> bool {
    filter.is_some_and(|f| !f.conditions.is_empty())
}

/// The raw `WHERE` text of `filter`, if it is in raw mode.
fn raw_text(filter: Option<&DataFilter>) -> Option<String> {
    filter.and_then(|f| f.raw_where.clone())
}

/// A raw `WHERE` the checker refused: a `config` error on the filter.
fn raw_error(e: BuildError) -> DbError {
    let message = match e {
        BuildError::Invalid { reason, .. } => reason,
        other => other.to_string(),
    };
    config_error("filter", &message)
}

/// ` WHERE (<raw>\n)` for `dialect` (see [`raw_where_clause`]).
fn raw_clause(raw: &str, dialect: Dialect) -> Result<String, DbError> {
    Ok(raw_where_clause(raw, dialect).map_err(raw_error)?.sql)
}

/// What a data read of an object runs: the relation, `ORDER BY`, implicit key order and
/// `WHERE` (structured with its parameters, or raw), all checked against the object's
/// columns.
pub(crate) struct DataQuery {
    pub kind: SqlKind,
    pub relation: String,
    pub order: OrderBy,
    pub keys: Vec<String>,
    /// The structured `WHERE` (empty in raw mode).
    pub filter: WhereClause,
    /// Raw mode: the user's `WHERE` text, checked when the read is built on its session.
    pub raw_where: Option<String>,
    /// The object's table detail, when it could be read.
    pub detail: Option<TableDetail>,
}

impl DataQuery {
    /// The read of `extra` columns then `*`, paged with `(limit, offset)` or whole.
    pub fn request(
        &self,
        extra: &[String],
        page: Option<(u64, u64)>,
        max_rows: usize,
    ) -> DataRequest {
        let Some(raw) = self.raw_where.clone() else {
            let order: Vec<(&str, bool)> = self
                .order
                .iter()
                .map(|(c, asc)| (c.as_str(), *asc))
                .collect();
            return DataRequest {
                sql: select_sql(
                    self.kind,
                    &self.relation,
                    extra,
                    &self.filter.sql,
                    &order,
                    page,
                ),
                params: self.filter.params.clone(),
                max_rows,
                ..DataRequest::default()
            };
        };
        let (kind, relation, extra, order) = (
            self.kind,
            self.relation.clone(),
            extra.to_vec(),
            self.order.clone(),
        );
        let build = move |dialect: Dialect| {
            let order: Vec<(&str, bool)> =
                order.iter().map(|(c, asc)| (c.as_str(), *asc)).collect();
            let where_sql = raw_clause(&raw, dialect)?;
            Ok(select_sql(
                kind, &relation, &extra, &where_sql, &order, page,
            ))
        };
        DataRequest {
            raw: Some(RawRead(Arc::new(build))),
            max_rows,
            ..DataRequest::default()
        }
    }
}

/// What a page reads besides `*` (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageExtras {
    /// PostgreSQL `xmin::text`, first.
    pub row_version: bool,
    /// Hidden key columns, after it.
    pub hidden_key: Vec<String>,
}

impl PageExtras {
    /// For a page of `detail`: the row version only when the data view may edit it; the
    /// hidden key columns whenever the table has a key (a read-only connection still
    /// fetches complete cells by key, `data_cell_fetch`).
    pub fn of(detail: &TableDetail, kind: SqlKind, read_only: bool) -> PageExtras {
        match editable(detail, kind, false) {
            Ok(e) => PageExtras {
                row_version: match kind {
                    SqlKind::MySql => false,
                    SqlKind::Postgres => !read_only,
                },
                hidden_key: e.hidden_key,
            },
            Err(_) => PageExtras::default(),
        }
    }

    /// How many columns come before `*`.
    pub fn width(&self) -> usize {
        usize::from(self.row_version) + self.hidden_key.len()
    }

    /// The select-list expressions, in order.
    pub fn columns(&self, kind: SqlKind) -> Vec<String> {
        let version = self.row_version.then(|| "xmin::text".to_string());
        version
            .into_iter()
            .chain(self.hidden_key.iter().map(|c| quote_ident(kind, c)))
            .collect()
    }
}

/// A fetched page split into its parts: `limit` rows at most, the extra columns taken
/// off the front of `columns`, every row and the cut-cell coordinates.
pub fn split_page(
    page: BoundedPage,
    extras: &PageExtras,
    limit: u32,
) -> Result<TableDataPage, DbError> {
    let limit = limit as usize;
    let n_ver = usize::from(extras.row_version);
    let n_extra = extras.width();
    let short = || DbError::Internal {
        message: "the page has fewer columns than it selected".into(),
    };
    let mut columns = page.columns;
    if columns.len() < n_extra {
        return Err(short());
    }
    let rest = columns.split_off(n_extra);
    let hidden_key = columns.split_off(n_ver);
    let has_more = page.rows.len() > limit || page.capped;
    let mut row_versions = extras.row_version.then(Vec::new);
    let mut hidden_key_values = Vec::new();
    let mut rows = Vec::with_capacity(page.rows.len().min(limit));
    for mut row in page.rows.into_iter().take(limit) {
        if row.len() < n_extra {
            return Err(short());
        }
        let cells = row.split_off(n_extra);
        let hidden = row.split_off(n_ver);
        if let Some(versions) = row_versions.as_mut() {
            match row.pop() {
                Some(Value::Text(v)) => versions.push(v),
                other => {
                    return Err(DbError::Internal {
                        message: format!("unexpected row version {other:?}"),
                    })
                }
            }
        }
        if !extras.hidden_key.is_empty() {
            hidden_key_values.push(hidden);
        }
        rows.push(cells);
    }
    let shift = u32::try_from(n_extra).unwrap_or(u32::MAX);
    let truncated: Vec<(u32, u32)> = page
        .truncated
        .into_iter()
        .filter(|&(r, c)| (r as usize) < limit && c >= shift)
        .map(|(r, c)| (r, c - shift))
        .collect();
    Ok(TableDataPage {
        columns: rest,
        rows,
        has_more,
        truncated_cells: u32::try_from(truncated.len()).unwrap_or(u32::MAX),
        truncated,
        size_capped: page.capped,
        key_order: Vec::new(),
        row_versions,
        hidden_key,
        hidden_key_values,
        elapsed_ms: 0,
    })
}

/// Refuses an oversized sort or filter at once, before the object's columns are read
/// (the same bounds [`build_where`] / [`page_order`] apply, checked without them). The
/// IPC payload itself is already deserialized by then: Tauri gives no hook before that.
pub fn check_request_size(sort: &[SortSpec], filter: Option<&DataFilter>) -> Result<(), DbError> {
    if sort.len() > MAX_SORT_KEYS {
        return Err(config_error(
            "order",
            &format!("sort by at most {MAX_SORT_KEYS} columns"),
        ));
    }
    let Some(filter) = filter else {
        return Ok(());
    };
    if let Some(raw) = &filter.raw_where {
        if !filter.conditions.is_empty() {
            return Err(config_error(
                "filter",
                "a raw WHERE filter takes no other conditions",
            ));
        }
        if raw.len() > MAX_VALUE_BYTES {
            return Err(config_error(
                "filter",
                &format!("the WHERE text is too long (at most {MAX_VALUE_BYTES} bytes)"),
            ));
        }
    }
    if filter.conditions.len() > MAX_CONDITIONS {
        return Err(config_error(
            "filter",
            &format!("a filter has at most {MAX_CONDITIONS} conditions"),
        ));
    }
    for c in &filter.conditions {
        if c.values.len() > MAX_IN_VALUES {
            return Err(config_error(
                "filter",
                &format!("a condition has at most {MAX_IN_VALUES} values"),
            ));
        }
        if c.column.len() > MAX_VALUE_BYTES || c.values.iter().any(|v| v.len() > MAX_VALUE_BYTES) {
            return Err(config_error(
                "filter",
                &format!("a filter value is too long (at most {MAX_VALUE_BYTES} bytes)"),
            ));
        }
    }
    Ok(())
}

/// Builds the [`DataQuery`] of `obj`. The object's columns (the tree's cached table
/// detail) are required for a sort or a structured filter; without either, a failed
/// detail read only loses the stable key order (and the extra page columns).
pub(crate) async fn data_query(
    state: &AppState,
    id: Uuid,
    obj: &ObjectRef,
    sort: &[SortSpec],
    filter: Option<&DataFilter>,
) -> Result<DataQuery, DbError> {
    check_request_size(sort, filter)?;
    let kind = state.repo.get(id)?.kind.require_sql()?;
    check_object(kind, obj)?;
    let raw_where = raw_text(filter);
    let structured = if raw_where.is_some() { None } else { filter };
    // The detail is cached by the tree's Info view (a metadata call, short).
    let detail = if !sort.is_empty() || filtered(structured) {
        Some(table_detail_impl(state, id, obj.clone()).await?)
    } else {
        // No stable order without it; the SELECT reports a real problem itself.
        table_detail_impl(state, id, obj.clone()).await.ok()
    };
    let (order, keys, filter) = match &detail {
        Some(detail) => {
            let (order, keys) = page_order(kind, detail, sort)?;
            (order, keys, build_where(kind, &detail.columns, structured)?)
        }
        None => (Vec::new(), Vec::new(), WhereClause::default()),
    };
    Ok(DataQuery {
        kind,
        relation: relation(kind, obj),
        order,
        keys,
        filter,
        raw_where,
        detail,
    })
}

/// An offset as JS sends it: a negative, fractional or non-finite one is clamped.
pub fn clamp_offset(offset: f64) -> u64 {
    if offset.is_finite() && offset > 0.0 {
        offset.trunc() as u64
    } else {
        0
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn relation(kind: SqlKind, obj: &ObjectRef) -> String {
    qualified_name(kind, &obj.database, obj.schema.as_deref(), &obj.name)
}

/// A page of `obj`: `sort` keys in order (empty: the primary key), rows matching
/// `filter` (none: all rows).
#[allow(clippy::too_many_arguments)]
pub async fn table_data_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    limit: u32,
    offset: u64,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
    request_key: String,
) -> Result<TableDataPage, DbError> {
    require_sql(state, id)?;
    check_limit(limit)?;
    let query = data_query(state, id, &obj, &sort, filter.as_ref()).await?;
    let read_only = state.repo.get(id)?.read_only;
    let extras = query
        .detail
        .as_ref()
        .map(|d| PageExtras::of(d, query.kind, read_only))
        .unwrap_or_default();
    let fetch = u64::from(limit) + 1;
    let request = query.request(
        &extras.columns(query.kind),
        Some((fetch, offset)),
        fetch as usize,
    );
    let started = Instant::now();
    let page = state
        .sessions
        .data_read_request(id, Some(&obj.database), &request_key, &request)
        .await?;
    let mut page = split_page(page, &extras, limit)?;
    page.key_order = query.keys;
    page.elapsed_ms = elapsed_ms(started);
    Ok(page)
}

/// The count from the first cell of a `COUNT(*)` result.
fn count_value(v: Option<&Value>) -> Result<u64, DbError> {
    let n = match v {
        Some(Value::Int(n)) => u64::try_from(*n).ok(),
        Some(Value::UInt(n)) => Some(*n),
        Some(Value::Decimal(s) | Value::Text(s)) => s.parse().ok(),
        _ => None,
    };
    n.ok_or_else(|| DbError::Internal {
        message: "the server returned no row count".into(),
    })
}

/// The exact count of the rows of `obj` matching `filter` (none: all rows).
pub async fn table_row_count_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    filter: Option<DataFilter>,
    request_key: String,
) -> Result<TableRowCount, DbError> {
    let kind = require_sql(state, id)?;
    check_request_size(&[], filter.as_ref())?;
    check_object(kind, &obj)?;
    let rel = relation(kind, &obj);
    let request = match raw_text(filter.as_ref()) {
        Some(raw) => {
            let build =
                move |dialect: Dialect| Ok(count_where_sql(&rel, &raw_clause(&raw, dialect)?));
            DataRequest {
                raw: Some(RawRead(Arc::new(build))),
                max_rows: 1,
                ..DataRequest::default()
            }
        }
        None => {
            let clause = if filtered(filter.as_ref()) {
                let detail = table_detail_impl(state, id, obj.clone()).await?;
                build_where(kind, &detail.columns, filter.as_ref())?
            } else {
                WhereClause::default()
            };
            DataRequest {
                sql: count_where_sql(&rel, &clause.sql),
                params: clause.params,
                max_rows: 1,
                ..DataRequest::default()
            }
        }
    };
    let started = Instant::now();
    let read = state
        .sessions
        .data_read_request(id, Some(&obj.database), &request_key, &request)
        .await?;
    let rows = count_value(read.rows.first().and_then(|r| r.first()))?;
    Ok(TableRowCount {
        rows,
        elapsed_ms: elapsed_ms(started),
    })
}

pub async fn table_data_cancel_impl(
    state: &AppState,
    id: Uuid,
    request_key: String,
) -> Result<bool, DbError> {
    Ok(state.sessions.data_cancel(id, &request_key))
}

/// The machine-readable reason of [`EditableInfo::reason`].
fn reason_code(n: &NotEditable) -> String {
    match n {
        NotEditable::ReadOnlyConnection => "read_only_connection".into(),
        NotEditable::NotATable => "not_a_table".into(),
        NotEditable::NoKey => "no_key".into(),
        NotEditable::Unsupported { reason } => format!("unsupported:{reason}"),
    }
}

/// Whether the data view may edit `detail` on a connection of `kind`.
pub fn editable_info(detail: &TableDetail, kind: SqlKind, read_only: bool) -> EditableInfo {
    match editable(detail, kind, read_only) {
        Ok(e) => EditableInfo {
            editable: true,
            reason: None,
            key: e.key,
            hidden_key: e.hidden_key,
            editable_columns: e.editable_columns,
            atomic: e.atomic,
            protection: e.protection_kind,
        },
        Err(n) => {
            // The key still identifies rows for reads (`data_cell_fetch`), e.g. on a
            // read-only connection.
            let (key, hidden_key) = match editable(detail, kind, false) {
                Ok(e) => (e.key, e.hidden_key),
                Err(_) => (Vec::new(), Vec::new()),
            };
            EditableInfo {
                editable: false,
                reason: Some(reason_code(&n)),
                key,
                hidden_key,
                editable_columns: Vec::new(),
                atomic: atomicity(detail, kind),
                protection: match kind {
                    SqlKind::Postgres => ProtectionKind::RowVersion,
                    SqlKind::MySql => ProtectionKind::Snapshot,
                },
            }
        }
    }
}

pub async fn data_editable_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
) -> Result<EditableInfo, DbError> {
    let kind = require_cap(state, id, Cap::DataEdit)?;
    let cfg = state.repo.get(id)?;
    check_object(kind, &obj)?;
    let detail = table_detail_impl(state, id, obj).await?;
    Ok(editable_info(&detail, kind, cfg.read_only))
}

pub async fn data_preview_impl(
    state: &AppState,
    id: Uuid,
    set: EditSet,
) -> Result<EditPreview, DbError> {
    require_cap(state, id, Cap::DataEdit)?;
    state.sessions.edit_preview(id, set).await
}

pub async fn data_apply_impl(
    state: &AppState,
    plan_id: Uuid,
    confirmed: bool,
) -> Result<ApplyReport, DbError> {
    state.sessions.edit_apply(plan_id, confirmed).await
}

pub async fn data_cell_fetch_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    key: Vec<(String, Value)>,
    column: String,
) -> Result<LoadedCell, DbError> {
    require_cap(state, id, Cap::DataEdit)?;
    state.sessions.cell_fetch(id, obj, key, column).await
}

/// `path` must have been picked in the backend's dialog for `access` (see
/// [`crate::file_picks`]).
fn admitted(state: &AppState, access: FileAccess, path: &str) -> Result<(), DbError> {
    if path.trim().is_empty() {
        return Err(config_error("path", "No file was chosen."));
    }
    state.file_picks.check(access, path)
}

/// The bytes of `path`, at most `cap` (`config`, field `path`, beyond).
fn read_capped(path: &str, cap: usize) -> Result<Vec<u8>, DbError> {
    let file = std::fs::File::open(Path::new(path)).map_err(|e| path_error("read", path, &e))?;
    let mut bytes = Vec::new();
    // One byte past the limit tells "too large" apart from "exactly at the limit".
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| path_error("read", path, &e))?;
    if bytes.len() > cap {
        return Err(config_error(
            "path",
            &format!("The file is larger than {}.", size_text(cap)),
        ));
    }
    Ok(bytes)
}

pub async fn data_cell_load_file_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    column: String,
    path: String,
) -> Result<LoadedCell, DbError> {
    admitted(state, FileAccess::CellValueRead, &path)?;
    require_cap(state, id, Cap::DataEdit)?;
    let cap = state.sessions.cell_fetch_cap;
    let bytes = blocking(move || read_capped(&path, cap)).await?;
    state.sessions.cell_load(id, obj, column, bytes).await
}

pub async fn data_cell_save_file_impl(
    state: &AppState,
    token: Uuid,
    path: String,
) -> Result<(), DbError> {
    admitted(state, FileAccess::CellValueWrite, &path)?;
    let bytes = state.sessions.loaded_bytes(token)?;
    blocking(move || {
        write_atomic(Path::new(&path), &bytes).map_err(|e| path_error("write", &path, &e))
    })
    .await
}

/// The complete value of one cell (`column` of the row of `obj` whose key is `key`, as
/// `data_editable` reports the key), at most 64 MB, and a token that stands for it in
/// `data_preview` (as a `loaded` cell input, or in `RowRef.loaded` for the conflict
/// check) for as long as a plan lives. `config` errors: field `column:<name>` (bad
/// column or key, or a value over the limit), `row` (the row is gone), `edit`.
#[tauri::command]
#[specta::specta]
pub async fn data_cell_fetch(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    key: Vec<(String, Value)>,
    column: String,
) -> Result<LoadedCell, AppError> {
    guard(data_cell_fetch_impl(&state, id, obj, key, column)).await
}

/// A value for `column` of `obj` read from `path` (picked through `file_pick_open` with
/// kind `cell_value`; `config` error, field `path`, else), with its token.
#[tauri::command]
#[specta::specta]
pub async fn data_cell_load_file(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    column: String,
    path: String,
) -> Result<LoadedCell, AppError> {
    guard(data_cell_load_file_impl(&state, id, obj, column, path)).await
}

/// Writes loaded value `token` to `path` (picked through `file_pick_save` with kind
/// `cell_value`; `config` error, field `path`, else): binary data as is, other values
/// as UTF-8 text.
#[tauri::command]
#[specta::specta]
pub async fn data_cell_save_file(
    state: State<'_, AppState>,
    token: Uuid,
    path: String,
) -> Result<(), AppError> {
    guard(data_cell_save_file_impl(&state, token, path)).await
}

/// Builds and stores the plan of the staged changes `set` (see the `edit_plans`
/// module): the exact statements `data_apply` will send, their verdict,
/// atomicity and conflict checks. A set that cannot be planned is a `config` error with
/// field `edit`, `one_at_a_time` (the table is not atomic: one change at a time) or
/// `column:<name>`.
#[tauri::command]
#[specta::specta]
pub async fn data_preview(
    state: State<'_, AppState>,
    id: Uuid,
    set: EditSet,
) -> Result<EditPreview, AppError> {
    guard(data_preview_impl(&state, id, set)).await
}

/// Runs the plan `plan_id` once (`confirmed`: the user confirmed a Production plan).
/// `plan_stale`: preview again. Once anything was sent, the outcome is in the report.
#[tauri::command]
#[specta::specta]
pub async fn data_apply(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirmed: bool,
) -> Result<ApplyReport, AppError> {
    guard(data_apply_impl(&state, plan_id, confirmed)).await
}

/// Whether the data view may edit `obj` (a table / view of the data view), and how:
/// its key, the columns an UPDATE may set, atomicity and row-guard kind.
#[tauri::command]
#[specta::specta]
pub async fn data_editable(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
) -> Result<EditableInfo, AppError> {
    guard(data_editable_impl(&state, id, obj)).await
}

/// A page of a table / view. `sort`: keys in order (empty: the primary key);
/// `filter`: a structured filter (columns checked, values bound; `config` error with
/// field `filter` / `order` for a bad one).
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn table_data(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    limit: u32,
    offset: f64,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
    request_key: String,
) -> Result<TableDataPage, AppError> {
    let offset = clamp_offset(offset);
    guard(table_data_impl(
        &state,
        id,
        obj,
        limit,
        offset,
        sort,
        filter,
        request_key,
    ))
    .await
}

/// Exact `COUNT(*)` of the rows matching `filter` (null: all rows).
#[tauri::command]
#[specta::specta]
pub async fn table_row_count(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    filter: Option<DataFilter>,
    request_key: String,
) -> Result<TableRowCount, AppError> {
    guard(table_row_count_impl(&state, id, obj, filter, request_key)).await
}

/// Cancels the `table_data` / `table_row_count` request running under `request_key`;
/// whether one was running (its call then fails with `cancelled`).
#[tauri::command]
#[specta::specta]
pub async fn table_data_cancel(
    state: State<'_, AppState>,
    id: Uuid,
    request_key: String,
) -> Result<bool, AppError> {
    guard(table_data_cancel_impl(&state, id, request_key)).await
}

/// What `data_copy_sql` writes for each row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CopySqlFormat {
    Insert,
    Update,
}

/// Why `data_copy_sql` refuses a row holding a value the page cut short.
pub const COPY_TRUNCATED: &str = "Some cells are truncated; open them first or use Export.";

/// `INSERT` / `UPDATE` statements (one `INSERT` for all rows, one `UPDATE` per row, keyed
/// and unguarded: [`RowGuard::KeyOnly`]) that recreate `rows` of the table `detail`
/// describes. `metas` give the columns' result types. A value the page cut short is
/// refused ([`COPY_TRUNCATED`]), never written truncated.
pub fn copy_sql(
    detail: &TableDetail,
    metas: &[ColumnMeta],
    dialect: Dialect,
    rows: &[RowRef],
    format: CopySqlFormat,
) -> Result<String, DbError> {
    let kind = match dialect {
        Dialect::MySql(_) => SqlKind::MySql,
        Dialect::Postgres => SqlKind::Postgres,
    };
    if rows.is_empty() {
        return Err(config_error("copy", "There are no rows to copy."));
    }
    let table = TableRef {
        kind,
        database: detail.object.database.clone(),
        schema: detail.object.schema.clone(),
        name: detail.object.name.clone(),
    };
    let target = |cd: &ColumnDetail| {
        let logical = metas
            .iter()
            .find(|m| m.name == cd.name)
            .map_or(LogicalType::Other, |m| m.logical);
        column_target(cd, logical)
    };
    let cell = |cd: &ColumnDetail, v: &Value| -> Result<FullValue, DbError> {
        if v.may_be_page_truncated() || !v.is_complete() {
            return Err(config_error("copy", COPY_TRUNCATED));
        }
        let full = FullValue::try_from_value(v.clone())
            .map_err(|_| config_error("copy", COPY_TRUNCATED))?;
        normalise(full, &target(cd), dialect)
            .map_err(|reason| config_error("copy", &format!("{}: {reason}", cd.name)))
    };
    // The grid's rows keep the key columns apart from the other cells (`RowRef::key`).
    let original = |row: &RowRef, name: &str| {
        row.original
            .iter()
            .chain(&row.key)
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let build = |e: BuildError| config_error("copy", &e.to_string());
    match format {
        CopySqlFormat::Insert => {
            let cols: Vec<&ColumnDetail> = detail
                .columns
                .iter()
                .filter(|c| insertable(c) && original(&rows[0], &c.name).is_some())
                .collect();
            let mut values = Vec::with_capacity(rows.len());
            for row in rows {
                let mut line = Vec::with_capacity(cols.len());
                for cd in &cols {
                    let v = original(row, &cd.name).ok_or_else(|| {
                        config_error("copy", &format!("{}: the row has no value", cd.name))
                    })?;
                    line.push(cell(cd, &v)?);
                }
                values.push(line);
            }
            let targets: Vec<ColumnTarget> = cols.iter().map(|c| target(c)).collect();
            insert_sql(&table, &targets, &values, &[], dialect).map_err(build)
        }
        CopySqlFormat::Update => {
            let ed = editable(detail, kind, false)
                .map_err(|_| config_error("copy", "The table has no key to find its rows by."))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let mut key = Vec::with_capacity(ed.key.len());
                for name in &ed.key {
                    let missing =
                        || config_error("copy", &format!("{name}: the row's key is missing"));
                    let cd = detail
                        .columns
                        .iter()
                        .find(|c| &c.name == name)
                        .ok_or_else(missing)?;
                    let v = row
                        .key
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v)
                        .ok_or_else(missing)?;
                    key.push((target(cd), cell(cd, v)?));
                }
                let key = RowKey::new(key).map_err(build)?;
                let mut set = Vec::new();
                for cd in &detail.columns {
                    if ed.key.contains(&cd.name) || !updatable(cd) {
                        continue;
                    }
                    if let Some(v) = original(row, &cd.name) {
                        set.push((target(cd), cell(cd, &v)?));
                    }
                }
                out.push(
                    update_sql(&table, &key, &set, &RowGuard::KeyOnly, dialect).map_err(build)?,
                );
            }
            Ok(out.join("\n"))
        }
    }
}

pub async fn data_copy_sql_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    rows: Vec<RowRef>,
    format: CopySqlFormat,
) -> Result<String, DbError> {
    require_sql(state, id)?;
    let (detail, metas, dialect) = state.sessions.copy_context(id, &obj).await?;
    copy_sql(&detail, &metas, dialect, &rows, format)
}

/// `INSERT` or `UPDATE` statements that recreate `rows` (as the grid holds them) of table
/// `obj`, to copy to the clipboard; nothing is run. A row with a cell the page cut short
/// is a `config` error, field `copy`.
#[tauri::command]
#[specta::specta]
pub async fn data_copy_sql(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    rows: Vec<RowRef>,
    format: CopySqlFormat,
) -> Result<String, AppError> {
    guard(data_copy_sql_impl(&state, id, obj, rows, format)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::driver::RowSink;
    use dnt_core::meta::{IndexDetail, TableOptions};

    fn obj(schema: Option<&str>, kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            database: "shop".into(),
            schema: schema.map(Into::into),
            name: "orders".into(),
            kind,
        }
    }

    fn typed(name: &str, db_type: &str) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 1,
            db_type: db_type.into(),
            nullable: true,
            default: None,
            auto_increment: false,
            identity: None,
            generated: None,
            invisible: false,
            collation: None,
            comment: None,
            ..Default::default()
        }
    }

    fn col(name: &str) -> ColumnDetail {
        typed(name, "int")
    }

    fn field(e: DbError) -> Option<String> {
        match e {
            DbError::Config { field, .. } => field,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oversized_requests_are_refused_before_any_read() {
        let cond = |values: Vec<String>| FilterCond {
            column: "c".into(),
            op: FilterOp::In,
            values,
        };
        let filter = |conditions| DataFilter {
            mode: FilterMode::And,
            conditions,
            raw_where: None,
        };
        let ok = filter(vec![cond(vec!["1".into()]); MAX_CONDITIONS]);
        assert!(check_request_size(&[], Some(&ok)).is_ok());
        assert!(check_request_size(&[], None).is_ok());
        for bad in [
            filter(vec![cond(vec!["1".into()]); MAX_CONDITIONS + 1]),
            filter(vec![cond(vec!["1".into(); MAX_IN_VALUES + 1])]),
            filter(vec![cond(vec!["x".repeat(MAX_VALUE_BYTES + 1)])]),
        ] {
            assert_eq!(
                field(check_request_size(&[], Some(&bad)).unwrap_err()).as_deref(),
                Some("filter")
            );
        }
        // Raw mode: the text is bounded and takes no conditions.
        let raw = |text: String, conditions| DataFilter {
            raw_where: Some(text),
            ..filter(conditions)
        };
        assert!(check_request_size(&[], Some(&raw("id > 0".into(), vec![]))).is_ok());
        for bad in [
            raw("x".repeat(MAX_VALUE_BYTES + 1), vec![]),
            raw("id > 0".into(), vec![cond(vec!["1".into()])]),
        ] {
            assert_eq!(
                field(check_request_size(&[], Some(&bad)).unwrap_err()).as_deref(),
                Some("filter")
            );
        }
        let sort = vec![
            SortSpec {
                column: "a".into(),
                dir: SortDir::Asc
            };
            MAX_SORT_KEYS + 1
        ];
        assert_eq!(
            field(check_request_size(&sort, None).unwrap_err()).as_deref(),
            Some("order")
        );
    }

    #[test]
    fn objects_need_data_and_the_right_scope() {
        assert!(check_object(SqlKind::MySql, &obj(None, ObjectKind::Table)).is_ok());
        assert!(check_object(SqlKind::MySql, &obj(None, ObjectKind::View)).is_ok());
        assert!(check_object(
            SqlKind::Postgres,
            &obj(Some("public"), ObjectKind::MaterializedView)
        )
        .is_ok());
        let e = check_object(SqlKind::MySql, &obj(None, ObjectKind::Function)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("kind"));
        let e = check_object(SqlKind::Postgres, &obj(None, ObjectKind::Table)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("schema"));
        let e = check_object(SqlKind::MySql, &obj(Some("x"), ObjectKind::Table)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("schema"));
    }

    #[test]
    fn object_kinds_with_data_differ_per_engine() {
        assert!(check_object(SqlKind::Postgres, &obj(Some("public"), ObjectKind::Table)).is_ok());
        assert!(check_object(SqlKind::Postgres, &obj(Some("public"), ObjectKind::View)).is_ok());
        let e = check_object(SqlKind::MySql, &obj(None, ObjectKind::MaterializedView)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("kind"));
        let e = check_object(
            SqlKind::Postgres,
            &obj(Some("public"), ObjectKind::Function),
        )
        .unwrap_err();
        assert_eq!(field(e).as_deref(), Some("kind"));
    }

    #[test]
    fn page_size_is_bounded() {
        assert!(check_limit(1).is_ok());
        assert!(check_limit(MAX_PAGE_SIZE).is_ok());
        assert_eq!(field(check_limit(0).unwrap_err()).as_deref(), Some("limit"));
        assert_eq!(
            field(check_limit(MAX_PAGE_SIZE + 1).unwrap_err()).as_deref(),
            Some("limit")
        );
    }

    #[test]
    fn order_must_name_a_column_exactly() {
        let cols = [col("id"), col("we`ird\"col")];
        assert_eq!(resolve_order(SqlKind::MySql, &cols, None).unwrap(), None);
        let spec = |c: &str, dir| SortSpec {
            column: c.into(),
            dir,
        };
        assert_eq!(
            resolve_order(
                SqlKind::MySql,
                &cols,
                Some(spec("we`ird\"col", SortDir::Desc))
            )
            .unwrap(),
            Some(("we`ird\"col".to_string(), false))
        );
        for bad in ["ID", "id; DROP TABLE orders", "1", ""] {
            let e =
                resolve_order(SqlKind::MySql, &cols, Some(spec(bad, SortDir::Asc))).unwrap_err();
            assert_eq!(field(e).as_deref(), Some("order"), "{bad}");
        }
    }

    #[test]
    fn postgres_types_without_order_are_refused_kindly() {
        for t in [
            "json",
            "JSON",
            "xml",
            "point",
            "polygon",
            "json[]",
            "pg_catalog.json",
        ] {
            assert!(unsortable(SqlKind::Postgres, t), "{t}");
            assert!(!unsortable(SqlKind::MySql, t), "{t}");
        }
        for t in [
            "jsonb",
            "text",
            "integer",
            "timestamp with time zone",
            "int[]",
        ] {
            assert!(!unsortable(SqlKind::Postgres, t), "{t}");
        }
        let cols = [typed("doc", "json")];
        let e = resolve_order(
            SqlKind::Postgres,
            &cols,
            Some(SortSpec {
                column: "doc".into(),
                dir: SortDir::Asc,
            }),
        )
        .unwrap_err();
        match e {
            DbError::Config { field, message } => {
                assert_eq!(field.as_deref(), Some("order"));
                assert_eq!(
                    message,
                    "Can't sort by \"doc\": values of type json have no order"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    fn detail(pk: Option<Vec<&str>>) -> TableDetail {
        TableDetail {
            object: obj(None, ObjectKind::Table),
            columns: vec![col("a"), col("b")],
            primary_key: pk.map(|c| IndexDetail {
                name: "PRIMARY".into(),
                unique: true,
                primary: true,
                partial: false,
                columns: c.into_iter().map(Into::into).collect(),
                expression: None,
                method: None,
                definition: None,
                ..Default::default()
            }),
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            options: TableOptions {
                engine: None,
                collation: None,
                tablespace: None,
                partitioned_by: None,
                system_versioned: false,
                ..Default::default()
            },
            row_estimate: None,
            size_bytes: None,
            comment: None,
        }
    }

    #[test]
    fn primary_key_gives_the_default_order() {
        assert_eq!(key_order(&detail(Some(vec!["b", "a"]))), ["b", "a"]);
        assert!(key_order(&detail(None)).is_empty());
        assert!(key_order(&detail(Some(vec![]))).is_empty());
        assert!(key_order(&detail(Some(vec!["gone"]))).is_empty());
    }

    #[test]
    fn an_explicit_sort_gets_the_primary_key_as_tie_breaker() {
        let spec = |c: &str, dir| {
            vec![SortSpec {
                column: c.into(),
                dir,
            }]
        };
        let d = detail(Some(vec!["a", "b"]));
        let owned = |v: &[(&str, bool)]| -> Vec<(String, bool)> {
            v.iter().map(|(c, asc)| (c.to_string(), *asc)).collect()
        };
        assert_eq!(
            page_order(SqlKind::MySql, &d, &spec("b", SortDir::Desc)).unwrap(),
            (owned(&[("b", false), ("a", true)]), vec![])
        );
        assert_eq!(
            page_order(SqlKind::MySql, &d, &[]).unwrap(),
            (
                owned(&[("a", true), ("b", true)]),
                vec!["a".to_string(), "b".to_string()]
            )
        );
        let no_pk = detail(None);
        assert_eq!(
            page_order(SqlKind::MySql, &no_pk, &spec("a", SortDir::Asc)).unwrap(),
            (owned(&[("a", true)]), vec![])
        );
        assert_eq!(
            page_order(SqlKind::MySql, &no_pk, &[]).unwrap(),
            (vec![], vec![])
        );
    }

    #[test]
    fn several_sort_keys_keep_their_order_and_dedupe() {
        let d = detail(Some(vec!["a"]));
        let key = |c: &str, dir| SortSpec {
            column: c.into(),
            dir,
        };
        let sort = [
            key("b", SortDir::Desc),
            key("a", SortDir::Desc),
            key("b", SortDir::Asc),
        ];
        assert_eq!(
            page_order(SqlKind::MySql, &d, &sort).unwrap().0,
            vec![("b".to_string(), false), ("a".to_string(), false)]
        );
        // An unknown key anywhere is refused.
        let bad = [key("a", SortDir::Asc), key("nope", SortDir::Asc)];
        let e = page_order(SqlKind::MySql, &d, &bad).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("order"));
        let many = vec![key("a", SortDir::Asc); MAX_SORT_KEYS + 1];
        assert!(page_order(SqlKind::MySql, &d, &many).is_err());
    }

    #[test]
    fn js_offsets_are_clamped() {
        assert_eq!(clamp_offset(0.0), 0);
        assert_eq!(clamp_offset(250.0), 250);
        assert_eq!(clamp_offset(10.9), 10);
        assert_eq!(clamp_offset(-5.0), 0);
        assert_eq!(clamp_offset(f64::NAN), 0);
        assert_eq!(clamp_offset(f64::INFINITY), 0);
        assert_eq!(clamp_offset(f64::NEG_INFINITY), 0);
        assert_eq!(clamp_offset(9_007_199_254_740_992.0), 9_007_199_254_740_992);
    }

    #[test]
    fn count_values_of_every_engine_parse() {
        assert_eq!(count_value(Some(&Value::Int(12))).unwrap(), 12);
        assert_eq!(count_value(Some(&Value::UInt(7))).unwrap(), 7);
        assert_eq!(count_value(Some(&Value::Decimal("42".into()))).unwrap(), 42);
        assert!(count_value(None).is_err());
        assert!(count_value(Some(&Value::Int(-1))).is_err());
    }

    fn meta(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            db_type: "text".into(),
            logical: dnt_core::value::LogicalType::Text,
            nullable: None,
            origin_table: None,
            origin_column: None,
        }
    }

    #[test]
    fn extra_columns_are_split_off_the_page() {
        let mut page = BoundedPage::new(3, 1000);
        page.result_start(&[meta("xmin"), meta("hid"), meta("a"), meta("b")]);
        page.rows(vec![
            vec![
                Value::Text("701".into()),
                Value::Int(1),
                Value::Text("long".into()),
                Value::Null,
            ],
            vec![
                Value::Text("702".into()),
                Value::Int(2),
                Value::Null,
                Value::Text("cut!".into()),
            ],
            // The look-ahead row.
            vec![
                Value::Text("703".into()),
                Value::Int(3),
                Value::Text("more".into()),
                Value::Null,
            ],
        ]);
        let extras = PageExtras {
            row_version: true,
            hidden_key: vec!["hid".into()],
        };
        assert_eq!(extras.columns(SqlKind::Postgres), ["xmin::text", "\"hid\""]);
        let p = split_page(page, &extras, 2).unwrap();
        let names: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(p.rows.len(), 2);
        assert_eq!(p.rows[1], [Value::Null, Value::Text("cut".into())]);
        assert!(p.has_more);
        assert_eq!(p.row_versions, Some(vec!["701".into(), "702".into()]));
        assert_eq!(p.hidden_key, [meta("hid")]);
        assert_eq!(
            p.hidden_key_values,
            [vec![Value::Int(1)], vec![Value::Int(2)]]
        );
        // Coordinates in `rows`, the look-ahead row's cut cell left out.
        assert_eq!(p.truncated, [(0, 0), (1, 1)]);
        assert_eq!(p.truncated_cells, 2);
        assert_eq!(
            serde_json::to_value(&p).unwrap()["truncated"],
            serde_json::json!([[0, 0], [1, 1]])
        );

        // No extras: the page as read.
        let mut page = BoundedPage::new(100, 1000);
        page.result_start(&[meta("a")]);
        page.rows(vec![vec![Value::Int(1)]]);
        let p = split_page(page, &PageExtras::default(), 10).unwrap();
        assert_eq!(p.rows, [vec![Value::Int(1)]]);
        assert_eq!(p.row_versions, None);
        assert!(p.hidden_key.is_empty() && p.hidden_key_values.is_empty());
        assert!(!p.has_more);
        assert_eq!(
            PageExtras {
                row_version: false,
                hidden_key: vec!["my`id".into()]
            }
            .columns(SqlKind::MySql),
            ["`my``id`"]
        );
    }

    #[test]
    fn editability_reasons_are_machine_readable() {
        let d = detail(Some(vec!["a"]));
        let mut d = d;
        d.columns[0].nullable = false;
        let ok = editable_info(&d, SqlKind::Postgres, false);
        assert!(ok.editable);
        assert_eq!(ok.key, ["a"]);
        assert_eq!(ok.editable_columns, ["a", "b"]);
        assert_eq!(ok.protection, ProtectionKind::RowVersion);
        assert_eq!(ok.reason, None);

        let ro = editable_info(&d, SqlKind::MySql, true);
        assert!(!ro.editable);
        assert_eq!(ro.reason.as_deref(), Some("read_only_connection"));
        // The key is still reported (complete cells are fetched by it); nothing to set.
        assert_eq!(ro.key, ["a"]);
        assert!(ro.hidden_key.is_empty() && ro.editable_columns.is_empty());
        let mut hidden = d.clone();
        hidden.columns[0].invisible = true;
        let ro = editable_info(&hidden, SqlKind::MySql, true);
        assert_eq!(ro.hidden_key, ["a"]);
        // A read-only page still reads the hidden key, without row versions.
        assert_eq!(
            PageExtras::of(&hidden, SqlKind::MySql, true),
            PageExtras {
                row_version: false,
                hidden_key: vec!["a".into()]
            }
        );
        assert_eq!(
            PageExtras::of(&d, SqlKind::Postgres, true),
            PageExtras::default()
        );
        assert!(PageExtras::of(&d, SqlKind::Postgres, false).row_version);
        assert!(!PageExtras::of(&d, SqlKind::MySql, false).row_version);
        assert_eq!(ro.protection, ProtectionKind::Snapshot);

        let mut v = d.clone();
        v.object.kind = ObjectKind::View;
        assert_eq!(
            editable_info(&v, SqlKind::MySql, false).reason.as_deref(),
            Some("not_a_table")
        );
        assert_eq!(
            editable_info(&detail(None), SqlKind::MySql, false)
                .reason
                .as_deref(),
            Some("no_key")
        );
        assert_eq!(
            reason_code(&NotEditable::Unsupported {
                reason: "array key".into()
            }),
            "unsupported:array key"
        );
    }

    #[test]
    fn raw_where_is_checked_with_the_sessions_dialect() {
        let q = DataQuery {
            kind: SqlKind::MySql,
            relation: "`shop`.`orders`".into(),
            order: vec![("id".into(), true)],
            keys: vec!["id".into()],
            filter: WhereClause::default(),
            raw_where: Some("note = \"a;b\"".into()),
            detail: None,
        };
        let r = q.request(&["`hid`".into()], Some((11, 0)), 11);
        let build = r.raw.expect("a raw read").0;
        assert_eq!(
            build(Dialect::MYSQL).unwrap(),
            "SELECT `hid`, `shop`.`orders`.* FROM `shop`.`orders` WHERE (note = \"a;b\"\n) \
             ORDER BY `id` ASC LIMIT 11 OFFSET 0"
        );
        let e = DataQuery {
            raw_where: Some("1; DROP TABLE t".into()),
            ..q
        }
        .request(&[], None, 1)
        .raw
        .unwrap()
        .0(Dialect::Postgres)
        .unwrap_err();
        assert_eq!(field(e).as_deref(), Some("filter"));
    }

    #[test]
    fn sort_dir_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&SortDir::Desc).unwrap(), "\"desc\"");
    }

    fn copy_detail() -> TableDetail {
        let mut d = detail(Some(vec!["id"]));
        d.columns = vec![
            ColumnDetail {
                nullable: false,
                ..typed("id", "bigint")
            },
            typed("name", "varchar(50)"),
        ];
        d
    }

    fn copy_metas() -> Vec<ColumnMeta> {
        use dnt_core::value::LogicalType;
        let m = |n: &str, t: &str, l| ColumnMeta {
            name: n.into(),
            db_type: t.into(),
            logical: l,
            nullable: None,
            origin_table: None,
            origin_column: None,
        };
        vec![
            m("id", "bigint", LogicalType::Integer),
            m("name", "varchar", LogicalType::Text),
        ]
    }

    fn copy_row(id: i64, name: Value) -> dnt_query::edit::RowRef {
        dnt_query::edit::RowRef {
            key: vec![("id".into(), Value::Int(id))],
            // As the grid builds it: the key apart from the other cells.
            original: vec![("name".into(), name)],
            xmin: None,
            loaded: vec![],
        }
    }

    #[test]
    fn data_copy_sql_insert_and_update() {
        let (d, m) = (copy_detail(), copy_metas());
        let rows = [
            copy_row(1, Value::Text("it's".into())),
            copy_row(2, Value::Null),
        ];
        let sql = copy_sql(&d, &m, Dialect::MYSQL, &rows, CopySqlFormat::Insert).unwrap();
        assert!(sql.starts_with("INSERT INTO `shop`.`orders` (`id`, `name`) VALUES"));
        assert!(sql.contains("(1, _utf8mb4'it\\'s'), (2, NULL)"), "{sql}");
        let sql = copy_sql(&d, &m, Dialect::MYSQL, &rows, CopySqlFormat::Update).unwrap();
        assert!(
            sql.contains("UPDATE `shop`.`orders` SET `name` = _utf8mb4'it\\'s' WHERE `id` = 1"),
            "{sql}"
        );
        assert!(sql.contains("SET `name` = NULL WHERE `id` = 2"), "{sql}");
        assert_eq!(sql.matches("UPDATE").count(), 2);
    }

    #[test]
    fn data_copy_sql_refuses_truncated_cells() {
        let (d, m) = (copy_detail(), copy_metas());
        let cut = Value::Text("x".repeat(dnt_core::value::PAGE_CELL_CAP));
        for f in [CopySqlFormat::Insert, CopySqlFormat::Update] {
            let e = copy_sql(&d, &m, Dialect::MYSQL, &[copy_row(1, cut.clone())], f).unwrap_err();
            match e {
                DbError::Config { message, .. } => assert_eq!(
                    message,
                    "Some cells are truncated; open them first or use Export."
                ),
                other => panic!("{other:?}"),
            }
        }
        let bytes = Value::Bytes {
            len: 10,
            preview: vec![1, 2],
        };
        assert!(copy_sql(
            &d,
            &m,
            Dialect::MYSQL,
            &[copy_row(1, bytes)],
            CopySqlFormat::Insert
        )
        .is_err());
    }
}
