//! The executor shared by the dry run and the import job (spec §6.6, R15–R18, R21).
//!
//! The file is read and converted on a blocking thread ([`spawn_reader`]); the job task
//! batches the rows and sends them on its own dedicated session through [`Writer`],
//! which judges every statement with the query guard right before it is sent and never
//! sends once Stop was raised.
//!
//! Statement sequences (`[]` = optional):
//! - PostgreSQL import: `SET standard_conforming_strings = on`, `BEGIN`,
//!   [`DELETE FROM t` | `TRUNCATE t`], batches, `COMMIT`.
//! - MySQL import, transactional target: [`TRUNCATE t` (irreversible, autocommit)],
//!   `START TRANSACTION`, [`DELETE FROM t`], batches, `COMMIT`.
//! - MySQL import, other targets: [`DELETE FROM t` | `TRUNCATE t` (irreversible)], then
//!   batches in autocommit.
//! - Skip mode inside a transaction: `SAVEPOINT dnt_import` before each batch, `RELEASE
//!   SAVEPOINT dnt_import` after it; a batch the server rejects is undone with `ROLLBACK
//!   TO SAVEPOINT dnt_import` and its rows are re-sent one per statement, each under its
//!   own savepoint. Skip mode outside a transaction sends one row per statement.
//! - Dry run with server check, PostgreSQL: `SET standard_conforming_strings = on`,
//!   `BEGIN`, `CREATE TEMP TABLE <tmp> (LIKE …)`, [`ALTER TABLE <tmp> ALTER COLUMN c DROP
//!   DEFAULT, ALTER COLUMN c DROP NOT NULL` for `nextval(` defaults], inserts into `<tmp>`
//!   as in skip mode, `ROLLBACK` (drops `<tmp>`).
//! - Dry run with server check, MySQL: `CREATE TEMPORARY TABLE <tmp> LIKE <t>` (before
//!   the transaction: GTID consistency), `START TRANSACTION`, inserts, `ROLLBACK`, `DROP
//!   TEMPORARY TABLE IF EXISTS <tmp>`. Without server check only the target read is sent.
//!
//! The import job re-hashes the file before anything is sent (a queued job may find it
//! changed) and after the last record (before `COMMIT`).
use super::target::{read_target_bounded, statement_atomic, table_changed, TargetRead};
use super::{
    build_error, guard_ctx, max_errors, mode_label, session_db, verdict, ImportRequest, Layout,
    PreStep, SAVEPOINT, TEMP_PREFIX,
};
use crate::ddl_exec::{execute, stopped, HISTORY_FAILED};
use crate::jobs::{JobCtx, JobDone, JobFailure, JobLogLevel, JobRunner, ProgressUnit};
use crate::object_actions::stale;
use crate::session_manager::{
    close_quietly, config_error, ConnectionRuntime, METADATA_CALL_TIMEOUT,
};
use async_trait::async_trait;
use dnt_core::config::EnvTag;
use dnt_core::driver::{Session, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::server::SqlKind;
use dnt_core::sql::build::{BuildError, TableRef};
use dnt_core::sql::ident::quote_ident;
use dnt_core::sql::Dialect;
use dnt_core::value::FullValue;
use dnt_query::{GuardCtx, Verdict};
use dnt_store::history::{HistoryOrigin, HistoryStatus, NewHistoryEntry};
use dnt_store::repo::ConnectionRepo;
use dnt_transfer::batch::{Batch, BatchBuilder};
use dnt_transfer::convert::CellError;
use dnt_transfer::mapping::{check_mapping, ErrorPolicy};
use dnt_transfer::read::SourceOptions;
use dnt_transfer::FileFacts;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Records in flight between the reader thread and the job.
const CHANNEL_DEPTH: usize = 256;
/// Progress is reported every this many records (and after each batch).
const PROGRESS_EVERY: u64 = 256;
/// Bound of a cleanup statement (`ROLLBACK`, dropping the temporary table).
const CLEANUP_TIMEOUT_S: u32 = 30;

// ---------------------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------------------

/// What a job re-checks before it writes: the connection's settings and runtime are the
/// ones the job started with, and the connection is not read-only.
pub(crate) struct Context {
    pub(crate) repo: Arc<ConnectionRepo>,
    pub(crate) rt: Arc<ConnectionRuntime>,
    pub(crate) conn: Uuid,
    pub(crate) env: Option<EnvTag>,
    pub(crate) generation: u64,
}

impl Context {
    pub(crate) fn check(&self) -> Result<(), DbError> {
        let cfg = self.repo.get(self.conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if cfg.env_tag != self.env || cfg != self.rt.cfg {
            return Err(stale());
        }
        if self.rt.closed.load(Ordering::SeqCst)
            || self.rt.tunnel_dead()
            || self.rt.tunnel_gen.load(Ordering::SeqCst) != self.generation
        {
            return Err(stale());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------------------

pub(crate) enum Item {
    Row {
        record: u64,
        values: Vec<FullValue>,
    },
    Bad {
        record: u64,
        error: CellError,
    },
    /// Every record was read. `sha256`: the file hashed again afterwards (R21).
    End {
        warnings: Vec<String>,
        sha256: Option<String>,
    },
    /// The file cannot be read on (the reader's message: never a cell value).
    Failed(String),
}

pub(crate) struct Msg {
    pub(crate) item: Item,
    pub(crate) bytes: u64,
    pub(crate) total: u64,
}

impl Msg {
    /// A converted row; `done` / `total` are the progress counters (bytes or rows).
    pub(crate) fn row(record: u64, values: Vec<FullValue>, done: u64, total: u64) -> Msg {
        Msg {
            item: Item::Row { record, values },
            bytes: done,
            total,
        }
    }

    /// The source cannot go on (`message` names why, never a value).
    pub(crate) fn failed(message: String) -> Msg {
        Msg {
            item: Item::Failed(message),
            bytes: 0,
            total: 0,
        }
    }

    pub(crate) fn is_failed(&self) -> bool {
        matches!(self.item, Item::Failed(_))
    }

    /// The source ended (no warnings, no file hash).
    pub(crate) fn end(done: u64) -> Msg {
        Msg {
            item: Item::End {
                warnings: Vec::new(),
                sha256: None,
            },
            bytes: done,
            total: done,
        }
    }
}

/// Reads and converts every record of the file on a blocking thread. The thread ends
/// when the file ends or the receiver is dropped.
pub(crate) fn spawn_reader(
    path: PathBuf,
    source: SourceOptions,
    layout: Layout,
    hash: bool,
) -> mpsc::Receiver<Msg> {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    tokio::task::spawn_blocking(move || {
        let fail = |message: String| {
            let _ = tx.blocking_send(Msg {
                item: Item::Failed(message),
                bytes: 0,
                total: 0,
            });
        };
        let mut reader = match dnt_transfer::read::open(&path, &source) {
            Ok(r) => r,
            Err(e) => return fail(e.to_string()),
        };
        let total = reader.total_bytes();
        loop {
            match reader.next_record() {
                Ok(Some(rec)) => {
                    let item = match layout.convert(&rec) {
                        Ok(values) => Item::Row {
                            record: rec.number,
                            values,
                        },
                        Err(error) => Item::Bad {
                            record: rec.number,
                            error,
                        },
                    };
                    let msg = Msg {
                        item,
                        bytes: reader.bytes_read(),
                        total,
                    };
                    if tx.blocking_send(msg).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => return fail(e.to_string()),
            }
        }
        let sha256 = if hash {
            match dnt_transfer::file_facts(&path) {
                Ok(f) => Some(f.sha256),
                Err(e) => return fail(format!("Could not read the file: {e}")),
            }
        } else {
            None
        };
        let _ = tx.blocking_send(Msg {
            item: Item::End {
                warnings: reader.warnings().to_vec(),
                sha256,
            },
            bytes: total,
            total,
        });
    });
    rx
}

/// The file's name, size and SHA-256, on the blocking pool.
pub(crate) async fn hash_file(path: PathBuf) -> Result<FileFacts, DbError> {
    tokio::task::spawn_blocking(move || dnt_transfer::file_facts(&path))
        .await
        .map_err(|e| DbError::Internal {
            message: format!("hashing the file failed: {e}"),
        })?
        .map_err(|e| config_error("path", &format!("Could not read the file: {e}.")))
}

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// The file is not the one the preview hashed (R21).
pub(crate) fn file_changed_before() -> DbError {
    DbError::PlanStale {
        reason: "The file changed since the preview; preview again.".into(),
    }
}

/// The mapped source columns exist in the file (`columns` of its reader).
pub(crate) fn check_sources(req: &ImportRequest, columns: usize) -> Result<(), DbError> {
    for m in &req.mapping {
        if let Some(src) = m.source.filter(|s| *s as usize >= columns) {
            return Err(config_error(
                "mapping",
                &format!(
                    "Column {} reads source column {} but the file has {} columns.",
                    m.target,
                    src + 1,
                    columns
                ),
            ));
        }
    }
    Ok(())
}

/// The number of columns the file has (its reader's), on the blocking pool.
pub(crate) async fn file_columns(path: PathBuf, source: SourceOptions) -> Result<usize, DbError> {
    tokio::task::spawn_blocking(move || {
        dnt_transfer::read::open(&path, &source)
            .map(|r| r.columns().len())
            .map_err(|e| config_error("file", &e.to_string()))
    })
    .await
    .map_err(|e| DbError::Internal {
        message: format!("reading the file failed: {e}"),
    })?
}

/// The error ending a job once more rows than allowed were refused.
fn too_many_errors(max: u32) -> DbError {
    config_error(
        "errors",
        &format!("More than {max} rows were refused; the import stopped."),
    )
}

fn is_too_many(e: &DbError) -> bool {
    matches!(e, DbError::Config { field: Some(f), .. } if f == "errors")
}

/// A server error that ends the job whatever the error policy (R16): deadlock, lock wait
/// or statement timeout, cancel, serialization failure, connection-level states.
fn fatal_server(code: Option<&str>, sqlstate: Option<&str>) -> bool {
    matches!(code, Some("1205" | "1213" | "1317" | "1969" | "3024"))
        || matches!(sqlstate, Some("40P01" | "55P03" | "57014" | "40001"))
        || sqlstate
            .is_some_and(|s| s.starts_with("08") || s.starts_with("57P") || s.starts_with("53"))
}

/// MySQL / MariaDB errors about the rows' data: NOT NULL (1048), duplicate key (1062),
/// out of range (1264), truncated (1265), bad date/time (1292), bad value (1366), too
/// long (1406), foreign key (1451, 1452), CHECK (MySQL 3819, MariaDB 4025).
const MYSQL_DATA_ERRORS: [&str; 11] = [
    "1048", "1062", "1264", "1265", "1292", "1366", "1406", "1451", "1452", "3819", "4025",
];

/// The server refused this statement's rows for their data (a duplicate key, a
/// constraint, a bad value): skip mode skips them. Anything else — a missing privilege
/// or object, a syntax error, a resource limit — would refuse every row the same way,
/// so it stops the job (R16). MySQL errors carry their number (their SQLSTATE is often
/// the generic `HY000`); PostgreSQL ones only a SQLSTATE: classes 22 (data exception)
/// and 23 (integrity constraint violation).
fn row_level(e: &DbError) -> bool {
    match e {
        DbError::Server { code, sqlstate, .. } => {
            if fatal_server(code.as_deref(), sqlstate.as_deref()) {
                return false;
            }
            match code.as_deref() {
                Some(c) => MYSQL_DATA_ERRORS.contains(&c),
                None => sqlstate
                    .as_deref()
                    .is_some_and(|s| s.starts_with("22") || s.starts_with("23")),
            }
        }
        _ => false,
    }
}

/// Whether a failed write may have been applied (outside a transaction): the driver's
/// flag for a lost connection, timeout or cancel; for a server error, a multi-row
/// statement on a table whose statements are not atomic (R16).
fn may_have_applied(e: &DbError, multi_row_not_atomic: bool) -> bool {
    match e {
        DbError::Server { .. } => multi_row_not_atomic,
        DbError::Timeout {
            outcome_unknown, ..
        }
        | DbError::ConnectionLost {
            outcome_unknown, ..
        }
        | DbError::Cancelled {
            outcome_unknown, ..
        } => *outcome_unknown,
        _ => false,
    }
}

/// `e` with its `outcome_unknown` flag set to what the job knows.
fn with_outcome(e: DbError, unknown: bool) -> DbError {
    match e {
        DbError::Timeout {
            stage, was_in_tx, ..
        } => DbError::Timeout {
            stage,
            outcome_unknown: unknown,
            was_in_tx,
        },
        DbError::ConnectionLost {
            was_in_tx, message, ..
        } => DbError::ConnectionLost {
            was_in_tx,
            outcome_unknown: unknown,
            message,
        },
        DbError::Cancelled { was_in_tx, .. } => DbError::Cancelled {
            outcome_unknown: unknown,
            was_in_tx,
        },
        e => e,
    }
}

/// `(message, code)` of an error for a job log line.
fn error_text(e: &DbError) -> (String, Option<String>) {
    match e {
        DbError::Server {
            code,
            sqlstate,
            message,
            ..
        } => (message.clone(), code.clone().or_else(|| sqlstate.clone())),
        e => (e.to_string(), None),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

// ---------------------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------------------

/// Sends the statements of one job and keeps its counters.
pub(crate) struct Writer<'a> {
    pub(crate) ctx: &'a JobCtx,
    pub(crate) session: &'a dyn Session,
    context: &'a Context,
    dialect: Dialect,
    pub(crate) guard: GuardCtx,
    /// The job's confirmation covers `Confirm` verdicts.
    confirmed: bool,
    grace: Duration,
    timeout_s: u32,
    reconnects: u64,
    /// `false`: nothing is sent, batches only count (file-only dry run).
    send_rows: bool,
    skip: bool,
    max_errors: u32,
    /// Skip mode inside a transaction: savepoints and row isolation.
    savepoints: bool,
    pub(crate) in_tx: bool,
    pub(crate) commit_sent: bool,
    /// A write was sent and has not been answered (or failed).
    pub(crate) inflight: bool,
    pub(crate) inflight_rows: u32,
    pub(crate) sent_any: bool,
    pub(crate) imported: u64,
    pub(crate) skipped: u64,
    /// `INSERT` statements that succeeded.
    pub(crate) written: u64,
    records: u64,
    phase: &'static str,
    /// What `progress` counts: bytes of the file (import), rows (seeder).
    unit: ProgressUnit,
}

impl<'a> Writer<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ctx: &'a JobCtx,
        session: &'a dyn Session,
        context: &'a Context,
        dialect: Dialect,
        guard: GuardCtx,
        confirmed: bool,
        grace: Duration,
        timeout_s: u32,
        phase: &'static str,
        unit: ProgressUnit,
    ) -> Writer<'a> {
        Writer {
            ctx,
            session,
            context,
            dialect,
            guard,
            confirmed,
            grace,
            timeout_s,
            reconnects: session.reconnects(),
            send_rows: true,
            skip: false,
            max_errors: ErrorPolicy::DEFAULT_MAX_ERRORS,
            savepoints: false,
            in_tx: false,
            commit_sent: false,
            inflight: false,
            inflight_rows: 0,
            sent_any: false,
            imported: 0,
            skipped: 0,
            written: 0,
            records: 0,
            phase,
            unit,
        }
    }

    /// Sends one statement: never after Stop, never on a session that reconnected or
    /// changed its dialect, never when the guard blocks it. A write (`rows` > 0 or a
    /// pre-step) re-checks the connection first.
    pub(crate) async fn send(&mut self, sql: &str, write: Option<u32>) -> Result<(), DbError> {
        self.send_as(sql, write, false).await
    }

    /// Sends `COMMIT`; `commit_sent` is set only once it passed every check and is
    /// handed to the driver.
    pub(crate) async fn send_commit(&mut self) -> Result<(), DbError> {
        self.send_as("COMMIT", None, true).await
    }

    async fn send_as(
        &mut self,
        sql: &str,
        write: Option<u32>,
        commit: bool,
    ) -> Result<(), DbError> {
        if self.ctx.is_stopped() {
            return Err(stopped());
        }
        if self.session.reconnects() != self.reconnects || self.session.dialect() != self.dialect {
            return Err(stale());
        }
        if write.is_some() {
            self.context.check()?;
        }
        match verdict(sql, self.dialect, self.guard) {
            Verdict::Block => return Err(DbError::Blocked { count: 1 }),
            Verdict::Confirm if !self.confirmed => {
                return Err(DbError::ConfirmationRequired { count: 1 })
            }
            _ => {}
        }
        if let Some(rows) = write {
            self.sent_any = true;
            self.inflight = true;
            self.inflight_rows = rows;
        }
        if commit {
            self.commit_sent = true;
        }
        execute(
            self.session,
            sql,
            self.ctx.stop(),
            self.grace,
            self.timeout_s,
        )
        .await?;
        self.inflight = false;
        Ok(())
    }

    /// Sends `sql` even after Stop (`ROLLBACK`, dropping the temporary table).
    pub(crate) async fn cleanup(&self, sql: &str) -> Result<(), DbError> {
        execute(
            self.session,
            sql,
            StopSignal::new(),
            self.grace,
            CLEANUP_TIMEOUT_S,
        )
        .await
    }

    fn progress(&self, done: u64, total: u64) {
        self.ctx.progress(self.phase, done, Some(total), self.unit);
    }

    /// One refused row: an error in stop mode; else logged, counted, and the job stops
    /// past `max_errors`.
    fn refuse(&mut self, line: String, server: Option<Option<String>>) -> Result<(), DbError> {
        if !self.skip {
            return Err(config_error("row", &line));
        }
        self.skipped += 1;
        match server {
            Some(code) => self
                .ctx
                .log_server(JobLogLevel::Warn, line, code.as_deref()),
            None => self.ctx.log(JobLogLevel::Warn, line),
        }
        if self.skipped > u64::from(self.max_errors) {
            return Err(too_many_errors(self.max_errors));
        }
        Ok(())
    }

    fn refuse_cell(&mut self, record: u64, e: &CellError) -> Result<(), DbError> {
        self.refuse(format!("Row {record}, {e}"), None)
    }

    fn refuse_build(&mut self, record: u64, e: &BuildError) -> Result<(), DbError> {
        self.refuse(format!("Row {record}: {e}"), None)
    }

    fn refuse_server(&mut self, record: u64, e: &DbError) -> Result<(), DbError> {
        let (message, code) = error_text(e);
        self.refuse(format!("Row {record}: {message}"), Some(code))
    }

    /// Sends one batch (and in skip mode isolates the rows the server refuses).
    async fn write_batch(
        &mut self,
        batch: Batch,
        rows: Vec<(u64, Vec<FullValue>)>,
        layout: &Layout,
    ) -> Result<(), DbError> {
        let n = u64::from(batch.rows);
        if !self.send_rows {
            self.imported += n;
            return Ok(());
        }
        if !self.savepoints {
            return match self.send(&batch.sql, Some(batch.rows)).await {
                Ok(()) => {
                    self.imported += n;
                    self.written += 1;
                    Ok(())
                }
                Err(e) if self.skip && batch.rows == 1 && row_level(&e) => {
                    self.inflight = false;
                    self.refuse_server(batch.first_record, &e)
                }
                Err(e) => {
                    if row_level(&e) {
                        let (message, code) = error_text(&e);
                        let last = batch.records.last().copied().unwrap_or(batch.first_record);
                        self.ctx.log_server(
                            JobLogLevel::Error,
                            format!("Rows {} to {last}: {message}", batch.first_record),
                            code.as_deref(),
                        );
                    }
                    Err(e)
                }
            };
        }
        let sp = format!("SAVEPOINT {SAVEPOINT}");
        let release = format!("RELEASE SAVEPOINT {SAVEPOINT}");
        let back = format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}");
        self.send(&sp, None).await?;
        match self.send(&batch.sql, Some(batch.rows)).await {
            Ok(()) => {
                self.send(&release, None).await?;
                self.imported += n;
                self.written += 1;
                return Ok(());
            }
            Err(e) if row_level(&e) => {
                self.inflight = false;
                self.send(&back, None).await?;
                if batch.rows == 1 {
                    return self.refuse_server(batch.first_record, &e);
                }
            }
            Err(e) => return Err(e),
        }
        for (record, row) in rows {
            let sql = layout.single(&row).map_err(build_error)?;
            self.send(&sp, None).await?;
            match self.send(&sql, Some(1)).await {
                Ok(()) => {
                    self.send(&release, None).await?;
                    self.imported += 1;
                    self.written += 1;
                }
                Err(e) if row_level(&e) => {
                    self.inflight = false;
                    self.send(&back, None).await?;
                    self.refuse_server(record, &e)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Reads every record from `rx` into batches of `builder` and sends them; returns the
    /// reader's end (warnings, hash).
    pub(crate) async fn pump(
        &mut self,
        rx: &mut mpsc::Receiver<Msg>,
        builder: &mut BatchBuilder,
        layout: &Layout,
    ) -> Result<(Vec<String>, Option<String>), DbError> {
        // Rows of the current batch, kept for row isolation only.
        let mut pending: VecDeque<(u64, Vec<FullValue>)> = VecDeque::new();
        let keep = self.savepoints && self.send_rows;
        loop {
            if self.ctx.is_stopped() {
                return Err(stopped());
            }
            let Some(msg) = rx.recv().await else {
                return Err(DbError::Internal {
                    message: "the file reader ended unexpectedly".into(),
                });
            };
            self.records += 1;
            if self.records.is_multiple_of(PROGRESS_EVERY) {
                self.progress(msg.bytes, msg.total);
            }
            match msg.item {
                Item::Row { record, values } => {
                    if keep {
                        pending.push_back((record, values.clone()));
                    }
                    match builder.push(values, record) {
                        Ok(Some(batch)) => {
                            let rows = if keep {
                                pending.drain(..batch.rows as usize).collect()
                            } else {
                                Vec::new()
                            };
                            self.write_batch(batch, rows, layout).await?;
                            self.progress(msg.bytes, msg.total);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            if keep {
                                pending.pop_back();
                            }
                            self.refuse_build(record, &e)?;
                        }
                    }
                }
                Item::Bad { record, error } => self.refuse_cell(record, &error)?,
                Item::End { warnings, sha256 } => {
                    if let Some(batch) = builder.flush() {
                        let rows = pending.drain(..).collect();
                        self.write_batch(batch, rows, layout).await?;
                    }
                    self.progress(msg.bytes, msg.total);
                    return Ok((warnings, sha256));
                }
                Item::Failed(message) => return Err(config_error("file", &message)),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Import job
// ---------------------------------------------------------------------------------------

/// Rows per `INSERT` of an import job: one outside a transaction in skip mode (a
/// failing multi-row statement may have written part of its rows, R16), else the
/// request's batch size. The preview's sample follows the same rule.
pub(crate) fn rows_per_statement(request: &ImportRequest, transactional: bool) -> u32 {
    if matches!(request.errors, ErrorPolicy::Skip { .. }) && !transactional {
        1
    } else {
        request.batch_rows
    }
}

/// The notes of a failed write job that name what it writes.
pub(crate) struct FailureWords {
    pub(crate) rolled_back: &'static str,
    pub(crate) refused_commit: &'static str,
}

const IMPORT_WORDS: FailureWords = FailureWords {
    rolled_back: "The import was rolled back; no row was imported.",
    refused_commit: "The server refused the COMMIT; no row was imported.",
};

/// What a failed write job did (spec §6.6 step 6). An open transaction is rolled back.
/// Also returns the rows the server kept (`None`: unknown).
pub(crate) async fn describe_failure(
    w: &Writer<'_>,
    e: DbError,
    pre: PreStepState,
    statement_atomic: bool,
    words: &FailureWords,
) -> (JobFailure, Option<u64>) {
    // A stale plan says why in its own words (the file or the table changed).
    let base = match &e {
        DbError::PlanStale { reason } => reason.clone(),
        e => JobFailure::from(e.clone()).summary,
    };
    let mut base = base.trim_end().to_string();
    if !base.ends_with('.') {
        base.push('.');
    }
    let refused_commit = w.commit_sent && matches!(e, DbError::Server { .. });
    let (unknown, mut note) = if refused_commit {
        // The server answered COMMIT with an error: nothing was committed.
        if w.session.in_transaction() {
            let _ = w.cleanup("ROLLBACK").await;
        }
        (false, words.refused_commit.to_string())
    } else if w.commit_sent {
        (
            true,
            "COMMIT was sent but not answered; the outcome is unknown.".to_string(),
        )
    } else if w.in_tx {
        let rolled_back = w.session.in_transaction() && w.cleanup("ROLLBACK").await.is_ok();
        (
            false,
            if rolled_back {
                words.rolled_back.to_string()
            } else {
                "Not committed; the server rolls the transaction back when it notices the \
                 lost connection."
                    .to_string()
            },
        )
    } else {
        let multi = w.inflight_rows > 1 && !statement_atomic;
        let unknown = w.inflight && may_have_applied(&e, multi);
        let mut note = format!(
            "{} before the error.",
            if w.written == 1 {
                "1 batch was written".to_string()
            } else {
                format!("{} batches were written", w.written)
            }
        );
        if unknown {
            note.push_str(" The last statement may or may not have been applied.");
        }
        (unknown, note)
    };
    match pre {
        PreStepState::Done => {
            note.push_str(" The existing rows were already deleted by the pre-step.")
        }
        // Its answer was lost (the unknown outcome above): it may have run.
        PreStepState::Sent if unknown => note
            .push_str(" The pre-step's answer was lost: the existing rows may already be deleted."),
        _ => {}
    }
    let committed = if unknown {
        None
    } else if w.in_tx || w.commit_sent {
        Some(0)
    } else {
        // Autocommit: the batches that succeeded stay.
        Some(w.imported)
    };
    let failure = JobFailure {
        error: with_outcome(e, unknown),
        outcome_unknown: unknown,
        summary: format!("{base} {note}"),
    };
    (failure, committed)
}

/// Where the irreversible pre-step (committed on its own) stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreStepState {
    NotSent,
    /// Sent; no answer yet (or the error that ended the job is its own).
    Sent,
    Done,
}

/// Everything a started import needs (copied from its plan).
pub(crate) struct ImportJob {
    pub(crate) request: ImportRequest,
    pub(crate) file: FileFacts,
    pub(crate) layout: Layout,
    pub(crate) pre_step: Option<PreStep>,
    pub(crate) transactional: bool,
    pub(crate) statement_atomic: bool,
    pub(crate) fingerprint: u64,
    pub(crate) dialect: Dialect,
    /// The plan's templates, judged again before anything is sent.
    pub(crate) templates: Vec<String>,
    pub(crate) context: Context,
    pub(crate) grace: Duration,
}

#[async_trait]
impl JobRunner for ImportJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let kind = self.layout.kind;
        let (session, generation) = self
            .context
            .rt
            .open_query_session(session_db(kind, &self.request.table))
            .await
            .map_err(JobFailure::from)?;
        ctx.set_cancel(Some(session.cancel_handle()));
        let cfg_timeout = self.context.rt.cfg.options.query_timeout_s;
        let mut w = Writer::new(
            &ctx,
            session.as_ref(),
            &self.context,
            self.dialect,
            guard_ctx(self.context.env, false),
            true,
            self.grace,
            cfg_timeout,
            "Importing",
            ProgressUnit::Bytes,
        );
        w.skip = matches!(self.request.errors, ErrorPolicy::Skip { .. });
        w.max_errors = max_errors(&self.request.errors);
        w.savepoints = w.skip && self.transactional;
        let mut pre = PreStepState::NotSent;
        let result = self.body(&mut w, generation, &mut pre).await;
        // Rows the server kept: `None` when the outcome is unknown.
        let (outcome, committed) = match result {
            Ok(()) => (
                Ok(JobDone {
                    summary: format!(
                        "Imported {} rows ({} skipped) into {} in {} batches",
                        w.imported, w.skipped, self.request.table.name, w.written
                    ),
                    result_path: None,
                }),
                Some(w.imported),
            ),
            Err(e) => {
                let (f, committed) = self.failure(&w, e, pre).await;
                (Err(f), committed)
            }
        };
        let (sent_any, written) = (w.sent_any, w.written);
        ctx.set_cancel(None);
        close_quietly(session).await;
        if sent_any {
            let error = outcome.as_ref().err().map(|f| {
                let mut e = AppError::from(f.error.clone());
                e.outcome_unknown = f.outcome_unknown;
                e
            });
            let saved = self
                .history(
                    &started_at,
                    started.elapsed(),
                    committed,
                    written,
                    error.as_ref(),
                )
                .await;
            if !saved {
                ctx.log(JobLogLevel::Warn, HISTORY_FAILED);
            }
        }
        outcome
    }
}

impl ImportJob {
    async fn body(
        &self,
        w: &mut Writer<'_>,
        generation: u64,
        pre: &mut PreStepState,
    ) -> Result<(), DbError> {
        if generation != self.context.generation || w.session.dialect() != self.dialect {
            return Err(stale());
        }
        // The file as it is now (R21): a job that waited in the queue may find it
        // changed since the start's check; nothing is sent then.
        let now = hash_file(PathBuf::from(&self.request.path)).await?;
        if now.sha256 != self.file.sha256 || now.size != self.file.size {
            return Err(file_changed_before());
        }
        let begin = match self.layout.kind {
            SqlKind::MySql => "START TRANSACTION",
            SqlKind::Postgres => {
                w.send("SET standard_conforming_strings = on", None).await?;
                "BEGIN"
            }
        };
        // The table as it is now (R19): a changed table is stale, nothing is written.
        let detail = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            w.session.introspector().table_detail(&self.request.table),
        )
        .await
        .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute))??;
        if dnt_query::fingerprint(&detail) != self.fingerprint {
            return Err(table_changed());
        }
        self.context.check()?;
        super::judge_all(&self.templates, self.dialect, w.guard, true)?;
        let max_rows = rows_per_statement(&self.request, self.transactional);
        let mut builder = self.layout.builder(max_rows, self.request.batch_bytes)?;
        let mut rx = spawn_reader(
            PathBuf::from(&self.request.path),
            self.request.source.clone(),
            self.layout.clone(),
            true,
        );
        let ctx = w.ctx;
        if let Some(p) = self.pre_step.as_ref().filter(|p| p.irreversible) {
            *pre = PreStepState::Sent;
            w.send(&p.sql, Some(0)).await?;
            *pre = PreStepState::Done;
            ctx.log(
                JobLogLevel::Info,
                "Existing rows deleted (irreversible pre-step, committed).",
            );
        }
        if self.transactional {
            w.send(begin, None).await?;
            w.in_tx = true;
        }
        if let Some(p) = self.pre_step.as_ref().filter(|p| !p.irreversible) {
            w.send(&p.sql, Some(0)).await?;
            ctx.log(
                JobLogLevel::Info,
                "Existing rows deleted (in the transaction).",
            );
        }
        let (warnings, sha256) = w.pump(&mut rx, &mut builder, &self.layout).await?;
        for warning in warnings {
            ctx.log(JobLogLevel::Warn, warning);
        }
        if sha256.as_deref() != Some(self.file.sha256.as_str()) {
            return Err(DbError::PlanStale {
                reason: "The file changed while it was imported.".into(),
            });
        }
        if self.transactional {
            self.context.check()?;
            if ctx.is_stopped() {
                return Err(stopped());
            }
            w.send_commit().await?;
            w.in_tx = false;
        }
        Ok(())
    }

    /// What a failed import did (spec §6.6 step 6), in the import's words.
    async fn failure(
        &self,
        w: &Writer<'_>,
        e: DbError,
        pre: PreStepState,
    ) -> (JobFailure, Option<u64>) {
        describe_failure(w, e, pre, self.statement_atomic, &IMPORT_WORDS).await
    }

    /// The job's one History entry (R10): a summary comment, the pre-step and the
    /// `INSERT` template; never a value of the file.
    async fn history(
        &self,
        started_at: &str,
        elapsed: Duration,
        committed: Option<u64>,
        written: u64,
        error: Option<&AppError>,
    ) -> bool {
        let head = self.layout.head().unwrap_or_default();
        let tail = self.layout.tail().unwrap_or_default();
        let mut text = format!(
            "-- Import {} into {}: {}, {} rows, {} batches\n",
            self.file.name,
            self.request.table.name,
            mode_label(&self.request.mode),
            committed.map_or_else(|| "unknown".to_string(), |n| n.to_string()),
            written
        );
        if let Some(p) = &self.pre_step {
            text.push_str(&p.sql);
            text.push_str(";\n");
        }
        text.push_str(&format!("{head}…{tail}"));
        let sql = dnt_query::redact::redact(&text, self.dialect);
        let entry = NewHistoryEntry {
            connection_id: self.context.conn,
            database: Some(self.request.table.database.clone()),
            sql_text: sql.text,
            redacted: sql.changed,
            kind: "write".into(),
            started_at: started_at.to_string(),
            elapsed_ms: Some(elapsed.as_millis().min(u64::MAX as u128) as u64),
            status: if error.is_none() {
                HistoryStatus::Ok
            } else {
                HistoryStatus::Error
            },
            row_count: None,
            affected: committed,
            error_kind: error.map(|e| e.kind.clone()),
            error_code: error.and_then(|e| e.server_code.clone().or_else(|| e.sqlstate.clone())),
            origin: HistoryOrigin::Import,
            outcome_unknown: error.is_some_and(|e| e.outcome_unknown),
        };
        let repo = Arc::clone(&self.context.repo);
        matches!(
            tokio::task::spawn_blocking(move || repo.history_insert(&entry)).await,
            Ok(Ok(_))
        )
    }
}

// ---------------------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------------------

/// The dry run's temporary table: a session-private copy of the target (R18).
pub(crate) fn temp_table(kind: SqlKind, target: &TableRef) -> TableRef {
    let suffix = Uuid::new_v4().simple().to_string();
    TableRef {
        kind,
        database: target.database.clone(),
        schema: match kind {
            SqlKind::MySql => None,
            SqlKind::Postgres => Some("pg_temp".to_string()),
        },
        name: format!("{TEMP_PREFIX}{}", &suffix[..8]),
    }
}

/// `CREATE TEMPORARY TABLE <tmp> LIKE <t>` (MySQL) / `CREATE TEMP TABLE <tmp> (LIKE <t>
/// INCLUDING DEFAULTS INCLUDING CONSTRAINTS INCLUDING IDENTITY INCLUDING GENERATED)`.
pub(crate) fn create_temp_sql(kind: SqlKind, tmp: &TableRef, target: &TableRef) -> String {
    match kind {
        SqlKind::MySql => format!(
            "CREATE TEMPORARY TABLE {} LIKE {}",
            tmp.relation(),
            target.relation()
        ),
        SqlKind::Postgres => format!(
            "CREATE TEMP TABLE {} (LIKE {} INCLUDING DEFAULTS INCLUDING CONSTRAINTS \
             INCLUDING IDENTITY INCLUDING GENERATED)",
            tmp.relation(),
            target.relation()
        ),
    }
}

/// PostgreSQL: the temporary table's columns whose default calls `nextval(` lose the
/// default (and NOT NULL), so the target's sequences are never advanced.
fn drop_sequence_defaults_sql(tmp: &TableRef, read: &TargetRead) -> Option<String> {
    let parts: Vec<String> = read
        .detail
        .columns
        .iter()
        .filter(|c| {
            c.default
                .as_deref()
                .is_some_and(|d| d.to_ascii_lowercase().contains("nextval("))
        })
        .map(|c| {
            let q = quote_ident(SqlKind::Postgres, &c.name);
            format!("ALTER COLUMN {q} DROP DEFAULT, ALTER COLUMN {q} DROP NOT NULL")
        })
        .collect();
    (!parts.is_empty()).then(|| format!("ALTER TABLE {} {}", tmp.relation(), parts.join(", ")))
}

pub(crate) struct DryRunJob {
    pub(crate) request: ImportRequest,
    pub(crate) server_check: bool,
    pub(crate) confirmed: bool,
    pub(crate) context: Context,
    pub(crate) grace: Duration,
}

/// What the dry run did on the server, for the cleanup.
#[derive(Default)]
struct DryState {
    temp: Option<TableRef>,
}

#[async_trait]
impl JobRunner for DryRunJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let kind = self.context.rt.cfg.kind.as_sql(); // R1: boundary
        let (session, generation) = self
            .context
            .rt
            .open_query_session(session_db(kind, &self.request.table))
            .await
            .map_err(JobFailure::from)?;
        ctx.set_cancel(Some(session.cancel_handle()));
        let cfg = self.context.rt.cfg.clone();
        let mut w = Writer::new(
            &ctx,
            session.as_ref(),
            &self.context,
            session.dialect(),
            guard_ctx(self.context.env, cfg.read_only),
            self.confirmed,
            self.grace,
            cfg.options.query_timeout_s,
            "Checking",
            ProgressUnit::Bytes,
        );
        w.skip = true;
        w.max_errors = max_errors(&self.request.errors);
        w.send_rows = self.server_check;
        let mut state = DryState::default();
        let result = self.body(&mut w, generation, &mut state).await;
        // Nothing the dry run did stays: the transaction is rolled back (PostgreSQL drops
        // the temporary table with it), MySQL drops the temporary table.
        if w.in_tx && w.cleanup("ROLLBACK").await.is_ok() {
            w.in_tx = false;
        }
        match (kind, &state.temp) {
            (SqlKind::MySql, Some(tmp)) => {
                let _ = w
                    .cleanup(&format!(
                        "DROP TEMPORARY TABLE IF EXISTS {}",
                        tmp.relation()
                    ))
                    .await;
            }
            (SqlKind::MySql, None) | (SqlKind::Postgres, _) => {}
        }
        let (valid, rejected) = (w.imported, w.skipped);
        ctx.set_cancel(None);
        close_quietly(session).await;
        let counts = format!(
            "{} valid, {} rejected (first errors in the log)",
            plural(valid, "row", "rows"),
            plural(rejected, "row", "rows")
        );
        match result {
            Ok(()) => Ok(JobDone {
                summary: counts,
                result_path: None,
            }),
            Err(e) if is_too_many(&e) => Ok(JobDone {
                summary: format!(
                    "{counts}; stopped after more than {} rejected rows",
                    max_errors(&self.request.errors)
                ),
                result_path: None,
            }),
            Err(e) => {
                let mut f = JobFailure::from(with_outcome(e, false));
                // The target is never written by a dry run.
                f.outcome_unknown = false;
                Err(f)
            }
        }
    }
}

impl DryRunJob {
    async fn body(
        &self,
        w: &mut Writer<'_>,
        generation: u64,
        state: &mut DryState,
    ) -> Result<(), DbError> {
        let kind = self.context.rt.cfg.kind.as_sql(); // R1: boundary
                                                      // The session must reach the server the job was submitted for (the tunnel of
                                                      // the submit time), or the server check writes elsewhere.
        if self.server_check && generation != self.context.generation {
            return Err(stale());
        }
        let read = read_target_bounded(w.session, &self.request.table).await?;
        let report = check_mapping(
            &self.request.mapping,
            &read.columns,
            &read.keys,
            &self.request.mode,
            self.request.override_identity,
        );
        if !report.errors.is_empty() {
            return Err(super::plan::mapping_error(&report));
        }
        let layout = Layout::new(&self.request, &read.columns, kind, w.dialect)?;
        let columns = file_columns(
            PathBuf::from(&self.request.path),
            self.request.source.clone(),
        )
        .await?;
        check_sources(&self.request, columns)?;
        let reader = spawn_reader(
            PathBuf::from(&self.request.path),
            self.request.source.clone(),
            layout.clone(),
            false,
        );
        let mut rx = reader;
        if !self.server_check {
            let mut builder = layout.builder(self.request.batch_rows, self.request.batch_bytes)?;
            let (warnings, _) = w.pump(&mut rx, &mut builder, &layout).await?;
            for warning in warnings {
                w.ctx.log(JobLogLevel::Warn, warning);
            }
            return Ok(());
        }
        self.context.check()?;
        let tmp = temp_table(kind, &layout.table);
        let tmp_layout = layout.appending_to(tmp.clone());
        let create = create_temp_sql(kind, &tmp, &layout.table);
        let alter = match kind {
            SqlKind::MySql => None,
            SqlKind::Postgres => {
                w.send("SET standard_conforming_strings = on", None).await?;
                drop_sequence_defaults_sql(&tmp, &read)
            }
        };
        // The cleanup drops the table if it exists, so it is known before it is created.
        state.temp = Some(tmp);
        match kind {
            // MySQL: outside the transaction (GTID consistency refuses CREATE TEMPORARY
            // TABLE inside one on some servers); the cleanup drops it.
            SqlKind::MySql => {
                w.send(&create, Some(0)).await?;
                w.send("START TRANSACTION", None).await?;
                w.in_tx = true;
            }
            SqlKind::Postgres => {
                w.send("BEGIN", None).await?;
                w.in_tx = true;
                w.send(&create, Some(0)).await?;
            }
        }
        if let Some(alter) = alter {
            w.send(&alter, Some(0)).await?;
        }
        // Row isolation needs savepoints that undo: a transactional engine.
        let atomic = statement_atomic(&read.detail, kind);
        w.savepoints = atomic;
        let max_rows = if atomic { self.request.batch_rows } else { 1 };
        let mut builder = tmp_layout.builder(max_rows, self.request.batch_bytes)?;
        let (warnings, _) = w.pump(&mut rx, &mut builder, &tmp_layout).await?;
        for warning in warnings {
            w.ctx.log(JobLogLevel::Warn, warning);
        }
        Ok(())
    }
}
