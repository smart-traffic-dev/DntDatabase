//! Export of results to a file (CSV, JSON, Excel, SQL `INSERT`, Markdown; see
//! [`crate::export_format`]).
//!
//! - `export_rows`: the rows the UI already holds (the loaded rows), written as they are
//!   (a binary value held only as its preview is written as NULL / empty and counted).
//! - `export_table`: every row of a table / view under the data view's filter and sort
//!   (the same AST as `table_data`, so the same checks; no user SQL), read again on a
//!   data session.
//! - `export_query`: every row of a query result, by running its statement again ON THE
//!   TAB'S OWN SESSION, as Fetch all does (see `SessionManager::query_export`): the same
//!   `sql_mode`, variables, `search_path`, role, temp tables and transaction as the result
//!   the user saw. Only a statement the plan / guard classify as a read that needs no
//!   confirmation qualifies, only while the tab is idle; it is a run of the tab
//!   (`query_cancel(run_id)` stops it). Other results export their loaded rows.
//!
//! Reading again streams straight to the file: no page cap, no row limit, no statement
//! timeout; progress goes through the channel; a cancel removes the half-written file.

use super::data::{data_query, DataFilter, SortSpec};
use super::{blocking, guard};
use crate::data_sessions::DataRequest;
use crate::export_format::{
    check_options, open_writer, write_rows, ExportFormat, ExportOptions, ExportProgress,
    ExportSink, ExportSummary, Target, Written,
};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::ObjectRef;
use dnt_core::server::DbKind;
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use futures_util::future::{select, Either};
use std::sync::Arc;
use std::time::Instant;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Notify;
use uuid::Uuid;

/// Where progress goes (the command's channel; tests collect it).
pub type Progress = Box<dyn FnMut(u64) + Send>;

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn summary(w: Written, path: String, started: Instant) -> ExportSummary {
    ExportSummary {
        rows: w.rows,
        path,
        cut_cells: w.cut_cells,
        partial_binaries: w.partial_binaries,
        elapsed_ms: elapsed_ms(started),
    }
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
        let written = write_rows(&opts, kind, &target, &columns, &rows)?;
        tracing::info!(rows = written.rows, format = ?opts.format, "loaded rows exported");
        Ok(summary(written, path, started))
    })
    .await
}

/// A result streaming into its file: the temp file, the sink writing it, and the signal
/// its first write error raises (which must stop the statement).
struct Stream {
    started: Instant,
    target: Target,
    sink: ExportSink,
    failed: Arc<Notify>,
    format: ExportFormat,
}

impl Stream {
    fn open(
        opts: &ExportOptions,
        kind: DbKind,
        path: &str,
        progress: Progress,
    ) -> Result<Stream, DbError> {
        let started = Instant::now();
        let target = Target::new(path)?;
        let writer = open_writer(opts, kind, &target.part, path)?;
        let failed = Arc::new(Notify::new());
        let notify = Arc::clone(&failed);
        let sink = ExportSink::new(writer, progress, Box::new(move || notify.notify_one()));
        Ok(Stream {
            started,
            target,
            sink,
            failed,
            format: opts.format,
        })
    }

    /// Completes the file after the statement ended with `outcome` (a write error of the
    /// sink wins); any failure removes the temp file.
    async fn close(
        mut self,
        outcome: Result<(), DbError>,
        path: String,
    ) -> Result<ExportSummary, DbError> {
        let error = self.sink.take_error();
        let Stream {
            started,
            target,
            sink,
            format,
            ..
        } = self;
        if let Some(e) = error.or(outcome.err()) {
            // Close the file before removing it.
            drop(sink);
            target.discard();
            return Err(e);
        }
        let written = blocking(move || match sink.finish() {
            Ok(done) => {
                target.commit()?;
                Ok(done)
            }
            Err(e) => {
                target.discard();
                Err(e)
            }
        })
        .await?;
        tracing::info!(rows = written.rows, format = ?format, "rows exported");
        Ok(summary(written, path, started))
    }
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
    let mut s = Stream::open(&opts, kind, &path, progress)?;
    let outcome = {
        let exec = std::pin::pin!(state.sessions.data_exec(id, db, key, &request, &mut s.sink));
        // A write error (disk full…) stops the statement through its key.
        let failed = Arc::clone(&s.failed);
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
    s.close(outcome, path).await
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

/// [`export_query_impl`], a failure with the tab session's transaction state after it.
async fn export_query_reported(
    state: &AppState,
    plan_id: Uuid,
    index: u32,
    opts: ExportOptions,
    path: String,
    run_id: Uuid,
    progress: Progress,
) -> Result<ExportSummary, (DbError, Option<TxState>)> {
    let early = |e: DbError| (e, None);
    check_options(&opts).map_err(early)?;
    let kind = state.sessions.plan_kind(plan_id).map_err(early)?;
    let mut s = Stream::open(&opts, kind, &path, progress).map_err(early)?;
    let failed = Arc::clone(&s.failed);
    let (outcome, tx) = match state
        .sessions
        .query_export(plan_id, index, run_id, &mut s.sink, &failed)
        .await
    {
        Ok(()) => (Ok(()), None),
        Err((e, tx)) => (Err(e), tx),
    };
    s.close(outcome, path).await.map_err(|e| (e, tx))
}

/// Every row of statement `index` of plan `plan_id` (a read), run again on its tab's
/// session as run `run_id` (see the module docs).
pub async fn export_query_impl(
    state: &AppState,
    plan_id: Uuid,
    index: u32,
    opts: ExportOptions,
    path: String,
    run_id: Uuid,
    progress: Progress,
) -> Result<ExportSummary, DbError> {
    export_query_reported(state, plan_id, index, opts, path, run_id, progress)
        .await
        .map_err(|(e, _)| e)
}

/// `name` as a file name stem: characters Windows refuses (and control characters)
/// become `_`, at most 100 characters; `export` when nothing is left.
pub fn file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(100)
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        "export".into()
    } else {
        trimmed.into()
    }
}

