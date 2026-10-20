//! Query tabs (spec §4.7, §4.8): one query session per tab, separate from the metadata
//! sessions, and the run plans the UI confirms before a run.
//!
//! - A tab's session lives in the `ConnectionRuntime` of the connection the tab is on
//!   (sharing its tunnel) and opens lazily on the tab's first `query_plan`, which needs
//!   the session's `dialect()`. Moving the tab to another connection closes it first.
//! - Every tab has a [`TabSlot`]: an async mutex around its session. Whoever uses the
//!   session holds the slot for that long; a run (Task 15) holds it for the whole run
//!   with an owned guard. A second user of a held slot gets `Busy` at once
//!   (`try_lock`), so a tab's session is never used concurrently. Only closing waits
//!   for the slot (bounded by [`SLOT_CLOSE_WAIT`]).
//! - Plans live in the [`SessionManager`]'s [`PlanStore`] for [`PLAN_TTL`]. Every stored
//!   plan went through `dnt_query::guard`, with the connection's config as it is in the
//!   repository when the plan is made; `query_run` judges it again with the config as
//!   it is then.
//! - A run (`query_run`) is a task that holds the tab's slot until it ends and streams
//!   [`RunEvent`]s; `query_cancel`, closing the tab and closing the connection stop it
//!   (the run still ends with `done`).

use crate::run_events::{
    tick_rows, ChannelSink, EventSink, HistoryCtx, RunEvent, RunFlags, RunOptions,
};
use crate::session_manager::{
    close_quietly, closed_error, config_error, connect_db, lock, with_database, ConnState,
    ConnectionRuntime, SessionManager,
};
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::{AppError, DbError};
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, Dialect, StatementKind};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::{
    explain_sql, guard, plan, run, ExplainFormat, RunCancel, RunReport, ScriptPlan, StmtKindLabel,
    StopReason, Target, Verdict,
};
use dnt_store::repo::ConnectionRepo;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

/// How long a plan stays available to `query_run` after `query_plan` (and, once taken,
/// to [`SessionManager::plan_statement`] after the take).
pub const PLAN_TTL: Duration = Duration::from_secs(120);

/// Characters of a statement shown in the confirmation and block dialogs.
pub const PREVIEW_CHARS: usize = 120;

/// How long closing a tab (or its connection) waits for whoever holds the tab's session.
pub(crate) const SLOT_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Rows per `rows` event and per driver batch.
pub const RUN_BATCH_SIZE: usize = 500;

/// Upper bound of `RunOptions::max_rows`.
pub const MAX_ROWS_LIMIT: u32 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct PlanStmtView {
    pub index: u32,
    /// UTF-16 range in the editor text (end-exclusive).
    pub range: (u32, u32),
    pub kind: StmtKindLabel,
    pub verdict: Verdict,
    /// The first [`PREVIEW_CHARS`] characters, line breaks turned into spaces.
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct PlanView {
    pub plan_id: Uuid,
    pub stmts: Vec<PlanStmtView>,
    pub dialect: Dialect,
    pub connection_name: String,
    pub env_tag: Option<EnvTag>,
    pub read_only: bool,
    /// The tab session's transaction state when the plan was made.
    pub tx_state: TxState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum TxAction {
    Commit,
    Rollback,
}

impl TxAction {
    fn sql(self) -> &'static str {
        match self {
            TxAction::Commit => "COMMIT",
            TxAction::Rollback => "ROLLBACK",
        }
    }
}

/// `text` without line breaks (`\r\n`, `\n`, `\r` each become one space), cut to
/// [`PREVIEW_CHARS`] characters.
pub fn preview(text: &str) -> String {
    text.replace("\r\n", " ")
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(PREVIEW_CHARS)
        .collect()
}

/// Tab ids are the UI's tab keys: non-empty, at most 64 characters.
fn validate_tab(tab: &str) -> Result<(), DbError> {
    if tab.is_empty() || tab.chars().count() > 64 {
        return Err(config_error(
            "tab_id",
            "a tab id must have 1 to 64 characters",
        ));
    }
    Ok(())
}

/// A tab's query session.
///
/// After a run (or `query_tx`) whose statement failed with `ConnectionLost` or
/// `Connect`, `session` is dropped, so the next plan opens a fresh session instead of
/// reusing a dead one. When every such error of the run is retryable (no transaction
/// was lost, the outcome is known), the tab stays bound to its database (`reopen`), so
/// its plans stay valid: Retry / Fetch all (`query_plan_statement`) and `query_run`
/// reopen the session. A `USE db` the user ran (MySQL,
/// `dnt_core::sql::session_effect`) updates `database` once it succeeded, so the tab's
/// next plan switches back to the database the UI shows.
#[derive(Default)]
pub struct TabSession {
    /// The database the session is on: the one the tab asked for, else the connect
    /// database. Meaningful only while `session` is set, or `reopen`.
    pub(crate) database: Option<String>,
    pub(crate) session: Option<Box<dyn Session>>,
    /// `session` was dropped after a retryable lost connection: the tab keeps its
    /// binding to `database` and the session is reopened on its next use.
    pub(crate) reopen: bool,
    /// `session` was dropped with a transaction open (the server rolled it back): the
    /// next `query_tx` reports `ConnectionLost` instead of `Idle`. Cleared once
    /// reported, or when a session opens again.
    pub(crate) lost_tx: bool,
    /// `ConnectionRuntime::tunnel_gen` when `session` was opened.
    pub(crate) tunnel_gen: u64,
    /// Set once the slot was closed (tab or connection closed): a caller that was
    /// waiting for it must get a fresh slot instead.
    pub(crate) closed: bool,
}

impl TabSession {
    fn tx_open(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.tx_state() != TxState::Idle)
    }

    async fn close(&mut self) {
        self.closed = true;
        if let Some(session) = self.session.take() {
            close_quietly(session).await;
        }
    }
}

/// One tab's session, held by one user at a time (see the module docs).
pub type TabSlot = Arc<tokio::sync::Mutex<TabSession>>;

#[derive(Default)]
struct Tabs {
    slots: HashMap<String, TabSlot>,
    /// The database of every tab that has a session: a copy of `TabSession::database`
    /// readable without the slot, so a stored plan can be checked against the tab's
    /// current database while a run holds the slot.
    bound: HashMap<String, Option<String>>,
}

/// The query tabs of one connection.
#[derive(Default)]
pub(crate) struct QuerySessions {
    tabs: Mutex<Tabs>,
}

impl QuerySessions {
    fn get(&self, tab: &str) -> Option<TabSlot> {
        lock(&self.tabs).slots.get(tab).cloned()
    }

