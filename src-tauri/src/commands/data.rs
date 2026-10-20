//! The table data view: pages of a table / view / materialized view, and its exact row
//! count.
//!
//! # Where the statements run
//!
//! On the connection's metadata session for the object's database
//! ([`crate::session_manager::SessionManager::metadata_read`]), the session the tree's
//! introspection uses: never on a query tab's session, so the view neither waits for a
//! tab's run nor reads inside a tab's open transaction. The statements are plain `SELECT`s
//! built here (identifiers quoted per engine, numbers formatted from integers), so they
//! need no Production confirmation and run on read-only connections. Trade-off: while a
//! page loads, tree browsing on that connection waits for it (both share the metadata
//! link); the statement timeout (see `metadata_read`) bounds that wait.

use super::browse::table_detail_impl;
use super::guard;
use crate::session_manager::config_error;
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ColumnDetail, ObjectKind, ObjectRef};
use dnt_core::server::DbKind;
use dnt_core::sql::ident::{count_sql, qualified_name, select_page_sql};
use dnt_core::value::{ColumnMeta, Value};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::time::Instant;
use tauri::State;
use uuid::Uuid;

/// Largest page the data view may ask for.
pub const MAX_PAGE_SIZE: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

/// Server-side `ORDER BY` of one column (which must be a column of the object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SortSpec {
    pub column: String,
    pub dir: SortDir,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct TableDataPage {
    pub columns: Vec<ColumnMeta>,
    /// At most `limit` rows, starting at `offset`.
    pub rows: Vec<Vec<Value>>,
    /// Whether a row follows this page.
    pub has_more: bool,
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

/// `order` checked against the object's columns (exact name): `(column, ascending)`.
pub fn resolve_order(
    columns: &[ColumnDetail],
    order: Option<SortSpec>,
) -> Result<Option<(String, bool)>, DbError> {
    let Some(order) = order else {
        return Ok(None);
    };
    if !columns.iter().any(|c| c.name == order.column) {
        return Err(config_error(
            "order",
            &format!("\"{}\" is not a column of this object", order.column),
        ));
    }
    Ok(Some((order.column, order.dir == SortDir::Asc)))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn relation(kind: DbKind, obj: &ObjectRef) -> String {
    qualified_name(kind, &obj.database, obj.schema.as_deref(), &obj.name)
}

pub async fn table_data_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    limit: u32,
    offset: u64,
    order: Option<SortSpec>,
) -> Result<TableDataPage, DbError> {
    let kind = state.repo.get(id)?.kind;
    check_object(kind, &obj)?;
    check_limit(limit)?;
    let order = match order {
        Some(order) => {
            // Cached by the tree's Info view; also tells us the object exists.
            let detail = table_detail_impl(state, id, obj.clone()).await?;
            resolve_order(&detail.columns, Some(order))?
        }
        None => None,
    };
    let fetch = u64::from(limit) + 1;
    let sql = select_page_sql(
        kind,
        &relation(kind, &obj),
        order.as_ref().map(|(c, asc)| (c.as_str(), *asc)),
        fetch,
        offset,
    );
    let started = Instant::now();
    let read = state
        .sessions
        .metadata_read(id, Some(&obj.database), sql, fetch as usize)
        .await?;
    let mut rows = read.rows;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(TableDataPage {
        columns: read.columns,
        rows,
        has_more,
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

pub async fn table_row_count_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
) -> Result<TableRowCount, DbError> {
    let kind = state.repo.get(id)?.kind;
    check_object(kind, &obj)?;
    let started = Instant::now();
    let read = state
        .sessions
        .metadata_read(id, Some(&obj.database), count_sql(&relation(kind, &obj)), 1)
        .await?;
    let rows = count_value(read.rows.first().and_then(|r| r.first()))?;
    Ok(TableRowCount {
        rows,
        elapsed_ms: elapsed_ms(started),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn table_data(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    limit: u32,
    offset: f64,
    order: Option<SortSpec>,
) -> Result<TableDataPage, AppError> {
    // JS numbers: a negative or fractional offset is clamped to a whole one.
    let offset = if offset.is_finite() && offset > 0.0 {
        offset.trunc() as u64
    } else {
        0
    };
    guard(table_data_impl(&state, id, obj, limit, offset, order)).await
}

#[tauri::command]
#[specta::specta]
pub async fn table_row_count(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
) -> Result<TableRowCount, AppError> {
    guard(table_row_count_impl(&state, id, obj)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(schema: Option<&str>, kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            database: "shop".into(),
            schema: schema.map(Into::into),
            name: "orders".into(),
            kind,
        }
    }

    fn col(name: &str) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal: 1,
            db_type: "int".into(),
            nullable: true,
            default: None,
            auto_increment: false,
            identity: None,
            generated: None,
            invisible: false,
            comment: None,
        }
    }

    fn field(e: DbError) -> Option<String> {
        match e {
            DbError::Config { field, .. } => field,
            other => panic!("{other:?}"),
        }
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
        assert_eq!(resolve_order(&cols, None).unwrap(), None);
        let spec = |c: &str, dir| SortSpec {
            column: c.into(),
            dir,
        };
        assert_eq!(
            resolve_order(&cols, Some(spec("we`ird\"col", SortDir::Desc))).unwrap(),
            Some(("we`ird\"col".to_string(), false))
        );
        for bad in ["ID", "id; DROP TABLE orders", "1", ""] {
            let e = resolve_order(&cols, Some(spec(bad, SortDir::Asc))).unwrap_err();
            assert_eq!(field(e).as_deref(), Some("order"), "{bad}");
        }
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
