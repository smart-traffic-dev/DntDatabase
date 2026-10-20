//! Query editor commands: planning and transactions of query tabs (spec §4.8) and
//! autocomplete data (spec §7.3).
//!
//! `completion_scope` returns the SchemaCache of the database a query tab is on (loaded on
//! first use, one load per database at a time). `completion_objects` lists the tables and
//! views after `db.` (MySQL) / `schema.` (PostgreSQL) through the tree's browse cache.

use super::guard;
use crate::cache::{CompletionObject, SchemaCache};
use crate::query_sessions::{with_tx_state, PlanView, TabTxInfo, TxAction, TxMode};
use crate::run_events::{EventSink, RunEvent, RunOptions};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::sql::Dialect;
use dnt_core::tx::TxState;
use dnt_query::Target;
use std::sync::Arc;
use tauri::ipc::Channel;
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
) -> Result<TxState, AppError> {
    state.sessions.query_tx_reported(&tab_id, action).await
}

pub async fn query_run_impl(
    state: &AppState,
    plan_id: Uuid,
    confirmed: bool,
    confirm_names: Vec<String>,
    opts: RunOptions,
    emit: EventSink,
) -> Result<Uuid, DbError> {
    state
        .sessions
        .query_run_named(plan_id, confirmed, confirm_names, opts, emit)
        .await
}

pub async fn query_cancel_impl(state: &AppState, run_id: Uuid) -> Result<(), DbError> {
    state.sessions.query_cancel(run_id).await
}

pub async fn query_explain_impl(
    state: &AppState,
    tab_id: String,
    conn_id: Uuid,
    database: Option<String>,
    text: String,
    cursor: u32,
    analyze: bool,
) -> Result<PlanView, DbError> {
    state
        .sessions
        .query_explain(&tab_id, conn_id, database, &text, cursor, analyze)
        .await
}

pub async fn query_plan_statement_impl(
    state: &AppState,
    plan_id: Uuid,
    index: u32,
) -> Result<PlanView, DbError> {
    state.sessions.query_plan_statement(plan_id, index).await
}

pub async fn query_close_tab_impl(
    state: &AppState,
    tab_id: String,
    force: bool,
) -> Result<(), DbError> {
    state.sessions.query_close_tab(&tab_id, force).await
}

