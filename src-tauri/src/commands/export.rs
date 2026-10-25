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
//!
//! The three streaming exports are tracked jobs (P5 R3): they keep this execution path,
//! session and dialog, and also appear in the Jobs panel with their progress and
//! outcome; the panel's Stop takes each one's own cancel path (`table_data_cancel`,
//! `query_cancel(run_id)`, `export_query_result_cancel`).

use super::data::{data_query, DataFilter, SortSpec};
use super::{blocking, guard};
use crate::data_sessions::DataRequest;
use crate::export_format::{
    check_options, open_writer, write_rows, ExportFormat, ExportOptions, ExportProgress,
    ExportSink, ExportSummary, Target, Written,
};
use crate::jobs::{JobDone, JobKind, JobSpec, TrackedJob};
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

/// Title of a tracked export job: `Export <name> to <file name>`.
pub fn export_job_title(name: &str, path: &str) -> String {
    let file = std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |f| f.to_string_lossy().into_owned());
    format!("Export {name} to {file}")
}

/// Registers a streaming export as a tracked job (R3): shown in the Jobs panel, not
/// re-runnable, no run slot. Its Stop runs `cancel` (the export's own cancel path).
fn track_export(
    state: &AppState,
    connection_id: Option<Uuid>,
    name: &str,
    path: &str,
    cancel: Box<dyn Fn() + Send + Sync>,
) -> Arc<TrackedJob> {
    let spec = JobSpec {
        kind: JobKind::Export,
        connection_id,
        title: export_job_title(name, path),
        params: None,
        rerunnable: false,
    };
    Arc::new(state.sessions.jobs.track(spec, cancel))
}

/// A Stop closure that wakes `stop` (a permit is kept when nothing waits yet).
fn notify_on_stop(stop: &Arc<Notify>) -> Box<dyn Fn() + Send + Sync> {
    let stop = Arc::clone(stop);
    Box::new(move || stop.notify_one())
}

/// `progress`, also reported to the tracked job.
fn tracked_progress(mut progress: Progress, job: &Arc<TrackedJob>) -> Progress {
    let job = Arc::clone(job);
    Box::new(move |rows| {
        progress(rows);
        job.progress(rows);
    })
}

/// Records the export's outcome on its tracked job.
async fn finish_job(job: &TrackedJob, result: &Result<ExportSummary, DbError>) {
    let outcome = match result {
        Ok(s) => Ok(JobDone {
            summary: if s.rows == 1 {
                "1 row exported".to_owned()
            } else {
                format!("{} rows exported", s.rows)
            },
            result_path: Some(s.path.clone()),
        }),
        Err(e) => Err(e.clone()),
    };
    job.finish(outcome).await;
}