    /// The slot of `tab`, created on first use. Refused once `closed` (the runtime's
    /// flag, which `shutdown` sets before it drains the slots) is set.
    fn get_or_create(&self, tab: &str, closed: &AtomicBool) -> Result<TabSlot, DbError> {
        let mut tabs = lock(&self.tabs);
        if closed.load(Ordering::SeqCst) {
            return Err(closed_error());
        }
        Ok(Arc::clone(tabs.slots.entry(tab.to_string()).or_default()))
    }

    /// Records the database of `ts` (or that it has no session) for `tab`. Call after
    /// every change of `ts.session` / `ts.database`, still holding the slot.
    pub(crate) fn sync_bound(&self, tab: &str, ts: &TabSession) {
        let mut tabs = lock(&self.tabs);
        if (ts.session.is_some() || ts.reopen) && !ts.closed {
            tabs.bound.insert(tab.to_string(), ts.database.clone());
        } else {
            tabs.bound.remove(tab);
        }
    }

    /// The database `tab`'s session is on; `None` when the tab has no session here.
    fn bound(&self, tab: &str) -> Option<Option<String>> {
        lock(&self.tabs).bound.get(tab).cloned()
    }

    /// Whether `slot` is still the slot of `tab` (not closed and replaced meanwhile).
    fn is_slot(&self, tab: &str, slot: &TabSlot) -> bool {
        lock(&self.tabs)
            .slots
            .get(tab)
            .is_some_and(|s| Arc::ptr_eq(s, slot))
    }

    /// Forgets `tab` if `slot` is still its slot.
    fn remove_if(&self, tab: &str, slot: &TabSlot) {
        let mut tabs = lock(&self.tabs);
        if tabs.slots.get(tab).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            tabs.slots.remove(tab);
            tabs.bound.remove(tab);
        }
    }

    pub(crate) fn drain(&self) -> Vec<(String, TabSlot)> {
        let mut tabs = lock(&self.tabs);
        tabs.bound.clear();
        tabs.slots.drain().collect()
    }
}

/// Closes a slot taken out of its map, waiting at most `wait` for its holder.
pub(crate) async fn close_slot(tab: String, slot: TabSlot, wait: Duration) {
    match tokio::time::timeout(wait, slot.lock_owned()).await {
        Ok(mut ts) => ts.close().await,
        // The holder (a run) still has the session; it is dropped with the slot.
        Err(_) => tracing::warn!(tab = %tab, "the tab's session is busy; closing without it"),
    }
}

/// A [`RowSink`] that drops everything (`USE`, `COMMIT`, `ROLLBACK`).
struct NullSink;

impl RowSink for NullSink {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Runs one statement on a tab session, its results dropped.
async fn execute_quietly(session: &dyn Session, sql: &str) -> Result<(), DbError> {
    let kind = classify(sql, session.dialect());
    session
        .execute_one(sql, kind, &ExecOptions::default(), &mut NullSink)
        .await
        .map(|_| ())
}

/// A MySQL identifier in backticks (embedded backticks doubled).
fn quote_mysql_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// What a stored plan was made for: it only runs while its tab is still there.
#[derive(Clone)]
pub(crate) struct PlanTarget {
    tab: String,
    conn: Uuid,
    /// The runtime the plan was made on (a reopened connection is another one).
    runtime: Weak<ConnectionRuntime>,
    /// The database the tab's session was on.
    database: Option<String>,
}

/// An Explain plan: its statement's text and kind are the explain statement's;
/// `original` is the plan of the explained statement, which is what the guard judges.
#[derive(Clone)]
pub(crate) struct ExplainPlan {
    format: ExplainFormat,
    original: ScriptPlan,
}

struct StoredPlan {
    target: PlanTarget,
    plan: ScriptPlan,
    explain: Option<ExplainPlan>,
    /// When the plan was made.
    at: Instant,
    /// `query_run` took it: it cannot be run again, only single statements of it.
    taken: bool,
    /// A derived plan (`query_plan_statement`: Retry / Fetch all): the user plan it
    /// was cut from. Taking it never evicts that plan.
    source: Option<Uuid>,
}

/// A copy of a stored plan.
#[derive(Clone)]
pub(crate) struct PlanCopy {
    target: PlanTarget,
    plan: ScriptPlan,
    explain: Option<ExplainPlan>,
    /// See [`StoredPlan::source`]; for a statement copy, the plan it was cut from (its
    /// own source when that plan is itself derived).
    source: Option<Uuid>,
}

/// Statement `index` of `plan` as a one-statement plan: its text is the whole source
/// (`source_range` `(0, len)`, `base` its editor start); dialect, explain flag, guard
/// context, index and range are kept.
fn one_statement(plan: &ScriptPlan, index: u32) -> Option<ScriptPlan> {
    let mut one = plan.stmts.iter().find(|s| s.index == index)?.clone();
    one.source_range = (0, one.text.len());
    Some(ScriptPlan {
        dialect: plan.dialect,
        source: one.text.clone(),
        base: one.range.0,
        stmts: vec![one],
        explain: plan.explain,
        guard: plan.guard,
    })
}

/// Plans by id. A pending (never taken) plan lives for the plan TTL and only the newest
/// one of a tab is kept. Of the taken user plans (`query_plan`, `query_explain`) only
/// the latest of each tab is kept (its results are what the tab shows: Retry / Fetch
/// all use it), with no TTL: it goes with the next take of a user plan of the tab, or
/// when the tab or its connection closes. Taking a derived plan (Retry / Fetch all)
/// keeps the user plan and only replaces the tab's previous taken derived plan.
#[derive(Default)]
pub(crate) struct PlanStore {
    plans: Mutex<HashMap<Uuid, StoredPlan>>,
}

impl PlanStore {
    fn prune(plans: &mut HashMap<Uuid, StoredPlan>, ttl: Duration) {
        plans.retain(|_, p| p.taken || p.at.elapsed() < ttl);
    }

    /// Stores a guarded plan; drops the tab's pending plans (a dialog the user
    /// abandoned): only the newest pending plan of a tab can run.
    fn insert(
        &self,
        ttl: Duration,
        target: PlanTarget,
        plan: ScriptPlan,
        explain: Option<ExplainPlan>,
        source: Option<Uuid>,
    ) -> Uuid {
        debug_assert!(
            explain
                .as_ref()
                .map_or(&plan, |e| &e.original)
                .guard
                .is_some(),
            "stored plans must be guarded"
        );
        let id = Uuid::new_v4();
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        plans.retain(|_, p| p.taken || p.target.tab != target.tab);
        plans.insert(
            id,
            StoredPlan {
                target,
                plan,
                explain,
                at: Instant::now(),
                taken: false,
                source,
            },
        );
        id
    }

    /// The tab and connection of plan `id`, taken or not.
    fn locate(
        &self,
        ttl: Duration,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<(String, Uuid), DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        Ok((p.target.tab.clone(), p.target.conn))
    }

