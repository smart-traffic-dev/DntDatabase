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

use super::browse::table_detail_impl;
use super::guard;
use crate::session_manager::config_error;
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ColumnDetail, ObjectKind, ObjectRef, TableDetail};
use dnt_core::server::DbKind;
use dnt_core::sql::filter::{
    build_where, pg_unordered, WhereClause, MAX_CONDITIONS, MAX_IN_VALUES, MAX_SORT_KEYS,
    MAX_VALUE_BYTES,
};
pub use dnt_core::sql::filter::{DataFilter, FilterCond, FilterMode, FilterOp, SortDir, SortSpec};
use dnt_core::sql::ident::{count_where_sql, qualified_name, select_sql};
use dnt_core::value::{ColumnMeta, Value};
use serde::{Deserialize, Serialize};
use specta::Type;
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
    /// The page reached its byte budget: rows were left for the next page.
    pub size_capped: bool,
    /// Without an explicit sort: the primary key columns the page is ordered by (stable
    /// paging); empty when there is no primary key or a sort was given.
    pub key_order: Vec<String>,
    #[specta(type = f64)]
    pub elapsed_ms: u64,
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
pub fn check_object(kind: DbKind, obj: &ObjectRef) -> Result<(), DbError> {
    let allowed = match kind {
        DbKind::MySql => matches!(obj.kind, ObjectKind::Table | ObjectKind::View),
        DbKind::Postgres => matches!(
            obj.kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
        ),
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
        (DbKind::Postgres, None) => {
            Err(config_error("schema", "a schema is required on PostgreSQL"))
        }
        (DbKind::MySql, Some(_)) => Err(config_error(
            "schema",
            "MySQL has no schemas inside a database",
        )),
        _ => Ok(()),
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
pub fn unsortable(kind: DbKind, db_type: &str) -> bool {
    kind == DbKind::Postgres && pg_unordered(db_type)
}

/// `order` checked against the object's columns (exact name, sortable type):
/// `(column, ascending)`.
pub fn resolve_order(
    kind: DbKind,
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
    kind: DbKind,
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

/// Whether `filter` has any condition.
fn filtered(filter: Option<&DataFilter>) -> bool {
    filter.is_some_and(|f| !f.conditions.is_empty())
}

/// What a data read of an object runs: the relation, `ORDER BY`, implicit key order and
/// `WHERE` (with its parameters), all checked against the object's columns.
pub(crate) struct DataQuery {
    pub kind: DbKind,
    pub relation: String,
    pub order: OrderBy,
    pub keys: Vec<String>,
    pub filter: WhereClause,
}

impl DataQuery {
    /// The `SELECT`, paged with `(limit, offset)` or whole.
    pub fn select(&self, page: Option<(u64, u64)>) -> String {
        let order: Vec<(&str, bool)> = self
            .order
            .iter()
            .map(|(c, asc)| (c.as_str(), *asc))
            .collect();
        select_sql(
            self.kind,
            &self.relation,
            &[],
            &self.filter.sql,
            &order,
            page,
        )
    }
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
/// detail) are required for a sort or a filter; without either, a failed detail read
/// only loses the stable key order.
pub(crate) async fn data_query(
    state: &AppState,
    id: Uuid,
    obj: &ObjectRef,
    sort: &[SortSpec],
    filter: Option<&DataFilter>,
) -> Result<DataQuery, DbError> {
    check_request_size(sort, filter)?;
    let kind = state.repo.get(id)?.kind;
    check_object(kind, obj)?;
    // The detail is cached by the tree's Info view (a metadata call, short).
    let (order, keys, filter) = if !sort.is_empty() || filtered(filter) {
        let detail = table_detail_impl(state, id, obj.clone()).await?;
        let (order, keys) = page_order(kind, &detail, sort)?;
        (order, keys, build_where(kind, &detail.columns, filter)?)
    } else {
        match table_detail_impl(state, id, obj.clone()).await {
            Ok(detail) => {
                let (order, keys) = page_order(kind, &detail, &[])?;
                (order, keys, WhereClause::default())
            }
            // No stable order then; the SELECT reports a real problem itself.
            Err(_) => (Vec::new(), Vec::new(), WhereClause::default()),
        }
    };
    Ok(DataQuery {
        kind,
        relation: relation(kind, obj),
        order,
        keys,
        filter,
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

fn relation(kind: DbKind, obj: &ObjectRef) -> String {
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
    check_limit(limit)?;
    let query = data_query(state, id, &obj, &sort, filter.as_ref()).await?;
    let fetch = u64::from(limit) + 1;
    let sql = query.select(Some((fetch, offset)));
    let started = Instant::now();
    let page = state
        .sessions
        .data_read_params(
            id,
            Some(&obj.database),
            &request_key,
            &sql,
            query.filter.params,
            fetch as usize,
        )
        .await?;
    let keys = query.keys;
    let mut rows = page.rows;
    let has_more = rows.len() > limit as usize || page.capped;
    rows.truncate(limit as usize);
    Ok(TableDataPage {
        columns: page.columns,
        rows,
        has_more,
        truncated_cells: page.truncated_cells,
        size_capped: page.capped,
        key_order: keys,
        elapsed_ms: elapsed_ms(started),
    })
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
    let kind = state.repo.get(id)?.kind;
    check_object(kind, &obj)?;
    let clause = if filtered(filter.as_ref()) {
        let detail = table_detail_impl(state, id, obj.clone()).await?;
        build_where(kind, &detail.columns, filter.as_ref())?
    } else {
        WhereClause::default()
    };
    let started = Instant::now();
    let read = state
        .sessions
        .data_read_params(
            id,
            Some(&obj.database),
            &request_key,
            &count_where_sql(&relation(kind, &obj), &clause.sql),
            clause.params,
            1,
        )
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

#[cfg(test)]
mod tests {
    use super::*;
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
            comment: None,
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
        assert!(check_object(DbKind::MySql, &obj(None, ObjectKind::Table)).is_ok());
        assert!(check_object(DbKind::MySql, &obj(None, ObjectKind::View)).is_ok());
        assert!(check_object(
            DbKind::Postgres,
            &obj(Some("public"), ObjectKind::MaterializedView)
        )
        .is_ok());
        let e = check_object(DbKind::MySql, &obj(None, ObjectKind::Function)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("kind"));
        let e = check_object(DbKind::Postgres, &obj(None, ObjectKind::Table)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("schema"));
        let e = check_object(DbKind::MySql, &obj(Some("x"), ObjectKind::Table)).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("schema"));
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
        assert_eq!(resolve_order(DbKind::MySql, &cols, None).unwrap(), None);
        let spec = |c: &str, dir| SortSpec {
            column: c.into(),
            dir,
        };
        assert_eq!(
            resolve_order(
                DbKind::MySql,
                &cols,
                Some(spec("we`ird\"col", SortDir::Desc))
            )
            .unwrap(),
            Some(("we`ird\"col".to_string(), false))
        );
        for bad in ["ID", "id; DROP TABLE orders", "1", ""] {
            let e = resolve_order(DbKind::MySql, &cols, Some(spec(bad, SortDir::Asc))).unwrap_err();
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
            assert!(unsortable(DbKind::Postgres, t), "{t}");
            assert!(!unsortable(DbKind::MySql, t), "{t}");
        }
        for t in [
            "jsonb",
            "text",
            "integer",
            "timestamp with time zone",
            "int[]",
        ] {
            assert!(!unsortable(DbKind::Postgres, t), "{t}");
        }
        let cols = [typed("doc", "json")];
        let e = resolve_order(
            DbKind::Postgres,
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
                columns: c.into_iter().map(Into::into).collect(),
                expression: None,
                method: None,
                definition: None,
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
            page_order(DbKind::MySql, &d, &spec("b", SortDir::Desc)).unwrap(),
            (owned(&[("b", false), ("a", true)]), vec![])
        );
        assert_eq!(
            page_order(DbKind::MySql, &d, &[]).unwrap(),
            (
                owned(&[("a", true), ("b", true)]),
                vec!["a".to_string(), "b".to_string()]
            )
        );
        let no_pk = detail(None);
        assert_eq!(
            page_order(DbKind::MySql, &no_pk, &spec("a", SortDir::Asc)).unwrap(),
            (owned(&[("a", true)]), vec![])
        );
        assert_eq!(
            page_order(DbKind::MySql, &no_pk, &[]).unwrap(),
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
            page_order(DbKind::MySql, &d, &sort).unwrap().0,
            vec![("b".to_string(), false), ("a".to_string(), false)]
        );
        // An unknown key anywhere is refused.
        let bad = [key("a", SortDir::Asc), key("nope", SortDir::Asc)];
        let e = page_order(DbKind::MySql, &d, &bad).unwrap_err();
        assert_eq!(field(e).as_deref(), Some("order"));
        let many = vec![key("a", SortDir::Asc); MAX_SORT_KEYS + 1];
        assert!(page_order(DbKind::MySql, &d, &many).is_err());
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

    #[test]
    fn sort_dir_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&SortDir::Desc).unwrap(), "\"desc\"");
    }
}
