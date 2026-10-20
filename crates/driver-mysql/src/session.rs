use crate::cancel::{kill_query, CancelState, MySqlCanceller, RunGuard};
use crate::map_mysql_error;
use crate::queries::quote_ident;
#[cfg(feature = "test-helpers")]
use crate::values::value_text;
use crate::values::{column_meta, to_value};
use async_trait::async_trait;
use dnt_core::driver::{
    CancelHandle, ExecOptions, ExecProgress, Introspector, ResultSummary, RowSink, Session,
    SharedSink, StmtOutcome,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::{DbKind, MySqlCapabilities, ServerInfo};
use dnt_core::sql::{
    leading_keywords, readings, session_effect, Dialect, MySqlMode, StatementKind, TxControl,
};
use dnt_core::tx::{next_autocommit_off, next_tx_state, StmtResult, TxState};
use dnt_core::value::ColumnMeta;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, Error as MyError, Opts};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

/// Bound on re-opening the connection after it was lost (same as the connect timeout).
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an abandoned connection may take to say goodbye (`COM_QUIT`) before its
/// socket is just closed.
const ABANDON_TIMEOUT: Duration = Duration::from_secs(1);

/// Server error: "Query execution was interrupted" (`KILL QUERY`).
const ER_QUERY_INTERRUPTED: u16 = 1317;
/// Error codes meaning the connection is gone: client-side "server has gone away" /
/// "lost connection" (2006, 2013), MySQL 8 "disconnected by the client" (4031) and
/// MariaDB "Connection was killed" (1927).
const LOST_CODES: [u16; 4] = [2006, 2013, 4031, 1927];
/// The subset of [`LOST_CODES`] meaning the statement was never run (see
/// [`lost_before_sent`]). Only 4031, which the server sends before reading the next
/// command; 2006 is a client code and does not prove the statement never ran.
const LOST_BEFORE_SENT: [u16; 1] = [4031];

/// `ConnectionLost` message for a statement that ignored `KILL QUERY` after its timeout.
const STUCK: &str = "the statement did not stop after the timeout, so its connection was closed";
/// `ConnectionLost` message for a statement that ignored the user's cancel.
const STUCK_CANCEL: &str =
    "the statement did not stop after it was cancelled, so its connection was closed";
/// `ConnectionLost` message for a statement that found its connection poisoned: an
/// earlier statement ended during its grace period while the `KILL QUERY` sent after its
/// timeout was still unanswered (see `run_timed`).
const UNCONFIRMED: &str = "an earlier KILL QUERY could not be confirmed, so the connection \
                           was replaced before it could hit this statement";

/// One MySQL-family connection. It is its own [`Introspector`] (see `introspect.rs`).
pub struct MySqlSession {
    /// The physical connection. `None` once it was abandoned (a statement ignored
    /// `KILL QUERY` past its timeout, or the connection was lost) and opening a
    /// replacement failed: `execute` opens a new one before its first statement, other
    /// calls report `ConnectionLost`.
    pub(crate) conn: Mutex<Option<Conn>>,
    info: ServerInfo,
    caps: MySqlCapabilities,
    /// Options of the established connection (TLS fallback applied); used to reconnect
    /// and by the canceller's temporary connection.
    opts: Opts,
    /// Transaction state, changed only as the server answered each statement
    /// ([`next_tx_state`]). Reset to `Idle` by a reconnect (the server rolled the
    /// transaction back); [`Session::tx_state`] adds `autocommit_off`.
    tx: std::sync::Mutex<TxState>,
    /// Set while the user has `SET autocommit = 0` ([`next_autocommit_off`]). A session
    /// setting rather than transaction state: it survives a reconnect (re-applied on the
    /// new connection).
    autocommit_off: AtomicBool,
    /// The lexing flags of the session's `sql_mode`, as last read from the server: on
    /// connect, after every reconnect and after every statement that may change it.
    /// Changes `session_effect` does not see are not tracked: `SET sql_mode` run through
    /// `PREPARE` / `EXECUTE`, and MariaDB `SET STATEMENT sql_mode=… FOR …` (which only
    /// lasts for its own statement anyway).
    mode: std::sync::Mutex<MySqlMode>,
    /// Set when reading the `sql_mode` after a statement failed (e.g. a Stop's
    /// `KILL QUERY` landed in it): `mode` may be out of date, so the next
    /// `execute_one` reads it again before lexing its statement.
    mode_stale: AtomicBool,
    cancel: Arc<CancelState>,
    /// Database of the last successful `USE`, re-applied after a reconnect.
    last_db: std::sync::Mutex<Option<String>>,
    /// Set once a statement changed session state that a reconnect cannot restore
    /// (see [`session_effect`]); cleared on a fresh connection. While set, a lost
    /// read is not silently re-run and `ConnectionLost` says the state was reset.
    session_custom: AtomicBool,
    /// Successful [`MySqlSession::reconnect`]s ([`Session::reconnects`]).
    reconnects: AtomicU64,
}

/// How one statement attempt failed.
enum StmtError {
    My(MyError),
    /// `opts.timeout` expired and the statement was stopped by the `KILL QUERY` (whether
    /// part of it was applied is [`killed_outcome_unknown`]).
    Timeout,
    /// The statement did not stop within `opts.cancel_grace` after `KILL QUERY` (sent at
    /// its timeout, or by the user's cancel): its completion was never observed and the
    /// connection must be abandoned. Holds the `ConnectionLost` message ([`STUCK`] or
    /// [`STUCK_CANCEL`]).
    Stuck(&'static str),
}

/// A statement that ran to its end.
struct Done {
    /// Whether any result had columns.
    had_columns: bool,
    /// It completed within the cancel grace after its timeout.
    late_completion: bool,
}

/// What made `run_timed` stop waiting for the statement alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Timeout,
    Cancel,
}

