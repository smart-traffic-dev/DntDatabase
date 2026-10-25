//! Spec §6: the apply of a stored plan. It re-checks the context and the object on the
//! DDL session, sends exactly the plan's statements (PostgreSQL in one transaction with
//! its post-checks), reports an explicit outcome and object state, writes one History
//! entry and refreshes the edit.
use super::{
    catalog, current_fingerprint, judge, name_taken, object_changed, object_listed, object_ref,
    read_object, timed_out, EditPlan, ABSENT, HISTORY_ORIGIN,
};
use crate::ddl_exec::{
    execute, failure_error, failure_outcome, stopped, write_history, ActionOutcome, DdlHistory,
    HISTORY_FAILED,
};
use crate::object_actions::stale;
use crate::session_manager::{
    close_quietly, config_error, lock, SessionManager, METADATA_CALL_TIMEOUT,
};
use dnt_core::driver::{ExecOptions, Session, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::DbKind;
use dnt_core::sql::ddl_edit::{CatalogFacts, PostCheck};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

const NOT_RELOADED: &str = "The definition could not be reloaded; reopen the editor.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum EditOutcome {
    Applied,
    NotApplied,
    RolledBack,
    /// MySQL: some statements ran, then one failed and the object is not as before.
    Partial,
    Unknown,
}

/// The edited object after the apply, as far as the app knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ObjectState {
    Unchanged,
    Changed,
    Missing,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DdlEditReport {
    pub outcome: EditOutcome,
    pub error: Option<AppError>,
    pub warnings: Vec<String>,
    pub statements_sent: u32,
    /// MySQL / MariaDB: the plan statements that completed. Empty on PostgreSQL, where
    /// the plan runs in one transaction and no statement takes effect on its own (only
    /// a `Partial` outcome keeps it, which PostgreSQL never reports).
    pub executed: Vec<String>,
    /// MySQL / MariaDB: the plan statement that failed. `None` on PostgreSQL, as
    /// `executed`.
    pub failed: Option<String>,
    pub object_state: ObjectState,
    /// `Missing`: the original DDL, offered for a create of the same name.
    pub restore_text: Option<String>,
    /// Applied: the object as it is now, and its DDL re-read from the server.
    pub object: Option<ObjectRef>,
    pub text: Option<String>,
}

impl From<EditOutcome> for ActionOutcome {
    fn from(o: EditOutcome) -> Self {
        match o {
            EditOutcome::Applied => ActionOutcome::Applied,
            // A partial apply is a failed one; its object state says what is left.
            EditOutcome::NotApplied | EditOutcome::Partial => ActionOutcome::NotApplied,
            EditOutcome::RolledBack => ActionOutcome::RolledBack,
            EditOutcome::Unknown => ActionOutcome::Unknown,
        }
    }
}

fn edit_outcome(o: ActionOutcome) -> EditOutcome {
    match o {
        ActionOutcome::Applied => EditOutcome::Applied,
        ActionOutcome::NotApplied => EditOutcome::NotApplied,
        ActionOutcome::RolledBack => EditOutcome::RolledBack,
        ActionOutcome::Unknown => EditOutcome::Unknown,
    }
}

fn kind_word(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Event => "EVENT",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        _ => "VIEW",
    }
}

/// The History text of a plan: the statements joined with `;`, or for a MySQL
/// compound statement (routine, trigger, event) a `DELIMITER $$` script that a query
/// tab can run again.
pub(crate) fn history_sql(engine: DbKind, kind: ObjectKind, statements: &[String]) -> String {
    let compound = engine == DbKind::MySql && kind != ObjectKind::View;
    if compound {
        format!("DELIMITER $$\n{}$$\nDELIMITER ;", statements.join("$$\n"))
    } else {
        statements.join(";\n")
    }
}

/// Runs one post-check: its single text value must equal `expect`.
async fn run_check(
    session: &dyn Session,
    check: &PostCheck,
    stop: StopSignal,
    grace: Duration,
    timeout_s: u32,
) -> Result<(), DbError> {
    let options = ExecOptions {
        params: check.params.clone(),
        stop,
        timeout: (timeout_s > 0).then(|| Duration::from_secs(u64::from(timeout_s))),
        cancel_grace: grace,
        ..ExecOptions::default()
    };
    let rows = catalog::rows_with(session, &check.sql, options).await?;
    let value = rows.first().and_then(|r| r.first().cloned().flatten());
    if value.as_deref() == Some(check.expect.as_str()) {
        Ok(())
    } else {
        Err(config_error("check", &check.failure))
    }
}

/// The object of a plan read again after a failed MySQL statement (spec §6.3).
async fn reread_state(session: &dyn Session, plan: &EditPlan) -> ObjectState {
    let Some(name) = plan.target.name.as_deref() else {
        return ObjectState::Unknown;
    };
    let read = async {
        if !object_listed(session, &plan.target, name).await? {
            return Ok(if plan.new {
                ObjectState::Unchanged
            } else {
                ObjectState::Missing
            });
        }
        if plan.new {
            return Ok(ObjectState::Changed);
        }
        let now = read_object(session, &plan.target, name).await?;
        Ok::<_, DbError>(if now.fingerprint == plan.fingerprint {
            ObjectState::Unchanged
        } else {
            ObjectState::Changed
        })
    };
    match tokio::time::timeout(METADATA_CALL_TIMEOUT, read).await {
        Ok(Ok(state)) => state,
        _ => ObjectState::Unknown,
    }
}

