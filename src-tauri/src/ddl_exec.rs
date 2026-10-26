//! What every DDL apply shares (object actions, the table designer): the registry of
//! running operations (UI-issued ids, pre-cancel, cancel), sending one statement, the
//! outcome of a failed apply, its History entry and the cache refresh after it.
use crate::cache::NodeKey;
use crate::session_manager::{config_error, lock, SessionManager};
use dnt_core::driver::{CancelHandle, ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::sql::{classify, Dialect};
use dnt_core::value::{ColumnMeta, Value};
use dnt_store::history::{HistoryOrigin, HistoryStatus, NewHistoryEntry};
use dnt_store::repo::ConnectionRepo;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) const MAX_OPERATIONS: usize = 256;

/// How long a pre-cancel (a cancel for an id no apply has registered yet) is kept.
const PRECANCEL_TTL: Duration = Duration::from_secs(300);

/// Added to the report when the apply finished but its History entry was not saved.
pub(crate) const HISTORY_FAILED: &str =
    "Operation finished, but saving History failed. Do not retry the operation.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Applied,
    NotApplied,
    RolledBack,
    Unknown,
}

pub(crate) struct Operation {
    pub(crate) connection: Option<Uuid>,
    pub(crate) stop: StopSignal,
    pub(crate) cancel: Option<CancelHandle>,
    pub(crate) since: Instant,
}

/// Running applies by their UI-issued operation id.
#[derive(Default)]
pub(crate) struct DdlOperations {
    pub(crate) ops: Mutex<HashMap<Uuid, Operation>>,
}

/// Removes its operation when the apply ends (however it ends).
pub(crate) struct OperationGuard<'a> {
    ops: &'a DdlOperations,
    id: Uuid,
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        lock(&self.ops.ops).remove(&self.id);
    }
}

fn retain_live(ops: &mut HashMap<Uuid, Operation>) {
    ops.retain(|_, op| op.connection.is_some() || op.since.elapsed() < PRECANCEL_TTL);
}

impl DdlOperations {
    /// Registers apply `id` on `conn`. A pre-cancel of `id` is kept (its signal is
    /// already raised); an id in use is a Config error.
    pub(crate) fn register(
        &self,
        id: Uuid,
        conn: Uuid,
    ) -> Result<(OperationGuard<'_>, StopSignal), DbError> {
        let mut ops = lock(&self.ops);
        retain_live(&mut ops);
        if let Some(op) = ops.get_mut(&id) {
            if op.connection.is_some() {
                return Err(config_error("operation_id", "Operation id is in use."));
            }
            op.connection = Some(conn);
            return Ok((OperationGuard { ops: self, id }, op.stop.clone()));
        }
        if ops.len() >= MAX_OPERATIONS {
            return Err(DbError::Busy);
        }
        let stop = StopSignal::new();
        ops.insert(
            id,
            Operation {
                connection: Some(conn),
                stop: stop.clone(),
                cancel: None,
                since: Instant::now(),
            },
        );
        Ok((OperationGuard { ops: self, id }, stop))
    }

    /// The handle a cancel of `id` reaches (`None`: nothing is running).
    pub(crate) fn set_cancel(&self, id: Uuid, handle: Option<CancelHandle>) {
        if let Some(op) = lock(&self.ops).get_mut(&id) {
            op.cancel = handle;
        }
    }

    /// Stops `id`; an id not registered yet is remembered as a pre-cancel.
    pub(crate) async fn cancel(&self, id: Uuid) -> Result<(), DbError> {
        let handle = {
            let mut ops = lock(&self.ops);
            retain_live(&mut ops);
            if !ops.contains_key(&id) && ops.len() >= MAX_OPERATIONS {
                return Err(DbError::Busy);
            }
            let op = ops.entry(id).or_insert_with(|| Operation {
                connection: None,
                stop: StopSignal::new(),
                cancel: None,
                since: Instant::now(),
            });
            op.stop.stop();
            op.cancel.clone()
        };
        if let Some(handle) = handle {
            handle.cancel().await?;
        }
        Ok(())
    }

    /// Cancels every apply running on `conn`.
    pub(crate) async fn cancel_connection(&self, conn: Uuid) {
        let ids: Vec<_> = lock(&self.ops)
            .iter()
            .filter(|(_, op)| op.connection == Some(conn))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = self.cancel(id).await;
        }
    }
}

pub(crate) fn stopped() -> DbError {
    DbError::Cancelled {
        outcome_unknown: false,
        was_in_tx: false,
    }
}

#[derive(Default)]
struct Sink;
impl RowSink for Sink {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, _: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _: &ResultSummary) {}
}

