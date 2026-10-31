//! Preview (re-check, render, count, guard, store a single-use plan) and apply (re-check
//! on the DDL session, send exactly the stored statements) of a design.
use super::{
    compute, current_fingerprint, current_state, session_db, table_changed, timed_out, Design,
};
use crate::ddl_exec::{
    execute, failure_error, failure_outcome, mysql_statement_refused, stopped, write_history,
    ActionOutcome, DdlHistory, HISTORY_FAILED,
};
use crate::object_actions::{stale, MAX_PLANS};
use crate::session_manager::{
    close_quietly, config_error, lock, sql_or_close, ConnectionRuntime, SessionManager,
    METADATA_CALL_TIMEOUT,
};
use dnt_core::config::EnvTag;
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, SqlSession, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::server::{ServerInfo, ServerVariant, SqlKind};
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
    /// The table's row estimate read with the preview (shown next to rebuild warnings);
    /// `None` for a new table or when the server has none. Exported as a JS number.
    #[specta(type = Option<f64>)]
    pub row_estimate: Option<u64>,
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
    session: &dyn SqlSession,
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

/// The statement that bounds every later `SELECT` of the session on the server to
/// `limit`: MySQL 5.7.8+ `max_execution_time` (milliseconds), MariaDB 10.1+
/// `max_statement_time` (seconds); `None` on PostgreSQL (it uses `statement_timeout`)
/// and on older servers.
pub(super) fn mysql_count_bound(server: &ServerInfo, limit: Duration) -> Option<String> {
    match server.variant {
        ServerVariant::MariaDb if server.version >= (10, 1, 0) => Some(format!(
            "SET SESSION max_statement_time = {:.3}",
            limit.as_secs_f64()
        )),
        ServerVariant::MySql57 | ServerVariant::MySql8 if server.version >= (5, 7, 8) => Some(
            format!("SET SESSION max_execution_time = {}", limit.as_millis()),
        ),
        _ => None,
    }
}

