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
    /// Set while the user has `SET autocommit = 0`.
    autocommit_off: AtomicBool,
    cancel: Arc<CancelState>,
    /// Database of the last successful `USE`, re-applied after a reconnect.
    last_db: std::sync::Mutex<Option<String>>,
}

/// InnoDB deadlock: the server rolled the whole transaction back.
const ER_LOCK_DEADLOCK: u16 = 1213;

/// How one statement attempt failed.
enum StmtError {
    My(MyError),
    /// `opts.timeout` expired and the statement was stopped by the `KILL QUERY` without
    /// taking effect.
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

    /// Tracking applied after a statement succeeded: remembers the last `USE`.
    fn track_after(&self, kind: StatementKind, sql: &str) {
        if kind == StatementKind::Other {
            if let Some(db) = use_target(sql) {
                *self.last_db.lock().unwrap_or_else(|p| p.into_inner()) = Some(db);
            }
        }
    }

    /// Abandons the connection in `slot` (if any) and opens a fresh one (startup
    /// statements come from `opts.init`), re-applying the last `USE`. Clears transaction
    /// state: the server rolls back whatever was open on the old connection. On failure
    /// `slot` is left empty, so nothing ever runs on the abandoned connection again.
    async fn reconnect(&self, slot: &mut Option<Conn>) -> Result<(), DbError> {
        self.reset_tx();
        if let Some(old) = slot.take() {
            abandon(old);
        }
        let db = self
            .last_db
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
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
            Ok::<_, DbError>(fresh)
        };
        let fresh = tokio::time::timeout(RECONNECT_TIMEOUT, open)
            .await
            .map_err(|_| DbError::timeout(ConnectStage::ConnectDb))??;
        self.cancel
            .connection_id
            .store(fresh.id(), Ordering::SeqCst);
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

fn lost(was_in_tx: bool, message: String) -> DbError {
    DbError::ConnectionLost { was_in_tx, message }
}

/// Whether a statement of `kind` may have made a durable change by the time it was
/// abandoned. Inside a transaction an ordinary statement's work is rolled back with the
/// connection; DDL and transaction control can commit by themselves.
fn may_have_committed(kind: StatementKind, was_in_tx: bool) -> bool {
    match kind {
        StatementKind::Read
        | StatementKind::Transaction(TxControl::Rollback | TxControl::AutocommitOff) => false,
        StatementKind::Ddl | StatementKind::Transaction(_) => true,
        StatementKind::Write | StatementKind::Other => !was_in_tx,
    }
}

/// The error for a statement that ignored `KILL QUERY` after its timeout, once its
/// connection was abandoned and replaced (`Ok`) or replacing it failed (`Err`).
fn stuck_error(kind: StatementKind, was_in_tx: bool, replaced: Result<(), DbError>) -> DbError {
    let unknown = may_have_committed(kind, was_in_tx);
    match replaced {
        Ok(()) if unknown => DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: true,
        },
        Ok(()) => lost(was_in_tx, STUCK.into()),
        Err(e) => {
            let note = if unknown {
                "; the statement may have been applied"
            } else {
                ""
            };
            lost(
                was_in_tx,
                format!("{STUCK} and reconnecting failed: {e}{note}"),
            )
        }
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

/// "Query execution was interrupted": the statement was stopped by `KILL QUERY`.
fn is_interrupted(e: &MyError) -> bool {
    matches!(e, MyError::Server(se)
        if se.code == ER_QUERY_INTERRUPTED
            || se.message.contains("Query execution was interrupted"))
}

fn map_exec_error(e: MyError) -> DbError {
    if is_interrupted(&e) {
        return DbError::Cancelled;
    }
    map_mysql_error(e, ConnectStage::Execute)
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
    /// (`outcome_unknown` when it ignored the kill and may have committed).
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
                return Err(DbError::Cancelled);
            }
            let kind = classify(&stmt.text, Dialect::MySql);
            // An earlier call abandoned the connection and could not replace it. Nothing
            // was sent yet, so opening one now is safe for any statement.
            if slot.is_none() {
                reconnected = true;
                if let Err(e) = self.reconnect(&mut slot).await {
                    return Err(lost(false, format!("reconnecting failed: {e}")));
                }
                if run.cancelled() {
                    return Err(DbError::Cancelled);
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
                        return Err(DbError::Cancelled)
                    }
                    Ok(_) => break,
                    Err(StmtError::Timeout) => {
                        return Err(DbError::timeout(ConnectStage::Execute));
                    }
                    Err(StmtError::Stuck) => {
                        let replaced = self.reconnect(&mut slot).await;
                        return Err(stuck_error(kind, was_in_tx, replaced));
                    }
                    Err(StmtError::My(e)) => e,
                };
                // InnoDB rolled the whole transaction back. Clear the flag before the
                // cancel check so a coinciding cancel cannot skip it.
                if matches!(&err, MyError::Server(se) if se.code == ER_LOCK_DEADLOCK) {
                    self.in_tx.store(false, Ordering::SeqCst);
                }
                if run.cancelled() {
                    return Err(DbError::Cancelled);
                }
                if !is_connection_lost(&err) {
                    return Err(map_exec_error(err));
                }
                let cause = err.to_string();
                if was_in_tx {
                    // The server rolled the transaction back; never replay anything.
                    // Best effort so the next statement finds a live connection.
                    let _ = self.reconnect(&mut slot).await;
                    return Err(lost(true, cause));
                }
                if reconnected {
                    return Err(lost(false, cause));
                }
                reconnected = true;
                if let Err(e) = self.reconnect(&mut slot).await {
                    return Err(lost(false, format!("{cause}; reconnecting failed: {e}")));
                }
                if run.cancelled() {
                    return Err(DbError::Cancelled);
                }
                // Only a read that produced nothing yet is safe to run again.
                if kind != StatementKind::Read || emitted {
                    return Err(lost(false, cause));
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
    fn interrupted_queries_are_cancelled() {
        assert!(matches!(
            map_exec_error(server(1317, "Query execution was interrupted")),
            DbError::Cancelled
        ));
        assert!(matches!(
            map_exec_error(server(1064, "syntax")),
            DbError::Server { .. }
        ));
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

    #[test]
    fn stuck_write_with_replaced_connection_may_have_been_applied() {
        for kind in [
            StatementKind::Write,
            StatementKind::Ddl,
            StatementKind::Other,
            StatementKind::Transaction(TxControl::Commit),
        ] {
            assert!(
                matches!(
                    stuck_error(kind, false, Ok(())),
                    DbError::Timeout {
                        stage: ConnectStage::Execute,
                        outcome_unknown: true
                    }
                ),
                "{kind:?}"
            );
        }
        // DDL and COMMIT commit by themselves even inside a transaction.
        for kind in [
            StatementKind::Ddl,
            StatementKind::Transaction(TxControl::Commit),
        ] {
            assert!(
                matches!(
                    stuck_error(kind, true, Ok(())),
                    DbError::Timeout {
                        outcome_unknown: true,
                        ..
                    }
                ),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn stuck_statement_that_cannot_have_committed_is_connection_lost() {
        // A write inside a transaction is rolled back with the abandoned connection.
        let e = stuck_error(StatementKind::Write, true, Ok(()));
        assert!(
            matches!(&e, DbError::ConnectionLost { was_in_tx: true, message } if message == STUCK),
            "{e:?}"
        );
        let e = stuck_error(StatementKind::Read, false, Ok(()));
        assert!(
            matches!(
                e,
                DbError::ConnectionLost {
                    was_in_tx: false,
                    ..
                }
            ),
            "{e:?}"
        );
        let e = stuck_error(
            StatementKind::Transaction(TxControl::Rollback),
            true,
            Ok(()),
        );
        assert!(
            matches!(
                e,
                DbError::ConnectionLost {
                    was_in_tx: true,
                    ..
                }
            ),
            "{e:?}"
        );
    }

    #[test]
    fn stuck_statement_whose_replacement_failed_is_connection_lost() {
        let e = stuck_error(StatementKind::Write, false, Err(refused()));
        match e {
            DbError::ConnectionLost { was_in_tx, message } => {
                assert!(!was_in_tx);
                assert!(message.contains("connection refused"), "{message}");
                assert!(message.contains("may have been applied"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        let e = stuck_error(StatementKind::Read, true, Err(refused()));
        match e {
            DbError::ConnectionLost { was_in_tx, message } => {
                assert!(was_in_tx);
                assert!(!message.contains("may have been applied"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}