/// Runs `work`; once `stop` is woken, `cancel` runs (once) while `work` keeps going to
/// its end, which the cancel makes come early.
async fn stoppable<T>(
    work: impl std::future::Future<Output = T>,
    stop: &Notify,
    cancel: impl std::future::Future<Output = ()>,
) -> T {
    let work = std::pin::pin!(work);
    let watch = std::pin::pin!(async {
        stop.notified().await;
        cancel.await;
        std::future::pending::<()>().await
    });
    match select(work, watch).await {
        Either::Left((out, _)) => out,
        Either::Right(((), _)) => unreachable!("the watcher never completes"),
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
            sink.abort();
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
/// result into a writer of `opts` at `path`. Waking `stop` (the tracked job's Stop)
/// cancels it like `table_data_cancel(id, key)`.
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
    stop: &Notify,
) -> Result<ExportSummary, DbError> {
    let mut s = Stream::open(&opts, kind, &path, progress)?;
    let outcome = {
        let exec = std::pin::pin!(state.sessions.data_exec(id, db, key, &request, &mut s.sink));
        // A write error (disk full…) or a Stop stops the statement through its key.
        let failed = Arc::clone(&s.failed);
        let watch = std::pin::pin!(async {
            let failed = std::pin::pin!(failed.notified());
            let stopped = std::pin::pin!(stop.notified());
            let _ = select(failed, stopped).await;
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

/// Writes the exact rows retained for a query result without running its SQL again; a
/// result still running is followed until it ends. `operation` cancels it
/// ([`export_query_result_cancel`]); it is a tracked job whose Stop does the same.
#[allow(clippy::too_many_arguments)]
pub async fn export_query_result_impl(
    state: &AppState,
    token: Uuid,
    kind: DbKind,
    opts: ExportOptions,
    path: String,
    operation: Uuid,
    progress: Progress,
) -> Result<ExportSummary, DbError> {
    check_options(&opts)?;
    let store = state.sessions.results.clone();
    let cancel = store.begin_export(operation);
    let stopper = store.clone();
    // Tracked under its connection, so closing or deleting it (R9) stops the export.
    let job = track_export(
        state,
        store.connection_of(token),
        "query result",
        &path,
        Box::new(move || {
            stopper.cancel_export(operation);
        }),
    );
    let progress = tracked_progress(progress, &job);
    let result = async {
        let stream = Stream::open(&opts, kind, &path, progress)?;
        // A write error (disk full…) stops reading the stored rows.
        let failed = Arc::clone(&stream.failed);
        let stop = Arc::clone(&cancel);
        let watcher = tokio::spawn(async move {
            failed.notified().await;
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let reader = store.clone();
        let flag = Arc::clone(&cancel);
        let read = blocking(move || {
            let mut stream = stream;
            let outcome = reader
                .stream_cancellable(token, &mut stream.sink, &flag)
                .map(|_| ());
            Ok((stream, outcome))
        })
        .await;
        watcher.abort();
        let (stream, outcome) = read?;
        stream.close(outcome, path).await
    }
    .await;
    store.finish_export(operation);
    finish_job(&job, &result).await;
    result
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
        full_bytes: true,
        no_timeout: true,
        ..query.request(&[], None, usize::MAX)
    };
    // A tracked job (R3): its Stop cancels the read like `table_data_cancel`.
    let stop = Arc::new(Notify::new());
    let job = track_export(state, Some(id), &obj.name, &path, notify_on_stop(&stop));
    let progress = tracked_progress(progress, &job);
    let result = stream_to_file(
        state,
        id,
        Some(&obj.database),
        &request_key,
        request,
        query.kind,
        opts,
        path,
        progress,
        &stop,
    )
    .await;
    finish_job(&job, &result).await;
    result
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
    // The tab is held (not busy) and the statement judged before the file is created.
    let run = state
        .sessions
        .query_export_begin(plan_id, index, run_id)
        .await?;
    // A tracked job (R3): its Stop is `query_cancel(run_id)`.
    let stop = Arc::new(Notify::new());
    let job = track_export(
        state,
        Some(run.connection_id()),
        "query result",
        &path,
        notify_on_stop(&stop),
    );
    let progress = tracked_progress(progress, &job);
    let result = async {
        let mut s = Stream::open(&opts, kind, &path, progress).map_err(early)?;
        let failed = Arc::clone(&s.failed);
        let executed = stoppable(run.execute(&mut s.sink, &failed), &stop, async {
            if let Err(e) = state.sessions.query_cancel(run_id).await {
                tracing::warn!(error = %e, "could not stop an export");
            }
        })
        .await;
        let (outcome, tx) = match executed {
            Ok(()) => (Ok(()), None),
            Err((e, tx)) => (Err(e), tx),
        };
        s.close(outcome, path).await.map_err(|e| (e, tx))
    }
    .await;
    let plain = result.clone().map_err(|(e, _)| e);
    finish_job(&job, &plain).await;
    result
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

/// Exports the exact disk-backed rows of a completed query result.
#[tauri::command]
#[specta::specta]
pub async fn export_query_result(
    state: State<'_, AppState>,
    token: Uuid,
    kind: DbKind,
    opts: ExportOptions,
    path: String,
    operation: Uuid,
    channel: Channel<ExportProgress>,
) -> Result<ExportSummary, AppError> {
    picked(&state, &path)?;
    let summary = guard(export_query_result_impl(
        &state,
        token,
        kind,
        opts,
        path,
        operation,
        channel_progress(channel),
    ))
    .await?;
    state.export_paths.written(&summary.path);
    Ok(summary)
}

/// Stops the stored-result export `operation`; false when none runs. Its half-written
/// file is removed.
#[tauri::command]
#[specta::specta]
pub fn export_query_result_cancel(state: State<'_, AppState>, operation: Uuid) -> bool {
    state.sessions.results.cancel_export(operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export_format::ExportFormat;
    use crate::jobs::JobState;
    use crate::state::AppPaths;
    use dnt_core::secret::MemorySecretStore;
    use dnt_core::value::LogicalType;
    use std::time::Duration;

    fn app_state(dir: &std::path::Path) -> AppState {
        AppState::open(
            AppPaths::in_dir(dir.join("data")),
            Arc::new(MemorySecretStore::default()),
        )
        .unwrap()
    }

    fn csv() -> ExportOptions {
        ExportOptions {
            format: ExportFormat::Csv,
            delimiter: ",".into(),
            header: true,
            quote_all: false,
            bom: false,
            table_name: String::new(),
            formula_guard: false,
        }
    }

    fn col(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            db_type: "text".into(),
            logical: LogicalType::Text,
            nullable: None,
            origin_table: None,
            origin_column: None,
        }
    }

    fn no_progress() -> Progress {
        Box::new(|_| {})
    }

    #[test]
    fn export_job_titles_name_the_file() {
        assert_eq!(
            export_job_title("orders", r"C:\exports\orders.csv"),
            "Export orders to orders.csv"
        );
        assert_eq!(export_job_title("q", "x.json"), "Export q to x.json");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tracked_result_export_appears_and_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let store = state.sessions.results.clone();
        let token = store.create_result(Uuid::nil(), "tab", Uuid::new_v4(), 0, vec![col("a")]);
        store.append(
            token,
            vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            Arc::new(|_, _, _| {}),
            Arc::new(|e| panic!("{e}")),
        );
        store.finish(token).unwrap();
        let path = dir.path().join("rows.csv").to_string_lossy().into_owned();
        let summary = export_query_result_impl(
            &state,
            token.0,
            DbKind::MySql,
            csv(),
            path.clone(),
            Uuid::new_v4(),
            no_progress(),
        )
        .await
        .unwrap();
        assert_eq!(summary.rows, 2);

        let jobs = state.sessions.jobs.list(10).unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.kind, JobKind::Export);
        assert_eq!(job.title, "Export query result to rows.csv");
        assert!(!job.rerunnable);
        assert!(
            state
                .sessions
                .jobs
                .wait(job.id, Duration::from_secs(5))
                .await
        );
        let detail = state.sessions.jobs.detail(job.id).unwrap();
        assert_eq!(detail.job.state, JobState::Done);
        assert_eq!(detail.job.result_path.as_deref(), Some(path.as_str()));
        assert_eq!(detail.job.summary.as_deref(), Some("2 rows exported"));
        assert_eq!(state.sessions.jobs.active_count(), 0);
    }

    /// Stop from the Jobs panel, or closing the result's connection (R9), reaches
    /// `cancel_export`: a result still running is followed until the Stop, then the
    /// export ends cancelled without its file.
    #[tokio::test(flavor = "multi_thread")]
    async fn job_stop_cancels_result_export() {
        for by_connection in [false, true] {
            job_stop_cancels_result_export_by(by_connection).await;
        }
    }

    async fn job_stop_cancels_result_export_by(by_connection: bool) {
        let conn = Uuid::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(app_state(dir.path()));
        let store = state.sessions.results.clone();
        // Never finished: the export waits for more rows.
        let token = store.create_result(conn, "tab", Uuid::new_v4(), 0, vec![col("a")]);
        store.append(
            token,
            vec![vec![Value::Int(1)]],
            Arc::new(|_, _, _| {}),
            Arc::new(|e| panic!("{e}")),
        );
        let path = dir.path().join("rows.csv");
        let export = {
            let state = Arc::clone(&state);
            let path = path.to_string_lossy().into_owned();
            tokio::spawn(async move {
                export_query_result_impl(
                    &state,
                    token.0,
                    DbKind::MySql,
                    csv(),
                    path,
                    Uuid::new_v4(),
                    no_progress(),
                )
                .await
            })
        };
        let mut id = None;
        for _ in 0..400 {
            if let Some(j) = state.sessions.jobs.list(10).unwrap().first() {
                id = Some(j.id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let id = id.expect("the export is tracked");
        assert_eq!(state.sessions.jobs.active_count(), 1);
        let job = state.sessions.jobs.detail(id).unwrap().job;
        assert_eq!(job.connection_id, Some(conn));
        if by_connection {
            state.sessions.jobs.cancel_connection(conn).await;
        } else {
            assert!(state.sessions.jobs.cancel(id).await);
        }
        let err = tokio::time::timeout(Duration::from_secs(5), export)
            .await
            .expect("the export stops")
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, DbError::Cancelled { .. }), "{err:?}");
        assert!(state.sessions.jobs.wait(id, Duration::from_secs(5)).await);
        let detail = state.sessions.jobs.detail(id).unwrap();
        assert_eq!(detail.job.state, JobState::Cancelled);
        assert_eq!(detail.job.result_path, None);
        assert!(!path.exists());
    }

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