impl MySqlSession {
    /// `mode`: the connection's `sql_mode` flags ([`read_sql_mode`]).
    pub(crate) fn new(
        conn: Conn,
        info: ServerInfo,
        caps: MySqlCapabilities,
        opts: Opts,
        mode: MySqlMode,
    ) -> Self {
        let cancel = Arc::new(CancelState::default());
        cancel.set_connection(conn.id());
        MySqlSession {
            conn: Mutex::new(Some(conn)),
            info,
            caps,
            opts,
            tx: std::sync::Mutex::new(TxState::Idle),
            autocommit_off: AtomicBool::new(false),
            mode: std::sync::Mutex::new(mode),
            mode_stale: AtomicBool::new(false),
            cancel,
            last_db: std::sync::Mutex::new(None),
            session_custom: AtomicBool::new(false),
            reconnects: AtomicU64::new(0),
        }
    }

    pub fn capabilities(&self) -> MySqlCapabilities {
        self.caps
    }

    /// Locks the connection for a call that does not reconnect (metadata, ping).
    pub(crate) async fn live_conn(&self) -> Result<MappedMutexGuard<'_, Conn>, DbError> {
        MutexGuard::try_map(self.conn.lock().await, Option::as_mut).map_err(|_| {
            lost(
                false,
                false,
                "the connection was closed after an earlier failure".into(),
            )
        })
    }

    /// Test-only (`test-helpers` feature): runs one statement and returns its first row as
    /// text (`None` for SQL NULL). Bypasses transaction tracking; `execute` is the real path.
    #[cfg(feature = "test-helpers")]
    pub async fn query_row_text(&self, sql: &str) -> Result<Option<Vec<Option<String>>>, DbError> {
        let mut conn = self.live_conn().await?;
        let row: Option<mysql_async::Row> = conn
            .query_first(sql)
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::Execute))?;
        Ok(row.map(|r| r.unwrap().into_iter().map(value_text).collect()))
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

    fn mode(&self) -> MySqlMode {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Records flags just read from the server (no longer stale).
    fn set_mode(&self, mode: MySqlMode) {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner()) = mode;
        self.mode_stale.store(false, Ordering::SeqCst);
    }

    /// Reads the `sql_mode` of `conn` into `mode`; on failure the flags stay as they
    /// were, marked stale (read again by the next `execute_one`).
    async fn refresh_mode(&self, conn: &mut Conn) {
        match read_sql_mode(conn).await {
            Ok(mode) => self.set_mode(mode),
            Err(_) => self.mode_stale.store(true, Ordering::SeqCst),
        }
    }

    /// Test-only (`test-helpers` feature): puts the session in the state a failed
    /// `sql_mode` read leaves behind, with `assumed` as the out-of-date flags.
    #[cfg(feature = "test-helpers")]
    pub fn mark_mode_stale(&self, assumed: MySqlMode) {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner()) = assumed;
        self.mode_stale.store(true, Ordering::SeqCst);
    }

    /// Applies [`next_tx_state`] and [`next_autocommit_off`] for a statement that ended
    /// with `result`, both reading the `autocommit` mode as it was before it.
    fn track_tx(&self, kind: StatementKind, sql: &str, result: StmtResult<'_>) {
        let mut tx = self.tx.lock().unwrap_or_else(|p| p.into_inner());
        let off = self.autocommit_off.load(Ordering::SeqCst);
        *tx = next_tx_state(DbKind::MySql, *tx, off, kind, sql, result);
        self.autocommit_off.store(
            next_autocommit_off(DbKind::MySql, off, kind, result),
            Ordering::SeqCst,
        );
    }

    /// Transaction tracking for a statement the server answered with `err` (a
    /// deadlock ends the transaction; failing DDL still committed it).
    fn track_failed(&self, kind: StatementKind, sql: &str, err: &MyError) {
        let (sqlstate, code) = match err {
            MyError::Server(se) => (Some(se.state.as_str()), Some(se.code)),
            _ => (None, None),
        };
        self.track_tx(kind, sql, StmtResult::Failed { sqlstate, code });
    }

    /// Tracking applied after a statement succeeded: transaction state, the last `USE`
    /// and whether session state was customised. `dialect`: the one `sql` was split
    /// with. Returns whether the session `sql_mode` may have changed.
    /// (`session_effect` already skips the `autocommit` assignment, which is
    /// transaction control re-applied separately.)
    fn track_after(&self, kind: StatementKind, sql: &str, dialect: Dialect) -> bool {
        self.track_tx(kind, sql, StmtResult::Ok);
        let Some(effect) = session_effect(sql, dialect) else {
            return false;
        };
        if let Some(db) = effect.use_db {
            *self.last_db.lock().unwrap_or_else(|p| p.into_inner()) = Some(db);
        }
        if effect.custom {
            self.session_custom.store(true, Ordering::SeqCst);
        }
        effect.sql_mode
    }

    /// The `ConnectionLost` message suffix for losing the current connection.
    fn notes(&self, was_in_tx: bool) -> String {
        reset_notes(was_in_tx, self.session_custom.load(Ordering::SeqCst))
    }

    /// Abandons the connection in `slot` (if any) and opens a fresh one (startup
    /// statements come from `opts.init`), re-applying the last `USE` and
    /// `autocommit = 0`, and re-reads the `sql_mode` (a changed one is not restored: it
    /// is custom session state, reported as reset). Clears the open transaction (the
    /// server rolls it back) and the custom-session-state flag. On failure `slot` is
    /// left empty, so nothing ever runs on the abandoned connection again.
    async fn reconnect(&self, slot: &mut Option<Conn>) -> Result<(), DbError> {
        self.set_tx(TxState::Idle);
        // From here on no cancel may kill anything: not the abandoned thread, and not the
        // fresh connection's before this call returned (see `RunGuard::retire`).
        self.cancel.clear_connection();
        if let Some(old) = slot.take() {
            abandon(old);
        }
        let db = self
            .last_db
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let autocommit_off = self.autocommit_off.load(Ordering::SeqCst);
        let open = async {
            let mut fresh = Conn::new(self.opts.clone())
                .await
                .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))?;
            if let Some(db) = db {
                fresh
                    .query_drop(format!("USE {}", quote_ident(&db)))
                    .await
                    .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))?;
            }
            if autocommit_off {
                fresh
                    .query_drop("SET autocommit = 0")
                    .await
                    .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))?;
            }
            let mode = read_sql_mode(&mut fresh)
                .await
                .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))?;
            Ok::<_, DbError>((fresh, mode))
        };
        let (fresh, mode) = tokio::time::timeout(RECONNECT_TIMEOUT, open)
            .await
            .map_err(|_| DbError::timeout(ConnectStage::ConnectDb))??;
        self.set_mode(mode);
        self.cancel.set_connection(fresh.id());
        self.session_custom.store(false, Ordering::SeqCst);
        *slot = Some(fresh);
        self.reconnects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Runs one statement, enforcing `opts.timeout` per statement (not per script), and
    /// watching for a cancel aimed at `run`.
    ///
    /// On expiry the query is killed; on a cancel the canceller kills it. Either way
    /// `sink` gets [`ExecProgress::CancelSent`] at once (before the `KILL QUERY` is
    /// actually sent) and the statement is awaited until `opts.cancel_grace` after that,
    /// so the connection stays in sync. On expiry the kill runs while the statement is
    /// awaited, under that same deadline: a slow kill never extends it. A statement still
    /// running at the deadline is `Stuck` (after [`ExecProgress::GraceExpired`]). One
    /// that ended while the timeout's kill was still unanswered keeps its own result, but
    /// the kill could still stop the next statement, so the connection is poisoned and
    /// replaced before that one is sent ([`UNCONFIRMED`]).
    ///
    /// After a timeout, a statement that completes during the grace period reports what
    /// really happened (it may have committed) with `late_completion`, except a result
    /// set of a `Read` (a killed `SELECT SLEEP(n)` returns a normal row) and the kill's
    /// own "interrupted" error, which are `Timeout`. After a cancel, whatever the
    /// statement ended with is returned as is (the caller sees `run.cancelled()`).
    #[allow(clippy::too_many_arguments)]
    async fn run_timed(
        &self,
        conn: &mut Conn,
        kind: StatementKind,
        sql: &str,
        opts: &ExecOptions,
        run: &RunGuard,
        sink: &mut dyn RowSink,
        results: &mut Vec<ResultSummary>,
        emitted: &mut bool,
    ) -> Result<Done, StmtError> {
        let shared = SharedSink::new(sink);
        let mut handle = &shared;
        let mut fut = std::pin::pin!(run_statement(
            conn,
            sql,
            opts,
            &mut handle,
            results,
            emitted
        ));
        let expiry = async {
            match opts.timeout {
                Some(limit) => tokio::time::sleep(limit).await,
                None => std::future::pending().await,
            }
        };
        let trigger = tokio::select! {
            biased;
            res = fut.as_mut() => {
                return res
                    .map(|had_columns| Done { had_columns, late_completion: false })
                    .map_err(StmtError::My);
            }
            () = expiry => Trigger::Timeout,
            () = run.cancel_requested() => Trigger::Cancel,
        };
        // One deadline for everything that follows: the `KILL QUERY` and the statement's
        // end.
        let deadline = tokio::time::Instant::now() + opts.cancel_grace;
        shared.progress(ExecProgress::CancelSent);
        let res = match trigger {
            // The canceller sends the `KILL QUERY` itself.
            Trigger::Cancel => {
                let Ok(res) = tokio::time::timeout_at(deadline, fut.as_mut()).await else {
                    shared.progress(ExecProgress::GraceExpired);
                    return Err(StmtError::Stuck(STUCK_CANCEL));
                };
                res
            }
            Trigger::Timeout => {
                let id = self.cancel.kill_target(run.gen());
                let mut kill = std::pin::pin!(kill_query(&self.opts, || id, opts.cancel_grace));
                let expired = tokio::time::sleep_until(deadline);
                let mut expired = std::pin::pin!(expired);
                let (mut ended, mut killed) = (None, None);
                // Until both the statement ended and the kill was answered (it then
                // cannot hit a later statement), or the deadline.
                while ended.is_none() || killed.is_none() {
                    tokio::select! {
                        res = fut.as_mut(), if ended.is_none() => ended = Some(res),
                        res = kill.as_mut(), if killed.is_none() => killed = Some(res),
                        () = expired.as_mut() => break,
                    }
                }
                let Some(res) = ended else {
                    shared.progress(ExecProgress::GraceExpired);
                    return Err(StmtError::Stuck(STUCK));
                };
                let unconfirmed = match killed {
                    Some(Ok(())) => false,
                    Some(Err(f)) => f.maybe_sent,
                    // Still pending: abandoned with this future, maybe already written.
                    None => true,
                };
                // The kill's own "interrupted" error: the `KILL QUERY` already ran.
                if unconfirmed && !matches!(&res, Err(e) if is_interrupted(e)) {
                    // It may still land, on whatever runs next: the statement's own
                    // result stands, but the connection is replaced before the next one.
                    self.cancel.poison();
                }
                res
            }
        };
        let late = trigger == Trigger::Timeout;
        match res {
            Ok(true) if late && kind == StatementKind::Read => Err(StmtError::Timeout),
            Ok(had_columns) => Ok(Done {
                had_columns,
                late_completion: late,
            }),
            Err(e) if late && is_interrupted(&e) => Err(StmtError::Timeout),
            Err(e) => Err(StmtError::My(e)),
        }
    }
}