    /// A copy of the pending (not taken) plan `id`.
    fn pending(
        &self,
        ttl: Duration,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<PlanCopy, DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        if p.taken {
            return Err(DbError::PlanExpired);
        }
        Ok(PlanCopy {
            target: p.target.clone(),
            plan: p.plan.clone(),
            explain: p.explain.clone(),
            source: p.source,
        })
    }

    /// The live plan `id` when `current` still accepts its target; a plan whose target
    /// is gone is dropped.
    fn checked<'a>(
        plans: &'a mut HashMap<Uuid, StoredPlan>,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<&'a mut StoredPlan, DbError> {
        let ok = plans.get(&id).map(|p| current(&p.target));
        match ok {
            Some(true) => plans.get_mut(&id).ok_or(DbError::PlanExpired),
            Some(false) => {
                plans.remove(&id);
                Err(DbError::PlanExpired)
            }
            None => Err(DbError::PlanExpired),
        }
    }

    fn take(
        &self,
        ttl: Duration,
        id: Uuid,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        if p.taken {
            return Err(DbError::PlanExpired);
        }
        p.taken = true;
        let derived = p.source.is_some();
        let taken = (p.target.tab.clone(), p.target.conn, p.plan.clone());
        // Of the tab's taken plans, a user plan replaces all others; a derived plan
        // replaces only the previous derived one (its source stays).
        plans.retain(|pid, p| {
            *pid == id || !p.taken || p.target.tab != taken.0 || (derived && p.source.is_none())
        });
        Ok(taken)
    }

    /// Statement `index` of plan `id` (taken or not) as a one-statement plan.
    fn statement(
        &self,
        ttl: Duration,
        id: Uuid,
        index: u32,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<PlanCopy, DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        let plan = one_statement(&p.plan, index).ok_or(DbError::PlanExpired)?;
        let explain = match &p.explain {
            Some(e) => Some(ExplainPlan {
                format: e.format,
                original: one_statement(&e.original, index).ok_or(DbError::PlanExpired)?,
            }),
            None => None,
        };
        Ok(PlanCopy {
            target: p.target.clone(),
            plan,
            explain,
            source: Some(p.source.unwrap_or(id)),
        })
    }

    pub(crate) fn drop_tab(&self, tab: &str) {
        lock(&self.plans).retain(|_, p| p.target.tab != tab);
    }

    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.target.conn != conn);
    }

    fn drop_tab_on(&self, tab: &str, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.target.tab != tab || p.target.conn != conn);
    }
}

impl ConnectionRuntime {
    /// The database a tab's session should be on: the requested one, else the connect
    /// database (none on MySQL without a default database).
    fn query_database(&self, database: Option<String>) -> Option<String> {
        database
            .filter(|d| !d.is_empty())
            .or_else(|| self.home_database())
    }

    /// Opens a query session on `database` through the connection's tunnel, reopening
    /// the tunnel first when it died (as metadata calls do). Returns the session and the
    /// tunnel generation it was opened through.
    pub(crate) async fn open_query_session(
        &self,
        database: Option<String>,
    ) -> Result<(Box<dyn Session>, u64), DbError> {
        let (resolved, generation) = {
            let mut link = self.link.lock().await;
            self.ensure_open(&link)?;
            if self.tunnel_dead() {
                self.set_state(ConnState::Reconnecting);
                let reopened = self.replace_dead_tunnel(&mut link).await;
                self.set_state(if reopened.is_ok() {
                    ConnState::Connected
                } else {
                    ConnState::Failed
                });
                reopened?;
            }
            (
                with_database(&link.resolved, database),
                self.tunnel_gen.load(Ordering::SeqCst),
            )
        };
        let session = connect_db(self.driver.as_ref(), &resolved).await?;
        Ok((session, generation))
    }

    /// A session opened through a tunnel that died or was replaced since cannot reach
    /// the server any more (its driver would reconnect to the old local port).
    fn is_stale(&self, ts: &TabSession) -> bool {
        ts.session.is_some()
            && (self.tunnel_dead() || ts.tunnel_gen != self.tunnel_gen.load(Ordering::SeqCst))
    }

    /// Puts `ts` on the database `database` resolves to: opens the session if there is
    /// none; otherwise switches (refused with an open transaction): MySQL runs `USE`,
    /// PostgreSQL opens a session on the new database and closes the old one.
    ///
    /// A stale session (see [`Self::is_stale`]) is closed first. Idle, it is simply
    /// replaced; with a transaction open, which the server has lost with the tunnel, the
    /// call fails once with `ConnectionLost { was_in_tx: true }` and the next one opens
    /// a fresh session.
    async fn ensure_tab_session(
        &self,
        ts: &mut TabSession,
        database: Option<String>,
    ) -> Result<(), DbError> {
        if self.is_stale(ts) {
            let in_tx = ts.tx_open();
            if let Some(old) = ts.session.take() {
                close_quietly(old).await;
            }
            if in_tx {
                return Err(DbError::ConnectionLost {
                    was_in_tx: true,
                    outcome_unknown: false,
                    message: "the SSH tunnel was re-established; the open transaction was lost"
                        .into(),
                });
            }
        }
        let target = self.query_database(database);
        let Some(session) = ts.session.as_deref() else {
            let (session, generation) = self.open_query_session(target.clone()).await?;
            ts.session = Some(session);
            ts.tunnel_gen = generation;
            ts.database = target;
            ts.reopen = false;
            ts.lost_tx = false;
            return Ok(());
        };
        if ts.database == target {
            return Ok(());
        }
        if session.tx_state() != TxState::Idle {
            return Err(DbError::TransactionOpen);
        }
        match (self.cfg.kind, &target) {
            (DbKind::MySql, Some(db)) => {
                execute_quietly(session, &format!("USE {}", quote_mysql_ident(db))).await?;
            }
            // PostgreSQL, or MySQL back to "no database" (which `USE` cannot do).
            _ => {
                let (fresh, generation) = self.open_query_session(target.clone()).await?;
                if let Some(old) = ts.session.replace(fresh) {
                    close_quietly(old).await;
                }
                ts.tunnel_gen = generation;
            }
        }
        ts.database = target;
        Ok(())
    }
}

impl SessionManager {
    /// Overrides [`PLAN_TTL`] (tests).
    pub fn with_plan_ttl(mut self, ttl: Duration) -> Self {
        self.plan_ttl = ttl;
        self
    }

    /// Marks connection `conn`'s SSH tunnel as replaced, as a dead tunnel being reopened
    /// does (tests: the fake driver has no tunnel).
    #[doc(hidden)]
    pub fn simulate_tunnel_replaced(&self, conn: Uuid) {
        if let Some(rt) = self.runtime_opt(conn) {
            rt.tunnel_gen.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The connection's config as stored now (env tag and read-only may have been edited
    /// since it was opened); the one it was opened with when it cannot be read.
    fn current_config(&self, rt: &ConnectionRuntime) -> ConnectionConfig {
        match self.repo.get(rt.cfg.id) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(connection = %rt.cfg.id, error = %e, "could not re-read the connection");
                rt.cfg.clone()
            }
        }
    }

