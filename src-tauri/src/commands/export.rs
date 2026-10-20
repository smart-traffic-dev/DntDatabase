//! Export of results to a file (CSV, JSON, Excel, SQL `INSERT`, Markdown; see
//! [`crate::export_format`]).
//!
//! - `export_rows`: the rows the UI already holds (the loaded rows), written as they are.
//! - `export_table`: every row of a table / view under the data view's filter and sort
//!   (the same AST as `table_data`, so the same checks), read again on a data session.
//! - `export_query`: every row of a query result, by reading its statement again on a
//!   data session. Only a statement the plan / guard classify as a read that needs no
//!   confirmation qualifies (see `SessionManager::export_statement`); other results
//!   export their loaded rows.
//!
//! Reading again streams straight to the file: no page cap, no row limit, no statement
//! timeout; progress goes through the channel, and `table_data_cancel` with the export's
//! request key stops it (the half-written file is removed).

use super::data::{data_query, DataFilter, SortSpec};
use super::{blocking, guard};
use crate::data_sessions::DataRequest;
use crate::export_format::{
    check_options, open_writer, write_rows, ExportOptions, ExportProgress, ExportSink,
    ExportSummary, Target,
};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::ObjectRef;
use dnt_core::server::DbKind;
use dnt_core::value::{ColumnMeta, Value};
use futures_util::future::{select, Either};
use std::sync::Arc;
use std::time::Instant;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Notify;
use uuid::Uuid;

/// Where progress goes (the command's channel; tests collect it).
pub type Progress = Box<dyn FnMut(u64) + Send>;

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

pub async fn export_rows_impl(
    kind: DbKind,
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    opts: ExportOptions,
    path: String,
) -> Result<ExportSummary, DbError> {
    let started = Instant::now();
    blocking(move || {
        let target = Target::new(&path)?;
        let (rows, cut_cells) = write_rows(&opts, kind, &target, &columns, &rows)?;
        tracing::info!(rows, format = ?opts.format, "loaded rows exported");
        Ok(ExportSummary {
            rows,
            path,
            cut_cells,
            elapsed_ms: elapsed_ms(started),
        })
    })
    .await
}

/// Runs `request` on a data session of `id` / `db` under `key`, streaming its first
/// result into a writer of `opts` at `path`.
#[allow(clippy::too_many_arguments)]
async fn stream_to_file(
    state: &AppState,
    id: Uuid,
    db: Option<&str>,
    key: &str,
    request: DataRequest,
    kind: DbKind,
    opts: ExportOptions,
    path: String,
    progress: Progress,
) -> Result<ExportSummary, DbError> {
    let started = Instant::now();
    let target = Target::new(&path)?;
    let writer = open_writer(&opts, kind, &target.part, &path)?;
    // A write error (disk full…) stops the statement through its key.
    let failed = Arc::new(Notify::new());
    let notify = Arc::clone(&failed);
    let mut sink = ExportSink::new(writer, progress, Box::new(move || notify.notify_one()));
    let outcome = {
        let exec = std::pin::pin!(state.sessions.data_exec(id, db, key, &request, &mut sink));
        let watch = std::pin::pin!(async {
            failed.notified().await;
            state.sessions.data_cancel(id, key);
            std::future::pending::<()>().await
        });
        match select(exec, watch).await {
            Either::Left((outcome, _)) => outcome,
            Either::Right(((), _)) => unreachable!("the watcher never completes"),
        }
    };
    let error = sink.take_error();
    if let Some(e) = error.or(outcome.err()) {
        // Close the file before removing it.
        drop(sink);
        target.discard();
        return Err(e);
    }
    let (rows, cut_cells) = blocking(move || {
        let done = sink.finish();
        match done {
            Ok(done) => {
                target.commit()?;
                Ok(done)
            }
            Err(e) => {
                target.discard();
                Err(e)
            }
        }
    })
    .await?;
    tracing::info!(rows, format = ?opts.format, "rows exported");
    Ok(ExportSummary {
        rows,
        path,
        cut_cells,
        elapsed_ms: elapsed_ms(started),
    })
}

