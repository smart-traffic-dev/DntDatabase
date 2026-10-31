use crate::cancel::{send_cancel, CancelState, CancelTarget, PgCanceller, RunGuard};
use crate::values::{column_meta, to_full_value, to_value, Raw};
use crate::{close_conn, io_error, map_pg_error, open, take_notices, ConnectParams, PgConn};
use async_trait::async_trait;
use dnt_core::driver::{
    check_cell_size, CancelHandle, CellMode, ExecOptions, ExecProgress, Introspector,
    ResultSummary, RowSink, Session, SharedSink, StmtOutcome, StopSignal,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::sql::{leading_keywords, session_effect, Dialect, StatementKind, TxControl};
use dnt_core::tx::{next_tx_state, StmtResult, TxState};
use dnt_core::value::{ColumnMeta, FullValue, Value};
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::Client;

/// Bound on re-opening the connection after it was lost (same as the connect timeout).
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `query_canceled`: the statement was stopped by a cancel request.
const QUERY_CANCELED: &str = "57014";
/// SQLSTATEs meaning the backend is gone: `admin_shutdown` (e.g. `pg_terminate_backend`),
/// `crash_shutdown`, `cannot_connect_now`, `connection_exception`,
/// `connection_does_not_exist`, `connection_failure`.
const LOST_STATES: [&str; 6] = ["57P01", "57P02", "57P03", "08000", "08003", "08006"];

/// `ConnectionLost` message for a statement that ignored the cancel request sent after
/// its timeout.
const STUCK: &str = "the statement did not stop after the timeout, so its connection was closed";
/// `ConnectionLost` message for a statement that ignored the user's cancel.
const STUCK_CANCEL: &str =
    "the statement did not stop after it was cancelled, so its connection was closed";
/// `ConnectionLost` message for a statement that ended during the grace period while the
/// cancel request sent after its timeout was not confirmed (see `cancel.rs`).
const UNCONFIRMED: &str = "the cancel request sent after the timeout could not be confirmed, \
                           so the connection was closed before a late cancel could hit the \
                           next statement";
/// `ConnectionLost` message for a statement that found its connection poisoned by a
/// user's cancel request that was never confirmed (see `cancel.rs`).
const UNCONFIRMED_CANCEL: &str = "an earlier cancel request could not be confirmed, so the \
                                  connection was replaced before a late cancel could hit \
                                  this statement";

/// One PostgreSQL connection. It is its own [`Introspector`] (see `introspect.rs`).
pub struct PgSession {
    pub(crate) conn: Mutex<PgConn>,
    info: ServerInfo,
    /// Settings of the established connection (TLS fallback applied); used to reconnect
    /// and by cancel requests.
    params: ConnectParams,
    /// Transaction state, changed only as the server answered each statement
    /// ([`next_tx_state`]): `Aborted` once a statement failed inside a block (PostgreSQL
    /// then rejects everything with `25P02` until it ends). Reset to `Idle` by a
    /// reconnect (the server rolled the block back).
    tx: std::sync::Mutex<TxState>,
    cancel: Arc<CancelState>,
    /// Last session-level `SET search_path …` that succeeded, re-applied after a reconnect.
    search_path: std::sync::Mutex<Option<String>>,
    /// Set once a statement changed session state that a reconnect cannot restore
    /// (see [`session_effect`]); cleared when the connection is replaced. While
    /// set, a lost read is not silently re-run and `ConnectionLost` says what was reset.
    session_custom: AtomicBool,
    /// Successful [`PgSession::reconnect`]s ([`Session::reconnects`]).
    reconnects: AtomicU64,
}

/// How one statement attempt failed.
enum StmtError {
    Pg(tokio_postgres::Error),
    /// `opts.timeout` expired and the cancel request stopped the statement
    /// (`query_canceled`); whether it applied anything is [`killed_outcome_unknown`].
    Timeout,
    /// The statement did not stop within `opts.cancel_grace` after the cancel request
    /// (sent at its timeout, or by the user's cancel): its completion was never observed
    /// and the connection must be abandoned. Holds the `ConnectionLost` message
    /// ([`STUCK`] or [`STUCK_CANCEL`]).
    Stuck(&'static str),
    /// `opts.timeout` expired and the statement ended during the grace period, but the
    /// cancel request was not confirmed: the signal may still arrive and cancel whatever
    /// runs next, so the connection must be abandoned. `failed`: the server error it
    /// ended with (`None`: it succeeded); `cancel`: why the request was not confirmed.
    Unconfirmed {
        failed: Option<tokio_postgres::Error>,
        cancel: DbError,
    },
}

/// What made `run_timed` stop waiting for the statement alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Timeout,
    Cancel,
}

impl PgSession {
    pub(crate) fn new(conn: PgConn, info: ServerInfo, params: ConnectParams) -> Self {
        let cancel = Arc::new(CancelState::new(conn.client.cancel_token()));
        PgSession {
            conn: Mutex::new(conn),
            info,
            params,
            tx: std::sync::Mutex::new(TxState::Idle),
            cancel,
            search_path: std::sync::Mutex::new(None),
            session_custom: AtomicBool::new(false),
            reconnects: AtomicU64::new(0),
        }
    }

    /// Test-only (`test-helpers` feature): runs one statement over the simple protocol and
    /// returns its first row as text (`None` for SQL NULL). Bypasses transaction tracking;
    /// `execute` is the real path.
    #[cfg(feature = "test-helpers")]
    pub async fn query_row_text(&self, sql: &str) -> Result<Option<Vec<Option<String>>>, DbError> {
        let conn = self.conn.lock().await;
        let msgs = conn
            .client
            .simple_query(sql)
            .await
            .map_err(|e| map_pg_error(e, ConnectStage::Execute))?;
        Ok(msgs.iter().find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => {
                Some((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect())
            }
            _ => None,
        }))
    }

    pub(crate) fn unsupported(&self, feature: &str) -> DbError {
        DbError::Unsupported {
            feature: feature.into(),
            version: self.info.version_string.clone(),
        }
    }

    fn set_tx(&self, state: TxState) {
        *self.tx.lock().unwrap_or_else(|p| p.into_inner()) = state;
    }

    /// Applies [`next_tx_state`] for a statement that ended with `result`.
    ///
    /// Unlike the MySQL driver this never asks the server for the state afterwards: a
    /// PostgreSQL statement never holds several `;`-separated pieces
    /// (`dnt_core::sql::resplits` is always false for it, as PG has no `DELIMITER`), so
    /// its kind always describes everything that ran.
    fn track_tx(&self, kind: StatementKind, sql: &str, result: StmtResult<'_>) {
        let mut tx = self.tx.lock().unwrap_or_else(|p| p.into_inner());
        *tx = next_tx_state(DbKind::Postgres.as_sql(), *tx, false, kind, sql, result);
    }

    /// Tracking applied after a statement succeeded: transaction state, the last
    /// `search_path` and whether session state was customised.
    fn track_after(&self, kind: StatementKind, sql: &str) {
        self.track_tx(kind, sql, StmtResult::Ok);
        if let Some(effect) = session_effect(sql, Dialect::Postgres) {
            if let Some(change) = effect.search_path {
                *self.search_path.lock().unwrap_or_else(|p| p.into_inner()) = change;
            }
            if effect.custom {
                self.session_custom.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Transaction tracking for a statement the server answered with an error
    /// (`sqlstate`: its SQLSTATE; `57014` for one stopped by our cancel). Not for a lost
    /// connection: reconnecting resets it.
    fn track_failed(&self, kind: StatementKind, sql: &str, sqlstate: Option<&str>) {
        self.track_tx(
            kind,
            sql,
            StmtResult::Failed {
                sqlstate,
                code: None,
            },
        );
    }

    /// The `ConnectionLost` message suffix for losing the current connection.
    fn notes(&self, was_in_tx: bool) -> String {
        reset_notes(was_in_tx, self.session_custom.load(Ordering::SeqCst))
    }

    /// Replaces `conn` with a fresh connection (startup statements included) and re-applies
    /// the last `SET search_path`. Clears the transaction state (the server rolled it
    /// back) and the custom-session-state flag: whatever the old session held is gone
    /// either way. On failure the old connection's task is aborted, so the next `execute`
    /// finds it closed and opens a new one before sending anything; nothing ever runs on
    /// the abandoned connection again.
    async fn reconnect(&self, conn: &mut PgConn) -> Result<(), DbError> {
        self.set_tx(TxState::Idle);
        self.session_custom.store(false, Ordering::SeqCst);
        let search_path = self
            .search_path
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let fresh = async {
            let fresh = open(&self.params).await?;
            if let Some(sql) = search_path {
                if let Err(e) = fresh.client.batch_execute(&sql).await {
                    close_conn(fresh).await;
                    return Err(map_pg_error(e, ConnectStage::ConnectDb));
                }
            }
            Ok::<_, DbError>(fresh)
        };
        let fresh = match tokio::time::timeout(RECONNECT_TIMEOUT, fresh).await {
            Ok(Ok(fresh)) => fresh,
            Ok(Err(e)) => {
                conn.task.abort();
                return Err(e);
            }
            Err(_) => {
                conn.task.abort();
                return Err(DbError::timeout(ConnectStage::ConnectDb));
            }
        };
        self.cancel.set_token(fresh.client.cancel_token());
        // The old socket may be stuck behind a statement that ignored the cancel: drop it.
        let old = std::mem::replace(conn, fresh);
        old.task.abort();
        self.reconnects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Runs one statement, enforcing `opts.timeout` per statement (not per script), and
    /// watching for a cancel aimed at `run`.
    ///
    /// On expiry a cancel request is sent; on a cancel the canceller sends it. Either way
    /// `sink` gets [`ExecProgress::CancelSent`] at once (before the request is actually
    /// sent) and the statement is awaited until `opts.cancel_grace` after that, so the
    /// connection stays in sync. On expiry the request is sent while the statement is
    /// awaited, under that same deadline: a slow or unanswered request never extends it.
    /// A statement still running at the deadline is `Stuck` (after
    /// [`ExecProgress::GraceExpired`]).
    ///
    /// After a timeout, PostgreSQL reports a statement stopped by the cancel with
    /// `query_canceled` (`Timeout`); a statement that completes during the grace period
    /// instead (it finished first, or swallowed the cancel) reports what really happened,
    /// a success with `late_completion`. After a user's cancel, whatever the statement
    /// ended with is returned as is (the caller sees `run.cancelled()`). Returns whether
    /// the statement produced a result set (had columns) and whether it was late.
    ///
    /// A statement that completed during the grace period after a timeout needs no new
    /// connection: `send_cancel` returns only once the server closed the request's
    /// connection, after which the signal can no longer reach a later statement (see
    /// `cancel.rs`). Only when that was not confirmed by the deadline is the connection
    /// given up (`Unconfirmed`), since the signal may still be on its way.
    #[allow(clippy::too_many_arguments)]
    async fn run_timed(
        &self,
        conn: &PgConn,
        sql: &str,
        opts: &ExecOptions,
        run: &RunGuard,
        sink: &mut dyn RowSink,
        results: &mut Vec<ResultSummary>,
        emitted: &mut bool,
        cell_error: &mut Option<DbError>,
    ) -> Result<(bool, bool), StmtError> {
        let shared = SharedSink::new(sink);
        let mut handle = &shared;
        let mut fut = std::pin::pin!(run_statement(
            &conn.client,
            sql,
            opts,
            &mut handle,
            results,
            emitted,
            cell_error
        ));
        let expiry = async {
            match opts.timeout {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending().await,
            }
        };
        let trigger = tokio::select! {
            biased;
            res = fut.as_mut() => return res.map(|had| (had, false)).map_err(StmtError::Pg),
            () = expiry => Trigger::Timeout,
            () = run.cancel_requested() => Trigger::Cancel,
        };
        // One deadline for everything that follows: sending the cancel request (and its
        // confirmation) and the statement's end.
        let deadline = tokio::time::Instant::now() + opts.cancel_grace;
        shared.progress(ExecProgress::CancelSent);
        if trigger == Trigger::Cancel {
            // The canceller sends the request itself.
            let Ok(res) = tokio::time::timeout_at(deadline, fut.as_mut()).await else {
                shared.progress(ExecProgress::GraceExpired);
                return Err(StmtError::Stuck(STUCK_CANCEL));
            };
            return res.map(|had| (had, false)).map_err(StmtError::Pg);
        }
        let token = self.cancel.token();
        let target = CancelTarget::new(&self.params);
        let mut send = std::pin::pin!(send_cancel(&token, &target, opts.cancel_grace));
        let expired = tokio::time::sleep_until(deadline);
        let mut expired = std::pin::pin!(expired);
        let mut ended = None;
        let mut sent = None;
        // Until both the statement ended and the request was answered, or the deadline.
        while ended.is_none() || sent.is_none() {
            tokio::select! {
                res = fut.as_mut(), if ended.is_none() => ended = Some(res),
                res = send.as_mut(), if sent.is_none() => sent = Some(res),
                () = expired.as_mut() => break,
            }
        }
        let Some(res) = ended else {
            shared.progress(ExecProgress::GraceExpired);
            return Err(StmtError::Stuck(STUCK));
        };
        let unconfirmed = match sent {
            Some(Ok(())) => None,
            // Never reached the server: no signal can follow.
            Some(Err(f)) if !f.maybe_sent => None,
            Some(Err(f)) => Some(f.error),
            // Still waiting for the server to close the request's connection.
            None => Some(DbError::timeout(ConnectStage::Execute)),
        };
        match res {
            // Given up anyway: no statement follows on this connection.
            Err(e) if is_connection_lost(&e, &conn.client) => Err(StmtError::Pg(e)),
            // Even a `query_canceled` may have come from elsewhere (server
            // `statement_timeout`), with our signal still on its way.
            res => match unconfirmed {
                Some(cancel) => Err(StmtError::Unconfirmed {
                    failed: res.err(),
                    cancel,
                }),
                // Our confirmed signal stopped it, so it cannot reach anything else.
                None => res.map(|had| (had, true)).map_err(|e| match sqlstate(&e) {
                    Some(QUERY_CANCELED) => StmtError::Timeout,
                    _ => StmtError::Pg(e),
                }),
            },
        }
    }
}

/// A bound value sent in the TEXT format, whatever type the server inferred for its
/// placeholder: the server parses it with that type's input function, exactly like a
/// quoted literal (see `ExecOptions::params`).
#[derive(Debug)]
struct TextParam<'a>(&'a str);

impl ToSql for TextParam<'_> {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(self.0.as_bytes());
        Ok(tokio_postgres::types::IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> tokio_postgres::types::Format {
        tokio_postgres::types::Format::Text
    }

    tokio_postgres::types::to_sql_checked!();
}

/// Prepares one statement (for its column types), then streams its rows into `sink`, or
/// runs it for its affected-row count when it returns no columns. `cell_error`: set when
/// a cell was larger than [`CellMode::Full`] allows; the rows are still read to the end,
/// and the error is reported once the statement is tracked.
async fn run_statement(
    client: &Client,
    sql: &str,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    results: &mut Vec<ResultSummary>,
    emitted: &mut bool,
    cell_error: &mut Option<DbError>,
) -> Result<bool, tokio_postgres::Error> {
    let started = Instant::now();
    let stmt = client.prepare(sql).await?;
    let cols = stmt.columns();
    let text_params: Vec<TextParam<'_>> = opts.params.iter().map(|p| TextParam(p)).collect();
    let params: Vec<&(dyn ToSql + Sync)> = text_params
        .iter()
        .map(|p| p as &(dyn ToSql + Sync))
        .collect();
    if cols.is_empty() {
        let affected = client.execute(&stmt, &params).await?;
        sink.result_start(&[]);
        *emitted = true;
        finish(sink, results, started, Some(affected), 0, false);
        return Ok(false);
    }
    let metas: Vec<ColumnMeta> = cols.iter().map(column_meta).collect();
    let types: Vec<Type> = cols.iter().map(|c| c.type_().clone()).collect();
    let rows = client.query_raw(&stmt, params).await?;
    sink.result_start(&metas);
    *emitted = true;
    let batch_size = opts.batch_size.max(1);
    let mut rows = std::pin::pin!(rows);
    let cap = batch_size.min(opts.max_rows.max(1));
    let mut batch: Vec<Vec<Value>> = Vec::new();
    let mut full_batch: Vec<Vec<FullValue>> = Vec::new();
    match opts.cells {
        CellMode::Preview => batch.reserve(cap),
        CellMode::Full { .. } => full_batch.reserve(cap),
    }
    let mut row_count = 0u64;
    let mut truncated = false;
    while let Some(row) = rows.next().await {
        let row = row?;
        // Past `max_rows`, or once a cell was too large, the rest of the set is still
        // read, just discarded.
        if cell_error.is_some() {
            continue;
        }
        if row_count as usize >= opts.max_rows {
            truncated = true;
            continue;
        }
        let raws = (0..types.len()).map(|i| row.try_get::<_, Raw>(i).ok().and_then(|r| r.0));
        match opts.cells {
            CellMode::Preview => {
                let values = types
                    .iter()
                    .zip(raws)
                    .map(|(ty, raw)| match (to_value(ty, raw), raw) {
                        // Export keeps every byte (bytea, or a domain over it).
                        (Value::Bytes { len, .. }, Some(raw)) if opts.full_bytes => Value::Bytes {
                            len,
                            preview: raw.to_vec(),
                        },
                        (v, _) => v,
                    })
                    .collect();
                batch.push(values);
                if batch.len() >= batch_size {
                    sink.rows(std::mem::take(&mut batch));
                }
            }
            CellMode::Full { max_cell_bytes } => {
                let values: Vec<FullValue> = types
                    .iter()
                    .zip(raws)
                    .map(|(ty, raw)| to_full_value(ty, raw))
                    .collect();
                if let Some(e) = values
                    .iter()
                    .find_map(|v| check_cell_size(v, max_cell_bytes).err())
                {
                    // Never a cut value: no further row reaches the sink.
                    *cell_error = Some(e);
                    full_batch.clear();
                    continue;
                }
                full_batch.push(values);
                if full_batch.len() >= batch_size {
                    sink.full_rows(std::mem::take(&mut full_batch));
                }
            }
        }
        row_count += 1;
    }
    if !batch.is_empty() {
        sink.rows(batch);
    }
    if !full_batch.is_empty() {
        sink.full_rows(full_batch);
    }
    finish(sink, results, started, None, row_count, truncated);
    Ok(true)
}

fn finish(
    sink: &mut dyn RowSink,
    results: &mut Vec<ResultSummary>,
    started: Instant,
    affected: Option<u64>,
    row_count: u64,
    truncated: bool,
) {
    let elapsed = started.elapsed();
    let result = ResultSummary {
        affected,
        row_count,
        truncated,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    };
    sink.result_end(&result);
    results.push(result);
}

/// How often [`stop_raised`] looks at the stop signal (it has no wake-up of its own).
const STOP_POLL: Duration = Duration::from_millis(20);

/// Completes once `stop` is raised.
async fn stop_raised(stop: &StopSignal) {
    while !stop.is_stopped() {
        tokio::time::sleep(STOP_POLL).await;
    }
}

fn sqlstate(e: &tokio_postgres::Error) -> Option<&str> {
    e.code().map(|c| c.code())
}

fn is_connection_lost(e: &tokio_postgres::Error, client: &Client) -> bool {
    e.is_closed()
        || client.is_closed()
        || io_error(e).is_some()
        || sqlstate(e).is_some_and(|s| LOST_STATES.contains(&s))
}

fn lost(was_in_tx: bool, outcome_unknown: bool, message: String) -> DbError {
    DbError::ConnectionLost {
        was_in_tx,
        outcome_unknown,
        message,
    }
}

/// How a statement error (not a timeout of ours) is reported.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The connection went away: `ConnectionLost` with [`may_have_committed`], even when a
    /// cancel was pending (the statement may have committed before the socket died).
    Lost,
    /// Our cancel was pending; `outcome_unknown` only when the cancel stopped a
    /// non-atomic statement ([`killed_outcome_unknown`]).
    Cancelled { outcome_unknown: bool },
    /// The server's own error, `query_canceled` we did not ask for included (server
    /// `statement_timeout`, a DBA's `pg_cancel_backend`): its message and SQLSTATE are
    /// what the user should see.
    Server,
}

/// `cancelled`: our cancel targeted this call; `query_canceled`: the error is `57014`;
/// `killed_unknown`: [`killed_outcome_unknown`] for the statement.
fn failure_kind(
    conn_lost: bool,
    cancelled: bool,
    query_canceled: bool,
    killed_unknown: bool,
) -> Failure {
    if conn_lost {
        Failure::Lost
    } else if cancelled {
        Failure::Cancelled {
            outcome_unknown: query_canceled && killed_unknown,
        }
    } else {
        Failure::Server
    }
}

/// Whether `sql` starts with the keywords `kw` (comments allowed before and between).
fn starts_with_words(sql: &str, kw: &[&str]) -> bool {
    leading_keywords(sql, Dialect::Postgres, kw.len()) == kw
}

/// `PREPARE TRANSACTION 'id'` ends the current transaction (it is handed to 2PC).
fn is_prepare_transaction(sql: &str) -> bool {
    starts_with_words(sql, &["PREPARE", "TRANSACTION"])
}

/// Whether a statement may have made a durable change by the time its connection went
/// away (or had to be abandoned). Inside a transaction block everything but the
/// statement ending it (`COMMIT`, `PREPARE TRANSACTION`) is rolled back with the
/// connection: `CALL` / `DO` cannot commit there, and statements that commit on their
/// own (`VACUUM`, `CREATE INDEX CONCURRENTLY`, …) cannot run there at all. Outside one,
/// anything but a read or a no-op transaction statement may have committed.
fn may_have_committed(kind: StatementKind, was_in_tx: bool, sql: &str) -> bool {
    match kind {
        StatementKind::Read
        | StatementKind::Transaction(
            TxControl::Begin
            | TxControl::Rollback
            | TxControl::AutocommitOff
            | TxControl::AutocommitOn,
        ) => false,
        StatementKind::Transaction(TxControl::Commit) => true,
        StatementKind::Other if is_prepare_transaction(sql) => true,
        StatementKind::Write | StatementKind::Ddl | StatementKind::Other => !was_in_tx,
    }
}

/// Whether a statement stopped by a cancel request (`query_canceled`, after a timeout or
/// a Cancel click) may still have applied part of its work.
///
/// PostgreSQL runs every statement atomically: one that fails is rolled back as a whole,
/// DDL included, so a stopped statement normally applied nothing. The exceptions are
/// statements that commit internally, outside an explicit transaction block only:
/// - `CALL` of a procedure and `DO` blocks, which may `COMMIT` part of their work;
/// - statements that cannot run in a transaction block because they commit as they go
///   or act outside transactions: `VACUUM`, `CLUSTER` without a table, `CREATE` /
///   `DROP INDEX CONCURRENTLY`, `REINDEX … CONCURRENTLY` and `REINDEX SCHEMA|DATABASE|
///   SYSTEM`, `CREATE` / `DROP DATABASE | TABLESPACE | SUBSCRIPTION`, `ALTER SYSTEM`,
///   `ALTER SUBSCRIPTION`, `ALTER DATABASE … SET TABLESPACE`, `COMMIT` / `ROLLBACK
///   PREPARED`.
///
/// Inside a transaction block none of these can commit (the procedural ones fail on
/// `COMMIT`, the others refuse to run), and the stopped statement aborts the block.
/// Detection is lexical (comments stripped), like `classify`.
fn killed_outcome_unknown(was_in_tx: bool, sql: &str) -> bool {
    !was_in_tx && is_non_atomic(sql)
}

fn is_non_atomic(sql: &str) -> bool {
    let words = leading_keywords(sql, Dialect::Postgres, 6);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let within = |n: usize, kw: &[&str]| w.iter().take(n).any(|x| kw.contains(x));
    match w.as_slice() {
        ["CALL" | "DO" | "VACUUM", ..] => true,
        ["CLUSTER", rest @ ..] => rest.iter().all(|x| *x == "VERBOSE"),
        ["REINDEX", ..] => within(5, &["CONCURRENTLY", "SCHEMA", "DATABASE", "SYSTEM"]),
        ["CREATE" | "DROP", "DATABASE" | "TABLESPACE" | "SUBSCRIPTION", ..] => true,
        ["CREATE" | "DROP", ..] => within(4, &["CONCURRENTLY"]),
        ["ALTER", "SYSTEM" | "SUBSCRIPTION", ..] => true,
        ["ALTER", "DATABASE", ..] => within(6, &["TABLESPACE"]),
        ["COMMIT" | "ROLLBACK", "PREPARED", ..] => true,
        _ => false,
    }
}

/// `ConnectionLost` message suffix naming what losing the connection reset.
fn reset_notes(was_in_tx: bool, session_custom: bool) -> String {
    let mut notes = String::new();
    if was_in_tx {
        notes.push_str("; the transaction was rolled back");
    }
    if session_custom {
        notes.push_str("; session settings, temporary tables and prepared statements were reset");
    }
    notes
}

/// The error for a statement whose connection was abandoned after its timeout (`cause`:
/// [`STUCK`] or [`UNCONFIRMED`]), once the connection was replaced (`Ok`) or replacing it
/// failed (`Err`). `unknown`: it may have committed; `notes`: [`reset_notes`]. Always
/// `ConnectionLost`, so the reset notes and `was_in_tx` reach the user.
fn abandoned_error(
    cause: &str,
    unknown: bool,
    was_in_tx: bool,
    notes: &str,
    replaced: Result<(), DbError>,
) -> DbError {
    match replaced {
        Ok(()) => lost(was_in_tx, unknown, format!("{cause}{notes}")),
        Err(e) => lost(
            was_in_tx,
            unknown,
            format!("{cause}{notes}; reconnecting failed: {e}"),
        ),
    }
}

#[async_trait]
impl Session for PgSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// `opts.max_rows` applies per result set and `opts.timeout` to the statement: an
    /// expired statement is cancelled and `Timeout { Execute }` returned
    /// (`outcome_unknown` when it may have committed part of its work, see
    /// [`killed_outcome_unknown`]). One that ignores the cancel (after a timeout or a
    /// user's cancel) for `opts.cancel_grace` loses its connection: `ConnectionLost`
    /// (see [`abandoned_error`]), and so does one that ended during the grace period
    /// while the timeout's cancel request was not confirmed (see `run_timed`). A user's
    /// cancel request that was never confirmed has the connection replaced before the
    /// next statement is sent (see `cancel.rs`).
    ///
    /// The server's notices (`RAISE NOTICE`, warnings…) that arrive while the statement
    /// runs go to `sink.server_notices` once, before this returns, also on failure.
    async fn execute_one(
        &self,
        stmt: &str,
        kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let mut conn = self.conn.lock().await;
        // Whatever arrived before (introspection, an earlier statement's stragglers).
        take_notices(&self.params.notices);
        let res = self.execute_locked(&mut conn, stmt, kind, opts, sink).await;
        let notices = take_notices(&self.params.notices);
        if !notices.is_empty() {
            sink.server_notices(notices);
        }
        res
    }

    fn tx_state(&self) -> TxState {
        *self.tx.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }

    fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::SeqCst)
    }

    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(PgCanceller {
            target: CancelTarget::new(&self.params),
            state: self.cancel.clone(),
        })
    }

    fn introspector(&self) -> &dyn Introspector {
        self
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.conn
            .lock()
            .await
            .client
            .simple_query("")
            .await
            .map(|_| ())
            .map_err(|e| map_pg_error(e, ConnectStage::Execute))
    }

    async fn close(self: Box<Self>) {
        close_conn(self.conn.into_inner()).await;
    }
}