    /// Whether a stored plan's tab is still where the plan was made: the same runtime of
    /// the connection (not closed, not reopened since) and a session on the same database.
    fn plan_target_current(&self, t: &PlanTarget) -> bool {
        self.runtime_opt(t.conn).is_some_and(|rt| {
            std::ptr::eq(Arc::as_ptr(&rt), t.runtime.as_ptr())
                && !rt.closed.load(Ordering::SeqCst)
                && rt.queries.bound(&t.tab).as_ref() == Some(&t.database)
        })
    }

    /// Plans `target` of `text` on tab `tab`, bound to connection `conn` and `database`
    /// (`None`: the connect database), and judges every statement with the guard. The
    /// tab's session opens on first use; see the module docs for switching connection or
    /// database. Errors: `Config` (bad tab id), `NotOpen`, `Busy` (the tab is running),
    /// `TransactionOpen` (switching away from an open transaction), `ConnectionLost`
    /// (once, after the SSH tunnel was replaced under an open transaction), connect
    /// errors.
    pub async fn query_plan(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
        text: &str,
        target: Target,
    ) -> Result<PlanView, DbError> {
        let (rt, ts) = self.tab_session(tab, conn, database).await?;
        let (dialect, tx_state, _) = session_facts(&ts)?;
        let plan_target = PlanTarget {
            tab: tab.to_string(),
            conn,
            runtime: Arc::downgrade(&rt),
            database: ts.database.clone(),
        };
        drop(ts);

        let cfg = self.current_config(&rt);
        let mut planned = plan(text, target, dialect);
        guard(&mut planned, cfg.env_tag, cfg.read_only, false);
        let copy = PlanCopy {
            target: plan_target,
            plan: planned,
            explain: None,
            source: None,
        };
        self.store_plan(&rt, &cfg, copy, dialect, tx_state)
    }

    /// Opens (or moves) the session of `tab` on `conn` / `database` and returns it held:
    /// see [`Self::query_plan`] for the rules and errors.
    async fn tab_session(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
    ) -> Result<(Arc<ConnectionRuntime>, OwnedMutexGuard<TabSession>), DbError> {
        validate_tab(tab)?;
        let rt = self.runtime(conn)?;
        self.release_tab_elsewhere(tab, conn).await?;
        let mut ts = loop {
            let slot = rt.queries.get_or_create(tab, &rt.closed)?;
            let ts = slot.try_lock_owned().map_err(|_| DbError::Busy)?;
            if !ts.closed {
                break ts;
            }
            // Closed while we got it (the tab was closed meanwhile): use a fresh slot.
        };
        let ensured = rt.ensure_tab_session(&mut ts, database).await;
        rt.queries.sync_bound(tab, &ts);
        ensured?;
        Ok((rt, ts))
    }

    /// Stores a guarded plan (for an Explain, `copy.plan` is the explain statement and
    /// `copy.explain.original` the guarded plan of the explained one) and returns its
    /// view.
    fn store_plan(
        &self,
        rt: &ConnectionRuntime,
        cfg: &ConnectionConfig,
        copy: PlanCopy,
        dialect: Dialect,
        tx_state: TxState,
    ) -> Result<PlanView, DbError> {
        let PlanCopy {
            target,
            plan: planned,
            explain,
            source,
        } = copy;
        let judged = explain.as_ref().map_or(&planned, |e| &e.original);
        let stmts = judged
            .stmts
            .iter()
            .map(|s| PlanStmtView {
                index: s.index,
                range: s.range,
                kind: s.kind.into(),
                verdict: s.verdict,
                preview: preview(&s.text),
            })
            .collect();
        // The connection closed while we planned: `shutdown` closes the tab's session;
        // keep no plan for it.
        if rt.closed.load(Ordering::SeqCst) {
            return Err(closed_error());
        }
        let plan_id = self
            .plans
            .insert(self.plan_ttl, target, planned, explain, source);
        Ok(PlanView {
            plan_id,
            stmts,
            dialect,
            connection_name: cfg.name.clone(),
            env_tag: cfg.env_tag,
            read_only: cfg.read_only,
            tx_state,
        })
    }

    /// Closes the session of `tab` on every connection other than `conn`: a tab is on one
    /// connection at a time. Refused while that session runs (`Busy`) or has a
    /// transaction open (`TransactionOpen`).
    async fn release_tab_elsewhere(&self, tab: &str, conn: Uuid) -> Result<(), DbError> {
        for (id, rt) in self.runtimes_snapshot() {
            if id == conn {
                continue;
            }
            let Some(slot) = rt.queries.get(tab) else {
                continue;
            };
            let mut ts = Arc::clone(&slot)
                .try_lock_owned()
                .map_err(|_| DbError::Busy)?;
            if ts.tx_open() {
                return Err(DbError::TransactionOpen);
            }
            rt.queries.remove_if(tab, &slot);
            ts.close().await;
            self.plans.drop_tab_on(tab, id);
        }
        Ok(())
    }

    /// The runtime and slot of `tab`, on whichever connection it is.
    fn find_tab_runtime(&self, tab: &str) -> Option<(Arc<ConnectionRuntime>, TabSlot)> {
        self.runtimes_snapshot()
            .into_iter()
            .find_map(|(_, rt)| rt.queries.get(tab).map(|slot| (rt, slot)))
    }

    /// Commits or rolls back the tab's transaction (`execute_one("COMMIT"/"ROLLBACK")`)
    /// and returns the session's new state. A tab without a session is `Idle`, except
    /// once after its session was dropped with a transaction open (a failed run, see
    /// [`TabSession::lost_tx`]): that is `ConnectionLost { was_in_tx }`.
    pub async fn query_tx(&self, tab: &str, action: TxAction) -> Result<TxState, DbError> {
        self.query_tx_inner(tab, action).await.map_err(|(e, _)| e)
    }

    /// [`Self::query_tx`] for the UI: a failed `COMMIT` / `ROLLBACK` reports the
    /// session's transaction state after the failure in `AppError::tx_state` (`idle`
    /// after a lost connection, whose transaction the server rolled back).
    // `AppError` is the IPC payload the command returns as is (see `commands`).
    #[allow(clippy::result_large_err)]
    pub async fn query_tx_reported(
        &self,
        tab: &str,
        action: TxAction,
    ) -> Result<TxState, AppError> {
        self.query_tx_inner(tab, action)
            .await
            .map_err(|(e, state)| {
                let mut app = AppError::from(e);
                app.tx_state = state.map(|s| s.as_str().to_string());
                app
            })
    }

