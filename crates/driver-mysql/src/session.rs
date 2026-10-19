use crate::cancel::{kill_query, CancelState, MySqlCanceller, CANCEL_TIMEOUT};
use crate::map_mysql_error;
use crate::queries::quote_ident;
#[cfg(feature = "test-helpers")]
use crate::values::value_text;
use crate::values::{column_meta, to_value};
use async_trait::async_trait;
use dnt_core::driver::{
    CancelHandle, ExecOptions, ExecSummary, Introspector, ResultSummary, RowSink, Session,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::{MySqlCapabilities, ServerInfo};
use dnt_core::sql::{classify, split, strip_comments, Dialect, StatementKind, TxControl};
use dnt_core::value::ColumnMeta;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, Error as MyError, Opts};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// [`lost_before_sent`]).
const LOST_BEFORE_SENT: [u16; 2] = [2006, 4031];

/// `ConnectionLost` message for a statement that ignored `KILL QUERY` after its timeout.
const STUCK: &str = "the statement did not stop after the timeout, so its connection was closed";

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
    /// Set by `BEGIN` / `START TRANSACTION`, cleared by `COMMIT` / `ROLLBACK` / DDL.
    in_tx: AtomicBool,
    /// Set while the user has `SET autocommit = 0`. A session setting rather than
    /// transaction state: it survives a reconnect (re-applied on the new connection).
    autocommit_off: AtomicBool,
    cancel: Arc<CancelState>,
    /// Database of the last successful `USE`, re-applied after a reconnect.
    last_db: std::sync::Mutex<Option<String>>,
    /// Set once a statement changed session state that a reconnect cannot restore
    /// (see [`changes_session_state`]); cleared on a fresh connection. While set, a lost
    /// read is not silently re-run and `ConnectionLost` says the state was reset.
    session_custom: AtomicBool,
}

/// InnoDB deadlock: the server rolled the whole transaction back.
const ER_LOCK_DEADLOCK: u16 = 1213;

/// How one statement attempt failed.
enum StmtError {
    My(MyError),
    /// `opts.timeout` expired and the statement was stopped by the `KILL QUERY` (whether
    /// part of it was applied is [`killed_outcome_unknown`]).
    Timeout,
    /// `opts.timeout` expired and the statement did not stop after `KILL QUERY`: its
    /// completion was never observed and the connection must be abandoned.
    Stuck,
}