/// Closes `conn` without waiting for whatever it is still running. A plain drop would
/// first drain the pending result in a background task, keeping a stuck statement's
/// socket (and its server thread) alive; here the socket is closed after at most
/// [`ABANDON_TIMEOUT`].
fn abandon(conn: Conn) {
    // Only reachable from `execute`, which always runs inside the runtime.
    tokio::spawn(async move {
        // Timing out drops the connection after `disconnect` marked it closed, which
        // closes the socket without draining anything.
        let _ = tokio::time::timeout(ABANDON_TIMEOUT, conn.disconnect()).await;
    });
}

/// The lexing flags of a `sql_mode` value (comma-separated, any case): `ANSI_QUOTES` or
/// the `ANSI` combination mode set `ansi_quotes`, `NO_BACKSLASH_ESCAPES` sets
/// `no_backslash_escapes`.
pub(crate) fn parse_sql_mode(s: &str) -> MySqlMode {
    let mut mode = MySqlMode::default();
    for flag in s.split(',').map(str::trim) {
        if flag.eq_ignore_ascii_case("ANSI_QUOTES") || flag.eq_ignore_ascii_case("ANSI") {
            mode.ansi_quotes = true;
        } else if flag.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES") {
            mode.no_backslash_escapes = true;
        }
    }
    mode
}

