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
use dnt_core::sql::{classify, split, Dialect, StatementKind, TxControl};
use dnt_core::value::ColumnMeta;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, Error as MyError, Opts};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Bound on re-opening the connection after it was lost (same as the connect timeout).
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Server error: "Query execution was interrupted" (`KILL QUERY`).
const ER_QUERY_INTERRUPTED: u16 = 1317;
/// Error codes meaning the connection is gone: client-side "server has gone away" /
/// "lost connection" (2006, 2013), MySQL 8 "disconnected by the client" (4031) and
/// MariaDB "Connection was killed" (1927).
const LOST_CODES: [u16; 4] = [2006, 2013, 4031, 1927];

/// One MySQL-family connection. It is its own [`Introspector`] (see `introspect.rs`).
pub struct MySqlSession {
    pub(crate) conn: Mutex<Conn>,
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
    /// `opts.timeout` expired and the killed statement stopped.
    Timeout,
    /// `opts.timeout` expired and the statement did not stop after `KILL QUERY`, so the
    /// connection was replaced (`Ok`) or replacing it failed (`Err`).
    Replaced(Result<(), DbError>),
}

impl MySqlSession {
    pub(crate) fn new(conn: Conn, info: ServerInfo, caps: MySqlCapabilities, opts: Opts) -> Self {
        let cancel = Arc::new(CancelState::default());
        cancel.connection_id.store(conn.id(), Ordering::SeqCst);
        MySqlSession {
            conn: Mutex::new(conn),
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

    /// Test-only (`test-helpers` feature): runs one statement and returns its first row as
    /// text (`None` for SQL NULL). Bypasses transaction tracking; `execute` is the real path.
    #[cfg(feature = "test-helpers")]
    pub async fn query_row_text(&self, sql: &str) -> Result<Option<Vec<Option<String>>>, DbError> {
        let mut conn = self.conn.lock().await;
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
            if let Some(db) = parse_use(sql) {
                *self.last_db.lock().unwrap_or_else(|p| p.into_inner()) = Some(db);
            }
        }
    }

    /// Replaces `conn` with a fresh connection (startup statements come from `opts.init`)
    /// and re-applies the last `USE`. Clears transaction state: the server rolled it back.
    async fn reconnect(&self, conn: &mut Conn) -> Result<(), DbError> {
        self.reset_tx();
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
            .map_err(|_| DbError::Timeout {
                stage: ConnectStage::ConnectDb,
            })??;
        self.cancel
            .connection_id
            .store(fresh.id(), Ordering::SeqCst);
        // Dropping the dead connection closes it in the background.
        drop(std::mem::replace(conn, fresh));
        Ok(())
    }

    /// Runs one statement, enforcing `opts.timeout` per statement (not per script): on
    /// expiry the query is killed and awaited so the connection stays in sync; if it does
    /// not stop, the connection is replaced. Returns whether any result had columns.
    async fn run_timed(
        &self,
        conn: &mut Conn,
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
        let stuck = {
            let mut fut = std::pin::pin!(run_statement(conn, sql, opts, sink, summary, emitted));
            match tokio::time::timeout(limit, fut.as_mut()).await {
                Ok(res) => return res.map_err(StmtError::My),
                Err(_) => {
                    let id = self.cancel.connection_id.load(Ordering::SeqCst);
                    let _ = kill_query(&self.opts, || Some(id)).await;
                    tokio::time::timeout(CANCEL_TIMEOUT, fut.as_mut())
                        .await
                        .is_err()
                }
            }
        };
        if stuck {
            return Err(StmtError::Replaced(self.reconnect(conn).await));
        }
        Err(StmtError::Timeout)
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

fn map_exec_error(e: MyError) -> DbError {
    match &e {
        MyError::Server(se)
            if se.code == ER_QUERY_INTERRUPTED
                || se.message.contains("Query execution was interrupted") =>
        {
            DbError::Cancelled
        }
        _ => map_mysql_error(e, ConnectStage::Execute),
    }
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
    /// whole script): an expired statement is killed and `Timeout { Execute }` returned.
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
        for stmt in split(sql, Dialect::MySql) {
            if run.cancelled() {
                return Err(DbError::Cancelled);
            }
            let kind = classify(&stmt.text, Dialect::MySql);
            let was_in_tx = self.in_transaction();
            self.track_before(kind);
            loop {
                let mut emitted = false;
                let res = self
                    .run_timed(
                        &mut conn,
                        &stmt.text,
                        &opts,
                        sink,
                        &mut summary,
                        &mut emitted,
                    )
                    .await;
                let err = match res {
                    // A killed `SELECT SLEEP(n)` returns a row instead of an error. A
                    // statement that only returned an OK status completed (and committed
                    // under autocommit): keep it; the flag stops the next statement.
                    Ok(true) if run.cancelled() => return Err(DbError::Cancelled),
                    Ok(_) => break,
                    Err(StmtError::Timeout) => {
                        return Err(DbError::Timeout {
                            stage: ConnectStage::Execute,
                        })
                    }
                    Err(StmtError::Replaced(Ok(()))) => {
                        return Err(DbError::ConnectionLost {
                            in_transaction: was_in_tx,
                        })
                    }
                    Err(StmtError::Replaced(Err(e))) => return Err(e),
                    Err(StmtError::My(e)) => e,
                };
                if run.cancelled() {
                    return Err(DbError::Cancelled);
                }
                if !is_connection_lost(&err) {
                    if matches!(&err, MyError::Server(se) if se.code == ER_LOCK_DEADLOCK) {
                        self.in_tx.store(false, Ordering::SeqCst);
                    }
                    return Err(map_exec_error(err));
                }
                if was_in_tx {
                    // The server rolled the transaction back; never replay anything.
                    // Best effort so the next statement finds a live connection.
                    let _ = self.reconnect(&mut conn).await;
                    return Err(DbError::ConnectionLost {
                        in_transaction: true,
                    });
                }
                if reconnected {
                    return Err(DbError::ConnectionLost {
                        in_transaction: false,
                    });
                }
                reconnected = true;
                // `ConnectionLost` has no message field: the reconnect error is dropped.
                if self.reconnect(&mut conn).await.is_err() {
                    return Err(DbError::ConnectionLost {
                        in_transaction: false,
                    });
                }
                if run.cancelled() {
                    return Err(DbError::Cancelled);
                }
                // Only a read that produced nothing yet is safe to run again.
                if kind != StatementKind::Read || emitted {
                    return Err(DbError::ConnectionLost {
                        in_transaction: false,
                    });
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
        self.conn
            .lock()
            .await
            .ping()
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::Execute))
    }

    async fn close(self: Box<Self>) {
        // Best effort: the server drops the session either way.
        let _ = self.conn.into_inner().disconnect().await;
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
}
