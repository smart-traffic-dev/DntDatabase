//! Edit plans of the data view (spec §4.2b–d, §5.2): `data_preview` builds an immutable
//! plan and stores it; `data_apply` runs exactly that plan, once.
//!
//! # Preview
//!
//! The plan is built from the table's detail as the server has it now (not the tree's
//! cache), the result-set types of its columns (a `LIMIT 0` read) and the `Dialect` of a
//! data session, with the guard context of the connection as stored. It is stored with
//! what it was made for: the connection's runtime and SSH tunnel generation, the table,
//! the env tag and read-only flag. A plan lives [`crate::query_sessions::PLAN_TTL`]; a
//! newer preview of the same table on the same connection replaces it.
//!
//! # Apply
//!
//! The plan is taken out of the store first: it runs at most once, whatever happens.
//! Before anything is sent it is checked again, and a mismatch is
//! [`DbError::PlanStale`]: the same connection runtime (not closed or reopened) and SSH
//! tunnel, the guard verdict with the config as stored now (`Blocked`,
//! `ConfirmationRequired`), the env tag and read-only flag, the table's fingerprint, and
//! the `Dialect` of the data session that will run it.
//!
//! The session is taken from the data pool (never retried on another session: a write
//! is never sent twice) and given back only when the transaction ended cleanly.
//!
//! - `atomic == Yes`: `BEGIN`; the session checks (PostgreSQL
//!   `standard_conforming_strings` on, MySQL `character_set_client` utf8mb4, the dialect
//!   again); every statement, each expecting its row count (a mismatch rolls back and
//!   marks the statement a conflict); `COMMIT`. Any failure before `COMMIT` was sent
//!   leaves nothing applied ([`ApplyOutcome::RolledBack`]: rolled back, or the server
//!   rolls back when the connection is gone); a `COMMIT` without an answer is
//!   [`ApplyOutcome::Unknown`].
//! - otherwise the plan has one statement, run without a transaction. Right before it
//!   the session is pinged and checked again; a silent reconnect during the checks is
//!   `PlanStale` (nothing sent), one seen only after the write, a lost connection or
//!   timeout while it runs, or a row count that is neither 0 nor the one expected is
//!   `Unknown { at: 0 }`.
//!
//! A session goes back to the pool only after a commit or a conflict's clean rollback:
//! after a failed statement a cancel request may still be on its way to it.
//!
//! MySQL counts changed rows, not matched ones: an UPDATE that reports 0 rows is checked
//! with its `recheck` (a locking `SELECT … FOR UPDATE`, which sees the latest committed
//! row as the UPDATE did) before it is called a conflict. An INSERT's row is read back
//! (PostgreSQL `RETURNING`, MySQL its `read_back` SELECT) into [`ApplyReport::inserted`].

use crate::commands::data::check_object;
use crate::commands::settings::saved_confirm_unbounded;
use crate::data_sessions::{BoundedPage, FirstCell, DATA_CANCEL_GRACE};
use crate::session_manager::{config_error, is_lost, lock, ConnectionRuntime, SessionManager};
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{ExecOptions, Session};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectRef, TableDetail};
use dnt_core::sql::ident::{qualified_name, quote_ident};
use dnt_core::sql::{Dialect, StatementKind, TxControl};
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::edit::{
    build_edit_plan, edit_verdict, fingerprint, Atomicity, EditError, EditPlan, EditSet,
    PlannedEdit, Protection,
};
use dnt_query::{redact, GuardCtx, Verdict};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Most edits one preview takes.
pub const MAX_EDITS: usize = 10_000;

/// What `data_preview` returns: the stored plan's id and exactly the statements
/// `data_apply` will send.
#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
pub struct EditPreview {
    pub plan_id: Uuid,
    pub statements: Vec<String>,
    pub verdict: Verdict,
    pub atomic: Atomicity,
    /// The conflict check of each statement, in order.
    pub protections: Vec<Protection>,
    /// Why the verdict is `block` (UI copy).
    pub reason: Option<String>,
}