    /// The error comes with the session's state after a failed statement.
    async fn query_tx_inner(
        &self,
        tab: &str,
        action: TxAction,
    ) -> Result<TxState, (DbError, Option<TxState>)> {
        validate_tab(tab).map_err(|e| (e, None))?;
        let Some((rt, slot)) = self.find_tab_runtime(tab) else {
            return Ok(TxState::Idle);
        };
        let mut ts = Arc::clone(&slot)
            .try_lock_owned()
            .map_err(|_| (DbError::Busy, None))?;
        let Some(session) = ts.session.as_deref() else {
            if std::mem::take(&mut ts.lost_tx) {
                // The session died with a transaction open: the server rolled it
                // back, so a Commit must not look like it worked.
                let lost = DbError::ConnectionLost {
                    was_in_tx: true,
                    outcome_unknown: false,
                    message: "the tab's session was closed during the transaction; \
                              the server rolled it back"
                        .into(),
                };
                return Err((lost, Some(TxState::Idle)));
            }
            return Ok(TxState::Idle);
        };
        match execute_quietly(session, action.sql()).await {
            Ok(()) => Ok(session.tx_state()),
            Err(e) => {
                let state = session.tx_state();
                if is_lost(&e) {
                    // The next plan opens a fresh session.
                    if let Some(dead) = ts.session.take() {
                        close_quietly(dead).await;
                    }
                    if rt.queries.is_slot(tab, &slot) {
                        rt.queries.sync_bound(tab, &ts);
                    }
                    Err((e, Some(TxState::Idle)))
                } else {
                    Err((e, Some(state)))
                }
            }
        }
    }

    /// Closes the tab's session and forgets its plans. Without `force`, a session with a
    /// transaction open is kept and `TransactionOpen` returned, so the UI can ask
    /// Commit / Rollback / Cancel (spec §7.2); with `force` the session is closed anyway
    /// (the server rolls the transaction back). Waits up to [`SLOT_CLOSE_WAIT`] for a
    /// holder of the session: without `force` that ends in `Busy`. Closing an unknown
    /// tab is a no-op.
    ///
    /// A run of the tab is stopped FIRST, before the open-transaction check: a call that
    /// then returns `TransactionOpen` (or `Busy`, when the stopped run does not release
    /// the session within [`SLOT_CLOSE_WAIT`]) has still stopped the run. The UI must
    /// therefore ask the user before closing a tab whose run is active.
    pub async fn query_close_tab(&self, tab: &str, force: bool) -> Result<(), DbError> {
        validate_tab(tab)?;
        // A run of the tab is stopped first (it still ends with `done`), so closing does
        // not wait for it to finish on its own.
        self.stop_runs(|r| r.tab == tab).await;
        for (_, rt) in self.runtimes_snapshot() {
            let Some(slot) = rt.queries.get(tab) else {
                continue;
            };
            match tokio::time::timeout(SLOT_CLOSE_WAIT, Arc::clone(&slot).lock_owned()).await {
                Ok(mut ts) => {
                    if !force && ts.tx_open() {
                        return Err(DbError::TransactionOpen);
                    }
                    rt.queries.remove_if(tab, &slot);
                    ts.close().await;
                }
                Err(_) if !force => return Err(DbError::Busy),
                Err(_) => {
                    rt.queries.remove_if(tab, &slot);
                    tracing::warn!(tab = %tab, "the tab's session is busy; closing without it");
                }
            }
        }
        self.plans.drop_tab(tab);
        Ok(())
    }

    /// Takes plan `plan_id` for `query_run`: `(tab, connection, plan)`. A plan runs once;
    /// unknown, expired, replaced or already taken plans are `PlanExpired`, and so is a
    /// plan whose tab has since moved to another connection or database, or whose
    /// connection was closed or reopened. Taking it drops the tab's previous taken plan.
    ///
    /// The plan was guarded with the config as it was when it was made (`plan.guard` is
    /// set); `query_run` judges it again with the current config before it runs. Not
    /// public: nothing outside may take a plan around `query_run`'s re-judgement (tests
    /// use [`SessionManager::test_take_plan`]).
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn take_plan(&self, plan_id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plans
            .take(self.plan_ttl, plan_id, &|t| self.plan_target_current(t))
    }

    /// Statement `index` of plan `plan_id` (taken or not) as a one-statement plan, for
    /// Retry and Fetch all: the stored statement text runs verbatim, never re-planned
    /// (so its `DELIMITER` context is kept). Keeps the plan's dialect, guard context and
    /// the statement's index and range; the plan's `source` is just the statement (its
    /// `source_range` is `(0, len)`, `base` its editor start). `PlanExpired` when the
    /// plan or index is unknown or the plan's target changed (see [`Self::take_plan`]).
    ///
    /// To run it, [`Self::query_plan_statement`] stores it as a new plan (tests use
    /// [`SessionManager::test_plan_statement`]).
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let copy = self.plans.statement(self.plan_ttl, plan_id, index, &|t| {
            self.plan_target_current(t)
        })?;
        Ok((copy.target.tab, copy.target.conn, copy.plan))
    }

    /// [`Self::take_plan`] for integration tests (feature `test-helpers`).
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn test_take_plan(&self, plan_id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.take_plan(plan_id)
    }

    /// [`Self::plan_statement`] for integration tests (feature `test-helpers`).
    #[cfg(any(test, feature = "test-helpers"))]
    #[doc(hidden)]
    pub fn test_plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plan_statement(plan_id, index)
    }

    /// Retry / Fetch all: stores statement `index` of plan `plan_id` (taken or not) as a
    /// new pending plan of the same tab, judged by the guard with the connection's
    /// current config, and returns its view (confirm it like any plan, then
    /// `query_run`). The stored statement text runs verbatim, never re-planned, so its
    /// `DELIMITER` context is kept. `PlanExpired` when the plan or index is unknown or
    /// its target changed; `Busy` while the tab runs.
    pub async fn query_plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<PlanView, DbError> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let (tab, conn) = self.plans.locate(self.plan_ttl, plan_id, &current)?;
        let rt = self.runtime(conn)?;
        let slot = rt.queries.get(&tab).ok_or(DbError::PlanExpired)?;
        let mut ts = Arc::clone(&slot)
            .try_lock_owned()
            .map_err(|_| DbError::Busy)?;
        let mut copy = self
            .plans
            .statement(self.plan_ttl, plan_id, index, &current)?;
        if ts.closed || ts.database != copy.target.database {
            return Err(DbError::PlanExpired);
        }
        if ts.session.is_none() && ts.reopen {
            // Dropped after a lost connection: reopen on the tab's database.
            let ensured = rt
                .ensure_tab_session(&mut ts, copy.target.database.clone())
                .await;
            if rt.queries.is_slot(&tab, &slot) {
                rt.queries.sync_bound(&tab, &ts);
            }
            ensured?;
        }
        let (dialect, tx_state, _) = session_facts(&ts)?;
        drop(ts);
        let cfg = self.current_config(&rt);
        let judged = match &mut copy.explain {
            Some(e) => &mut e.original,
            None => &mut copy.plan,
        };
        let analyze = judged.guard.is_some_and(|g| g.analyze);
        guard(judged, cfg.env_tag, cfg.read_only, analyze);
        self.store_plan(&rt, &cfg, copy, dialect, tx_state)
    }

    /// Export of all rows: statement `index` of plan `plan_id` (taken or not), to be
    /// read again on a data session: `(connection, database, text)`. Only a statement
    /// the planner classified as a read (one statement for the server, not an Explain)
    /// and the guard allows with the connection's current config (no confirmation
    /// needed) qualifies; anything else is a `config` error (field `scope`): export the
    /// loaded rows instead. `PlanExpired` as for [`Self::query_plan_statement`].
    pub fn export_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(Uuid, Option<String>, String), DbError> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let mut copy = self
            .plans
            .statement(self.plan_ttl, plan_id, index, &current)?;
        let rt = self.runtime(copy.target.conn)?;
        let cfg = self.current_config(&rt);
        let not_read = || {
            config_error(
                "scope",
                "Only the result of a read statement can be read again for export; \
                 export the loaded rows instead.",
            )
        };
        if copy.explain.is_some() || copy.plan.explain {
            return Err(not_read());
        }
        guard(&mut copy.plan, cfg.env_tag, cfg.read_only, false);
        let dialect = copy.plan.dialect;
        let stmt = copy.plan.stmts.first().ok_or(DbError::PlanExpired)?;
        if stmt.kind != StatementKind::Read
            || stmt.verdict != Verdict::Allow
            || dnt_core::sql::resplits(&stmt.text, dialect)
        {
            return Err(not_read());
        }
        Ok((copy.target.conn, copy.target.database, stmt.text.clone()))
    }
}

