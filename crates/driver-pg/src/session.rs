use crate::cancel::{send_cancel, CancelState, PgCanceller, CANCEL_TIMEOUT};
use crate::values::{column_meta, to_value, Raw};
use crate::{close_conn, io_error, map_pg_error, open, ConnectParams, PgConn};
use async_trait::async_trait;
use dnt_core::driver::{
    CancelHandle, ExecOptions, ExecSummary, Introspector, ResultSummary, RowSink, Session,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::ServerInfo;
use dnt_core::sql::{classify, split, strip_comments, Dialect, StatementKind, TxControl};
use dnt_core::value::{ColumnMeta, Value};
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// One PostgreSQL connection. It is its own [`Introspector`] (see `introspect.rs`).
pub struct PgSession {
    pub(crate) conn: Mutex<PgConn>,
    info: ServerInfo,
    /// Settings of the established connection (TLS fallback applied); used to reconnect
    /// and by cancel requests.
    params: ConnectParams,
    /// Whether a transaction block is open (possibly aborted by an error: PostgreSQL then
    /// rejects everything with `25P02` until `ROLLBACK`). Changed only once the server
    /// answered a transaction-control statement ([`tx_state_after`]); DDL is
    /// transactional and errors leave the block open, so neither clears it.
    in_tx: AtomicBool,
    cancel: Arc<CancelState>,
    /// Last session-level `SET search_path …` that succeeded, re-applied after a reconnect.
    search_path: std::sync::Mutex<Option<String>>,
    /// Set once a statement changed session state that a reconnect cannot restore
    /// (see [`changes_session_state`]); cleared when the connection is replaced. While
    /// set, a lost read is not silently re-run and `ConnectionLost` says what was reset.
    session_custom: AtomicBool,
}

/// How one statement attempt failed.
enum StmtError {
    Pg(tokio_postgres::Error),
    /// `opts.timeout` expired and the cancel request stopped the statement
    /// (`query_canceled`); whether it applied anything is [`killed_outcome_unknown`].
    Timeout,
    /// `opts.timeout` expired and the statement did not stop after the cancel request:
    /// its completion was never observed and the connection must be abandoned.
    Stuck,
}

impl PgSession {
    pub(crate) fn new(conn: PgConn, info: ServerInfo, params: ConnectParams) -> Self {
        let cancel = Arc::new(CancelState::new(conn.client.cancel_token()));
        PgSession {
            conn: Mutex::new(conn),
            info,
            params,
            in_tx: AtomicBool::new(false),
            cancel,
            search_path: std::sync::Mutex::new(None),
            session_custom: AtomicBool::new(false),
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

    /// Tracking applied after a statement succeeded: transaction state, the last
    /// `search_path` and whether session state was customised.
    fn track_after(&self, kind: StatementKind, sql: &str, was_in_tx: bool) {
        if let Some(in_tx) = tx_state_after(kind, sql, was_in_tx, None) {
            self.in_tx.store(in_tx, Ordering::SeqCst);
        }
        if kind == StatementKind::Other {
            if let Some(change) = search_path_change(sql) {
                *self.search_path.lock().unwrap_or_else(|p| p.into_inner()) = change;
            }
        }
        if changes_session_state(sql) {
            self.session_custom.store(true, Ordering::SeqCst);
        }
    }

    /// Transaction tracking for a statement the server answered with an error
    /// (`state`: its SQLSTATE). Not for a lost connection: reconnecting resets it.
    fn track_failed(&self, kind: StatementKind, sql: &str, was_in_tx: bool, state: Option<&str>) {
        if let Some(in_tx) = tx_state_after(kind, sql, was_in_tx, Some(state)) {
            self.in_tx.store(in_tx, Ordering::SeqCst);
        }
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
        self.in_tx.store(false, Ordering::SeqCst);
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
        Ok(())
    }

    /// Runs one statement, enforcing `opts.timeout` per statement (not per script).
    ///
    /// On expiry a cancel request is sent and the statement awaited for a grace period so
    /// the connection stays in sync. PostgreSQL reports a statement stopped by a cancel
    /// with `query_canceled` (`Timeout`); a statement that completes during the grace
    /// period instead (it finished first, or swallowed the cancel) reports what really
    /// happened. A statement still running after the grace period is `Stuck`. Returns
    /// whether the statement produced a result set (had columns).
    async fn run_timed(
        &self,
        conn: &PgConn,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        summary: &mut ExecSummary,
        emitted: &mut bool,
    ) -> Result<bool, StmtError> {
        let Some(limit) = opts.timeout else {
            return run_statement(&conn.client, sql, opts, sink, summary, emitted)
                .await
                .map_err(StmtError::Pg);
        };
        let mut fut = std::pin::pin!(run_statement(
            &conn.client,
            sql,
            opts,
            sink,
            summary,
            emitted
        ));
        if let Ok(res) = tokio::time::timeout(limit, fut.as_mut()).await {
            return res.map_err(StmtError::Pg);
        }
        let _ = send_cancel(&self.cancel.token(), &self.params.tls).await;
        match tokio::time::timeout(CANCEL_TIMEOUT, fut.as_mut()).await {
            Err(_) => Err(StmtError::Stuck),
            Ok(Ok(had_columns)) => Ok(had_columns),
            Ok(Err(e)) if sqlstate(&e) == Some(QUERY_CANCELED) => Err(StmtError::Timeout),
            Ok(Err(e)) => Err(StmtError::Pg(e)),
        }
    }
}

/// Prepares one statement (for its column types), then streams its rows into `sink`, or
/// runs it for its affected-row count when it returns no columns.
async fn run_statement(
    client: &Client,
    sql: &str,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    summary: &mut ExecSummary,
    emitted: &mut bool,
) -> Result<bool, tokio_postgres::Error> {
    let started = Instant::now();
    let stmt = client.prepare(sql).await?;
    let cols = stmt.columns();
    if cols.is_empty() {
        let affected = client.execute(&stmt, &[]).await?;
        sink.result_start(&[]);
        *emitted = true;
        finish(sink, summary, started, Some(affected), 0, false);
        return Ok(false);
    }
    let metas: Vec<ColumnMeta> = cols.iter().map(column_meta).collect();
    let types: Vec<Type> = cols.iter().map(|c| c.type_().clone()).collect();
    let no_params: [&(dyn ToSql + Sync); 0] = [];
    let rows = client.query_raw(&stmt, no_params).await?;
    sink.result_start(&metas);
    *emitted = true;
    let batch_size = opts.batch_size.max(1);
    let mut rows = std::pin::pin!(rows);
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(batch_size.min(opts.max_rows.max(1)));
    let mut row_count = 0u64;
    let mut truncated = false;
    while let Some(row) = rows.next().await {
        let row = row?;
        // Past `max_rows` the rest of the set is still read, just discarded.
        if row_count as usize >= opts.max_rows {
            truncated = true;
            continue;
        }
        let values = types
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                let raw = row.try_get::<_, Raw>(i).ok().and_then(|r| r.0);
                to_value(ty, raw)
            })
            .collect();
        batch.push(values);
        row_count += 1;
        if batch.len() >= batch_size {
            sink.rows(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        sink.rows(batch);
    }
    finish(sink, summary, started, None, row_count, truncated);
    Ok(true)
}

fn finish(
    sink: &mut dyn RowSink,
    summary: &mut ExecSummary,
    started: Instant,
    affected: Option<u64>,
    row_count: u64,
    truncated: bool,
) {
    let elapsed = started.elapsed();
    sink.result_end(affected, elapsed);
    summary.results.push(ResultSummary {
        affected,
        row_count,
        truncated,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    });
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

/// `killed_unknown`: [`killed_outcome_unknown`] for the statement that failed.
fn map_exec_error(e: tokio_postgres::Error, killed_unknown: bool) -> DbError {
    if sqlstate(&e) == Some(QUERY_CANCELED) {
        return DbError::Cancelled {
            outcome_unknown: killed_unknown,
        };
    }
    map_pg_error(e, ConnectStage::Execute)
}

/// Upper-cased leading words of `sql` with comments removed (at most `n`). Non-ASCII
/// letters count as word characters, so `ưư SET …` does not start with `SET`.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    strip_comments(sql, Dialect::Postgres)
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Whether `sql` starts with the keywords `kw` (comments allowed before and between).
fn starts_with_words(sql: &str, kw: &[&str]) -> bool {
    leading_words(sql, kw.len()) == kw
}

/// `PREPARE TRANSACTION 'id'` ends the current transaction (it is handed to 2PC).
fn is_prepare_transaction(sql: &str) -> bool {
    starts_with_words(sql, &["PREPARE", "TRANSACTION"])
}

/// `COMMIT` / `END` / `ROLLBACK` / `ABORT … AND CHAIN`: a new transaction starts at once.
fn is_and_chain(sql: &str) -> bool {
    let words = leading_words(sql, 5);
    words.windows(2).any(|w| w[0] == "AND" && w[1] == "CHAIN")
}

/// The transaction state after a transaction-control statement was answered: `failed`
/// is `None` when it succeeded, else `Some(sqlstate)` of its error (`57014` for one
/// stopped by a cancel). `None` when the statement does not change the state.
///
/// - `BEGIN` opens a block only when it succeeds.
/// - `COMMIT` / `END` / `ROLLBACK` / `ABORT` close it (an `AND CHAIN` variant opens the
///   next one at once). A failed `COMMIT` (deferred constraint, serialization failure,
///   cancel) still ended the transaction: the server rolled it back. Only one rejected
///   before it ran (class 42, syntax error or access rule violation) left the block as
///   it was (aborted, still open).
/// - `PREPARE TRANSACTION` hands the transaction to 2PC; when it fails it becomes a
///   `ROLLBACK`.
/// - PostgreSQL has no `autocommit` setting: `SET autocommit …` (classified as
///   transaction control for MySQL) is rejected by the server and never tracked.
fn tx_state_after(
    kind: StatementKind,
    sql: &str,
    was_in_tx: bool,
    failed: Option<Option<&str>>,
) -> Option<bool> {
    match kind {
        StatementKind::Transaction(TxControl::Begin) => Some(failed.is_none() || was_in_tx),
        StatementKind::Transaction(TxControl::Commit | TxControl::Rollback) => Some(match failed {
            None => is_and_chain(sql),
            Some(state) if state.is_some_and(|s| s.starts_with("42")) => was_in_tx,
            Some(_) => false,
        }),
        StatementKind::Other if is_prepare_transaction(sql) => Some(match failed {
            Some(Some(s)) if s.starts_with("42") => was_in_tx,
            _ => false,
        }),
        _ => None,
    }
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
    let words = leading_words(sql, 6);
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

/// Lexically: does a statement that succeeded change session state that a reconnect
/// cannot restore? Session-level `SET` other than the tracked `search_path` (not `SET
/// LOCAL`, `SET TRANSACTION`, `SET CONSTRAINTS`, which end with the transaction),
/// temporary tables / views / sequences, named prepared statements (`PREPARE`),
/// `LISTEN` and cursors (`DECLARE`). Settings changed through functions (`set_config`)
/// are not detected.
fn changes_session_state(sql: &str) -> bool {
    let words = leading_words(sql, 4);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["SET", "LOCAL" | "TRANSACTION" | "CONSTRAINTS" | "SEARCH_PATH", ..] => false,
        ["SET", "SESSION", "SEARCH_PATH", ..] => false,
        ["SET", _, ..] => true,
        ["CREATE", "TEMP" | "TEMPORARY", ..]
        | ["CREATE", "GLOBAL" | "LOCAL", "TEMP" | "TEMPORARY", ..]
        | ["CREATE", "OR", "REPLACE", "TEMP" | "TEMPORARY"] => true,
        ["PREPARE", second, ..] => *second != "TRANSACTION",
        ["LISTEN" | "DECLARE", ..] => true,
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

/// The error for a statement that ignored the cancel request after its timeout, once
/// its connection was abandoned and replaced (`Ok`) or replacing it failed (`Err`).
/// `unknown`: it may have committed ([`may_have_committed`]); `notes`: [`reset_notes`].
/// Always `ConnectionLost`, so the reset notes and `was_in_tx` reach the user.
fn stuck_error(
    unknown: bool,
    was_in_tx: bool,
    notes: &str,
    replaced: Result<(), DbError>,
) -> DbError {
    match replaced {
        Ok(()) => lost(was_in_tx, unknown, format!("{STUCK}{notes}")),
        Err(e) => lost(
            was_in_tx,
            unknown,
            format!("{STUCK}{notes}; reconnecting failed: {e}"),
        ),
    }
}

/// For a session-level `search_path` change: `Some(Some(stmt))` for `SET [SESSION]
/// search_path …` (the statement, comments removed, is replayed verbatim), `Some(None)`
/// for `RESET search_path` / `RESET ALL`, `None` for anything else (`SET LOCAL` is
/// transaction-scoped and not remembered).
fn search_path_change(sql: &str) -> Option<Option<String>> {
    let words = leading_words(sql, 3);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["SET", "SEARCH_PATH", ..] | ["SET", "SESSION", "SEARCH_PATH"] => Some(Some(
            strip_comments(sql, Dialect::Postgres).trim().to_string(),
        )),
        ["RESET", "SEARCH_PATH" | "ALL", ..] => Some(None),
        _ => None,
    }
}

#[async_trait]
impl Session for PgSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// `opts.max_rows` applies per result set and `opts.timeout` per statement (not to the
    /// whole script): an expired statement is cancelled and `Timeout { Execute }` returned
    /// (`outcome_unknown` when it may have committed part of its work, see
    /// [`killed_outcome_unknown`]). One that ignores the cancel loses its connection:
    /// `ConnectionLost` (see [`stuck_error`]).
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<ExecSummary, DbError> {
        let mut conn = self.conn.lock().await;
        // Cancels only apply to this call from here on (see `cancel.rs`).
        let run = self.cancel.begin();
        let mut summary = ExecSummary::default();
        let mut reconnected = false;
        for stmt in split(sql, Dialect::Postgres) {
            if run.cancelled() {
                return Err(DbError::cancelled());
            }
            let kind = classify(&stmt.text, Dialect::Postgres);
            let was_in_tx = self.in_transaction();
            // Found dead before anything was sent (the idle backend was terminated, or
            // an earlier call abandoned it). Reconnecting first is safe for any
            // statement, but silently only when nothing of the old session is lost.
            if conn.client.is_closed() {
                const CLOSED: &str = "the server closed the connection";
                let custom = self.session_custom.load(Ordering::SeqCst);
                let notes = self.notes(was_in_tx);
                if was_in_tx || custom || reconnected {
                    let mut message = format!("{CLOSED}{notes}");
                    if !reconnected {
                        // Best effort so the next statement finds a live connection.
                        if let Err(e) = self.reconnect(&mut conn).await {
                            message.push_str(&format!("; reconnecting failed: {e}"));
                        }
                    }
                    return Err(lost(was_in_tx, false, message));
                }
                reconnected = true;
                if let Err(e) = self.reconnect(&mut conn).await {
                    return Err(lost(
                        false,
                        false,
                        format!("{CLOSED}; reconnecting failed: {e}"),
                    ));
                }
                if run.cancelled() {
                    return Err(DbError::cancelled());
                }
            }
            loop {
                let mut emitted = false;
                let res = self
                    .run_timed(&conn, &stmt.text, &opts, sink, &mut summary, &mut emitted)
                    .await;
                let err = match res {
                    // A statement stopped by a cancel fails with `query_canceled`, so one
                    // that completed while a cancel was in flight really completed (and
                    // committed outside a transaction): keep its result; the flag stops
                    // the next statement.
                    Ok(_) => break,
                    Err(StmtError::Timeout) => {
                        self.track_failed(kind, &stmt.text, was_in_tx, Some(QUERY_CANCELED));
                        return Err(DbError::Timeout {
                            stage: ConnectStage::Execute,
                            outcome_unknown: killed_outcome_unknown(was_in_tx, &stmt.text),
                        });
                    }
                    Err(StmtError::Stuck) => {
                        let notes = self.notes(was_in_tx);
                        let unknown = may_have_committed(kind, was_in_tx, &stmt.text);
                        let replaced = self.reconnect(&mut conn).await;
                        return Err(stuck_error(unknown, was_in_tx, &notes, replaced));
                    }
                    Err(StmtError::Pg(e)) => e,
                };
                let conn_lost = is_connection_lost(&err, &conn.client);
                if !conn_lost {
                    // Before the cancel check, so a coinciding cancel cannot skip it.
                    // Any other error inside a block aborts it but leaves it open
                    // (`in_tx` kept): the server answers `25P02` until `ROLLBACK`.
                    self.track_failed(kind, &stmt.text, was_in_tx, sqlstate(&err));
                }
                let killed_unknown = killed_outcome_unknown(was_in_tx, &stmt.text);
                if run.cancelled() {
                    // A statement the cancel stopped (or whose connection went away) may
                    // have applied part of its work; one that failed on its own did not.
                    let killed = sqlstate(&err) == Some(QUERY_CANCELED) || conn_lost;
                    return Err(DbError::Cancelled {
                        outcome_unknown: killed && killed_unknown,
                    });
                }
                if !conn_lost {
                    return Err(map_exec_error(err, killed_unknown));
                }
                // Captured before the reconnect resets the session state.
                let notes = self.notes(was_in_tx);
                let custom = self.session_custom.load(Ordering::SeqCst);
                let unknown = may_have_committed(kind, was_in_tx, &stmt.text);
                let cause = format!("{err}{notes}");
                if was_in_tx {
                    // The server rolled the transaction back; never replay anything.
                    // Best effort so the next statement finds a live connection.
                    return Err(match self.reconnect(&mut conn).await {
                        Ok(()) => lost(true, unknown, cause),
                        Err(e) => lost(true, unknown, format!("{cause}; reconnecting failed: {e}")),
                    });
                }
                if reconnected {
                    return Err(lost(false, unknown, cause));
                }
                reconnected = true;
                if let Err(e) = self.reconnect(&mut conn).await {
                    return Err(lost(
                        false,
                        unknown,
                        format!("{cause}; reconnecting failed: {e}"),
                    ));
                }
                if run.cancelled() {
                    return Err(DbError::cancelled());
                }
                // Only a read that produced nothing yet, on a session with nothing a
                // reconnect cannot restore, is safe to run again silently.
                if kind != StatementKind::Read || emitted || custom {
                    return Err(lost(false, unknown, cause));
                }
            }
            self.track_after(kind, &stmt.text, was_in_tx);
        }
        Ok(summary)
    }

    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(PgCanceller {
            tls: self.params.tls.clone(),
            state: self.cancel.clone(),
        })
    }

    fn introspector(&self) -> &dyn Introspector {
        self
    }

    fn in_transaction(&self) -> bool {
        self.in_tx.load(Ordering::SeqCst)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_path_statements() {
        let set = |s: &str| Some(Some(s.to_string()));
        assert_eq!(
            search_path_change("SET search_path TO sales, public"),
            set("SET search_path TO sales, public")
        );
        assert_eq!(
            search_path_change("  set Search_Path=sales;"),
            set("set Search_Path=sales;")
        );
        assert_eq!(
            search_path_change("SET SESSION search_path = x"),
            set("SET SESSION search_path = x")
        );
        assert_eq!(search_path_change("RESET search_path"), Some(None));
        assert_eq!(search_path_change("reset all"), Some(None));
        assert_eq!(search_path_change("SET LOCAL search_path = x"), None);
        assert_eq!(search_path_change("SET search_path_x = 1"), None);
        assert_eq!(search_path_change("SET statement_timeout = 0"), None);
        assert_eq!(search_path_change("SELECT 1"), None);
    }

    #[test]
    fn prepare_transaction_detection() {
        assert!(is_prepare_transaction("PREPARE TRANSACTION 'x'"));
        assert!(is_prepare_transaction("prepare  transaction 'x'"));
        assert!(is_prepare_transaction("/* 2pc */ PREPARE TRANSACTION 'x'"));
        assert!(!is_prepare_transaction("PREPARE q AS SELECT 1"));
    }

    #[test]
    fn search_path_behind_comments_is_tracked() {
        let set = |s: &str| Some(Some(s.to_string()));
        assert_eq!(
            search_path_change("/* pick */ SET search_path TO sales, public"),
            set("SET search_path TO sales, public")
        );
        assert_eq!(
            search_path_change("-- pick\nSET search_path = sales -- why"),
            set("SET search_path = sales")
        );
        assert_eq!(
            search_path_change("/* a */ /* b */ RESET search_path"),
            Some(None)
        );
        assert_eq!(
            search_path_change("/* SET search_path = x */ SELECT 1"),
            None
        );
        // A comment inside the value is dropped from the replayed statement.
        assert_eq!(
            search_path_change("SET search_path = /* x */ sales"),
            set("SET search_path =   sales")
        );
    }

    use StatementKind::{Ddl, Other, Read, Transaction, Write};

    #[test]
    fn session_state_changes_are_lexical() {
        for sql in [
            "SET statement_timeout = 0",
            "set work_mem to '64MB'",
            "/* c */ SET SESSION statement_timeout = 0",
            "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET ROLE dnt_limited",
            "SET TIME ZONE 'UTC'",
            "CREATE TEMP TABLE t (a int)",
            "create temporary table t (a int)",
            "CREATE GLOBAL TEMPORARY TABLE t (a int)",
            "CREATE LOCAL TEMP TABLE t (a int)",
            "CREATE OR REPLACE TEMP VIEW v AS SELECT 1",
            "PREPARE q AS SELECT 1",
            "LISTEN chan",
            "DECLARE c CURSOR WITH HOLD FOR SELECT 1",
        ] {
            assert!(changes_session_state(sql), "{sql}");
        }
        for sql in [
            "SELECT 1",
            // Tracked and re-applied.
            "SET search_path TO sales",
            "SET SESSION search_path = sales",
            // Transaction-scoped.
            "SET LOCAL statement_timeout = 0",
            "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "SET CONSTRAINTS ALL DEFERRED",
            "PREPARE TRANSACTION 'x'",
            "CREATE TABLE t (a int)",
            "INSERT INTO t SELECT 'SET x = 1'",
            "-- SET x = 1\nSELECT 1",
            "ưư SET x = 1",
        ] {
            assert!(!changes_session_state(sql), "{sql}");
        }
    }

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
    fn transaction_state_changes_only_as_the_server_applied_it() {
        let ok = None;
        let failed = |state: &'static str| Some(Some(state));
        let begin = Transaction(TxControl::Begin);
        let commit = Transaction(TxControl::Commit);
        let rollback = Transaction(TxControl::Rollback);
        assert_eq!(tx_state_after(begin, "BEGIN", false, ok), Some(true));
        // A rejected BEGIN opens nothing (and inside a block leaves it open).
        assert_eq!(
            tx_state_after(begin, "BEGIN ISOLATION LEVEL x", false, failed("42601")),
            Some(false)
        );
        assert_eq!(
            tx_state_after(begin, "BEGIN", true, failed("25001")),
            Some(true)
        );
        assert_eq!(tx_state_after(commit, "COMMIT", true, ok), Some(false));
        assert_eq!(
            tx_state_after(commit, "COMMIT AND CHAIN", true, ok),
            Some(true)
        );
        assert_eq!(
            tx_state_after(commit, "COMMIT AND NO CHAIN", true, ok),
            Some(false)
        );
        assert_eq!(
            tx_state_after(rollback, "/* x */ ROLLBACK AND CHAIN", true, ok),
            Some(true)
        );
        // A COMMIT that failed (deferred constraint, serialization failure, cancel)
        // still ended the transaction ...
        assert_eq!(
            tx_state_after(commit, "COMMIT", true, failed("23503")),
            Some(false)
        );
        assert_eq!(
            tx_state_after(commit, "COMMIT", true, Some(None)),
            Some(false)
        );
        // ... one the server could not even parse did not.
        assert_eq!(
            tx_state_after(commit, "COMMIT AND x", true, failed("42601")),
            Some(true)
        );
        // A failed PREPARE TRANSACTION becomes a ROLLBACK.
        assert_eq!(
            tx_state_after(Other, "PREPARE TRANSACTION 'x'", true, failed("XX000")),
            Some(false)
        );
        assert_eq!(
            tx_state_after(Other, "PREPARE TRANSACTION 'x'", true, ok),
            Some(false)
        );
        // PostgreSQL has no autocommit setting (the server rejects the SET); never
        // tracked, whatever the outcome.
        for (sql, kind) in [
            ("SET autocommit = 0", Transaction(TxControl::AutocommitOff)),
            ("SET autocommit = on", Transaction(TxControl::AutocommitOn)),
        ] {
            for in_tx in [false, true] {
                assert_eq!(tx_state_after(kind, sql, in_tx, ok), None, "{sql}");
                assert_eq!(
                    tx_state_after(kind, sql, in_tx, failed("42704")),
                    None,
                    "{sql}"
                );
            }
        }
        assert_eq!(
            tx_state_after(Write, "INSERT …", true, failed("23505")),
            None
        );
        assert_eq!(tx_state_after(Read, "SELECT 1", false, ok), None);
    }

    fn refused() -> DbError {
        DbError::Connect {
            message: "connection refused".into(),
        }
    }

    #[test]
    fn stuck_statement_is_always_connection_lost() {
        let notes = reset_notes(true, true);
        match stuck_error(true, true, &notes, Ok(())) {
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
            stuck_error(false, false, "", Ok(())),
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: false,
                ..
            }
        ));
        match stuck_error(true, false, "", Err(refused())) {
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