/// Sends one statement, discarding any rows.
pub(crate) async fn execute(
    session: &dyn Session,
    sql: &str,
    stop: StopSignal,
    grace: Duration,
    timeout_s: u32,
) -> Result<(), DbError> {
    let options = ExecOptions {
        stop,
        timeout: (timeout_s > 0).then(|| Duration::from_secs(u64::from(timeout_s))),
        cancel_grace: grace,
        ..ExecOptions::default()
    };
    session
        .execute_one(sql, classify(sql, session.dialect()), &options, &mut Sink)
        .await?;
    Ok(())
}

/// MySQL / MariaDB error 1205 (`ER_LOCK_WAIT_TIMEOUT`): the statement waited
/// `lock_wait_timeout` for its lock and was refused before changing anything. Other
/// server errors of a MySQL DDL stay unknown (DDL is not transactional there).
fn mysql_lock_wait_refused(pg: bool, error: &DbError) -> bool {
    !pg && matches!(error, DbError::Server { code: Some(c), .. } if c == "1205")
}

/// MySQL refused one DDL statement as a whole (a bad charset, a name that exists, an
/// invalid default): a server error other than an interrupted query or a storage-engine
/// failure partway through (a failed rename, engine error, commit or rollback), which on a
/// server without atomic DDL may leave part of the change behind.
pub(crate) fn mysql_statement_refused(pg: bool, error: &DbError) -> bool {
    !pg && matches!(
        error,
        DbError::Server { code, .. }
            if !matches!(code.as_deref(), Some("1025" | "1030" | "1180" | "1181" | "1317"))
    )
}

/// What a failed apply did: `sent` = a change statement was sent, `commit_sent` = a
/// PostgreSQL `COMMIT` was. An open PostgreSQL transaction is rolled back here; it only
/// counts as rolled back when the server confirms the `ROLLBACK`.
pub(crate) async fn failure_outcome(
    session: &dyn Session,
    error: &DbError,
    pg: bool,
    sent: bool,
    commit_sent: bool,
    grace: Duration,
) -> ActionOutcome {
    if commit_sent {
        ActionOutcome::Unknown
    } else if pg && session.in_transaction() {
        if execute(session, "ROLLBACK", StopSignal::new(), grace, 30)
            .await
            .is_ok()
        {
            ActionOutcome::RolledBack
        } else {
            ActionOutcome::Unknown
        }
    } else if sent {
        // A definite server rejection is known; transport loss/cancel is not.
        if (pg && matches!(error, DbError::Server { .. })) || mysql_lock_wait_refused(pg, error) {
            ActionOutcome::NotApplied
        } else {
            ActionOutcome::Unknown
        }
    } else {
        ActionOutcome::NotApplied
    }
}

/// The error of a failed apply, with `outcome_unknown` set from its outcome.
pub(crate) fn failure_error(error: DbError, outcome: ActionOutcome) -> AppError {
    let mut error = AppError::from(error);
    error.outcome_unknown = outcome == ActionOutcome::Unknown;
    error
}

/// One History entry of an apply that sent something.
pub(crate) struct DdlHistory<'a> {
    pub(crate) connection_id: Uuid,
    pub(crate) database: Option<String>,
    pub(crate) sql: &'a str,
    pub(crate) dialect: Dialect,
    pub(crate) started_at: String,
    pub(crate) elapsed: Duration,
    pub(crate) outcome: ActionOutcome,
    pub(crate) error: Option<&'a AppError>,
    pub(crate) origin: HistoryOrigin,
}

/// Saves the entry (redacted); `false` when saving failed (the caller adds
/// [`HISTORY_FAILED`]).
pub(crate) async fn write_history(repo: Arc<ConnectionRepo>, h: DdlHistory<'_>) -> bool {
    let sql = dnt_query::redact::redact(h.sql, h.dialect);
    let entry = NewHistoryEntry {
        connection_id: h.connection_id,
        database: h.database,
        sql_text: sql.text,
        redacted: sql.changed,
        kind: "ddl".into(),
        started_at: h.started_at,
        elapsed_ms: Some(h.elapsed.as_millis().min(u64::MAX as u128) as u64),
        status: if h.outcome == ActionOutcome::Applied {
            HistoryStatus::Ok
        } else {
            HistoryStatus::Error
        },
        row_count: None,
        affected: None,
        error_kind: h.error.map(|e| e.kind.clone()),
        error_code: h
            .error
            .and_then(|e| e.server_code.clone().or_else(|| e.sqlstate.clone())),
        origin: h.origin,
        outcome_unknown: h.outcome == ActionOutcome::Unknown,
    };
    matches!(
        tokio::task::spawn_blocking(move || repo.history_insert(&entry)).await,
        Ok(Ok(_))
    )
}

impl SessionManager {
    /// After DDL was sent on `conn`: the tree, the SchemaCache of `database`, edit plans
    /// and loaded cell values of the connection are out of date.
    pub(crate) fn after_ddl(&self, conn: Uuid, database: &str) {
        self.refresh(conn, &NodeKey(vec![]));
        self.refresh_schema_cache(conn, database);
        self.edit_plans.drop_conn(conn);
        self.loaded.drop_conn(conn);
    }
}