impl MySqlSession {
    pub(crate) fn new(conn: Conn, info: ServerInfo, caps: MySqlCapabilities, opts: Opts) -> Self {
        let cancel = Arc::new(CancelState::default());
        cancel.connection_id.store(conn.id(), Ordering::SeqCst);
        MySqlSession {
            conn: Mutex::new(Some(conn)),
            info,
            caps,
            opts,
            in_tx: AtomicBool::new(false),
            autocommit_off: AtomicBool::new(false),
            cancel,
            last_db: std::sync::Mutex::new(None),
            session_custom: AtomicBool::new(false),
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

    fn reset_tx(&self) {
        self.in_tx.store(false, Ordering::SeqCst);
        self.autocommit_off.store(false, Ordering::SeqCst);
    }

    /// Transaction tracking applied before a statement runs.
    fn track_before(&self, kind: StatementKind) {
        let tx = match kind {
            // MySQL commits implicitly before running DDL, even DDL that then fails.
            StatementKind::Ddl => return self.in_tx.store(false, Ordering::SeqCst),
            StatementKind::Transaction(tx) => tx,
            _ => return,
        };
        match tx {
            TxControl::Begin => self.in_tx.store(true, Ordering::SeqCst),
            TxControl::Commit | TxControl::Rollback => self.in_tx.store(false, Ordering::SeqCst),
            TxControl::AutocommitOff => self.autocommit_off.store(true, Ordering::SeqCst),
            // Turning autocommit back on also commits the open transaction.
            TxControl::AutocommitOn => self.reset_tx(),
        }
    }

    /// Tracking applied after a statement succeeded: remembers the last `USE` and
    /// whether session state was customised.
    fn track_after(&self, kind: StatementKind, sql: &str) {
        if kind == StatementKind::Other {
            if let Some(db) = use_target(sql) {
                *self.last_db.lock().unwrap_or_else(|p| p.into_inner()) = Some(db);
            }
        }
        // `SET autocommit` is transaction control, tracked and re-applied separately.
        if !matches!(kind, StatementKind::Transaction(_)) && changes_session_state(sql) {
            self.session_custom.store(true, Ordering::SeqCst);
        }
    }

    /// The `ConnectionLost` message suffix for losing the current connection.
    fn notes(&self, was_in_tx: bool) -> String {
        reset_notes(was_in_tx, self.session_custom.load(Ordering::SeqCst))
    }

    /// Abandons the connection in `slot` (if any) and opens a fresh one (startup
    /// statements come from `opts.init`), re-applying the last `USE` and
    /// `autocommit = 0`. Clears the open transaction (the server rolls it back) and the
    /// custom-session-state flag. On failure `slot` is left empty, so nothing ever runs
    /// on the abandoned connection again.
    async fn reconnect(&self, slot: &mut Option<Conn>) -> Result<(), DbError> {
        self.in_tx.store(false, Ordering::SeqCst);
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
            Ok::<_, DbError>(fresh)
        };
        let fresh = tokio::time::timeout(RECONNECT_TIMEOUT, open)
            .await
            .map_err(|_| DbError::timeout(ConnectStage::ConnectDb))??;
        self.cancel
            .connection_id
            .store(fresh.id(), Ordering::SeqCst);
        self.session_custom.store(false, Ordering::SeqCst);
        *slot = Some(fresh);
        Ok(())
    }

    /// Runs one statement, enforcing `opts.timeout` per statement (not per script).
    ///
    /// On expiry the query is killed and awaited for a grace period so the connection
    /// stays in sync. A statement that completes during that grace period reports what
    /// really happened (it may have committed), except a result set of a `Read` (a killed
    /// `SELECT SLEEP(n)` returns a normal row) and the kill's own "interrupted" error,
    /// which are `Timeout`. A statement still running after the grace period is `Stuck`.
    /// Returns whether any result had columns.
    #[allow(clippy::too_many_arguments)]
    async fn run_timed(
        &self,
        conn: &mut Conn,
        kind: StatementKind,
        sql: &str,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
        summary: &mut ExecSummary,
        emitted: &mut bool,
    ) -> Result<bool, StmtError> {
        let Some(limit) = opts.timeout else {
            return run_statement(conn, sql, opts, sink, summary, emitted)
                .await
                .map_err(StmtError::My);
        };
        let mut fut = std::pin::pin!(run_statement(conn, sql, opts, sink, summary, emitted));
        if let Ok(res) = tokio::time::timeout(limit, fut.as_mut()).await {
            return res.map_err(StmtError::My);
        }
        let id = self.cancel.connection_id.load(Ordering::SeqCst);
        let _ = kill_query(&self.opts, || Some(id)).await;
        match tokio::time::timeout(CANCEL_TIMEOUT, fut.as_mut()).await {
            Err(_) => Err(StmtError::Stuck),
            Ok(Ok(true)) if kind == StatementKind::Read => Err(StmtError::Timeout),
            Ok(Ok(had_columns)) => Ok(had_columns),
            Ok(Err(e)) if is_interrupted(&e) => Err(StmtError::Timeout),
            Ok(Err(e)) => Err(StmtError::My(e)),
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

fn lost(was_in_tx: bool, outcome_unknown: bool, message: String) -> DbError {
    DbError::ConnectionLost {
        was_in_tx,
        outcome_unknown,
        message,
    }
}

/// Upper-cased leading keywords of `sql` with comments removed (at most `n`).
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    strip_comments(sql, Dialect::MySql)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// `CALL proc(…)`: its inner statements may each autocommit, so its work can be
/// partly applied even when the call as a whole fails or is killed.
fn is_call(sql: &str) -> bool {
    leading_words(sql, 1) == ["CALL"]
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

/// Lexically: does a statement that succeeded change session state that a reconnect
/// cannot restore? Session-scope `SET` (variables, user variables, `NAMES`, isolation
/// level; `GLOBAL` / `PERSIST` survive the session), `CREATE TEMPORARY TABLE` and named
/// prepared statements (`PREPARE`). `SET autocommit` is classified as transaction
/// control and handled by the caller.
fn changes_session_state(sql: &str) -> bool {
    let words = leading_words(sql, 2);
    match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["SET", ..] => {
            let stripped = strip_comments(sql, Dialect::MySql);
            let text = stripped.trim_start();
            let Some(rest) = text
                .get(..3)
                .filter(|kw| kw.eq_ignore_ascii_case("SET"))
                .and(text.get(3..))
            else {
                return false;
            };
            let rest = rest.trim_start().to_ascii_lowercase();
            let first = rest.split_whitespace().next().unwrap_or("");
            let global = ["global", "persist", "persist_only"].contains(&first)
                || ["@@global.", "@@persist.", "@@persist_only."]
                    .iter()
                    .any(|p| rest.starts_with(p));
            !global
        }
        ["CREATE", "TEMPORARY"] | ["PREPARE", ..] => true,
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

/// The error for a statement that ignored `KILL QUERY` after its timeout, once its
/// connection was abandoned and replaced (`Ok`) or replacing it failed (`Err`).
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

/// Streams every result of one statement into `sink`. Returns whether any result had
/// columns (a result set rather than an OK status).
async fn run_statement(
    conn: &mut Conn,
    sql: &str,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
    summary: &mut ExecSummary,
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
        sink.result_end(affected, elapsed);
        summary.results.push(ResultSummary {
            affected,
            row_count,
            truncated,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        });
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
/// idle session (4031, sent as the answer to our next command) or the server had gone
/// away (2006). The statement never ran, so like PG's closed-client pre-check it may be
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

/// The database a successful statement switched to, if it is a `USE` (comments around
/// it allowed). A `USE` inside an executable comment (`/*! USE db */`) is not tracked.
fn use_target(sql: &str) -> Option<String> {
    parse_use(&strip_comments(sql, Dialect::MySql))
}

/// The database named by a `USE db` statement (backticks removed), if `sql` is one.
fn parse_use(sql: &str) -> Option<String> {
    let s = sql.trim().trim_end_matches(';').trim_end();
    let kw = s.get(..3)?;
    let rest = &s[3..];
    if !kw.eq_ignore_ascii_case("USE") || !rest.starts_with(|c: char| c.is_whitespace() || c == '`')
    {
        return None;
    }
    let name = rest.trim_start();
    if let Some(inner) = name.strip_prefix('`').and_then(|n| n.strip_suffix('`')) {
        return (!inner.is_empty()).then(|| inner.replace("``", "`"));
    }
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string())
}

#[async_trait]
impl Session for MySqlSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// `opts.max_rows` applies per result set and `opts.timeout` per statement (not to the
    /// whole script): an expired statement is killed and `Timeout { Execute }` returned
    /// (`outcome_unknown` when the killed write may be partly applied). One that ignores
    /// the kill loses its connection: `ConnectionLost` (see `stuck_error`).
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<ExecSummary, DbError> {
        let mut slot = self.conn.lock().await;
        // Cancels only apply to this call from here on (see `cancel.rs`).
        let run = self.cancel.begin();
        let mut summary = ExecSummary::default();
        let mut reconnected = false;
        for stmt in split(sql, Dialect::MySql) {
            if run.cancelled() {
                return Err(DbError::cancelled());
            }
            let kind = classify(&stmt.text, Dialect::MySql);
            // An earlier call abandoned the connection and could not replace it (and
            // reported that). Nothing was sent yet, so opening one now is safe for any
            // statement.
            if slot.is_none() {
                reconnected = true;
                if let Err(e) = self.reconnect(&mut slot).await {
                    return Err(lost(false, false, format!("reconnecting failed: {e}")));
                }
                if run.cancelled() {
                    return Err(DbError::cancelled());
                }
            }
            let was_in_tx = self.in_transaction();
            self.track_before(kind);
            loop {
                let mut emitted = false;
                let res = match slot.as_mut() {
                    Some(conn) => {
                        self.run_timed(
                            conn,
                            kind,
                            &stmt.text,
                            &opts,
                            sink,
                            &mut summary,
                            &mut emitted,
                        )
                        .await
                    }
                    None => Err(StmtError::My(DriverError::ConnectionClosed.into())),
                };
                let err = match res {
                    // A killed `SELECT SLEEP(n)` returns a row instead of an error, so a
                    // read's result set that completed while a cancel was in flight is
                    // dropped. Anything else completed (and may have committed): keep
                    // it; the flag stops the next statement.
                    Ok(true) if kind == StatementKind::Read && run.cancelled() => {
                        return Err(DbError::cancelled())
                    }
                    Ok(_) => break,
                    Err(StmtError::Timeout) => {
                        return Err(DbError::Timeout {
                            stage: ConnectStage::Execute,
                            outcome_unknown: killed_outcome_unknown(kind, was_in_tx, &stmt.text),
                        });
                    }
                    Err(StmtError::Stuck) => {
                        let notes = self.notes(was_in_tx);
                        let unknown = may_have_committed(kind, was_in_tx, &stmt.text);
                        let replaced = self.reconnect(&mut slot).await;
                        return Err(stuck_error(unknown, was_in_tx, &notes, replaced));
                    }
                    Err(StmtError::My(e)) => e,
                };
                // InnoDB rolled the whole transaction back. Clear the flag before the
                // cancel check so a coinciding cancel cannot skip it.
                if matches!(&err, MyError::Server(se) if se.code == ER_LOCK_DEADLOCK) {
                    self.in_tx.store(false, Ordering::SeqCst);
                }
                match failure_kind(
                    is_connection_lost(&err),
                    run.cancelled(),
                    is_interrupted(&err),
                    killed_outcome_unknown(kind, was_in_tx, &stmt.text),
                ) {
                    Failure::Cancelled { outcome_unknown } => {
                        return Err(DbError::Cancelled { outcome_unknown })
                    }
                    Failure::Server => return Err(map_mysql_error(err, ConnectStage::Execute)),
                    Failure::Lost => {}
                }
                // Captured before the reconnect resets the session state.
                let notes = self.notes(was_in_tx);
                let custom = self.session_custom.load(Ordering::SeqCst);
                let not_sent = lost_before_sent(&err);
                let unknown = !not_sent && may_have_committed(kind, was_in_tx, &stmt.text);
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
                if custom || (!not_sent && (kind != StatementKind::Read || emitted)) {
                    return Err(lost(false, unknown, cause));
                }
                // ... and not when the user asked to stop it (nothing was applied).
                if run.cancelled() {
                    return Err(DbError::cancelled());
                }
            }
            self.track_after(kind, &stmt.text);
        }
        Ok(summary)
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

    fn in_transaction(&self) -> bool {
        self.in_tx.load(Ordering::SeqCst) || self.autocommit_off.load(Ordering::SeqCst)
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
    fn parses_use_statements() {
        assert_eq!(parse_use("USE shop"), Some("shop".into()));
        assert_eq!(parse_use("  use `my db`;"), Some("my db".into()));
        assert_eq!(parse_use("Use`a``b`"), Some("a`b".into()));
        assert_eq!(parse_use("USER()"), None);
        assert_eq!(parse_use("use"), None);
        assert_eq!(parse_use("use a b"), None);
        assert_eq!(quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn use_behind_comments_is_tracked() {
        assert_eq!(use_target("/* pick */ USE shop"), Some("shop".into()));
        assert_eq!(use_target("-- pick\nUSE shop -- why"), Some("shop".into()));
        assert_eq!(
            use_target("# pick\nuse `my db` /* end */;"),
            Some("my db".into())
        );
        assert_eq!(use_target("USE /* odd */ shop"), Some("shop".into()));
        assert_eq!(use_target("/*!40101 USE shop */"), None);
        assert_eq!(use_target("/* USE shop */ SELECT 1"), None);
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
        // 4031: MySQL 8 dropped the idle session; 2006: server has gone away.
        for code in [2006, 4031] {
            assert!(lost_before_sent(&server(code, "gone")), "{code}");
            assert!(is_connection_lost(&server(code, "gone")), "{code}");
        }
        // Lost mid-statement, or killed while running: it may have run.
        assert!(!lost_before_sent(&server(2013, "lost during query")));
        assert!(!lost_before_sent(&server(1927, "Connection was killed")));
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(!lost_before_sent(&eof.into()));
    }

    #[test]
    fn session_state_changes_are_lexical() {
        for sql in [
            "SET @x = 1",
            "set sql_mode = ''",
            "/* c */ SET SESSION wait_timeout = 10",
            "SET @@session.sql_mode = ''",
            "SET NAMES latin1",
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "CREATE TEMPORARY TABLE tmp (a int)",
            "create  temporary table if not exists tmp (a int)",
            "PREPARE s FROM 'SELECT 1'",
        ] {
            assert!(changes_session_state(sql), "{sql}");
        }
        for sql in [
            "SELECT 1",
            "SET GLOBAL max_connections = 10",
            "SET PERSIST max_connections = 10",
            "SET @@global.sql_mode = ''",
            "CREATE TABLE t (a int)",
            "INSERT INTO t SELECT 'SET @x = 1'",
            "-- SET @x = 1\nSELECT 1",
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
            both.contains("session settings, user variables and temporary tables were reset"),
            "{both}"
        );
    }

    #[test]
    fn stuck_statement_that_may_have_committed_is_lost_with_unknown_outcome() {
        let notes = reset_notes(true, true);
        match stuck_error(true, true, &notes, Ok(())) {
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
            stuck_error(true, false, "", Ok(())),
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                ..
            }
        ));
    }

    #[test]
    fn killed_ddl_inside_a_transaction_has_unknown_outcome() {
        // DDL implicitly committed the open transaction before it ran.
        assert!(killed_outcome_unknown(Ddl, true, "ALTER TABLE t ADD b int"));
    }

    #[test]
    fn session_state_lexing_survives_non_ascii_starts() {
        assert!(!changes_session_state("ưư SET @x = 1"));
        assert!(!changes_session_state("ế"));
        assert!(changes_session_state("/* ư */ SET @x = 1"));
    }

    #[test]
    fn stuck_statement_that_cannot_have_committed_is_connection_lost() {
        // A write inside a transaction is rolled back with the abandoned connection.
        let e = stuck_error(false, true, "; the transaction was rolled back", Ok(()));
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
        let e = stuck_error(true, false, "", Err(refused()));
        match e {
            DbError::ConnectionLost {
                was_in_tx: false,
                outcome_unknown: true,
                message,
            } => assert!(message.contains("connection refused"), "{message}"),
            other => panic!("{other:?}"),
        }
        let e = stuck_error(false, true, "", Err(refused()));
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