/// Every row of `obj` matching `filter`, in `sort` order (empty: the primary key).
#[allow(clippy::too_many_arguments)]
pub async fn export_table_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
    opts: ExportOptions,
    path: String,
    request_key: String,
    progress: Progress,
) -> Result<ExportSummary, DbError> {
    check_options(&opts)?;
    let query = data_query(state, id, &obj, &sort, filter.as_ref()).await?;
    let request = DataRequest {
        sql: query.select(None),
        params: query.filter.params.clone(),
        max_rows: usize::MAX,
        full_bytes: true,
        no_timeout: true,
    };
    stream_to_file(
        state,
        id,
        Some(&obj.database),
        &request_key,
        request,
        query.kind,
        opts,
        path,
        progress,
    )
    .await
}

/// Every row of statement `index` of plan `plan_id` (a read), read again.
pub async fn export_query_impl(
    state: &AppState,
    plan_id: Uuid,
    index: u32,
    opts: ExportOptions,
    path: String,
    request_key: String,
    progress: Progress,
) -> Result<ExportSummary, DbError> {
    check_options(&opts)?;
    let (id, database, sql) = state.sessions.export_statement(plan_id, index)?;
    let kind = state.repo.get(id)?.kind;
    let request = DataRequest {
        sql,
        params: Vec::new(),
        max_rows: usize::MAX,
        full_bytes: true,
        no_timeout: true,
    };
    stream_to_file(
        state,
        id,
        database.as_deref(),
        &request_key,
        request,
        kind,
        opts,
        path,
        progress,
    )
    .await
}

/// Shows an exported file in the system file manager ("Open folder"). Only an existing
/// file is shown; nothing is opened or run.
#[tauri::command]
#[specta::specta]
pub async fn reveal_export(app: AppHandle, path: String) -> Result<(), AppError> {
    guard(async move {
        let file = std::path::Path::new(&path);
        if !file.is_file() {
            return Err(DbError::Config {
                field: Some("path".into()),
                message: "The exported file is no longer there.".into(),
            });
        }
        app.opener()
            .reveal_item_in_dir(file)
            .map_err(|e| DbError::Internal {
                message: format!("cannot open the folder: {e}"),
            })
    })
    .await
}

fn channel_progress(channel: Channel<ExportProgress>) -> Progress {
    Box::new(move |rows| {
        if let Err(e) = channel.send(ExportProgress { rows }) {
            tracing::debug!(error = %e, "an export progress event was not delivered");
        }
    })
}

/// Writes rows the UI holds (the loaded rows) to `path`. `kind`: the engine whose
/// quoting the SQL format uses.
#[tauri::command]
#[specta::specta]
pub async fn export_rows(
    kind: DbKind,
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    opts: ExportOptions,
    path: String,
) -> Result<ExportSummary, AppError> {
    guard(export_rows_impl(kind, columns, rows, opts, path)).await
}

/// Every row of a table / view under the data view's `filter` and `sort`, streamed to
/// `path`; progress through `channel`; `table_data_cancel(id, request_key)` stops it.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn export_table(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
    sort: Vec<SortSpec>,
    filter: Option<DataFilter>,
    opts: ExportOptions,
    path: String,
    request_key: String,
    channel: Channel<ExportProgress>,
) -> Result<ExportSummary, AppError> {
    guard(export_table_impl(
        &state,
        id,
        obj,
        sort,
        filter,
        opts,
        path,
        request_key,
        channel_progress(channel),
    ))
    .await
}

/// Every row of statement `index` of plan `plan_id`, read again (a read statement
/// only: `config` error with field `scope` otherwise), streamed to `path`; progress
/// through `channel`; `table_data_cancel(<the tab's connection>, request_key)` stops it.
#[tauri::command]
#[specta::specta]
pub async fn export_query(
    state: State<'_, AppState>,
    plan_id: Uuid,
    index: u32,
    opts: ExportOptions,
    path: String,
    request_key: String,
    channel: Channel<ExportProgress>,
) -> Result<ExportSummary, AppError> {
    guard(export_query_impl(
        &state,
        plan_id,
        index,
        opts,
        path,
        request_key,
        channel_progress(channel),
    ))
    .await
}