/// Opens the save dialog (from the backend, so the UI never names a path itself) for a
/// `format` file called `base_name` by default, and approves what the user picked for
/// the export commands (for a while; see the `export_paths` module). `None`: the dialog
/// was cancelled.
#[tauri::command]
#[specta::specta]
pub async fn export_pick_path(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    base_name: String,
    format: ExportFormat,
) -> Result<Option<String>, AppError> {
    guard(async move {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let ext = format.extension();
        window
            .dialog()
            .file()
            .set_parent(&window)
            .set_file_name(format!("{}.{ext}", file_stem(&base_name)))
            .add_filter(format.filter_name(), &[ext])
            .save_file(move |picked| {
                let _ = tx.send(picked);
            });
        let picked = rx.await.map_err(|_| DbError::Internal {
            message: "the save dialog closed without an answer".into(),
        })?;
        let Some(picked) = picked else {
            return Ok(None);
        };
        let path = picked.into_path().map_err(|e| DbError::Internal {
            message: format!("the save dialog returned no file path: {e}"),
        })?;
        state.export_paths.approve(path).map(Some)
    })
    .await
}

/// Refuses a `path` the user did not pick through [`export_pick_path`].
fn picked(state: &AppState, path: &str) -> Result<(), AppError> {
    state.export_paths.check(path).map_err(AppError::from)
}

/// Shows an exported file in the system file manager ("Open folder"). Only a file an
/// export wrote, and that still exists, is shown; nothing is opened or run.
#[tauri::command]
#[specta::specta]
pub async fn reveal_export(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), AppError> {
    guard(async move {
        state.export_paths.check_written(&path)?;
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
    state: State<'_, AppState>,
    kind: DbKind,
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    opts: ExportOptions,
    path: String,
) -> Result<ExportSummary, AppError> {
    picked(&state, &path)?;
    let summary = guard(export_rows_impl(kind, columns, rows, opts, path)).await?;
    state.export_paths.written(&summary.path);
    Ok(summary)
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
    picked(&state, &path)?;
    let summary = guard(export_table_impl(
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
    .await?;
    state.export_paths.written(&summary.path);
    Ok(summary)
}

/// Every row of statement `index` of plan `plan_id`, run again on the tab's session as
/// run `run_id` (a fresh id; `query_cancel(run_id)` stops it) and streamed to `path`;
/// progress through `channel`. Refused (`config` error) while the tab runs (field `tab`)
/// and for anything but an allowed read (field `scope`). A failure carries the tab's
/// transaction state after it (`tx_state`).
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn export_query(
    state: State<'_, AppState>,
    plan_id: Uuid,
    index: u32,
    opts: ExportOptions,
    path: String,
    run_id: Uuid,
    channel: Channel<ExportProgress>,
) -> Result<ExportSummary, AppError> {
    picked(&state, &path)?;
    let reported = guard(async {
        Ok(export_query_reported(
            &state,
            plan_id,
            index,
            opts,
            path,
            run_id,
            channel_progress(channel),
        )
        .await)
    })
    .await?;
    let summary = reported.map_err(|(e, tx)| {
        let mut app = AppError::from(e);
        app.tx_state = tx.map(|s| s.as_str().to_string());
        app
    })?;
    state.export_paths.written(&summary.path);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::file_stem;

    #[test]
    fn file_stems_are_safe_names() {
        assert_eq!(file_stem("orders"), "orders");
        assert_eq!(file_stem("a/b\\c:d*?\"<>|"), "a_b_c_d______");
        assert_eq!(file_stem("..\\..\\evil"), ".._.._evil");
        assert_eq!(file_stem("x\u{0}y\n"), "x_y_");
        assert_eq!(file_stem("  "), "export");
        assert_eq!(file_stem("name."), "name");
        assert_eq!(file_stem(&"é".repeat(300)).chars().count(), 100);
    }
}