/// What became of an applied plan (spec §4.2d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ApplyOutcome {
    /// Every statement was applied.
    Committed,
    /// Nothing was applied: the transaction was rolled back (or the server rolls it back
    /// because the connection is gone).
    RolledBack,
    /// No transaction: the first `applied` statements are applied, the next one was not.
    PartiallyApplied { applied: u32 },
    /// The statement at position `at` of the plan (`statements.len()`: the `COMMIT`) got
    /// no answer: it may or may not have been applied.
    Unknown { at: u32 },
}

/// One statement of an applied plan.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct StmtApply {
    /// The edit's index in the `EditSet`.
    pub index: u32,
    /// Rows the statement matched.
    pub affected: Option<f64>,
    pub error: Option<AppError>,
    /// It matched no row (or not the rows expected): the row changed or went away since
    /// it was loaded.
    pub conflict: bool,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ApplyReport {
    pub outcome: ApplyOutcome,
    /// The statements that were sent, in order (those after a failure were not).
    pub statements: Vec<StmtApply>,
    /// One row per applied INSERT, in order: every column of the table (in its column
    /// order) as the server stored it; empty when it could not be read back. Empty when
    /// nothing was applied.
    pub inserted: Vec<Vec<Value>>,
    /// The error that ended the apply outside a statement (the `COMMIT`).
    pub error: Option<AppError>,
}

/// A stored plan and what it was made for.
struct StoredEdit {
    conn: Uuid,
    /// The runtime the plan was made on (a reopened connection is another one).
    runtime: Weak<ConnectionRuntime>,
    tunnel_gen: u64,
    object: ObjectRef,
    env: Option<EnvTag>,
    read_only: bool,
    plan: EditPlan,
    at: Instant,
}

impl StoredEdit {
    fn same_table(&self, conn: Uuid, o: &ObjectRef) -> bool {
        let t = &self.object;
        self.conn == conn && (&t.database, &t.schema, &t.name) == (&o.database, &o.schema, &o.name)
    }
}

/// Edit plans by id (see the module docs).
#[derive(Default)]
pub(crate) struct EditPlanStore {
    plans: Mutex<HashMap<Uuid, StoredEdit>>,
}

impl EditPlanStore {
    fn prune(plans: &mut HashMap<Uuid, StoredEdit>, ttl: Duration) {
        plans.retain(|_, p| p.at.elapsed() < ttl);
    }

    /// Stores `e`, replacing the plan of the same table on the same connection.
    fn insert(&self, ttl: Duration, e: StoredEdit) -> Uuid {
        let id = Uuid::new_v4();
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        plans.retain(|_, p| !p.same_table(e.conn, &e.object));
        plans.insert(id, e);
        id
    }

    /// Takes plan `id` out: `PlanExpired` when it is unknown, replaced, taken or too old.
    fn take(&self, ttl: Duration, id: Uuid) -> Result<StoredEdit, DbError> {
        let mut plans = lock(&self.plans);
        let plan = plans.remove(&id);
        Self::prune(&mut plans, ttl);
        match plan {
            Some(p) if p.at.elapsed() < ttl => Ok(p),
            _ => Err(DbError::PlanExpired),
        }
    }

    /// Forgets the plans of connection `conn` (closed).
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.conn != conn);
    }
}

fn stale(reason: &str) -> DbError {
    DbError::PlanStale {
        reason: reason.into(),
    }
}

/// An [`EditError`] as a `config` error: field `one_at_a_time` (apply one edit at a
/// time), `column:<name>` (that column's value), else `edit`.
pub fn edit_error(e: EditError) -> DbError {
    let (field, message) = match &e {
        EditError::OneAtATime => ("one_at_a_time".to_string(), e.to_string()),
        EditError::KeyNotComplete { column } => (
            format!("column:{column}"),
            "the key value may be cut; reload the row before editing it".to_string(),
        ),
        EditError::Invalid { column, reason } if !column.is_empty() => {
            (format!("column:{column}"), reason.clone())
        }
        _ => ("edit".to_string(), e.to_string()),
    };
    DbError::Config {
        field: Some(field),
        message,
    }
}