impl SessionManager {
    pub async fn ddl_edit_apply(
        &self,
        plan: Uuid,
        confirmed: bool,
        name: Option<String>,
        operation: Uuid,
        progress: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<DdlEditReport, DbError> {
        let stored = lock(&self.ddl_edits.plans)
            .remove(&plan)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        let conn = stored.conn;
        let ops = &self.ddl_edits.operations;
        let (_guard, stop) = ops.register(operation, conn)?;
        self.check_edit_context(&stored)?;
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
        let pg = stored.rt.cfg.kind == DbKind::Postgres;
        let timeout_s = stored.rt.cfg.options.query_timeout_s;
        let object_name = stored.target.name.clone().unwrap_or_default();
        let (session, generation) = stored
            .rt
            .open_query_session(Some(stored.scope.database.clone()))
            .await?;
        ops.set_cancel(operation, Some(session.cancel_handle()));
        let checked = async {
            if generation != stored.generation || session.dialect() != stored.dialect {
                return Err(stale());
            }
            let before = session.reconnects();
            session.ping().await?;
            // The lock wait bound covers the re-read too; a reconnect after this loses
            // it and is caught as stale below.
            execute(
                session.as_ref(),
                if pg {
                    "SET lock_timeout = '5s'"
                } else {
                    "SET SESSION lock_wait_timeout = 5"
                },
                stop.clone(),
                self.cancel_grace,
                timeout_s,
            )
            .await?;
            if stored.new {
                if name_taken(session.as_ref(), &stored.target, &object_name).await? {
                    return Err(object_changed());
                }
            } else {
                let object = object_ref(&stored.target, &object_name);
                if current_fingerprint(session.as_ref(), &stored.target, Some(&object)).await?
                    != stored.fingerprint
                {
                    return Err(object_changed());
                }
            }
            self.check_edit_context(&stored)?;
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
            .unwrap_or_else(|_| Err(timed_out()));
        if let Err(e) = checked {
            ops.set_cancel(operation, None);
            close_quietly(session).await;
            return Err(e);
        }
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let statements = &stored.statements;
        let total = statements.len();
        let mut sent = 0u32;
        let mut done = 0usize;
        let mut commit_sent = false;
        let result = async {
            macro_rules! send {
                ($sql:expr) => {
                    execute(
                        session.as_ref(),
                        $sql,
                        stop.clone(),
                        self.cancel_grace,
                        timeout_s,
                    )
                };
            }
            if pg {
                send!("BEGIN").await?;
                send!("SET LOCAL standard_conforming_strings = on").await?;
            }
            self.check_edit_context(&stored)?;
            if session.dialect() != stored.dialect || session.reconnects() != 0 {
                return Err(stale());
            }
            for (i, sql) in statements.iter().enumerate() {
                if stop.is_stopped() {
                    return Err(stopped());
                }
                progress(&format!("executing {}/{total}", i + 1));
                sent += 1;
                send!(sql).await?;
                done += 1;
                if session.reconnects() != 0 {
                    return Err(stale());
                }
            }
            if pg {
                for check in &stored.checks {
                    if stop.is_stopped() {
                        return Err(stopped());
                    }
                    progress("verifying");
                    run_check(
                        session.as_ref(),
                        check,
                        stop.clone(),
                        self.cancel_grace,
                        timeout_s,
                    )
                    .await?;
                }
                if stop.is_stopped() {
                    return Err(stopped());
                }
                progress("committing");
                commit_sent = true;
                send!("COMMIT").await?;
            }
            Ok(())
        }
        .await;
        let mut report = DdlEditReport {
            outcome: EditOutcome::Applied,
            error: None,
            warnings: Vec::new(),
            statements_sent: sent,
            executed: statements[..done].to_vec(),
            failed: None,
            object_state: ObjectState::Changed,
            restore_text: None,
            object: None,
            text: None,
        };
        let mut reloaded = None;
        match result {
            Ok(()) => {
                let read = tokio::time::timeout(
                    METADATA_CALL_TIMEOUT,
                    read_object(session.as_ref(), &stored.target, &object_name),
                )
                .await;
                match read {
                    Ok(Ok(state)) => reloaded = Some(state),
                    _ => report.warnings.push(NOT_RELOADED.into()),
                }
            }
            Err(e) => {
                if (done as u32) < sent {
                    report.failed = statements.get(done).cloned();
                }
                let action = if pg {
                    let action = failure_outcome(
                        session.as_ref(),
                        &e,
                        true,
                        sent > 0,
                        commit_sent,
                        self.cancel_grace,
                    )
                    .await;
                    // A failure after BEGIN but before the first plan statement (the
                    // SET LOCAL, a context re-check) changed nothing, whatever the
                    // ROLLBACK did.
                    let action = if sent == 0 {
                        ActionOutcome::NotApplied
                    } else {
                        action
                    };
                    report.outcome = edit_outcome(action);
                    report.object_state = match action {
                        ActionOutcome::RolledBack | ActionOutcome::NotApplied => {
                            ObjectState::Unchanged
                        }
                        _ => ObjectState::Unknown,
                    };
                    action
                } else {
                    report.object_state = if sent == 0 {
                        ObjectState::Unchanged
                    } else if matches!(e, DbError::Server { .. }) {
                        reread_state(session.as_ref(), &stored).await
                    } else {
                        self.reread_fresh(&stored).await
                    };
                    report.outcome = match report.object_state {
                        ObjectState::Unchanged => EditOutcome::NotApplied,
                        ObjectState::Missing | ObjectState::Changed => EditOutcome::Partial,
                        ObjectState::Unknown => EditOutcome::Unknown,
                    };
                    if let Some(r) = &stored.rehearsal {
                        // The rehearsal routine was created; its DROP did not complete.
                        if done == 1 {
                            let kind = kind_word(stored.kind);
                            report.warnings.push(format!(
                                "A check routine `{r}` was left behind; drop it with: DROP {kind} `{r}`"
                            ));
                        }
                    }
                    report.outcome.into()
                };
                report.error = Some(failure_error(e, action));
            }
        }
        if pg && report.outcome != EditOutcome::Partial {
            // One transaction: no statement took effect on its own.
            report.executed.clear();
            report.failed = None;
        }
        // No cancellation can reach a later statement or a pooled session: this session is closed.
        ops.set_cancel(operation, None);
        close_quietly(session).await;
        if sent > 0 {
            let sql = history_sql(stored.rt.cfg.kind, stored.kind, statements);
            let saved = write_history(
                self.repo.clone(),
                DdlHistory {
                    connection_id: conn,
                    database: Some(stored.scope.database.clone()),
                    sql: &sql,
                    dialect: stored.dialect,
                    started_at,
                    elapsed: started.elapsed(),
                    outcome: report.outcome.into(),
                    error: report.error.as_ref(),
                    origin: HISTORY_ORIGIN,
                },
            )
            .await;
            if !saved {
                report.warnings.push(HISTORY_FAILED.into());
            }
            self.after_ddl(conn, &stored.scope.database);
        }
        // The edit is updated and its other plans dropped under the edits lock (order
        // edits → plans, as the preview's insert): a preview running meanwhile either
        // stores its plan before (and it is dropped here) or sees the new fingerprint.
        {
            let mut edits = lock(&self.ddl_edits.edits);
            if let Some(state) = reloaded {
                let mut target = stored.target.clone();
                if target.kind == ObjectKind::Trigger && state.facts.table.is_some() {
                    target.table = state.facts.table.clone();
                }
                let object = object_ref(&target, &object_name);
                if let Some(edit) = edits.get_mut(&stored.edit_id) {
                    edit.target = target;
                    edit.object = Some(object.clone());
                    edit.base_text = state.text.clone();
                    edit.fingerprint = state.fingerprint;
                    edit.facts = state.facts;
                    edit.blocked = state.blocked;
                }
                report.object = Some(object);
                report.text = Some(state.text);
            }
            if report.object_state == ObjectState::Missing && !stored.new {
                // The restore is an ordinary create of the same name (spec §6.3).
                report.restore_text = Some(stored.base_text.clone());
                if let Some(edit) = edits.get_mut(&stored.edit_id) {
                    edit.target.name = None;
                    edit.object = None;
                    edit.base_text = String::new();
                    edit.fingerprint = ABSENT.into();
                    edit.facts = CatalogFacts::default();
                    edit.blocked = None;
                }
            }
            if sent > 0 {
                // Other plans of this edit were judged against the object as it was.
                lock(&self.ddl_edits.plans).retain(|_, p| p.edit_id != stored.edit_id);
            }
        }
        progress("finished");
        Ok(report)
    }

    /// The object state read on a fresh session (after a transport loss or a stop).
    async fn reread_fresh(&self, stored: &EditPlan) -> ObjectState {
        let opened = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            stored
                .rt
                .open_query_session(Some(stored.scope.database.clone())),
        )
        .await;
        let Ok(Ok((session, _))) = opened else {
            return ObjectState::Unknown;
        };
        let state = reread_state(session.as_ref(), stored).await;
        close_quietly(session).await;
        state
    }

    fn check_edit_context(&self, stored: &EditPlan) -> Result<(), DbError> {
        let cfg = self.repo.get(stored.conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if cfg.env_tag != stored.env || cfg != stored.rt.cfg {
            return Err(stale());
        }
        let rt = self.runtime(stored.conn)?;
        if !Arc::ptr_eq(&rt, &stored.rt)
            || rt.closed.load(Ordering::SeqCst)
            || rt.tunnel_dead()
            || rt.tunnel_gen.load(Ordering::SeqCst) != stored.generation
        {
            return Err(stale());
        }
        judge(
            &stored.statements,
            cfg.env_tag,
            cfg.read_only,
            stored.dialect,
        )?;
        rt.queries.check_ddl_idle()
    }
}
