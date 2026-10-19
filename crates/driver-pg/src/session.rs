use crate::cancel::{send_cancel, CancelState, PgCanceller, CANCEL_TIMEOUT};
use crate::values::{column_meta, to_value, Raw};
use crate::{close_conn, io_error, map_pg_error, open, ConnectParams, PgConn};
use async_trait::async_trait;
use dnt_core::driver::{
    CancelHandle, ExecOptions, ExecSummary, Introspector, ResultSummary, RowSink, Session,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::server::ServerInfo;
use dnt_core::sql::{classify, split, Dialect, StatementKind, TxControl};
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

/// One PostgreSQL connection. It is its own [`Introspector`] (see `introspect.rs`).
pub struct PgSession {
    conn: Mutex<PgConn>,
    info: ServerInfo,
    /// Settings of the established connection (TLS fallback applied); used to reconnect
    /// and by cancel requests.
    params: ConnectParams,
    /// Set by `BEGIN` / `START TRANSACTION`, cleared by `COMMIT` / `ROLLBACK` / `END` /
    /// `ABORT` / `PREPARE TRANSACTION`. DDL is transactional in PostgreSQL and errors leave
    /// the transaction open (aborted) until it is ended, so neither clears it.
    in_tx: AtomicBool,
    cancel: Arc<CancelState>,
    /// Last session-level `SET search_path …` that succeeded, re-applied after a reconnect.
    search_path: std::sync::Mutex<Option<String>>,
}

/// How one statement attempt failed.
enum StmtError {
    Pg(tokio_postgres::Error),
    /// `opts.timeout` expired and the cancelled statement stopped.
    Timeout,
    /// `opts.timeout` expired and the statement did not stop after the cancel request, so
    /// the connection was replaced (`Ok`) or replacing it failed (`Err`).
    Replaced(Result<(), DbError>),
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

    /// Transaction tracking applied before a statement runs.
    fn track_before(&self, kind: StatementKind, sql: &str) {
        match kind {
            StatementKind::Transaction(TxControl::Begin) => {
                self.in_tx.store(true, Ordering::SeqCst)
            }
            StatementKind::Transaction(TxControl::Commit | TxControl::Rollback) => {
                self.in_tx.store(false, Ordering::SeqCst)
            }
            // `autocommit` is not a PostgreSQL setting; the server rejects such a `SET`.
            StatementKind::Other if is_prepare_transaction(sql) => {
                self.in_tx.store(false, Ordering::SeqCst)
            }
            _ => {}
        }
    }

    /// Tracking applied after a statement succeeded: remembers the last `search_path`.
    fn track_after(&self, kind: StatementKind, sql: &str) {
        if kind == StatementKind::Other {
            if let Some(change) = search_path_change(sql) {
                *self.search_path.lock().unwrap_or_else(|p| p.into_inner()) = change;
            }
        }
    }

    /// Replaces `conn` with a fresh connection (startup statements included) and re-applies
    /// the last `SET search_path`. Clears transaction state: the server rolled it back.
    async fn reconnect(&self, conn: &mut PgConn) -> Result<(), DbError> {
        self.in_tx.store(false, Ordering::SeqCst);
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
        let fresh = tokio::time::timeout(RECONNECT_TIMEOUT, fresh)
            .await
            .map_err(|_| DbError::Timeout {
                stage: ConnectStage::ConnectDb,
            })??;
        self.cancel.set_token(fresh.client.cancel_token());
        // The old socket may be stuck behind a statement that ignored the cancel: drop it.
        let old = std::mem::replace(conn, fresh);
        old.task.abort();
        Ok(())
    }

    /// Runs one statement, enforcing `opts.timeout` per statement (not per script): on
    /// expiry a cancel request is sent and the statement awaited so the connection stays in
    /// sync; if it does not stop, the connection is replaced. Returns whether the statement
    /// produced a result set (had columns).
    async fn run_timed(
        &self,
        conn: &mut PgConn,
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
        let stuck = {
            let mut fut = std::pin::pin!(run_statement(
                &conn.client,
                sql,
                opts,
                sink,
                summary,
                emitted
            ));
            match tokio::time::timeout(limit, fut.as_mut()).await {
                Ok(res) => return res.map_err(StmtError::Pg),
                Err(_) => {
                    let _ = send_cancel(&self.cancel.token(), &self.params.tls).await;
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

fn map_exec_error(e: tokio_postgres::Error) -> DbError {
    if sqlstate(&e) == Some(QUERY_CANCELED) {
        return DbError::Cancelled;
    }
    map_pg_error(e, ConnectStage::Execute)
}

/// Leading keywords of `sql`, upper-cased (at most `n`).
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// `PREPARE TRANSACTION 'id'` ends the current transaction (it is handed to 2PC).
fn is_prepare_transaction(sql: &str) -> bool {
    leading_words(sql, 2) == ["PREPARE", "TRANSACTION"]
}

/// For a session-level `search_path` change: `Some(Some(stmt))` for `SET [SESSION]
/// search_path …` (the statement is replayed verbatim), `Some(None)` for `RESET
/// search_path` / `RESET ALL`, `None` for anything else (`SET LOCAL` is
/// transaction-scoped and not remembered).
fn search_path_change(sql: &str) -> Option<Option<String>> {
    let words = leading_words(sql, 3);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["SET", "SEARCH_PATH", ..] | ["SET", "SESSION", "SEARCH_PATH"] => {
            Some(Some(sql.trim().to_string()))
        }
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
    /// whole script): an expired statement is cancelled and `Timeout { Execute }` returned.
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
                return Err(DbError::Cancelled);
            }
            let kind = classify(&stmt.text);
            let was_in_tx = self.in_transaction();
            // Found dead before anything was sent (e.g. the idle backend was terminated):
            // reconnecting first is safe for any statement.
            if conn.client.is_closed() {
                if was_in_tx {
                    let _ = self.reconnect(&mut conn).await;
                    return Err(DbError::ConnectionLost {
                        in_transaction: true,
                    });
                }
                if reconnected || self.reconnect(&mut conn).await.is_err() {
                    return Err(DbError::ConnectionLost {
                        in_transaction: false,
                    });
                }
                reconnected = true;
                if run.cancelled() {
                    return Err(DbError::Cancelled);
                }
            }
            self.track_before(kind, &stmt.text);
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
                    // A result set that completed while a cancel was in flight is dropped;
                    // a statement that only returned a status completed (and committed
                    // outside a transaction): keep it, the flag stops the next statement.
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
                    Err(StmtError::Pg(e)) => e,
                };
                if run.cancelled() {
                    return Err(DbError::Cancelled);
                }
                if !is_connection_lost(&err, &conn.client) {
                    // Including deadlocks / serialization failures: in PostgreSQL the
                    // transaction stays open (aborted) until ROLLBACK, so `in_tx` is kept.
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
        assert!(!is_prepare_transaction("PREPARE q AS SELECT 1"));
    }
}
