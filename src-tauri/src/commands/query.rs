//! Query editor commands: planning and transactions of query tabs (spec §4.8) and
//! autocomplete data (spec §7.3).
//!
//! `completion_scope` returns the SchemaCache of the database a query tab is on (loaded on
//! first use, one load per database at a time). `completion_objects` lists the tables and
//! views after `db.` (MySQL) / `schema.` (PostgreSQL) through the tree's browse cache.

use super::guard;
use crate::cache::{CompletionObject, SchemaCache};
use crate::query_sessions::{PlanView, TxAction};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::sql::Dialect;
use dnt_core::tx::TxState;
use dnt_query::Target;
use tauri::State;
use uuid::Uuid;

pub async fn completion_scope_impl(
    state: &AppState,
    conn_id: Uuid,
    database: String,
) -> Result<SchemaCache, DbError> {
    state.sessions.schema_cache_for(conn_id, &database).await
}

pub async fn completion_objects_impl(
    state: &AppState,
    conn_id: Uuid,
    database: String,
    schema: Option<String>,
) -> Result<Vec<CompletionObject>, DbError> {
    state
        .sessions
        .completion_objects(conn_id, &database, schema.as_deref())
        .await
}

/// The SchemaCache of `database` for autocomplete. The whole scope comes back in one
/// payload: the UI must cache it per (tab connection, database) and fetch it again only
/// after a refresh or when the tab changes database, never per keystroke. Errors: an
/// empty `database` is a `config` error; a failed load is returned again (without a new
/// load) for 10 s.
#[tauri::command]
#[specta::specta]
pub async fn completion_scope(
    state: State<'_, AppState>,
    conn_id: Uuid,
    database: String,
) -> Result<SchemaCache, AppError> {
    guard(completion_scope_impl(&state, conn_id, database)).await
}

/// Tables and views after `db.` (MySQL: `schema` must be null) or `schema.` (PostgreSQL:
/// `schema` is required). Shares the object tree's cache.
#[tauri::command]
#[specta::specta]
pub async fn completion_objects(
    state: State<'_, AppState>,
    conn_id: Uuid,
    database: String,
    schema: Option<String>,
) -> Result<Vec<CompletionObject>, AppError> {
    guard(completion_objects_impl(&state, conn_id, database, schema)).await
}

pub async fn query_plan_impl(
    state: &AppState,
    tab_id: String,
    conn_id: Uuid,
    database: Option<String>,
    text: String,
    target: Target,
) -> Result<PlanView, DbError> {
    state
        .sessions
        .query_plan(&tab_id, conn_id, database, &text, target)
        .await
}

pub async fn query_tx_impl(
    state: &AppState,
    tab_id: String,
    action: TxAction,
) -> Result<TxState, DbError> {
    state.sessions.query_tx(&tab_id, action).await
}

pub async fn query_close_tab_impl(
    state: &AppState,
    tab_id: String,
    force: bool,
) -> Result<(), DbError> {
    state.sessions.query_close_tab(&tab_id, force).await
}

/// UTF-16 ranges (end-exclusive) of every statement of `text`, as `query_plan` would
/// split it with `Target::All`. Pure: no session needed.
pub fn statement_ranges_impl(text: &str, dialect: Dialect) -> Vec<(u32, u32)> {
    dnt_query::statement_ranges(text, dialect)
}

/// Plans what `target` selects in `text` for tab `tab_id` on `conn_id` / `database`
/// (null: the connect database) and returns every statement's guard verdict. Opens the
/// tab's session on first use and moves it when the connection or database changed.
/// Errors: `not_open`, `busy` (the tab is running), `transaction_open` (changing the
/// connection or database of a tab with an open transaction), connect errors. The plan
/// is kept 2 minutes for `query_run`.
#[tauri::command]
#[specta::specta]
pub async fn query_plan(
    state: State<'_, AppState>,
    tab_id: String,
    conn_id: Uuid,
    database: Option<String>,
    text: String,
    target: Target,
) -> Result<PlanView, AppError> {
    guard(query_plan_impl(
        &state, tab_id, conn_id, database, text, target,
    ))
    .await
}

/// Statement ranges for highlighting the statement under the cursor (debounced 150 ms).
#[tauri::command]
#[specta::specta]
pub async fn statement_ranges(text: String, dialect: Dialect) -> Vec<(u32, u32)> {
    statement_ranges_impl(&text, dialect)
}

/// Commits or rolls back the tab's transaction; returns the new state. `busy` while the
/// tab runs.
#[tauri::command]
#[specta::specta]
pub async fn query_tx(
    state: State<'_, AppState>,
    tab_id: String,
    action: TxAction,
) -> Result<TxState, AppError> {
    guard(query_tx_impl(&state, tab_id, action)).await
}

/// Closes the tab's session. With a transaction open and `force` false it returns
/// `transaction_open` and keeps the session (ask Commit / Rollback / Cancel, then call
/// again, or `query_tx` first); `force` closes anyway (the server rolls back).
#[tauri::command]
#[specta::specta]
pub async fn query_close_tab(
    state: State<'_, AppState>,
    tab_id: String,
    force: bool,
) -> Result<(), AppError> {
    guard(query_close_tab_impl(&state, tab_id, force)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_ranges_command() {
        let text = "SELECT 'Tiếng 😀';\r\nSELECT 2;\n-- tail";
        assert_eq!(
            statement_ranges_impl(text, Dialect::MYSQL),
            [(0, 17), (20, 28)]
        );
        assert_eq!(statement_ranges_impl("", Dialect::Postgres), []);
        let block = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;";
        assert_eq!(statement_ranges_impl(block, Dialect::MYSQL).len(), 1);
    }
}