fn is_lost(e: &DbError) -> bool {
    matches!(
        e.unstaged(),
        DbError::ConnectionLost { .. } | DbError::Connect { .. }
    )
}

/// The dialect, transaction state and server of a held tab session.
fn session_facts(
    ts: &TabSession,
) -> Result<(Dialect, TxState, dnt_core::server::ServerInfo), DbError> {
    let session = ts.session.as_deref().ok_or_else(|| DbError::Internal {
        message: "the tab's session vanished".into(),
    })?;
    Ok((
        session.dialect(),
        session.tx_state(),
        session.server_info().clone(),
    ))
}

/// A run in progress, for `query_cancel` and for closing its tab or connection.
#[derive(Clone)]
pub(crate) struct RunEntry {
    pub(crate) tab: String,
    pub(crate) conn: Uuid,
    cancel: RunCancel,
    flags: Arc<RunFlags>,
    emit: EventSink,
    /// See [`RunTask::finished`].
    finished: Arc<AtomicBool>,
}

impl RunEntry {
    /// Sends `stopping{user}` (once per run), then stops the run: no further statement
    /// is sent and the running one is cancelled. A run whose statements are over is
    /// left alone (a cancel then could hit the tab's next statement).
    async fn stop(&self) -> Result<(), DbError> {
        if self.finished.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.flags.user_stop(&self.emit);
        self.cancel.cancel().await
    }
}

/// The runs in progress, by run id.
#[derive(Default, Clone)]
pub(crate) struct Runs(Arc<Mutex<HashMap<Uuid, RunEntry>>>);

impl Runs {
    fn get(&self, id: Uuid) -> Option<RunEntry> {
        lock(&self.0).get(&id).cloned()
    }

    fn insert(&self, id: Uuid, entry: RunEntry) {
        lock(&self.0).insert(id, entry);
    }

    fn remove(&self, id: Uuid) {
        lock(&self.0).remove(&id);
    }

    fn matching(&self, pred: impl Fn(&RunEntry) -> bool) -> Vec<RunEntry> {
        lock(&self.0)
            .values()
            .filter(|r| pred(r))
            .cloned()
            .collect()
    }
}

/// How many statements of `plan` have the verdict `v`.
fn count_verdict(plan: &ScriptPlan, v: Verdict) -> u32 {
    let n = plan.stmts.iter().filter(|s| s.verdict == v).count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Judges a stored plan again with the connection's current config and returns the plan
/// to run. `Blocked` when any statement is Block; `ConfirmationRequired` when any is
/// Confirm and the user did not confirm. An Explain plan is judged on the explained
/// statement; the explain statement it runs carries those verdicts.
fn rejudge(
    copy: &PlanCopy,
    cfg: &ConnectionConfig,
    confirmed: bool,
) -> Result<ScriptPlan, DbError> {
    let mut judged = copy
        .explain
        .as_ref()
        .map_or(&copy.plan, |e| &e.original)
        .clone();
    let Some(ctx) = judged.guard else {
        return Err(DbError::Internal {
            message: "the run plan was never guarded".into(),
        });
    };
    guard(&mut judged, cfg.env_tag, cfg.read_only, ctx.analyze);
    let blocked = count_verdict(&judged, Verdict::Block);
    if blocked > 0 {
        return Err(DbError::Blocked { count: blocked });
    }
    let confirm = count_verdict(&judged, Verdict::Confirm);
    if confirm > 0 && !confirmed {
        return Err(DbError::ConfirmationRequired { count: confirm });
    }
    if copy.explain.is_none() {
        return Ok(judged);
    }
    let mut run_plan = copy.plan.clone();
    for (s, j) in run_plan.stmts.iter_mut().zip(&judged.stmts) {
        s.verdict = j.verdict;
    }
    // Intentional: an Explain plan skips `run`'s re-guard after a `sql_mode` change.
    // The explain statement's own text is not what the guard judged (`query_run` just
    // judged the explained statement with the current config), so `run` only re-checks
    // its boundaries.
    run_plan.guard = None;
    Ok(run_plan)
}

/// Wraps a run's `emit` so that the first event it cannot deliver (the window whose
/// channel listened is gone) stops the run: the stop signal is raised at once (no
/// further statement is sent) and the running statement is cancelled in the
/// background, so the run ends and frees the tab's session. Nothing is sent after that.
///
/// Once `finished` is set (the run's statements are over, before the slot is released
/// and `done` is sent) a failed send never cancels: the drivers' cancel targets the
/// `execute_one` running when it is invoked (generation-scoped), so a late cancel
/// could otherwise hit the next run or `query_tx` on the tab's session.
fn stop_when_undelivered(
    emit: EventSink,
    cancel: RunCancel,
    finished: Arc<AtomicBool>,
) -> EventSink {
    let dead = Arc::new(AtomicBool::new(false));
    Arc::new(move |event| {
        if dead.load(Ordering::SeqCst) {
            return false;
        }
        if emit(event) {
            return true;
        }
        if !dead.swap(true, Ordering::SeqCst) && !finished.load(Ordering::SeqCst) {
            tracing::warn!("a query run lost its listener; stopping it");
            cancel.signal().stop();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let cancel = cancel.clone();
                let finished = Arc::clone(&finished);
                handle.spawn(async move {
                    // The run may have ended meanwhile.
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Err(e) = cancel.cancel().await {
                        tracing::warn!(error = %e, "could not cancel a running query");
                    }
                });
            }
        }
        false
    })
}