impl PgSession {
    /// [`Session::execute_one`] once the connection is locked.
    async fn execute_locked(
        &self,
        conn: &mut PgConn,
        stmt: &str,
        kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        // Cancels only apply to this call from here on (see `cancel.rs`).
        let run = self.cancel.begin(&opts.stop, opts.cancel_grace);
        let was_in_tx = self.in_transaction();
        // Stopped before anything was sent.
        let stopped = || run.cancelled() || opts.stop.is_stopped();
        let not_sent = DbError::Cancelled {
            outcome_unknown: false,
            was_in_tx,
        };
        if stopped() {
            return Err(not_sent);
        }
        // An earlier call's cancel request may still be on its way: wait until it was
        // confirmed (harmless from then on) or given up (the connection is poisoned).
        // A Stop (or a cancel aimed at this call) ends the wait: nothing was sent.
        tokio::select! {
            biased;
            () = run.cancel_requested() => return Err(not_sent),
            () = stop_raised(&opts.stop) => return Err(not_sent),
            () = self.cancel.settled() => {}
        }
        let mut outcome = StmtOutcome::default();
        let mut reconnected = false;
        // Found dead before anything was sent (the idle backend was terminated, or an
        // earlier call abandoned it), or poisoned by an unconfirmed cancel request.
        // Reconnecting first is safe for any statement, but silently only when nothing of
        // the old session is lost.
        const CLOSED: &str = "the server closed the connection";
        let replace = if conn.client.is_closed() {
            Some(CLOSED)
        } else if self.cancel.poisoned() {
            Some(UNCONFIRMED_CANCEL)
        } else {
            None
        };
        if let Some(cause) = replace {
            let custom = self.session_custom.load(Ordering::SeqCst);
            let notes = self.notes(was_in_tx);
            if was_in_tx || custom {
                let mut message = format!("{cause}{notes}");
                // Best effort so the next statement finds a live connection.
                if let Err(e) = self.reconnect(conn).await {
                    message.push_str(&format!("; reconnecting failed: {e}"));
                }
                return Err(lost(was_in_tx, false, message));
            }
            reconnected = true;
            if let Err(e) = self.reconnect(conn).await {
                return Err(lost(
                    false,
                    false,
                    format!("{cause}; reconnecting failed: {e}"),
                ));
            }
            if stopped() {
                return Err(not_sent);
            }
        }
        // Only set once rows were emitted, and such a statement is never re-run.
        let mut cell_error = None;
        loop {
            let mut emitted = false;
            let res = self
                .run_timed(
                    conn,
                    stmt,
                    opts,
                    &run,
                    sink,
                    &mut outcome.results,
                    &mut emitted,
                    &mut cell_error,
                )
                .await;
            let err = match res {
                // A statement stopped by a cancel fails with `query_canceled`, so one
                // that completed while a cancel was in flight really completed (and
                // committed outside a transaction): keep its result. A Cancel click
                // raised the stop signal, which stops the next statement; a timeout's
                // confirmed cancel cannot reach it (see `run_timed`).
                Ok((_, late)) => {
                    outcome.late_completion = late;
                    break;
                }
                Err(StmtError::Timeout) => {
                    self.track_failed(kind, stmt, Some(QUERY_CANCELED));
                    return Err(DbError::Timeout {
                        stage: ConnectStage::Execute,
                        outcome_unknown: killed_outcome_unknown(was_in_tx, stmt),
                        was_in_tx,
                    });
                }
                Err(StmtError::Stuck(cause)) => {
                    let notes = self.notes(was_in_tx);
                    let unknown = may_have_committed(kind, was_in_tx, stmt);
                    // No cancel aimed at this call may reach the fresh connection.
                    run.retire();
                    let replaced = self.reconnect(conn).await;
                    return Err(abandoned_error(cause, unknown, was_in_tx, &notes, replaced));
                }
                Err(StmtError::Unconfirmed { failed, cancel }) => {
                    // A success is real (committed outside a transaction); a failure
                    // was atomic unless the statement commits as it goes.
                    let unknown = match failed {
                        None => may_have_committed(kind, was_in_tx, stmt),
                        Some(_) => killed_outcome_unknown(was_in_tx, stmt),
                    };
                    if failed.is_none() {
                        self.track_after(kind, stmt);
                    }
                    // After it: a successful `BEGIN` opened a block that is now lost.
                    let in_tx = self.in_transaction();
                    let notes = self.notes(in_tx);
                    let cause = match &failed {
                        None => format!("{UNCONFIRMED} ({cancel})"),
                        Some(e) => {
                            format!("{UNCONFIRMED} ({cancel}); the statement failed: {e}")
                        }
                    };
                    run.retire();
                    let replaced = self.reconnect(conn).await;
                    return Err(abandoned_error(&cause, unknown, in_tx, &notes, replaced));
                }
                Err(StmtError::Pg(e)) => e,
            };
            let conn_lost = is_connection_lost(&err, &conn.client);
            if !conn_lost {
                // Before the cancel check, so a coinciding cancel cannot skip it.
                // Any other error inside a block aborts it but leaves it open
                // (`Aborted`): the server answers `25P02` until it ends.
                self.track_failed(kind, stmt, sqlstate(&err));
            }
            match failure_kind(
                conn_lost,
                run.cancelled(),
                sqlstate(&err) == Some(QUERY_CANCELED),
                killed_outcome_unknown(was_in_tx, stmt),
            ) {
                Failure::Cancelled { outcome_unknown } => {
                    return Err(DbError::Cancelled {
                        outcome_unknown,
                        was_in_tx,
                    })
                }
                Failure::Server => return Err(map_pg_error(err, ConnectStage::Execute)),
                Failure::Lost => {}
            }
            // Captured before the reconnect resets the session state.
            let notes = self.notes(was_in_tx);
            let custom = self.session_custom.load(Ordering::SeqCst);
            let unknown = may_have_committed(kind, was_in_tx, stmt);
            let cause = format!("{err}{notes}");
            if was_in_tx {
                // The server rolled the transaction back; never replay anything.
                // Best effort so the next statement finds a live connection.
                return Err(match self.reconnect(conn).await {
                    Ok(()) => lost(true, unknown, cause),
                    Err(e) => lost(true, unknown, format!("{cause}; reconnecting failed: {e}")),
                });
            }
            if reconnected {
                return Err(lost(false, unknown, cause));
            }
            reconnected = true;
            if let Err(e) = self.reconnect(conn).await {
                return Err(lost(
                    false,
                    unknown,
                    format!("{cause}; reconnecting failed: {e}"),
                ));
            }
            // Only a read that produced nothing yet, on a session with nothing a
            // reconnect cannot restore, is safe to run again silently.
            if kind != StatementKind::Read || emitted || custom {
                return Err(lost(false, unknown, cause));
            }
            // ... and not when the user asked to stop it (nothing was applied).
            if stopped() {
                return Err(not_sent);
            }
        }
        self.track_after(kind, stmt);
        // The statement ran (and is tracked); only its rows were refused.
        if let Some(e) = cell_error {
            return Err(e);
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_transaction_detection() {
        assert!(is_prepare_transaction("PREPARE TRANSACTION 'x'"));
        assert!(is_prepare_transaction("prepare  transaction 'x'"));
        assert!(is_prepare_transaction("/* 2pc */ PREPARE TRANSACTION 'x'"));
        assert!(!is_prepare_transaction("PREPARE q AS SELECT 1"));
    }

    use StatementKind::{Ddl, Other, Read, Transaction, Write};

    #[test]
    fn reset_notes_name_what_was_lost() {
        assert_eq!(reset_notes(false, false), "");
        assert!(reset_notes(true, false).contains("transaction was rolled back"));
        let both = reset_notes(true, true);
        assert!(both.contains("transaction was rolled back"), "{both}");
        assert!(
            both.contains("session settings, temporary tables and prepared statements"),
            "{both}"
        );
    }

    /// PostgreSQL runs every statement atomically, and inside a transaction block nothing
    /// can commit on its own. Only statements that commit internally (or cannot run in a
    /// transaction block at all) can be left partly applied when stopped.
    #[test]
    fn killed_statement_outcome_is_unknown_only_for_non_atomic_statements() {
        for sql in [
            "CALL p()",
            "/* c */ call p()",
            "DO $$ BEGIN COMMIT; END $$",
            "VACUUM",
            "vacuum (verbose) t",
            "CLUSTER",
            "CLUSTER VERBOSE",
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "CREATE UNIQUE INDEX CONCURRENTLY i ON t (a)",
            "DROP INDEX CONCURRENTLY i",
            "REINDEX TABLE CONCURRENTLY t",
            "REINDEX (VERBOSE) SCHEMA s",
            "REINDEX DATABASE d",
            "CREATE DATABASE d",
            "DROP DATABASE d",
            "CREATE TABLESPACE ts LOCATION '/x'",
            "ALTER SYSTEM SET work_mem = '64MB'",
            "ALTER DATABASE d SET TABLESPACE ts",
            "CREATE SUBSCRIPTION s CONNECTION '' PUBLICATION p",
            "COMMIT PREPARED 'x'",
        ] {
            assert!(killed_outcome_unknown(false, sql), "{sql}");
            // Inside a transaction block none of them can commit anything.
            assert!(!killed_outcome_unknown(true, sql), "{sql}");
        }
        for sql in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
            "ALTER TABLE t ADD b int",
            "CREATE INDEX i ON t (a)",
            "REINDEX TABLE t",
            "CLUSTER t USING i",
            "ALTER DATABASE d SET work_mem = '1MB'",
            "SELECT pg_sleep(5)",
            "-- CALL p()\nSELECT 1",
        ] {
            assert!(!killed_outcome_unknown(false, sql), "{sql}");
        }
    }

    #[test]
    fn statements_that_may_have_committed_when_the_connection_went_away() {
        for (kind, sql) in [
            (Write, "INSERT INTO t VALUES (1)"),
            (Ddl, "CREATE TABLE t (a int)"),
            (Other, "COMMIT PREPARED 'x'"),
            (Transaction(TxControl::Commit), "COMMIT"),
        ] {
            assert!(may_have_committed(kind, false, sql), "{sql}");
        }
        // Inside a transaction only its end (or handing it to 2PC) can be durable.
        for (kind, sql) in [
            (Transaction(TxControl::Commit), "COMMIT"),
            (Transaction(TxControl::Commit), "END"),
            (Other, "PREPARE TRANSACTION 'x'"),
        ] {
            assert!(may_have_committed(kind, true, sql), "{sql}");
        }
        for (kind, in_tx, sql) in [
            (Read, false, "SELECT 1"),
            (Write, true, "UPDATE t SET a = 1"),
            (Write, true, "CALL p()"),
            (Ddl, true, "DROP TABLE t"),
            (Transaction(TxControl::Rollback), true, "ROLLBACK"),
            (Transaction(TxControl::Begin), false, "BEGIN"),
            (
                Transaction(TxControl::AutocommitOff),
                false,
                "SET autocommit = 0",
            ),
        ] {
            assert!(!may_have_committed(kind, in_tx, sql), "{sql}");
        }
    }

    #[test]
    fn a_lost_connection_wins_over_a_pending_cancel() {
        // The socket died after the Cancel click: reported like any other loss (the
        // caller then applies `may_have_committed`), never as a not-applied cancel.
        for (query_canceled, killed_unknown) in [(false, false), (true, true), (false, true)] {
            assert_eq!(
                failure_kind(true, true, query_canceled, killed_unknown),
                Failure::Lost
            );
            assert_eq!(
                failure_kind(true, false, query_canceled, killed_unknown),
                Failure::Lost
            );
        }
        // Stopped by our cancel: the outcome depends on the statement.
        assert_eq!(
            failure_kind(false, true, true, true),
            Failure::Cancelled {
                outcome_unknown: true
            }
        );
        assert_eq!(
            failure_kind(false, true, true, false),
            Failure::Cancelled {
                outcome_unknown: false
            }
        );
        // Failed on its own while a cancel was pending: nothing was killed.
        assert_eq!(
            failure_kind(false, true, false, true),
            Failure::Cancelled {
                outcome_unknown: false
            }
        );
    }

    #[test]
    fn query_canceled_we_did_not_ask_for_is_a_server_error() {
        // Server `statement_timeout` or a DBA's `pg_cancel_backend`: the server's own
        // message and SQLSTATE reach the user.
        assert_eq!(failure_kind(false, false, true, true), Failure::Server);
        assert_eq!(failure_kind(false, false, true, false), Failure::Server);
        assert_eq!(failure_kind(false, false, false, false), Failure::Server);
    }

    fn refused() -> DbError {
        DbError::Connect {
            message: "connection refused".into(),
        }
    }

    #[test]
    fn abandoned_statement_is_always_connection_lost() {
        let notes = reset_notes(true, true);
        match abandoned_error(UNCONFIRMED, false, true, &notes, Ok(())) {
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message,
            } => {
                assert!(message.starts_with(UNCONFIRMED), "{message}");
                assert!(message.contains("rolled back"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        match abandoned_error(STUCK, true, true, &notes, Ok(())) {
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: true,
                message,
            } => {
                assert!(message.starts_with(STUCK), "{message}");
                assert!(message.contains("rolled back"), "{message}");
                assert!(message.contains("temporary tables"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            abandoned_error(STUCK, false, false, "", Ok(())),
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                ..
            }
        ));
        match abandoned_error(STUCK, true, false, "", Err(refused())) {
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                message,
            } => {
                assert!(message.starts_with(STUCK), "{message}");
                assert!(
                    message.contains("reconnecting failed: ")
                        && message.contains("connection refused"),
                    "{message}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