/// Reads the session `sql_mode` of `conn` ([`parse_sql_mode`]).
pub(crate) async fn read_sql_mode(conn: &mut Conn) -> Result<MySqlMode, MyError> {
    let mode: Option<String> = conn.query_first("SELECT @@SESSION.sql_mode").await?;
    Ok(parse_sql_mode(mode.as_deref().unwrap_or_default()))
}

fn lost(was_in_tx: bool, outcome_unknown: bool, message: String) -> DbError {
    DbError::ConnectionLost {
        was_in_tx,
        outcome_unknown,
        message,
    }
}

/// `CALL proc(…)`: its inner statements may each autocommit, so its work can be
/// partly applied even when the call as a whole fails or is killed. Either reading
/// of an executable comment counts. The `sql_mode` flags do not matter: `CALL` leads the
/// statement, before any quoted text.
fn is_call(sql: &str) -> bool {
    readings(sql, Dialect::MYSQL)
        .iter()
        .any(|r| leading_keywords(r, Dialect::MYSQL, 1) == ["CALL"])
}

/// Whether a statement of `kind` may have made a durable change by the time its
/// connection went away. Inside a transaction an ordinary statement's work is rolled
/// back with the connection; DDL, transaction control and `CALL` can commit by themselves.
fn may_have_committed(kind: StatementKind, was_in_tx: bool, sql: &str) -> bool {
    match kind {
        StatementKind::Read
        | StatementKind::Transaction(TxControl::Rollback | TxControl::AutocommitOff) => false,
        StatementKind::Ddl | StatementKind::Transaction(_) => true,
        StatementKind::Write if is_call(sql) => true,
        StatementKind::Write | StatementKind::Other => !was_in_tx,
    }
}

/// Whether a `Write`/`Ddl`/`COMMIT` statement stopped by `KILL QUERY` (timeout or cancel) may
/// still have applied part of its work. Outside an explicit transaction a
/// non-transactional table (MyISAM, Aria) keeps the rows written before the kill and
/// DDL is not atomic everywhere (and always commits the open transaction first); a
/// `CALL` commits its inner statements one by one. Inside a transaction InnoDB rolls a
/// killed plain write back, so it is reported as not applied; a write to a
/// non-transactional table inside a transaction is the accepted residual case.
fn killed_outcome_unknown(kind: StatementKind, was_in_tx: bool, sql: &str) -> bool {
    match kind {
        // DDL implicitly committed any open transaction before it ran; a killed COMMIT
        // may have committed before the kill landed.
        StatementKind::Ddl | StatementKind::Transaction(TxControl::Commit) => true,
        StatementKind::Write => !was_in_tx || is_call(sql),
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
        notes.push_str("; session settings, user variables and temporary tables were reset");
    }
    notes
}

