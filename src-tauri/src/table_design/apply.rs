//! Preview (re-check, render, count, guard, store a single-use plan) and apply (re-check
//! on the DDL session, send exactly the stored statements) of a design.
use super::{compute, current_fingerprint, session_db, table_changed, timed_out, Design};
use crate::ddl_exec::{
    execute, failure_error, failure_outcome, stopped, write_history, ActionOutcome, DdlHistory,
    HISTORY_FAILED,
};
use crate::object_actions::{stale, MAX_PLANS};
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager, METADATA_CALL_TIMEOUT,
};
use dnt_core::config::EnvTag;
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, Dialect, StatementKind};
use dnt_core::value::{ColumnMeta, Value};
use dnt_schema::{Impact, TableDef};
use dnt_store::history::HistoryOrigin;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CountedImpact {
    pub impact: Impact,
    pub rows: Option<f64>,
    pub counted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DesignPreview {
    pub plan_id: Uuid,
    pub statements: Vec<String>,
    pub impacts: Vec<CountedImpact>,
    pub confirm_name: Option<String>,
    pub expires_in_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DesignReport {
    pub outcome: ActionOutcome,
    pub error: Option<AppError>,
    pub warnings: Vec<String>,
    pub statements_sent: u32,
}

pub(crate) struct DesignPlan {
    preview: DesignPreview,
    pub(super) design_id: Uuid,
    pub(super) conn: Uuid,
    scope: Scope,
    /// The table before the change (`None`: it is created by the plan) and after it.
    before: Option<ObjectRef>,
    after: ObjectRef,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    env: Option<EnvTag>,
    dialect: Dialect,
    fingerprint: String,
    expires: Instant,
}

/// First column of the first row as a number (a `COUNT(*)`).
#[derive(Default)]
struct FirstNumber(Option<f64>);

impl RowSink for FirstNumber {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.0.is_some() {
            return;
        }
        self.0 = match batch.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(Value::Int(n)) => Some(n as f64),
            Some(Value::UInt(n)) => Some(n as f64),
            Some(Value::Float(n)) => Some(n),
            Some(Value::Decimal(s) | Value::Text(s)) => s.trim().parse().ok(),
            _ => None,
        };
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}

/// Runs one count with the driver's own statement timeout: on expiry the driver stops
/// it on the server (MySQL `KILL QUERY`, PostgreSQL cancel request) and waits `grace`.
async fn count_rows(
    session: &dyn Session,
    sql: &str,
    kind: StatementKind,
    limit: Duration,
    grace: Duration,
) -> Result<Option<f64>, DbError> {
    let mut sink = FirstNumber::default();
    let options = ExecOptions {
        timeout: Some(limit),
        cancel_grace: grace,
        ..ExecOptions::default()
    };
    session.execute_one(sql, kind, &options, &mut sink).await?;
    Ok(sink.0)
}

/// Runs every `count_sql` (each must classify as a read) in one read-only transaction
/// (PostgreSQL: `standard_conforming_strings` on and `statement_timeout` = `limit`).
/// Each count is bounded by `limit` on the server too (ruling T10-1): the driver
/// cancels it when its statement timeout expires, and if it has not returned within
/// `limit + grace` the session's cancel handle is fired and the session is not used
/// again (the caller closes it). A count that fails or times out is "not counted".
pub(super) async fn count_impacts(
    session: &dyn Session,
    impacts: Vec<Impact>,
    limit: Duration,
    grace: Duration,
) -> Vec<CountedImpact> {
    let pg = session.server_info().kind == DbKind::Postgres;
    let mut out: Vec<CountedImpact> = impacts
        .into_iter()
        .map(|impact| CountedImpact {
            impact,
            rows: None,
            counted: false,
        })
        .collect();
    if !out.iter().any(|c| c.impact.count_sql.is_some()) {
        return out;
    }
    let begin = async {
        let start = if pg {
            "BEGIN READ ONLY"
        } else {
            "START TRANSACTION READ ONLY"
        };
        execute(session, start, StopSignal::new(), grace, 0).await?;
        if pg {
            for sql in [
                "SET LOCAL standard_conforming_strings = on".to_string(),
                format!("SET LOCAL statement_timeout = '{}ms'", limit.as_millis()),
            ] {
                execute(session, &sql, StopSignal::new(), grace, 0).await?;
            }
        }
        Ok::<(), DbError>(())
    };
    if !matches!(tokio::time::timeout(limit, begin).await, Ok(Ok(()))) {
        return out;
    }
    for c in out.iter_mut() {
        let Some(sql) = c.impact.count_sql.clone() else {
            continue;
        };
        let kind = classify(&sql, session.dialect());
        if kind != StatementKind::Read {
            continue;
        }
        match tokio::time::timeout(limit + grace, count_rows(session, &sql, kind, limit, grace))
            .await
        {
            Ok(Ok(Some(n))) => {
                c.rows = Some(n);
                c.counted = true;
            }
            Ok(_) => {}
            Err(_) => {
                // The driver did not stop it: stop it on the server ourselves.
                let _ = tokio::time::timeout(grace, session.cancel_handle().cancel()).await;
                return out;
            }
        }
    }
    let _ = tokio::time::timeout(
        limit,
        execute(session, "ROLLBACK", StopSignal::new(), grace, 0),
    )
    .await;
    out
}

/// Judges every statement on its own with the query guard: each must be exactly one
/// statement and none may be blocked.
fn judge(
    statements: &[String],
    env: Option<EnvTag>,
    read_only: bool,
    dialect: Dialect,
) -> Result<(), DbError> {
    let mut blocked = 0u32;
    for s in statements {
        let mut judged = dnt_query::plan(s, dnt_query::Target::All, dialect);
        dnt_query::guard_with(
            &mut judged,
            dnt_query::GuardCtx {
                env,
                read_only,
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
            blocked += 1;
        }
    }
    if blocked > 0 {
        Err(DbError::Blocked { count: blocked })
    } else {
        Ok(())
    }
}

/// MySQL / MariaDB apply a design as ONE statement (`ALTER TABLE` / `CREATE TABLE`);
/// PostgreSQL needs at least one.
pub(super) fn check_statement_count(kind: DbKind, statements: &[String]) -> Result<(), DbError> {
    match (kind, statements.len()) {
        (_, 0) => Err(config_error("draft", "There are no changes to apply.")),
        (DbKind::MySql, n) if n != 1 => Err(config_error(
            "draft",
            "A MySQL/MariaDB change must be a single statement.",
        )),
        _ => Ok(()),
    }
}

fn draft_problem(errors: &[dnt_schema::DraftError], error: Option<&str>) -> Option<DbError> {
    if let Some(e) = error {
        return Some(config_error("draft", e));
    }
    if errors.is_empty() {
        return None;
    }
    let text: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
    Some(config_error("draft", &text.join(" ")))
}

impl SessionManager {
    pub async fn table_design_preview(
        &self,
        design_id: Uuid,
        draft: TableDef,
    ) -> Result<DesignPreview, DbError> {
        let design: Design = self.table_designs.get(design_id)?;
        let conn = design.conn;
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        let rt = self.runtime(conn)?;
        rt.queries.check_ddl_idle()?;
        let (session, generation) = rt
            .open_query_session(session_db(cfg.kind, &design.scope))
            .await?;
        let dialect = session.dialect();
        let draft_name = draft.name.clone();
        let result = async {
            let fp = tokio::time::timeout(
                METADATA_CALL_TIMEOUT,
                current_fingerprint(session.as_ref(), design.object.as_ref()),
            )
            .await
            .map_err(|_| timed_out())??;
            if fp != design.fingerprint {
                return Err(table_changed());
            }
            let r = compute(&design, draft, dialect);
            if let Some(e) = draft_problem(&r.errors, r.error.as_deref()) {
                return Err(e);
            }
            if r.statements.is_empty() {
                return Err(config_error("draft", "There are no changes to apply."));
            }
            check_statement_count(cfg.kind, &r.statements)?;
            judge(&r.statements, cfg.env_tag, cfg.read_only, dialect)?;
            let impacts = count_impacts(
                session.as_ref(),
                r.impacts,
                self.table_designs.count_timeout,
                self.table_designs.count_grace,
            )
            .await;
            Ok((r.statements, impacts))
        }
        .await;
        close_quietly(session).await;
        let (statements, impacts) = result?;
        let table_name = design
            .base
            .original_name
            .clone()
            .unwrap_or_else(|| draft_name.clone());
        let preview = DesignPreview {
            plan_id: Uuid::new_v4(),
            statements,
            impacts,
            confirm_name: (cfg.env_tag == Some(EnvTag::Production)).then_some(table_name),
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let after = ObjectRef {
            database: design.scope.database.clone(),
            schema: design.scope.schema.clone(),
            name: draft_name,
            kind: ObjectKind::Table,
        };
        let mut plans = lock(&self.table_designs.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            DesignPlan {
                preview: preview.clone(),
                design_id,
                conn,
                scope: design.scope.clone(),
                before: design.object.clone(),
                after,
                rt,
                generation,
                env: cfg.env_tag,
                dialect,
                fingerprint: design.fingerprint.clone(),
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    pub async fn table_design_apply(
        &self,
        plan: Uuid,
        confirmed: bool,
        name: Option<String>,
        operation: Uuid,
        progress: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<DesignReport, DbError> {
        let stored = lock(&self.table_designs.plans)
            .remove(&plan)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        let conn = stored.conn;
        let ops = &self.table_designs.operations;
        let (_guard, stop) = ops.register(operation, conn)?;
        self.check_design_context(&stored)?;
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
        let (session, generation) = stored
            .rt
            .open_query_session(session_db(stored.rt.cfg.kind, &stored.scope))
            .await?;
        ops.set_cancel(operation, Some(session.cancel_handle()));
        let checked = async {
            if generation != stored.generation || session.dialect() != stored.dialect {
                return Err(stale());
            }
            let before = session.reconnects();
            session.ping().await?;
            // The lock wait bound covers the re-check too: reading the table waits for a
            // lock another session holds on it (PostgreSQL's size functions). A reconnect
            // after this loses it and is caught as stale below.
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
            if current_fingerprint(session.as_ref(), stored.before.as_ref()).await?
                != stored.fingerprint
            {
                return Err(table_changed());
            }
            self.check_design_context(&stored)?;
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
        let statements = &stored.preview.statements;
        let total = statements.len();
        let mut sent = 0u32;
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
            self.check_design_context(&stored)?;
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
                if session.reconnects() != 0 {
                    return Err(stale());
                }
            }
            if pg {
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
        let mut report = DesignReport {
            outcome: ActionOutcome::Applied,
            error: None,
            warnings: Vec::new(),
            statements_sent: sent,
        };
        if let Err(e) = result {
            report.outcome = failure_outcome(
                session.as_ref(),
                &e,
                pg,
                sent > 0,
                commit_sent,
                self.cancel_grace,
            )
            .await;
            report.error = Some(failure_error(e, report.outcome));
        }
        // No cancellation can reach a later statement or a pooled session: this session is closed.
        ops.set_cancel(operation, None);
        close_quietly(session).await;
        if sent > 0 {
            let sql = statements.join(";\n");
            let saved = write_history(
                self.repo.clone(),
                DdlHistory {
                    connection_id: conn,
                    database: Some(stored.scope.database.clone()),
                    sql: &sql,
                    dialect: stored.dialect,
                    started_at,
                    elapsed: started.elapsed(),
                    outcome: report.outcome,
                    error: report.error.as_ref(),
                    origin: HistoryOrigin::TableDesign,
                },
            )
            .await;
            if !saved {
                report.warnings.push(HISTORY_FAILED.into());
            }
            self.after_ddl(conn, &stored.scope.database);
            let target = if report.outcome == ActionOutcome::Applied {
                Some(stored.after.clone())
            } else {
                stored.before.clone()
            };
            if self
                .reload_design(stored.design_id, &stored.rt, &stored.scope, target)
                .await
                .is_err()
            {
                report.warnings.push(
                    "The table could not be reloaded after the change; open it again.".into(),
                );
            }
        }
        progress("finished");
        Ok(report)
    }

    fn check_design_context(&self, stored: &DesignPlan) -> Result<(), DbError> {
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
        check_statement_count(cfg.kind, &stored.preview.statements)?;
        judge(
            &stored.preview.statements,
            cfg.env_tag,
            cfg.read_only,
            stored.dialect,
        )?;
        rt.queries.check_ddl_idle()
    }
}