/// Whether a failed statement may have been applied.
fn outcome_unknown(e: &DbError) -> bool {
    match e.unstaged() {
        DbError::Timeout {
            outcome_unknown, ..
        }
        | DbError::ConnectionLost {
            outcome_unknown, ..
        }
        | DbError::Cancelled {
            outcome_unknown, ..
        } => *outcome_unknown,
        _ => false,
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The guard context of a connection whose config is `cfg` (as stored now).
fn guard_ctx(sm: &SessionManager, cfg: &ConnectionConfig) -> GuardCtx {
    GuardCtx {
        env: cfg.env_tag,
        read_only: cfg.read_only,
        analyze: false,
        confirm_unbounded: saved_confirm_unbounded(&sm.repo),
    }
}

impl SessionManager {
    /// The statement timeout of an edit: the connection's query timeout, else the data
    /// view's.
    fn edit_timeout(&self, rt: &ConnectionRuntime) -> Duration {
        match rt.cfg.options.query_timeout_s {
            0 => self.data_timeout,
            s => Duration::from_secs(s.into()),
        }
    }

    /// The result-set types of every column of `detail` (`SELECT <cols> … LIMIT 0`), the
    /// dialect of the data session that read them and its tunnel generation. A pooled
    /// session that went stale is replaced once (a read).
    async fn edit_metas(
        &self,
        rt: &ConnectionRuntime,
        detail: &TableDetail,
    ) -> Result<(Vec<ColumnMeta>, Dialect, u64), DbError> {
        let kind = rt.cfg.kind;
        let o = &detail.object;
        let cols: Vec<String> = detail
            .columns
            .iter()
            .map(|c| quote_ident(kind, &c.name))
            .collect();
        let sql = format!(
            "SELECT {} FROM {} LIMIT 0",
            cols.join(", "),
            qualified_name(kind, &o.database, o.schema.as_deref(), &o.name)
        );
        let opts = ExecOptions {
            max_rows: 1,
            timeout: Some(self.edit_timeout(rt)),
            cancel_grace: DATA_CANCEL_GRACE,
            ..ExecOptions::default()
        };
        let mut retried = false;
        loop {
            let taken = self.take_data_session(rt, Some(&o.database)).await?;
            let mut page = BoundedPage::new(self.data_cell_cap, self.data_page_cap);
            let out = taken
                .session
                .execute_one(&sql, StatementKind::Read, &opts, &mut page)
                .await;
            let (dialect, gen, pooled) = (taken.session.dialect(), taken.tunnel_gen, taken.pooled);
            self.return_data_session(rt, taken, out.is_ok());
            match out {
                Ok(_) => return Ok((page.columns, dialect, gen)),
                Err(e) if pooled && !retried && is_lost(&e) => retried = true,
                Err(e) => return Err(e),
            }
        }
    }

    /// Builds and stores the plan of `set` on connection `id` (see the module docs).
    /// Errors: `config` (field `edit`, `one_at_a_time` or `column:<name>`) for a set that
    /// cannot be planned, `not_open`, driver errors of the reads.
    pub async fn edit_preview(&self, id: Uuid, set: EditSet) -> Result<EditPreview, DbError> {
        if set.edits.len() > MAX_EDITS {
            return Err(config_error(
                "edit",
                &format!("apply at most {MAX_EDITS} changes at once"),
            ));
        }
        let cfg = self.repo.get(id)?;
        check_object(cfg.kind, &set.object)?;
        let rt = self.runtime(id)?;
        let detail = self.table_detail_fresh(id, &set.object).await?;
        let (metas, dialect, tunnel_gen) = self.edit_metas(&rt, &detail).await?;
        let plan = build_edit_plan(
            &set,
            &detail,
            &metas,
            dialect,
            guard_ctx(self, &cfg),
            &|_| None,
        )
        .map_err(edit_error)?;
        let mut preview = EditPreview {
            plan_id: Uuid::nil(),
            statements: plan.stmts.iter().map(|s| s.sql.clone()).collect(),
            verdict: plan.verdict,
            atomic: plan.atomic,
            protections: plan.stmts.iter().map(|s| s.protection.clone()).collect(),
            reason: plan.reason.clone(),
        };
        preview.plan_id = self.edit_plans.insert(
            self.plan_ttl,
            StoredEdit {
                conn: id,
                runtime: Arc::downgrade(&rt),
                tunnel_gen,
                object: set.object,
                env: cfg.env_tag,
                read_only: cfg.read_only,
                plan,
                at: Instant::now(),
            },
        );
        Ok(preview)
    }

    /// Runs the stored plan `plan_id` once (see the module docs). Errors before anything
    /// was applied: `plan_expired`, `plan_stale`, `blocked`, `confirmation_required`,
    /// `config` (the session cannot run app-built literals safely), driver errors of the
    /// checks; once statements were sent, the outcome is in the report.
    pub async fn edit_apply(&self, plan_id: Uuid, confirmed: bool) -> Result<ApplyReport, DbError> {
        let stored = self.edit_plans.take(self.plan_ttl, plan_id)?;
        let rt = match self.runtime_opt(stored.conn) {
            Some(rt)
                if std::ptr::eq(Arc::as_ptr(&rt), stored.runtime.as_ptr())
                    && !rt.closed.load(Ordering::SeqCst) =>
            {
                rt
            }
            _ => return Err(stale("the connection was closed or reopened")),
        };
        if rt.tunnel_dead() || rt.tunnel_gen.load(Ordering::SeqCst) != stored.tunnel_gen {
            return Err(stale("the connection's SSH tunnel was replaced"));
        }
        let plan = &stored.plan;
        let cfg = self.repo.get(stored.conn)?;
        let n = count(plan.stmts.len());
        match edit_verdict(
            &plan.stmts,
            plan.atomic,
            plan.dialect,
            guard_ctx(self, &cfg),
        ) {
            (Verdict::Block, Some(reason)) => return Err(config_error("edit", &reason)),
            (Verdict::Block, None) => return Err(DbError::Blocked { count: n }),
            (Verdict::Confirm, _) if !confirmed => {
                return Err(DbError::ConfirmationRequired { count: n })
            }
            _ => {}
        }
        if cfg.env_tag != stored.env || cfg.read_only != stored.read_only {
            return Err(stale(
                "the connection's environment or read-only setting changed",
            ));
        }
        let detail = self.table_detail_fresh(stored.conn, &stored.object).await?;
        if fingerprint(&detail) != plan.fingerprint {
            return Err(stale("the table's structure changed"));
        }
        let taken = self
            .take_data_session(&rt, Some(&stored.object.database))
            .await?;
        // The tunnel may have been replaced while the table was read (R24): the session
        // taken now must come through the tunnel the plan was made for.
        if taken.tunnel_gen != stored.tunnel_gen {
            self.return_data_session(&rt, taken, true);
            return Err(stale("the connection's SSH tunnel was replaced"));
        }
        if taken.session.dialect() != plan.dialect {
            self.return_data_session(&rt, taken, true);
            return Err(stale("the session's SQL mode changed"));
        }
        let apply = Apply {
            s: taken.session.as_ref(),
            plan,
            conn: stored.conn,
            opts: ExecOptions {
                max_rows: 2,
                timeout: Some(self.edit_timeout(&rt)),
                cancel_grace: DATA_CANCEL_GRACE,
                ..ExecOptions::default()
            },
            cell_cap: self.data_cell_cap,
            page_cap: self.data_page_cap,
        };
        let (result, clean) = if plan.atomic == Atomicity::Yes {
            apply.atomic().await
        } else {
            apply.single().await
        };
        if let Ok(r) = &result {
            tracing::info!(connection = %stored.conn, outcome = ?r.outcome, "edit plan applied");
        }
        self.return_data_session(&rt, taken, clean);
        result
    }
}

/// What one statement of a plan did.
struct Done {
    /// Rows it matched (after a recheck).
    affected: Option<u64>,
    /// An INSERT's row as read back.
    inserted: Option<Vec<Value>>,
}

/// One run of a plan on its session.
struct Apply<'a> {
    s: &'a dyn Session,
    plan: &'a EditPlan,
    conn: Uuid,
    opts: ExecOptions,
    cell_cap: usize,
    page_cap: usize,
}

fn report(
    outcome: ApplyOutcome,
    statements: Vec<StmtApply>,
    inserted: Vec<Vec<Value>>,
    error: Option<DbError>,
) -> ApplyReport {
    ApplyReport {
        outcome,
        statements,
        inserted,
        error: error.map(AppError::from),
    }
}

impl Apply<'_> {
    /// Runs `sql` (one statement of kind `kind`): the rows it affected (as the driver
    /// reports them) and its first result's rows.
    async fn exec(
        &self,
        sql: &str,
        kind: StatementKind,
    ) -> Result<(Option<u64>, Vec<Vec<Value>>), DbError> {
        tracing::debug!(
            target: "dnt::sql",
            connection = %self.conn,
            sql = %redact(sql, self.plan.dialect).text,
            "applying an edit plan"
        );
        let mut page = BoundedPage::new(self.cell_cap, self.page_cap);
        let out = self.s.execute_one(sql, kind, &self.opts, &mut page).await?;
        Ok((out.results.first().and_then(|r| r.affected), page.rows))
    }

    async fn tx(&self, sql: &str, c: TxControl) -> Result<(), DbError> {
        let mut cell = FirstCell::default();
        self.s
            .execute_one(sql, StatementKind::Transaction(c), &self.opts, &mut cell)
            .await
            .map(|_| ())
    }

    /// Ends the transaction; whether the session is clean afterwards.
    async fn rollback(&self) -> bool {
        self.tx("ROLLBACK", TxControl::Rollback).await.is_ok() && !self.s.in_transaction()
    }

    /// The first cell of `sql`.
    async fn first_cell(&self, sql: &str) -> Result<Option<Value>, DbError> {
        let mut cell = FirstCell::default();
        self.s
            .execute_one(sql, StatementKind::Read, &self.opts, &mut cell)
            .await?;
        Ok(cell.0)
    }

    /// The session must read app-built literals the way they were built (R9 / R12) and
    /// still have the plan's dialect.
    async fn check_session(&self) -> Result<(), DbError> {
        match self.plan.dialect {
            Dialect::Postgres => {
                let v = self.first_cell("SHOW standard_conforming_strings").await?;
                if !matches!(&v, Some(Value::Text(s)) if s.eq_ignore_ascii_case("on")) {
                    return Err(config_error(
                        "edit",
                        "the server has standard_conforming_strings off: changes cannot be \
                         written safely on this connection",
                    ));
                }
            }
            Dialect::MySql(_) => {
                let v = self.first_cell("SELECT @@character_set_client").await?;
                if !matches!(&v, Some(Value::Text(s)) if s.eq_ignore_ascii_case("utf8mb4")) {
                    return Err(config_error(
                        "edit",
                        "the session's client character set is not utf8mb4: changes cannot \
                         be written safely on this connection",
                    ));
                }
            }
        }
        if self.s.dialect() != self.plan.dialect {
            return Err(stale("the session's SQL mode changed"));
        }
        Ok(())
    }

    /// Runs one planned statement: its matched rows (an UPDATE reporting 0 on MySQL is
    /// rechecked) and an INSERT's row. A failed read-back fails the statement when
    /// `strict` (inside a transaction, which is then rolled back); otherwise its row is
    /// left empty.
    async fn statement(&self, st: &PlannedEdit, strict: bool) -> Result<Done, DbError> {
        let (mut affected, rows) = self.exec(&st.sql, StatementKind::Write).await?;
        // PostgreSQL `INSERT … RETURNING` reports rows, not an affected count: every
        // row it wrote is returned (a single-row insert is never cut by `max_rows`).
        if affected.is_none() && st.returning && self.plan.dialect == Dialect::Postgres {
            affected = Some(rows.len() as u64);
        }
        if affected == Some(0) {
            if let Some(recheck) = &st.recheck {
                let (_, found) = self.exec(recheck, StatementKind::Read).await?;
                affected = Some(found.len() as u64);
            }
        }
        if !st.returning || affected != Some(st.expect_affected) {
            return Ok(Done {
                affected,
                inserted: None,
            });
        }
        let row = match (&st.read_back, self.plan.dialect) {
            (_, Dialect::Postgres) => Ok(rows),
            (Some(sql), _) => self.exec(sql, StatementKind::Read).await.map(|(_, r)| r),
            (None, _) => Ok(Vec::new()),
        };
        let row = match row {
            Ok(rows) => rows.into_iter().next().unwrap_or_default(),
            Err(e) if strict => return Err(e),
            Err(e) => {
                tracing::warn!(connection = %self.conn, error = %e, "could not read the inserted row back");
                Vec::new()
            }
        };
        Ok(Done {
            affected,
            inserted: Some(row),
        })
    }

    /// `atomic == Yes`: one transaction. Returns the report (or an error when nothing of
    /// the plan was sent) and whether the session is clean.
    async fn atomic(&self) -> (Result<ApplyReport, DbError>, bool) {
        let begin = match self.plan.dialect {
            Dialect::Postgres => "BEGIN",
            Dialect::MySql(_) => "START TRANSACTION",
        };
        if let Err(e) = self.tx(begin, TxControl::Begin).await {
            return (Err(e), false);
        }
        // Inside the transaction: the driver never silently replaces the connection now.
        // After any failure the session is closed, never pooled: a timeout or cancel
        // sent a KILL QUERY / cancel request that could still land on the next request.
        if let Err(e) = self.check_session().await {
            if !is_lost(&e) {
                self.rollback().await;
            }
            return (Err(e), false);
        }
        let mut statements = Vec::with_capacity(self.plan.stmts.len());
        let mut inserted = Vec::new();
        for st in &self.plan.stmts {
            match self.statement(st, true).await {
                Ok(done) => {
                    let conflict = done.affected != Some(st.expect_affected);
                    statements.push(StmtApply {
                        index: st.index,
                        affected: done.affected.map(|n| n as f64),
                        error: None,
                        conflict,
                    });
                    if conflict {
                        let clean = self.rollback().await;
                        return (
                            Ok(report(ApplyOutcome::RolledBack, statements, vec![], None)),
                            clean,
                        );
                    }
                    inserted.extend(done.inserted);
                }
                Err(e) => {
                    // Not committed: rolled back here, or by the server once the
                    // connection is gone. Either way the session is closed (see above).
                    if !is_lost(&e) {
                        self.rollback().await;
                    }
                    statements.push(StmtApply {
                        index: st.index,
                        affected: None,
                        error: Some(AppError::from(e)),
                        conflict: false,
                    });
                    return (
                        Ok(report(ApplyOutcome::RolledBack, statements, vec![], None)),
                        false,
                    );
                }
            }
        }
        match self.tx("COMMIT", TxControl::Commit).await {
            Ok(()) => (
                Ok(report(ApplyOutcome::Committed, statements, inserted, None)),
                !self.s.in_transaction(),
            ),
            // A PostgreSQL COMMIT the server refused (a deferred constraint) rolled back.
            Err(e @ DbError::Server { .. }) if self.plan.dialect == Dialect::Postgres => (
                Ok(report(
                    ApplyOutcome::RolledBack,
                    statements,
                    vec![],
                    Some(e),
                )),
                false,
            ),
            Err(e) => {
                let at = count(self.plan.stmts.len());
                (
                    Ok(report(
                        ApplyOutcome::Unknown { at },
                        statements,
                        inserted,
                        Some(e),
                    )),
                    false,
                )
            }
        }
    }

    /// `atomic != Yes`: the plan's one statement, without a transaction.
    async fn single(&self) -> (Result<ApplyReport, DbError>, bool) {
        let [st] = self.plan.stmts.as_slice() else {
            return (
                Err(DbError::Internal {
                    message: "a plan without a transaction has one statement".into(),
                }),
                true,
            );
        };
        // No transaction, so the driver may replace a dead connection silently before a
        // statement (R22). Right before it: ping, then check the session again, and note
        // its reconnect count. A reconnect during the checks stops the plan (nothing
        // sent); one seen only after the write means it ran on a connection the checks
        // did not cover.
        if let Err(e) = self.s.ping().await {
            return (Err(e), false);
        }
        let reconnects = self.s.reconnects();
        if let Err(e) = self.check_session().await {
            return (Err(e), false);
        }
        if self.s.reconnects() != reconnects {
            return (
                Err(stale("the connection was replaced while it was checked")),
                false,
            );
        }
        match self.statement(st, false).await {
            Ok(done) => {
                let replaced = self.s.reconnects() != reconnects;
                let (outcome, conflict) = match done.affected {
                    _ if replaced => (ApplyOutcome::Unknown { at: 0 }, false),
                    Some(n) if n == st.expect_affected => (ApplyOutcome::Committed, false),
                    Some(0) => (ApplyOutcome::PartiallyApplied { applied: 0 }, true),
                    // No count, or another one: whether it applied cannot be told.
                    _ => (ApplyOutcome::Unknown { at: 0 }, false),
                };
                let clean = !matches!(outcome, ApplyOutcome::Unknown { .. });
                let stmt = StmtApply {
                    index: st.index,
                    affected: done.affected.map(|n| n as f64),
                    error: None,
                    conflict,
                };
                let inserted = match outcome {
                    ApplyOutcome::Committed => done.inserted.into_iter().collect(),
                    _ => vec![],
                };
                (Ok(report(outcome, vec![stmt], inserted, None)), clean)
            }
            Err(e) => {
                let outcome = if outcome_unknown(&e) {
                    ApplyOutcome::Unknown { at: 0 }
                } else {
                    ApplyOutcome::PartiallyApplied { applied: 0 }
                };
                let stmt = StmtApply {
                    index: st.index,
                    affected: None,
                    error: Some(AppError::from(e)),
                    conflict: false,
                };
                (Ok(report(outcome, vec![stmt], vec![], None)), false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_errors_become_config_errors_with_a_field() {
        let field = |e: EditError| match edit_error(e) {
            DbError::Config { field, message } => (field.unwrap(), message),
            other => panic!("{other:?}"),
        };
        assert_eq!(field(EditError::OneAtATime).0, "one_at_a_time");
        assert_eq!(
            field(EditError::Invalid {
                column: "qty".into(),
                reason: "the column cannot be NULL".into()
            }),
            ("column:qty".into(), "the column cannot be NULL".into())
        );
        assert_eq!(
            field(EditError::KeyNotComplete {
                column: "id".into()
            })
            .0,
            "column:id"
        );
        assert_eq!(
            field(EditError::Invalid {
                column: String::new(),
                reason: "there are no changes to apply".into()
            }),
            ("edit".into(), "there are no changes to apply".into())
        );
        assert_eq!(
            field(EditError::NotEditable(
                dnt_query::edit::NotEditable::ReadOnlyConnection
            ))
            .0,
            "edit"
        );
    }

    #[test]
    fn outcome_wire_format() {
        let j = |o: ApplyOutcome| serde_json::to_value(o).unwrap();
        assert_eq!(
            j(ApplyOutcome::Committed),
            serde_json::json!({"outcome": "committed"})
        );
        assert_eq!(
            j(ApplyOutcome::RolledBack),
            serde_json::json!({"outcome": "rolled_back"})
        );
        assert_eq!(
            j(ApplyOutcome::PartiallyApplied { applied: 2 }),
            serde_json::json!({"outcome": "partially_applied", "applied": 2})
        );
        assert_eq!(
            j(ApplyOutcome::Unknown { at: 3 }),
            serde_json::json!({"outcome": "unknown", "at": 3})
        );
    }

    #[test]
    fn store_keeps_one_plan_per_table_and_takes_once() {
        let store = EditPlanStore::default();
        let ttl = Duration::from_secs(60);
        let conn = Uuid::new_v4();
        let obj = |name: &str| ObjectRef {
            database: "shop".into(),
            schema: None,
            name: name.into(),
            kind: dnt_core::meta::ObjectKind::Table,
        };
        let stored = |o: ObjectRef| StoredEdit {
            conn,
            runtime: Weak::new(),
            tunnel_gen: 0,
            object: o,
            env: None,
            read_only: false,
            plan: EditPlan {
                stmts: vec![],
                atomic: Atomicity::Yes,
                verdict: Verdict::Allow,
                reason: None,
                fingerprint: 0,
                dialect: Dialect::MYSQL,
            },
            at: Instant::now(),
        };
        let a = store.insert(ttl, stored(obj("a")));
        let b = store.insert(ttl, stored(obj("b")));
        let a2 = store.insert(ttl, stored(obj("a")));
        assert!(matches!(store.take(ttl, a), Err(DbError::PlanExpired)));
        assert!(store.take(ttl, a2).is_ok());
        assert!(matches!(store.take(ttl, a2), Err(DbError::PlanExpired)));
        store.drop_conn(conn);
        assert!(matches!(store.take(ttl, b), Err(DbError::PlanExpired)));
    }
}