/// [`guard`] for the calls that open or move a tab's session: an error that dropped
/// the tab's transaction reports the tab's state after it in `AppError::tx_state`
/// (see [`with_tx_state`]).
async fn guard_tab<T>(
    fut: impl std::future::Future<Output = Result<T, DbError>>,
) -> Result<T, AppError> {
    guard(async { Ok(fut.await.map_err(with_tx_state)) })
        .await
        .and_then(|r| r)
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
/// is kept 2 minutes for `query_run`. A `connection_lost` that dropped the tab's
/// transaction (its SSH tunnel was replaced) carries `tx_state: "idle"`, here and in
/// `query_run`, `query_explain` and `query_plan_statement`.
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
    guard_tab(query_plan_impl(
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
/// tab runs. A failed statement reports the session's state after it in
/// `AppError.tx_state`.
#[tauri::command]
#[specta::specta]
pub async fn query_tx(
    state: State<'_, AppState>,
    tab_id: String,
    action: TxAction,
) -> Result<TxState, AppError> {
    guard(async { Ok(query_tx_impl(&state, tab_id, action).await) })
        .await
        .and_then(|r| r)
}

pub async fn query_tx_mode_impl(
    state: &AppState,
    tab_id: String,
    mode: TxMode,
) -> Result<TxState, DbError> {
    state.sessions.query_tx_mode(&tab_id, mode).await
}

pub async fn query_tx_info_impl(state: &AppState, tab_id: String) -> Result<TabTxInfo, DbError> {
    state.sessions.query_tx_info(&tab_id).await
}

/// Sets the tab's commit mode and returns its transaction state. `Manual`: before every
/// statement that needs a transaction while none is open, the run begins one; commit or
/// roll it back with `query_tx`. MySQL statements that would commit it are flagged
/// `implicit_commit` in the plan. A Manual transaction lost with the connection makes
/// `query_run` fail with `connection_lost` until `query_tx` acknowledges it. Errors:
/// `busy` while the tab runs, `transaction_open` when leaving Manual with a transaction
/// open.
#[tauri::command]
#[specta::specta]
pub async fn query_tx_mode(
    state: State<'_, AppState>,
    tab_id: String,
    mode: TxMode,
) -> Result<TxState, AppError> {
    guard(query_tx_mode_impl(&state, tab_id, mode)).await
}

/// The tab's transaction: state, start (`since_ms`, Unix epoch ms) and statements run
/// in it, for the idle-transaction warning (`get_idle_tx_warn_minutes`). `busy` while
/// the tab runs.
#[tauri::command]
#[specta::specta]
pub async fn query_tx_info(
    state: State<'_, AppState>,
    tab_id: String,
) -> Result<TabTxInfo, AppError> {
    guard(query_tx_info_impl(&state, tab_id)).await
}

/// Runs plan `plan_id` (from `query_plan`, `query_explain` or `query_plan_statement`)
/// and returns the run id at once; every result streams through `channel` (see
/// `RunEvent`), ending with `done`. The guard is applied again: `blocked` when any
/// statement is blocked, `confirmation_required` when any needs confirmation and
/// `confirmed` is false (the plan stays valid: ask, then call again with `confirmed`).
/// On Production, `confirm_names` must hold exactly the `confirm_name`s of the plan's
/// statements (case-sensitive), else `confirmation_required` too; pass `[]` when the
/// plan asks for none. `busy` while the tab runs; `plan_expired` for a plan already
/// run, expired or replaced.
#[tauri::command]
#[specta::specta]
pub async fn query_run(
    state: State<'_, AppState>,
    plan_id: Uuid,
    confirmed: bool,
    confirm_names: Vec<String>,
    opts: RunOptions,
    channel: Channel<RunEvent>,
) -> Result<Uuid, AppError> {
    // An undelivered event (the window is gone) stops the run: see `query_run`.
    let emit: EventSink = Arc::new(move |event| match channel.send(event) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = %e, "a run event was not delivered");
            false
        }
    });
    guard_tab(query_run_impl(
        &state,
        plan_id,
        confirmed,
        confirm_names,
        opts,
        emit,
    ))
    .await
}

/// Stops run `run_id` (emits `stopping{user}` first). Unknown or finished runs: no-op.
#[tauri::command]
#[specta::specta]
pub async fn query_cancel(state: State<'_, AppState>, run_id: Uuid) -> Result<(), AppError> {
    guard(query_cancel_impl(&state, run_id)).await
}

/// Plans an Explain (or Explain Analyze, `analyze`) of the statement at `cursor`;
/// run it with `query_run`, which streams one `explain` event. `config` (field
/// `explain`) for a DDL statement, or Analyze of a statement that is not a read;
/// `unsupported` where the server has no such explain (Analyze on MySQL 5.7).
#[tauri::command]
#[specta::specta]
pub async fn query_explain(
    state: State<'_, AppState>,
    tab_id: String,
    conn_id: Uuid,
    database: Option<String>,
    text: String,
    cursor: u32,
    analyze: bool,
) -> Result<PlanView, AppError> {
    guard_tab(query_explain_impl(
        &state, tab_id, conn_id, database, text, cursor, analyze,
    ))
    .await
}

/// Retry statement / Fetch all: plans statement `index` of plan `plan_id` again, its
/// stored text verbatim, judged with the current guard. Run it with `query_run`.
#[tauri::command]
#[specta::specta]
pub async fn query_plan_statement(
    state: State<'_, AppState>,
    plan_id: Uuid,
    index: u32,
) -> Result<PlanView, AppError> {
    guard_tab(query_plan_statement_impl(&state, plan_id, index)).await
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