/// Everything a spawned run owns.
struct RunTask {
    run_id: Uuid,
    tab: String,
    rt: Arc<ConnectionRuntime>,
    slot: TabSlot,
    ts: OwnedMutexGuard<TabSession>,
    plan: ScriptPlan,
    explain: Option<ExplainFormat>,
    exec: ExecOptions,
    continue_on_error: bool,
    cancel: RunCancel,
    flags: Arc<RunFlags>,
    emit: EventSink,
    repo: Arc<ConnectionRepo>,
    runs: Runs,
    /// Set when the run's statements are over: no cancel may be sent any more.
    finished: Arc<AtomicBool>,
}

impl RunTask {
    async fn execute(mut self) {
        let started = Instant::now();
        let mut sink = ChannelSink::new(
            Arc::clone(&self.emit),
            Arc::clone(&self.flags),
            self.exec.batch_size,
            self.plan.dialect,
            HistoryCtx {
                repo: Arc::clone(&self.repo),
                connection_id: self.rt.cfg.id,
                database: self.ts.database.clone(),
            },
            self.explain,
        );
        let rows = sink.rows();
        let index = sink.index();
        let ticker = tokio::spawn(tick_rows(Arc::clone(&rows)));
        let ran = AssertUnwindSafe(async {
            let session = self.ts.session.as_deref()?;
            Some(
                run(
                    session,
                    &self.plan,
                    &self.exec,
                    self.continue_on_error,
                    &self.cancel,
                    &mut sink,
                )
                .await,
            )
        })
        .catch_unwind()
        .await;
        // No statement runs any more: from here on nothing may cancel the session (a
        // late cancel would hit the tab's next statement).
        self.finished.store(true, Ordering::SeqCst);
        ticker.abort();
        rows.flush();
        if let Some(writer) = sink.take_writer() {
            writer.finish().await;
        }
        let stop = match ran {
            Ok(Some(report)) => {
                self.after_run(Some(&report), &sink).await;
                report.stop
            }
            Ok(None) | Err(_) => {
                tracing::error!(connection = %self.rt.cfg.id, "a query run failed unexpectedly");
                // The session's state is unknown: drop it (or close it, with its tab).
                self.after_run(None, &sink).await;
                let at = index.load(Ordering::SeqCst);
                let mut error = AppError::from(DbError::Internal {
                    message: "the run failed unexpectedly; see the log for details".into(),
                });
                // A statement that was sent and never ended may have been applied.
                error.outcome_unknown = sink.in_statement();
                (self.emit)(RunEvent::StatementError { index: at, error });
                Some(StopReason::Error { index: at })
            }
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            connection = %self.rt.cfg.id,
            statements = self.plan.stmts.len(),
            elapsed_ms,
            stopped = stop.is_some(),
            "query run finished"
        );
        // The slot is free (and the tab's plans usable) before the UI hears `done`.
        let RunTask {
            run_id,
            runs,
            emit,
            ts,
            ..
        } = self;
        drop(ts);
        runs.remove(run_id);
        emit(RunEvent::Done { stop, elapsed_ms });
    }

    /// Applies what the run did to the tab's session: a lost connection drops it, a
    /// MySQL `USE` moves its database, a closed tab or connection closes it; a
    /// successful `Ddl` reloads the database's SchemaCache in the background.
    ///
    /// `report: None` (the run task panicked, or the session vanished) is handled like
    /// a lost connection: the session, in an unknown state, is dropped (and the tab's
    /// binding with it). After a lost connection whose errors are all retryable the
    /// tab keeps its binding ([`TabSession::reopen`]); a session dropped with a
    /// transaction open sets [`TabSession::lost_tx`].
    ///
    /// The SchemaCache refresh covers only the session's current database (or the
    /// connect database): DDL on another database (`CREATE TABLE other.t`) or another
    /// PostgreSQL schema's database is not picked up until that cache is refreshed.
    async fn after_run(&mut self, report: Option<&RunReport>, sink: &ChannelSink) {
        let lost_errors: Vec<&AppError> = report.map_or_else(Vec::new, |report| {
            report
                .stmts
                .iter()
                .filter_map(|s| s.result.as_ref().err())
                .filter(|e| e.kind == "connection_lost" || e.kind == "connect")
                .collect()
        });
        let lost = report.is_none() || !lost_errors.is_empty();
        let ts = &mut *self.ts;
        if lost {
            let in_tx = ts.tx_open();
            if let Some(dead) = ts.session.take() {
                close_quietly(dead).await;
                ts.lost_tx = in_tx;
                // Retry / Fetch all may run the tab's statements again on a fresh
                // session, unless a transaction was lost or an outcome is unknown.
                ts.reopen = report.is_some() && !in_tx && lost_errors.iter().all(|e| e.retryable);
            }
        } else if let Some(db) = &sink.use_db {
            ts.database = Some(db.clone());
        }
        let current = self.rt.queries.is_slot(&self.tab, &self.slot);
        if self.rt.closed.load(Ordering::SeqCst) || !current || ts.closed {
            // The tab or connection was closed while the run held the session.
            ts.close().await;
        } else {
            self.rt.queries.sync_bound(&self.tab, ts);
        }
        if sink.ddl_ok {
            let database = ts.database.clone().or_else(|| self.rt.home_database());
            if let Some(db) = database {
                self.rt.refresh_schema_cache(&db);
            }
        }
    }
}

impl SessionManager {
    /// Overrides the cancel grace of query runs (tests).
    pub fn with_cancel_grace(mut self, grace: Duration) -> Self {
        self.cancel_grace = grace;
        self
    }