/// The error for a statement that ignored `KILL QUERY` (`cause`: [`STUCK`] after its
/// timeout, [`STUCK_CANCEL`] after a cancel), once its connection was abandoned and
/// replaced (`Ok`) or replacing it failed (`Err`). `unknown`: it may have committed
/// ([`may_have_committed`]); `notes`: [`reset_notes`]. Always `ConnectionLost`, so the
/// reset notes and `was_in_tx` reach the user.
fn stuck_error(
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

/// Streams every result of one statement into `sink`. Returns whether any result had
/// columns (a result set rather than an OK status).
async fn run_statement(
    conn: &mut Conn,
    sql: &str,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    results: &mut Vec<ResultSummary>,
    emitted: &mut bool,
) -> Result<bool, MyError> {
    let mut started = Instant::now();
    let mut result = conn.query_iter(sql).await?;
    let batch_size = opts.batch_size.max(1);
    let mut had_columns = false;
    loop {
        let cols = result.columns().unwrap_or_else(|| Arc::from([]));
        had_columns |= !cols.is_empty();
        let metas: Vec<ColumnMeta> = cols.iter().map(column_meta).collect();
        sink.result_start(&metas);
        *emitted = true;
        let mut row_count = 0u64;
        let mut truncated = false;
        if !cols.is_empty() {
            let mut batch = Vec::with_capacity(batch_size.min(opts.max_rows.max(1)));
            result
                .for_each(|row| {
                    // Past `max_rows` the rest of the set is still read, just discarded.
                    if row_count as usize >= opts.max_rows {
                        truncated = true;
                        return;
                    }
                    let values = row
                        .unwrap()
                        .into_iter()
                        .zip(cols.iter())
                        .map(|(v, c)| to_value(v, c))
                        .collect();
                    batch.push(values);
                    row_count += 1;
                    if batch.len() >= batch_size {
                        sink.rows(std::mem::take(&mut batch));
                    }
                })
                .await?;
            if !batch.is_empty() {
                sink.rows(batch);
            }
        }
        let affected = cols.is_empty().then(|| result.affected_rows());
        let elapsed = started.elapsed();
        let result_summary = ResultSummary {
            affected,
            row_count,
            truncated,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        };
        sink.result_end(&result_summary);
        results.push(result_summary);
        // More sets only follow for multi-result statements such as `CALL`; the
        // trailing status of a `CALL` is not reported as a separate result.
        if result.is_empty() {
            return Ok(had_columns);
        }
        started = Instant::now();
    }
}

fn is_connection_lost(e: &MyError) -> bool {
    match e {
        MyError::Io(_) | MyError::Driver(DriverError::ConnectionClosed) => true,
        MyError::Server(se) => LOST_CODES.contains(&se.code),
        _ => false,
    }
}

/// The connection was gone before the statement reached the server: MySQL 8 closed the
/// idle session (4031, sent as the answer to our next command). The statement never
/// ran, so like PG's closed-client pre-check it may be
/// re-sent on a fresh connection and its outcome is known (nothing applied).
fn lost_before_sent(e: &MyError) -> bool {
    matches!(e, MyError::Server(se) if LOST_BEFORE_SENT.contains(&se.code))
}

/// "Query execution was interrupted": the statement was stopped by `KILL QUERY`. Only
/// consulted when our own cancel or timeout fired; any other interrupt (another
/// session's `KILL QUERY`, 3024 `max_execution_time`, MariaDB 1969
/// `max_statement_time`) is reported as the server's own error.
fn is_interrupted(e: &MyError) -> bool {
    matches!(e, MyError::Server(se) if se.code == ER_QUERY_INTERRUPTED)
}

/// How a statement error (not a timeout of ours) is reported.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The connection went away: `ConnectionLost`, even when a cancel was pending (the
    /// statement may have committed before the socket died).
    Lost,
    /// Our cancel was pending; `outcome_unknown` only when the kill stopped a statement
    /// that may be partly applied ([`killed_outcome_unknown`]).
    Cancelled { outcome_unknown: bool },
    /// The server's own error, code and text kept.
    Server,
}

/// `cancelled`: our cancel targeted this call; `interrupted`: the error is 1317;
/// `killed_unknown`: [`killed_outcome_unknown`] for the statement.
fn failure_kind(
    conn_lost: bool,
    cancelled: bool,
    interrupted: bool,
    killed_unknown: bool,
) -> Failure {
    if conn_lost {
        Failure::Lost
    } else if cancelled {
        Failure::Cancelled {
            outcome_unknown: interrupted && killed_unknown,
        }
    } else {
        Failure::Server
    }
}

