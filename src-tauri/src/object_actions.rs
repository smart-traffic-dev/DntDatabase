//! Single-use DDL plans and dedicated, cancellable object-action sessions.
use crate::cache::NodeKey;
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager, METADATA_CALL_TIMEOUT,
};
use dnt_core::config::EnvTag;
use dnt_core::driver::{CancelHandle, ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, Scope};
use dnt_core::server::DbKind;
use dnt_core::sql::ddl::{action_sql, ObjectAction};
use dnt_core::sql::{classify, Dialect};
use dnt_core::value::{ColumnMeta, Value};
use dnt_store::history::{HistoryOrigin, HistoryStatus, NewHistoryEntry};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_PLANS: usize = 128;
const MAX_OPERATIONS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ActionPreview {
    pub plan_id: Uuid,
    pub connection_id: Uuid,
    pub action: ObjectAction,
    pub sql: String,
    pub confirm_name: Option<String>,
    pub warnings: Vec<String>,
    pub expires_in_seconds: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Applied,
    NotApplied,
    RolledBack,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ActionReport {
    pub outcome: ActionOutcome,
    pub error: Option<AppError>,
    pub warnings: Vec<String>,
}
struct StoredPlan {
    preview: ActionPreview,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    env: Option<EnvTag>,
    dialect: Dialect,
    fingerprint: String,
    expires: Instant,
}
struct Operation {
    connection: Option<Uuid>,
    stop: StopSignal,
    cancel: Option<CancelHandle>,
    since: Instant,
}
#[derive(Default)]
pub(crate) struct ActionStore {
    plans: Mutex<HashMap<Uuid, StoredPlan>>,
    operations: Mutex<HashMap<Uuid, Operation>>,
}
struct OperationGuard<'a> {
    store: &'a ActionStore,
    id: Uuid,
}
impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        lock(&self.store.operations).remove(&self.id);
    }
}
impl ActionStore {
    fn register(&self, id: Uuid, conn: Uuid) -> Result<(OperationGuard<'_>, StopSignal), DbError> {
        let mut ops = lock(&self.operations);
        ops.retain(|_, op| {
            op.connection.is_some() || op.since.elapsed() < Duration::from_secs(300)
        });
        if let Some(op) = ops.get_mut(&id) {
            if op.connection.is_some() {
                return Err(config_error("operation_id", "Operation id is in use."));
            }
            op.connection = Some(conn);
            return Ok((OperationGuard { store: self, id }, op.stop.clone()));
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
        Ok((OperationGuard { store: self, id }, stop))
    }
}
fn stale() -> DbError {
    DbError::PlanStale {
        reason: "The connection, policy, dialect or object changed; preview again.".into(),
    }
}
fn stopped() -> DbError {
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
async fn execute(
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

/// Metadata is read on this operation's session, never from tree caches.
async fn fingerprint(session: &dyn Session, action: &ObjectAction) -> Result<String, DbError> {
    let i = session.introspector();
    let kind = session.server_info().kind;
    if let Some(obj) = action.object() {
        let scope = Scope {
            database: obj.database.clone(),
            schema: obj.schema.clone(),
        };
        let objects = i.list_objects(&scope, obj.kind).await?;
        if !objects.iter().any(|o| o.name == obj.name) {
            return Err(stale());
        }
        if let ObjectAction::RenameTable { name, .. } = action {
            // Names share a namespace with views (and materialized views on PG).
            let mut kinds = vec![ObjectKind::Table, ObjectKind::View];
            if kind == DbKind::Postgres {
                kinds.push(ObjectKind::MaterializedView);
            }
            for k in kinds {
                if i.list_objects(&scope, k)
                    .await?
                    .iter()
                    .any(|o| &o.name == name)
                {
                    return Err(stale());
                }
            }
        }
        return i.object_ddl(obj).await;
    }
    let names: Vec<String> = if kind == DbKind::MySql {
        i.list_databases()
            .await?
            .into_iter()
            .map(|d| d.name)
            .collect()
    } else {
        i.list_schemas(action.database())
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect()
    };
    let exists = names.iter().any(|n| n == action.name());
    if exists == matches!(action, ObjectAction::CreateNamespace { .. }) {
        return Err(stale());
    }
    if !exists {
        return Ok("absent".into());
    }
    let scope = Scope {
        database: if kind == DbKind::MySql {
            action.name().into()
        } else {
            action.database().into()
        },
        schema: (kind == DbKind::Postgres).then(|| action.name().into()),
    };
    let mut entries = Vec::new();
    let kinds = if kind == DbKind::MySql {
        vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Trigger,
            ObjectKind::Event,
        ]
    } else {
        vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Sequence,
            ObjectKind::Type,
        ]
    };
    for k in kinds {
        for o in i.list_objects(&scope, k).await? {
            entries.push(format!("{k:?}:{}", o.name));
        }
    }
    entries.sort();
    Ok(entries.join("\0"))
}

impl SessionManager {
    pub async fn object_action_preview(
        &self,
        conn: Uuid,
        action: ObjectAction,
    ) -> Result<ActionPreview, DbError> {
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        let sql = action_sql(cfg.kind, &action)?;
        let rt = self.runtime(conn)?;
        rt.queries.check_ddl_idle()?;
        let db = if cfg.kind == DbKind::MySql {
            None
        } else {
            Some(action.database().to_string())
        };
        let (session, generation) = rt.open_query_session(db).await?;
        let dialect = session.dialect();
        let fp = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            fingerprint(session.as_ref(), &action),
        )
        .await;
        close_quietly(session).await;
        let fp =
            fp.map_err(|_| config_error("object", "Metadata timed out; nothing was changed."))??;
        let mut warnings = vec!["Each metadata lock wait is limited to 5 seconds.".into()];
        if cfg.kind == DbKind::MySql {
            warnings.push(
                "MySQL/MariaDB DDL may commit implicitly and cannot be undone by this app.".into(),
            );
        }
        if action.destructive() {
            warnings.push("This operation removes data or an object. No CASCADE or foreign-key bypass is added.".into());
        }
        if matches!(action, ObjectAction::TruncateTable { .. }) {
            warnings.push(if cfg.kind == DbKind::MySql { "TRUNCATE may reset AUTO_INCREMENT." } else { "TRUNCATE preserves identity values, includes descendant tables and fires ON TRUNCATE triggers." }.into());
        }
        let preview = ActionPreview {
            plan_id: Uuid::new_v4(),
            connection_id: conn,
            confirm_name: (action.destructive() || cfg.env_tag == Some(EnvTag::Production))
                .then(|| action.name().into()),
            action,
            sql,
            warnings,
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let mut plans = lock(&self.object_actions.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            StoredPlan {
                preview: preview.clone(),
                rt,
                generation,
                env: cfg.env_tag,
                dialect,
                fingerprint: fp,
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    pub async fn object_action_cancel(&self, id: Uuid) -> Result<(), DbError> {
        let handle = {
            let mut ops = lock(&self.object_actions.operations);
            ops.retain(|_, op| {
                op.connection.is_some() || op.since.elapsed() < Duration::from_secs(300)
            });
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

    pub(crate) async fn object_cancel_connection(&self, conn: Uuid) {
        lock(&self.object_actions.plans).retain(|_, p| p.preview.connection_id != conn);
        let ids: Vec<_> = lock(&self.object_actions.operations)
            .iter()
            .filter(|(_, op)| op.connection == Some(conn))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = self.object_action_cancel(id).await;
        }
    }

    pub async fn object_action_apply(
        &self,
        plan: Uuid,
        confirmed: bool,
        name: Option<String>,
        operation: Uuid,
        progress: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<ActionReport, DbError> {
        let stored = lock(&self.object_actions.plans)
            .remove(&plan)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        let conn = stored.preview.connection_id;
        let (_guard, stop) = self.object_actions.register(operation, conn)?;
        self.check_action_context(&stored)?;
        if !confirmed
            || stored
                .preview
                .confirm_name
                .as_ref()
                .is_some_and(|required| Some(required) != name.as_ref())
        {
            return Err(DbError::ConfirmationRequired { count: 1 });
        }
        if stop.is_stopped() {
            return Err(stopped());
        }
        progress("checking");
        let db = if stored.rt.cfg.kind == DbKind::MySql {
            None
        } else {
            Some(stored.preview.action.database().into())
        };
        let (session, generation) = stored.rt.open_query_session(db).await?;
        {
            let mut ops = lock(&self.object_actions.operations);
            if let Some(op) = ops.get_mut(&operation) {
                op.cancel = Some(session.cancel_handle());
            }
        }
        let checked = async {
            if generation != stored.generation || session.dialect() != stored.dialect {
                return Err(stale());
            }
            let before = session.reconnects();
            session.ping().await?;
            if fingerprint(session.as_ref(), &stored.preview.action).await? != stored.fingerprint {
                return Err(stale());
            }
            self.check_action_context(&stored)?;
            if before != session.reconnects() || session.dialect() != stored.dialect {
                return Err(stale());
            }
            if stop.is_stopped() {
                return Err(stopped());
            }
            Ok(())
        };
        let checked = tokio::time::timeout(METADATA_CALL_TIMEOUT, checked)
            .await
            .unwrap_or_else(|_| {
                Err(config_error(
                    "object",
                    "Metadata timed out; nothing was changed.",
                ))
            });
        if let Err(e) = checked {
            close_quietly(session).await;
            return Err(e);
        }
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let mut sent = false;
        let mut commit_sent = false;
        let pg = stored.rt.cfg.kind == DbKind::Postgres;
        let result = async {
            execute(
                session.as_ref(),
                if pg {
                    "SET lock_timeout = '5s'"
                } else {
                    "SET SESSION lock_wait_timeout = 5"
                },
                stop.clone(),
                self.cancel_grace,
                stored.rt.cfg.options.query_timeout_s,
            )
            .await?;
            if pg {
                execute(
                    session.as_ref(),
                    "BEGIN",
                    stop.clone(),
                    self.cancel_grace,
                    stored.rt.cfg.options.query_timeout_s,
                )
                .await?;
            }
            self.check_action_context(&stored)?;
            if session.dialect() != stored.dialect || session.reconnects() != 0 {
                return Err(stale());
            }
            if stop.is_stopped() {
                return Err(stopped());
            }
            progress("executing");
            sent = true;
            execute(
                session.as_ref(),
                &stored.preview.sql,
                stop.clone(),
                self.cancel_grace,
                stored.rt.cfg.options.query_timeout_s,
            )
            .await?;
            if session.reconnects() != 0 {
                return Err(stale());
            }
            if pg {
                if stop.is_stopped() {
                    return Err(stopped());
                }
                progress("committing");
                commit_sent = true;
                execute(
                    session.as_ref(),
                    "COMMIT",
                    stop.clone(),
                    self.cancel_grace,
                    stored.rt.cfg.options.query_timeout_s,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        let mut report = ActionReport {
            outcome: ActionOutcome::Applied,
            error: None,
            warnings: Vec::new(),
        };
        if let Err(e) = result {
            report.outcome = if commit_sent {
                ActionOutcome::Unknown
            } else if pg && session.in_transaction() {
                if execute(
                    session.as_ref(),
                    "ROLLBACK",
                    StopSignal::new(),
                    self.cancel_grace,
                    30,
                )
                .await
                .is_ok()
                {
                    ActionOutcome::RolledBack
                } else {
                    ActionOutcome::Unknown
                }
            } else if sent {
                // A definite server rejection is known; transport loss/cancel is not.
                if pg && matches!(e, DbError::Server { .. }) {
                    ActionOutcome::NotApplied
                } else {
                    ActionOutcome::Unknown
                }
            } else {
                ActionOutcome::NotApplied
            };
            let mut error = AppError::from(e);
            error.outcome_unknown = report.outcome == ActionOutcome::Unknown;
            report.error = Some(error);
        }
        // No cancellation can reach a later statement or a pooled session: this session is closed.
        if let Some(op) = lock(&self.object_actions.operations).get_mut(&operation) {
            op.cancel = None;
        }
        close_quietly(session).await;
        if sent {
            let sql = dnt_query::redact::redact(&stored.preview.sql, stored.dialect);
            let entry = NewHistoryEntry {
                connection_id: conn,
                database: Some(stored.preview.action.database().into()),
                sql_text: sql.text,
                redacted: sql.changed,
                kind: "ddl".into(),
                started_at,
                elapsed_ms: Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64),
                status: if report.outcome == ActionOutcome::Applied {
                    HistoryStatus::Ok
                } else {
                    HistoryStatus::Error
                },
                row_count: None,
                affected: None,
                error_kind: report.error.as_ref().map(|e| e.kind.clone()),
                error_code: report
                    .error
                    .as_ref()
                    .and_then(|e| e.server_code.clone().or_else(|| e.sqlstate.clone())),
                origin: HistoryOrigin::ObjectAction,
                outcome_unknown: report.outcome == ActionOutcome::Unknown,
            };
            let repo = self.repo.clone();
            if !matches!(
                tokio::task::spawn_blocking(move || repo.history_insert(&entry)).await,
                Ok(Ok(_))
            ) {
                report.warnings.push(
                    "Operation finished, but saving History failed. Do not retry the operation."
                        .into(),
                );
            }
        }
        if sent {
            self.refresh(conn, &NodeKey(vec![]));
            self.refresh_schema_cache(conn, stored.preview.action.database());
            self.edit_plans.drop_conn(conn);
            self.loaded.drop_conn(conn);
        }
        progress("finished");
        Ok(report)
    }

    fn check_action_context(&self, stored: &StoredPlan) -> Result<(), DbError> {
        let conn = stored.preview.connection_id;
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if cfg.env_tag != stored.env || cfg != stored.rt.cfg {
            return Err(stale());
        }
        let rt = self.runtime(conn)?;
        if !Arc::ptr_eq(&rt, &stored.rt)
            || rt.closed.load(Ordering::SeqCst)
            || rt.tunnel_dead()
            || rt.tunnel_gen.load(Ordering::SeqCst) != stored.generation
        {
            return Err(stale());
        }
        let mut judged =
            dnt_query::plan(&stored.preview.sql, dnt_query::Target::All, stored.dialect);
        dnt_query::guard_with(
            &mut judged,
            dnt_query::GuardCtx {
                env: cfg.env_tag,
                read_only: cfg.read_only,
                analyze: false,
                confirm_unbounded: true,
            },
        );
        if judged.stmts.len() != 1
            || judged
                .stmts
                .iter()
                .any(|s| s.verdict == dnt_query::Verdict::Block)
        {
            return Err(DbError::Blocked { count: 1 });
        }
        rt.queries.check_ddl_idle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_ids_are_exclusive_and_raii_releases_them() {
        let store = ActionStore::default();
        let id = Uuid::new_v4();
        let conn = Uuid::new_v4();
        let (guard, stop) = store.register(id, conn).unwrap();
        assert!(!stop.is_stopped());
        assert!(matches!(
            store.register(id, conn),
            Err(DbError::Config { .. })
        ));
        drop(guard);
        assert!(lock(&store.operations).is_empty());
    }
    #[test]
    fn precancel_is_not_overwritten_by_registration() {
        let store = ActionStore::default();
        let id = Uuid::new_v4();
        let signal = StopSignal::new();
        signal.stop();
        lock(&store.operations).insert(
            id,
            Operation {
                connection: None,
                stop: signal,
                cancel: None,
                since: Instant::now(),
            },
        );
        let (_guard, signal) = store.register(id, Uuid::new_v4()).unwrap();
        assert!(signal.is_stopped());
    }
}