    /// Runs plan `plan_id` on its tab and returns the run's id at once; everything the
    /// run produces goes to `emit` (see [`RunEvent`]), ending with `done`.
    ///
    /// The plan is judged again with the connection's current config: any Block →
    /// `Blocked`, any Confirm without `confirmed` → `ConfirmationRequired` (the plan
    /// stays available, so the UI can ask and run it again with `confirmed`). A tab
    /// that is already running (or planning) → `Busy`. A plan already run, expired,
    /// replaced, or whose tab moved → `PlanExpired`. The run holds the tab's session
    /// until it ends.
    pub async fn query_run(
        &self,
        plan_id: Uuid,
        confirmed: bool,
        opts: RunOptions,
        emit: EventSink,
    ) -> Result<Uuid, DbError> {
        let current = |t: &PlanTarget| self.plan_target_current(t);
        let (tab, conn) = self.plans.locate(self.plan_ttl, plan_id, &current)?;
        let rt = self.runtime(conn)?;
        let slot = rt.queries.get(&tab).ok_or(DbError::PlanExpired)?;
        let mut ts = Arc::clone(&slot)
            .try_lock_owned()
            .map_err(|_| DbError::Busy)?;
        let copy = self.plans.pending(self.plan_ttl, plan_id, &current)?;
        if ts.closed || (ts.session.is_none() && !ts.reopen) || ts.database != copy.target.database
        {
            return Err(DbError::PlanExpired);
        }
        let cfg = self.current_config(&rt);
        let run_plan = rejudge(&copy, &cfg, confirmed)?;
        // A stale session is replaced; one dropped after a lost connection reopens.
        if rt.is_stale(&ts) || ts.session.is_none() {
            let ensured = rt
                .ensure_tab_session(&mut ts, copy.target.database.clone())
                .await;
            rt.queries.sync_bound(&tab, &ts);
            ensured?;
        }
        self.plans.take(self.plan_ttl, plan_id, &current)?;
        let session = ts.session.as_deref().ok_or(DbError::PlanExpired)?;

        let exec = ExecOptions {
            max_rows: opts.max_rows.clamp(1, MAX_ROWS_LIMIT) as usize,
            batch_size: RUN_BATCH_SIZE,
            timeout: (cfg.options.query_timeout_s > 0)
                .then(|| Duration::from_secs(cfg.options.query_timeout_s.into())),
            cancel_grace: self.cancel_grace,
            ..ExecOptions::default()
        };
        let cancel = RunCancel::new(session.cancel_handle());
        let finished = Arc::new(AtomicBool::new(false));
        let emit = stop_when_undelivered(emit, cancel.clone(), Arc::clone(&finished));
        let flags = Arc::new(RunFlags::default());
        let run_id = Uuid::new_v4();
        self.runs.insert(
            run_id,
            RunEntry {
                tab: tab.clone(),
                conn,
                cancel: cancel.clone(),
                flags: Arc::clone(&flags),
                emit: Arc::clone(&emit),
                finished: Arc::clone(&finished),
            },
        );
        let task = RunTask {
            run_id,
            tab,
            rt,
            slot,
            ts,
            plan: run_plan,
            explain: copy.explain.map(|e| e.format),
            exec,
            continue_on_error: opts.continue_on_error,
            cancel,
            flags,
            emit,
            repo: Arc::clone(&self.repo),
            runs: self.runs.clone(),
            finished,
        };
        tokio::spawn(task.execute());
        Ok(run_id)
    }

    /// Stops run `run_id`: emits `stopping{user}` (once), then cancels it. The run ends
    /// with `done` as usual. An unknown or finished run is a no-op.
    pub async fn query_cancel(&self, run_id: Uuid) -> Result<(), DbError> {
        match self.runs.get(run_id) {
            Some(entry) => entry.stop().await,
            None => Ok(()),
        }
    }

    /// Stops every run `pred` selects (closing a tab or connection), each bounded by
    /// [`SLOT_CLOSE_WAIT`].
    pub(crate) async fn stop_runs(&self, pred: impl Fn(&RunEntry) -> bool) {
        let runs = self.runs.matching(pred);
        futures_util::future::join_all(runs.iter().map(|r| async move {
            match tokio::time::timeout(SLOT_CLOSE_WAIT, r.stop()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "could not cancel a running query"),
                Err(_) => tracing::warn!("cancelling a running query timed out"),
            }
        }))
        .await;
    }

    /// Plans an Explain of the statement at `cursor` (UTF-16) on tab `tab` (session rules
    /// as for [`Self::query_plan`]) and returns its view; run it with `query_run`, which
    /// streams one `explain` event. `Ddl` (or no statement) → `Config` "Explain is not
    /// available for this statement"; `analyze` on a statement that is not `Read` →
    /// `Config` "Explain Analyze is only available for read statements" (it would
    /// execute the statement); `Unsupported` where the server has no such explain.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_explain(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
        text: &str,
        cursor: u32,
        analyze: bool,
    ) -> Result<PlanView, DbError> {
        let (rt, ts) = self.tab_session(tab, conn, database).await?;
        let (dialect, tx_state, info) = session_facts(&ts)?;
        let target = PlanTarget {
            tab: tab.to_string(),
            conn,
            runtime: Arc::downgrade(&rt),
            database: ts.database.clone(),
        };
        drop(ts);

        let mut original = plan(text, Target::Cursor(cursor), dialect);
        let Some(stmt) = original.stmts.first() else {
            return Err(config_error(
                "explain",
                "Explain is not available for this statement",
            ));
        };
        if stmt.kind == StatementKind::Ddl {
            return Err(config_error(
                "explain",
                "Explain is not available for this statement",
            ));
        }
        if analyze && stmt.kind != StatementKind::Read {
            return Err(config_error(
                "explain",
                "Explain Analyze is only available for read statements",
            ));
        }
        let sql = explain_sql(&stmt.text, &info, dialect, analyze)?;
        let cfg = self.current_config(&rt);
        original.explain = true;
        guard(&mut original, cfg.env_tag, cfg.read_only, analyze);

        // The explain statement replaces the statement (one, the Cursor's): it is the
        // whole source of the plan that runs.
        let mut planned = original.clone();
        for s in &mut planned.stmts {
            s.kind = classify(&sql.sql, dialect);
            s.text = sql.sql.clone();
            s.source_range = (0, s.text.len());
        }
        planned.source = sql.sql;
        planned.base = planned.stmts.first().map_or(0, |s| s.range.0);
        let explain = ExplainPlan {
            format: sql.format,
            original,
        };
        let copy = PlanCopy {
            target,
            plan: planned,
            explain: Some(explain),
            source: None,
        };
        self.store_plan(&rt, &cfg, copy, dialect, tx_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_flattens_line_breaks_and_cuts() {
        assert_eq!(preview("a\r\nb\nc\rd"), "a b c d");
        let long = "é".repeat(200);
        assert_eq!(preview(&long).chars().count(), PREVIEW_CHARS);
    }

    #[test]
    fn mysql_ident_quoting() {
        assert_eq!(quote_mysql_ident("other db"), "`other db`");
        assert_eq!(quote_mysql_ident("a`b"), "`a``b`");
    }

    #[test]
    fn tx_action_wire_names() {
        let a: TxAction = serde_json::from_str("\"Rollback\"").unwrap();
        assert_eq!(a, TxAction::Rollback);
        assert_eq!(TxAction::Commit.sql(), "COMMIT");
    }
}