#[async_trait]
impl Session for MySqlSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// `opts.max_rows` applies per result set and `opts.timeout` to the statement: an
    /// expired statement is killed and `Timeout { Execute }` returned (`outcome_unknown`
    /// when the killed write may be partly applied). One that ignores the kill (after a
    /// timeout or a cancel) for `opts.cancel_grace` loses its connection:
    /// `ConnectionLost` (see `stuck_error`).
    async fn execute_one(
        &self,
        stmt: &str,
        kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError> {
        let mut slot = self.conn.lock().await;
        // Cancels only apply to this call from here on (see `cancel.rs`).
        let run = self.cancel.begin(&opts.stop);
        // Stopped before anything was sent.
        let stopped = || run.cancelled() || opts.stop.is_stopped();
        let not_sent = |was_in_tx| DbError::Cancelled {
            outcome_unknown: false,
            was_in_tx,
        };
        if stopped() {
            return Err(not_sent(self.in_transaction()));
        }
        let mut outcome = StmtOutcome::default();
        let mut reconnected = false;
        // An earlier call's `KILL QUERY` may still land on this connection (see
        // `run_timed`): replace it before anything is sent, silently only when nothing of
        // the old session is lost.
        if slot.is_some() && self.cancel.poisoned() {
            let was_in_tx = self.in_transaction();
            let custom = self.session_custom.load(Ordering::SeqCst);
            let notes = self.notes(was_in_tx);
            if was_in_tx || custom {
                let mut message = format!("{UNCONFIRMED}{notes}");
                // Best effort so the next statement finds a live connection.
                if let Err(e) = self.reconnect(&mut slot).await {
                    message.push_str(&format!("; reconnecting failed: {e}"));
                }
                return Err(lost(was_in_tx, false, message));
            }
            reconnected = true;
            if let Err(e) = self.reconnect(&mut slot).await {
                return Err(lost(
                    false,
                    false,
                    format!("{UNCONFIRMED}; reconnecting failed: {e}"),
                ));
            }
            if stopped() {
                return Err(not_sent(false));
            }
        }
        // An earlier call abandoned the connection and could not replace it (and
        // reported that). Nothing was sent yet, so opening one now is safe for any
        // statement.
        if slot.is_none() {
            reconnected = true;
            if let Err(e) = self.reconnect(&mut slot).await {
                return Err(lost(false, false, format!("reconnecting failed: {e}")));
            }
            if stopped() {
                return Err(not_sent(self.in_transaction()));
            }
        }
        // The last `sql_mode` read failed: read it again before lexing anything.
        if self.mode_stale.load(Ordering::SeqCst) {
            if let Some(conn) = slot.as_mut() {
                self.refresh_mode(conn).await;
            }
            if stopped() {
                return Err(not_sent(self.in_transaction()));
            }
        }
        // The `sql_mode` `stmt` runs under, read from the connection it is sent on
        // (after any reconnect above).
        let dialect = self.dialect();
        let was_in_tx = self.in_transaction();
        loop {
            let mut emitted = false;
            let res = match slot.as_mut() {
                Some(conn) => {
                    self.run_timed(
                        conn,
                        kind,
                        stmt,
                        opts,
                        &run,
                        sink,
                        &mut outcome.results,
                        &mut emitted,
                    )
                    .await
                }
                None => Err(StmtError::My(DriverError::ConnectionClosed.into())),
            };
            let err = match res {
                // A killed `SELECT SLEEP(n)` returns a row instead of an error, so a
                // read's result set that completed while a cancel was in flight is
                // dropped. Anything else completed (and may have committed): keep it;
                // the raised stop signal stops the next statement.
                Ok(done) if done.had_columns && kind == StatementKind::Read && run.cancelled() => {
                    // It did run (a read cannot change the `sql_mode`).
                    self.track_after(kind, stmt, dialect);
                    return Err(DbError::Cancelled {
                        outcome_unknown: false,
                        was_in_tx,
                    });
                }
                Ok(done) => {
                    outcome.late_completion = done.late_completion;
                    break;
                }
                Err(StmtError::Timeout) => {
                    // Stopped by the `KILL QUERY` (killed DDL still committed).
                    self.track_tx(
                        kind,
                        stmt,
                        StmtResult::Failed {
                            sqlstate: Some("70100"),
                            code: Some(ER_QUERY_INTERRUPTED),
                        },
                    );
                    return Err(DbError::Timeout {
                        stage: ConnectStage::Execute,
                        outcome_unknown: killed_outcome_unknown(kind, was_in_tx, stmt),
                        was_in_tx,
                    });
                }
                Err(StmtError::Stuck(cause)) => {
                    let notes = self.notes(was_in_tx);
                    let unknown = may_have_committed(kind, was_in_tx, stmt);
                    // No cancel aimed at this call may reach the fresh connection.
                    run.retire();
                    let replaced = self.reconnect(&mut slot).await;
                    return Err(stuck_error(cause, unknown, was_in_tx, &notes, replaced));
                }
                Err(StmtError::My(e)) => e,
            };
            let conn_lost = is_connection_lost(&err);
            // Before the cancel check, so a coinciding cancel cannot skip it (a
            // deadlock rolled the whole transaction back). A lost connection is
            // tracked by the reconnect.
            if !conn_lost {
                self.track_failed(kind, stmt, &err);
            }
            match failure_kind(
                conn_lost,
                run.cancelled(),
                is_interrupted(&err),
                killed_outcome_unknown(kind, was_in_tx, stmt),
            ) {
                Failure::Cancelled { outcome_unknown } => {
                    return Err(DbError::Cancelled {
                        outcome_unknown,
                        was_in_tx,
                    })
                }
                Failure::Server => return Err(map_mysql_error(err, ConnectStage::Execute)),
                Failure::Lost => {}
            }
            // Captured before the reconnect resets the session state.
            let notes = self.notes(was_in_tx);
            let custom = self.session_custom.load(Ordering::SeqCst);
            let not_sent_yet = lost_before_sent(&err);
            let unknown = !not_sent_yet && may_have_committed(kind, was_in_tx, stmt);
            let cause = format!("{err}{notes}");
            if was_in_tx {
                // The server rolled the transaction back; never replay anything.
                // Best effort so the next statement finds a live connection.
                return Err(match self.reconnect(&mut slot).await {
                    Ok(()) => lost(true, unknown, cause),
                    Err(e) => lost(true, unknown, format!("{cause}; reconnecting failed: {e}")),
                });
            }
            if reconnected {
                return Err(lost(false, unknown, cause));
            }
            reconnected = true;
            if let Err(e) = self.reconnect(&mut slot).await {
                return Err(lost(
                    false,
                    unknown,
                    format!("{cause}; reconnecting failed: {e}"),
                ));
            }
            // Only a statement that never reached the server, or a read that produced
            // nothing yet, on a session with nothing a reconnect cannot restore, is
            // safe to run again silently.
            if custom || (!not_sent_yet && (kind != StatementKind::Read || emitted)) {
                return Err(lost(false, unknown, cause));
            }
            // ... and not when the user asked to stop it (nothing was applied).
            if stopped() {
                return Err(not_sent(false));
            }
        }
        // A connection poisoned by an unconfirmed `KILL QUERY` is replaced before the
        // next statement, and the replacement's `sql_mode` read then.
        if self.track_after(kind, stmt, dialect) && !self.cancel.poisoned() {
            if let Some(conn) = slot.as_mut() {
                // On failure the flags are marked stale and read again by the next
                // call (a lost connection is replaced, and its `sql_mode` read, then).
                self.refresh_mode(conn).await;
            }
        }
        Ok(outcome)
    }

    /// The tracked state, `Active` while `autocommit` is off (also right after a
    /// reconnect re-applied it).
    fn tx_state(&self) -> TxState {
        let tx = *self.tx.lock().unwrap_or_else(|p| p.into_inner());
        if tx == TxState::Idle && self.autocommit_off.load(Ordering::SeqCst) {
            TxState::Active
        } else {
            tx
        }
    }

    /// MySQL with the session's current `sql_mode` lexing flags.
    fn dialect(&self) -> Dialect {
        Dialect::MySql(self.mode())
    }

    fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::SeqCst)
    }

    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(MySqlCanceller {
            opts: self.opts.clone(),
            state: self.cancel.clone(),
        })
    }

    fn introspector(&self) -> &dyn Introspector {
        self
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.live_conn()
            .await?
            .ping()
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::Execute))
    }

    async fn close(self: Box<Self>) {
        // Best effort: the server drops the session either way.
        if let Some(conn) = self.conn.into_inner() {
            let _ = conn.disconnect().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::ServerError;

    #[test]
    fn parse_sql_mode_flags() {
        let ansi = MySqlMode {
            ansi_quotes: true,
            ..MySqlMode::default()
        };
        assert_eq!(parse_sql_mode("STRICT_TRANS_TABLES,ANSI_QUOTES"), ansi);
        assert_eq!(parse_sql_mode("ansi"), ansi);
        assert_eq!(
            parse_sql_mode("NO_BACKSLASH_ESCAPES"),
            MySqlMode {
                no_backslash_escapes: true,
                ..MySqlMode::default()
            }
        );
        assert_eq!(parse_sql_mode(""), MySqlMode::default());
        // Both, any case, padded; names that merely contain a flag do not count.
        assert_eq!(
            parse_sql_mode(" no_backslash_escapes , Ansi_Quotes "),
            MySqlMode {
                ansi_quotes: true,
                no_backslash_escapes: true,
            }
        );
        assert_eq!(
            parse_sql_mode("REAL_AS_FLOAT,PIPES_AS_CONCAT,ONLY_FULL_GROUP_BY"),
            MySqlMode::default()
        );
    }

    #[test]
    fn reconnect_quotes_the_use_target() {
        assert_eq!(quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn call_detection_is_lexical() {
        assert!(is_call("/* p */ call p()"));
        assert!(is_call("/*!50000 CALL p() */"));
        assert!(!is_call("-- CALL p()\nSELECT 1"));
        assert!(!is_call("SELECT 'CALL'"));
        assert!(!is_call("ưCALL p()"));
    }

    fn server(code: u16, message: &str) -> MyError {
        MyError::Server(ServerError {
            code,
            state: "HY000".into(),
            message: message.into(),
        })
    }

    #[test]
    fn a_lost_connection_wins_over_a_pending_cancel() {
        // Lost: reported as lost whatever else happened.
        for (cancelled, interrupted, unknown) in [(true, false, true), (true, true, false)] {
            assert_eq!(
                failure_kind(true, cancelled, interrupted, unknown),
                Failure::Lost
            );
        }
        // Our cancel: the outcome is unknown only when the kill stopped a statement
        // that may be partly applied.
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
        // Failed on its own while a cancel was in flight: nothing was killed.
        assert_eq!(
            failure_kind(false, true, false, true),
            Failure::Cancelled {
                outcome_unknown: false
            }
        );
        // An interrupt we did not ask for (another session's KILL QUERY,
        // max_execution_time, max_statement_time) is the server's own error.
        assert_eq!(failure_kind(false, false, true, true), Failure::Server);
    }

    #[test]
    fn unrequested_interrupts_keep_the_server_code() {
        for (code, msg) in [
            (1317, "Query execution was interrupted"),
            (
                3024,
                "Query execution was interrupted, maximum statement execution time exceeded",
            ),
            (
                1969,
                "Query execution was interrupted (max_statement_time exceeded)",
            ),
        ] {
            match map_mysql_error(server(code, msg), ConnectStage::Execute) {
                DbError::Server {
                    code: c, message, ..
                } => {
                    assert_eq!(c, Some(code.to_string()));
                    assert_eq!(message, msg);
                }
                other => panic!("{code}: {other:?}"),
            }
        }
    }

    #[test]
    fn lost_connection_errors() {
        for code in LOST_CODES {
            assert!(is_connection_lost(&server(code, "gone")), "{code}");
        }
        assert!(!is_connection_lost(&server(1317, "interrupted")));
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(is_connection_lost(&eof.into()));
        assert!(is_connection_lost(&DriverError::ConnectionClosed.into()));
    }

    fn refused() -> DbError {
        DbError::Connect {
            message: "connection refused".into(),
        }
    }

    use StatementKind::{Ddl, Other, Read, Transaction, Write};

    #[test]
    fn statements_that_may_have_committed_when_abandoned() {
        for (kind, sql) in [
            (Write, "INSERT INTO t VALUES (1)"),
            (Ddl, "CREATE TABLE t (a int)"),
            (Other, "SET x = 1"),
            (Transaction(TxControl::Commit), "COMMIT"),
        ] {
            assert!(may_have_committed(kind, false, sql), "{sql}");
        }
        // DDL, COMMIT and CALL (whose inner statements may commit) even inside a
        // transaction.
        for (kind, sql) in [
            (Ddl, "DROP TABLE t"),
            (Transaction(TxControl::Commit), "COMMIT"),
            (Write, "/* p */ CALL p()"),
        ] {
            assert!(may_have_committed(kind, true, sql), "{sql}");
        }
        for (kind, in_tx, sql) in [
            (Read, false, "SELECT 1"),
            (Write, true, "UPDATE t SET a = 1"),
            (Transaction(TxControl::Rollback), true, "ROLLBACK"),
        ] {
            assert!(!may_have_committed(kind, in_tx, sql), "{sql}");
        }
    }

    /// Under a `DELIMITER` left on, `SELECT …; DELETE …` is one statement; the planner
    /// (`dnt_query::plan`) gives it the strictest kind of its pieces, `Write`. Lost
    /// after the DELETE piece may have run, it is outcome-unknown and not retryable.
    #[test]
    fn multi_statement_planned_as_write_is_outcome_unknown_when_lost() {
        let sql = "SELECT * FROM t; DELETE FROM t WHERE id = 1;";
        let unknown = may_have_committed(Write, false, sql);
        assert!(unknown);
        let app = dnt_core::error::AppError::from(lost(false, unknown, "gone".into()));
        assert!(app.outcome_unknown && !app.retryable);
        // Classified by its first piece alone (`Read`) it would have looked safe.
        assert!(!may_have_committed(Read, false, sql));
        // `SELECT 1; BEGIN` plans as `Transaction(Begin)`: may have committed too.
        assert!(may_have_committed(
            Transaction(TxControl::Begin),
            false,
            "SELECT 1; BEGIN"
        ));
    }

    #[test]
    fn killed_write_outside_a_transaction_or_call_has_unknown_outcome() {
        assert!(killed_outcome_unknown(
            Write,
            false,
            "INSERT INTO t VALUES (1)"
        ));
        assert!(killed_outcome_unknown(
            Ddl,
            false,
            "ALTER TABLE t ADD b int"
        ));
        assert!(killed_outcome_unknown(Write, true, "call p()"));
        // InnoDB rolls a killed statement back inside a transaction.
        assert!(!killed_outcome_unknown(
            Write,
            true,
            "INSERT INTO t VALUES (1)"
        ));
        assert!(!killed_outcome_unknown(Read, false, "SELECT SLEEP(5)"));
        assert!(!killed_outcome_unknown(Other, false, "SET @x = SLEEP(5)"));
    }

    #[test]
    fn killed_commit_has_unknown_outcome() {
        for in_tx in [false, true] {
            assert!(killed_outcome_unknown(
                Transaction(TxControl::Commit),
                in_tx,
                "COMMIT"
            ));
        }
        assert!(!killed_outcome_unknown(
            Transaction(TxControl::Rollback),
            true,
            "ROLLBACK"
        ));
    }

    #[test]
    fn idle_disconnects_mean_the_statement_was_not_sent() {
        // 4031: MySQL 8 dropped the idle session before reading our command.
        assert!(lost_before_sent(&server(4031, "gone")));
        assert!(is_connection_lost(&server(4031, "gone")));
        // Lost mid-statement, or killed while running: it may have run. 2006 is a
        // client code; arriving as a server error (e.g. from a proxy) proves nothing.
        assert!(!lost_before_sent(&server(2006, "gone away")));
        assert!(!lost_before_sent(&server(2013, "lost during query")));
        assert!(!lost_before_sent(&server(1927, "Connection was killed")));
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(!lost_before_sent(&eof.into()));
    }

    #[test]
    fn reset_notes_name_what_was_lost() {
        assert_eq!(reset_notes(false, false), "");
        assert!(reset_notes(true, false).contains("transaction was rolled back"));
        let both = reset_notes(true, true);
        assert!(both.contains("transaction was rolled back"), "{both}");
        assert!(
            both.contains("session settings, user variables and temporary tables were reset"),
            "{both}"
        );
    }

    #[test]
    fn stuck_statement_that_may_have_committed_is_lost_with_unknown_outcome() {
        let notes = reset_notes(true, true);
        match stuck_error(STUCK, true, true, &notes, Ok(())) {
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: true,
                message,
            } => {
                assert!(message.starts_with(STUCK), "{message}");
                assert!(message.contains("rolled back"), "{message}");
                assert!(message.contains("user variables"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            stuck_error(STUCK, true, false, "", Ok(())),
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                ..
            }
        ));
    }

    #[test]
    fn stuck_after_a_cancel_says_it_was_cancelled() {
        match stuck_error(STUCK_CANCEL, false, false, "", Ok(())) {
            DbError::ConnectionLost { message, .. } => {
                assert!(message.contains("after it was cancelled"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn killed_ddl_inside_a_transaction_has_unknown_outcome() {
        // DDL implicitly committed the open transaction before it ran.
        assert!(killed_outcome_unknown(Ddl, true, "ALTER TABLE t ADD b int"));
    }

    #[test]
    fn stuck_statement_that_cannot_have_committed_is_connection_lost() {
        // A write inside a transaction is rolled back with the abandoned connection.
        let e = stuck_error(
            STUCK,
            false,
            true,
            "; the transaction was rolled back",
            Ok(()),
        );
        match e {
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message,
            } => {
                assert!(message.starts_with(STUCK), "{message}");
                assert!(message.contains("rolled back"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stuck_statement_whose_replacement_failed_is_connection_lost() {
        let e = stuck_error(STUCK, true, false, "", Err(refused()));
        match e {
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                message,
            } => assert!(message.contains("connection refused"), "{message}"),
            other => panic!("{other:?}"),
        }
        let e = stuck_error(STUCK_CANCEL, false, true, "", Err(refused()));
        assert!(
            matches!(
                e,
                DbError::ConnectionLost {
                    was_in_tx: true,
                    outcome_unknown: false,
                    ..
                }
            ),
            "{e:?}"
        );
    }
}