/// Runs every `count_sql` (each must classify as a read) in one read-only transaction.
/// Each count is bounded by `limit` on the server (ruling T10-1): PostgreSQL by
/// `SET LOCAL statement_timeout` (with `standard_conforming_strings` on), MySQL /
/// MariaDB by the session's `max_execution_time` / `max_statement_time` (the session is
/// a dedicated one that the caller closes afterwards, so nothing is restored), and the
/// driver's own statement timeout stops it too. A count that has not returned within
/// `limit + grace` ends the counting: the session is not used again. A count that
/// fails or times out is "not counted".
pub(super) async fn count_impacts(
    session: &dyn SqlSession,
    impacts: Vec<Impact>,
    limit: Duration,
    grace: Duration,
) -> Result<Vec<CountedImpact>, DbError> {
    let kind = session.sql_kind();
    let bound = mysql_count_bound(session.server_info(), limit);
    let mut out: Vec<CountedImpact> = impacts
        .into_iter()
        .map(|impact| CountedImpact {
            impact,
            rows: None,
            counted: false,
        })
        .collect();
    if !out.iter().any(|c| c.impact.count_sql.is_some()) {
        return Ok(out);
    }
    let begin = async {
        match kind {
            SqlKind::MySql => {
                let start = "START TRANSACTION READ ONLY";
                execute(session, start, StopSignal::new(), grace, 0).await?;
                if let Some(sql) = &bound {
                    execute(session, sql, StopSignal::new(), grace, 0).await?;
                }
            }
            SqlKind::Postgres => {
                execute(session, "BEGIN READ ONLY", StopSignal::new(), grace, 0).await?;
                for sql in [
                    "SET LOCAL standard_conforming_strings = on".to_string(),
                    format!("SET LOCAL statement_timeout = '{}ms'", limit.as_millis()),
                ] {
                    execute(session, &sql, StopSignal::new(), grace, 0).await?;
                }
            }
        }
        Ok::<(), DbError>(())
    };
    if !matches!(tokio::time::timeout(limit, begin).await, Ok(Ok(()))) {
        return Ok(out);
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
            Err(_) => return Ok(out),
        }
    }
    let _ = tokio::time::timeout(
        limit,
        execute(session, "ROLLBACK", StopSignal::new(), grace, 0),
    )
    .await;
    Ok(out)
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
pub(super) fn check_statement_count(kind: SqlKind, statements: &[String]) -> Result<(), DbError> {
    match (kind, statements.len()) {
        (SqlKind::MySql | SqlKind::Postgres, 0) => {
            Err(config_error("draft", "There are no changes to apply."))
        }
        (SqlKind::MySql, n) if n != 1 => Err(config_error(
            "draft",
            "A MySQL/MariaDB change must be a single statement.",
        )),
        (SqlKind::MySql | SqlKind::Postgres, _) => Ok(()),
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
        let kind = cfg.kind.require_sql()?;
        let rt = self.runtime(conn)?;
        rt.queries.check_ddl_idle()?;
        let (session, generation) = rt
            .open_query_session(session_db(kind, &design.scope))
            .await?;
        let session = sql_or_close(session).await?;
        let sql_session = session.sql()?;
        let dialect = sql_session.dialect();
        let draft_name = draft.name.clone();
        let result = async {
            let (fp, row_estimate) = tokio::time::timeout(
                METADATA_CALL_TIMEOUT,
                current_state(sql_session, design.object.as_ref()),
            )
            .await
            .map_err(|_| timed_out())??;
            if fp != design.fingerprint {
                return Err(table_changed());
            }
            let r = compute(&design, draft, dialect)?;
            if let Some(e) = draft_problem(&r.errors, r.error.as_deref()) {
                return Err(e);
            }
            if r.statements.is_empty() {
                return Err(config_error("draft", "There are no changes to apply."));
            }
            check_statement_count(kind, &r.statements)?;
            judge(&r.statements, cfg.env_tag, cfg.read_only, dialect)?;
            let impacts = count_impacts(
                sql_session,
                r.impacts,
                self.table_designs.count_timeout,
                self.table_designs.count_grace,
            )
            .await?;
            Ok((r.statements, impacts, row_estimate))
        }
        .await;
        close_quietly(session).await;
        let (statements, impacts, row_estimate) = result?;
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
            row_estimate,
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
        let kind = stored.rt.cfg.kind.require_sql()?;
        let pg = match kind {
            SqlKind::MySql => false,
            SqlKind::Postgres => true,
        };
        let timeout_s = stored.rt.cfg.options.query_timeout_s;
        let (session, generation) = stored
            .rt
            .open_query_session(session_db(kind, &stored.scope))
            .await?;
        let session = sql_or_close(session).await?;
        let sql_session = session.sql()?;
        ops.set_cancel(operation, Some(session.cancel_handle()));
        let checked = async {
            if generation != stored.generation || sql_session.dialect() != stored.dialect {
                return Err(stale());
            }
            let before = session.reconnects();
            session.ping().await?;
            // The lock wait bound covers the re-check too: reading the table waits for a
            // lock another session holds on it (PostgreSQL's size functions). A reconnect
            // after this loses it and is caught as stale below.
            execute(
                sql_session,
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
            if current_fingerprint(sql_session, stored.before.as_ref()).await? != stored.fingerprint
            {
                return Err(table_changed());
            }
            self.check_design_context(&stored)?;
            if before != session.reconnects() || sql_session.dialect() != stored.dialect {
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
                        sql_session,
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
            if sql_session.dialect() != stored.dialect || session.reconnects() != 0 {
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
            // MySQL / MariaDB run each DDL statement on its own (no transaction), so a later
            // statement's failure would leave the earlier ones applied: unknown. A server error
            // on the first statement (a refused CREATE TABLE, an invalid default) changed
            // nothing, unless it was a Stop or failed partway (see `mysql_statement_refused`).
            // A MySQL design renders as one statement.
            report.outcome = if sent == 1 && mysql_statement_refused(pg, &e) && !stop.is_stopped() {
                ActionOutcome::NotApplied
            } else {
                failure_outcome(
                    sql_session,
                    &e,
                    pg,
                    sent > 0,
                    commit_sent,
                    self.cancel_grace,
                )
                .await
            };
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
        check_statement_count(cfg.kind.require_sql()?, &stored.preview.statements)?;
        judge(
            &stored.preview.statements,
            cfg.env_tag,
            cfg.read_only,
            stored.dialect,
        )?;
        rt.queries.check_ddl_idle()
    }
}
