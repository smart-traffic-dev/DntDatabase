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
//!   repository when the plan is made.

use crate::session_manager::{
    close_quietly, closed_error, config_error, connect_db, with_database, ConnState,
    ConnectionRuntime, SessionManager,
};
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, Dialect};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::{guard, plan, ScriptPlan, StmtKindLabel, Target, Verdict};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a plan stays available to `query_run` after `query_plan` (and, once taken,
/// to [`SessionManager::plan_statement`] after the take).
pub const PLAN_TTL: Duration = Duration::from_secs(120);

/// Characters of a statement shown in the confirmation and block dialogs.
pub const PREVIEW_CHARS: usize = 120;

/// How long closing a tab (or its connection) waits for whoever holds the tab's session.
pub(crate) const SLOT_CLOSE_WAIT: Duration = Duration::from_secs(5);

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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
/// For Task 15 (the run): when `execute_one` fails with `ConnectionLost` or `Connect`,
/// set `session = None` (and call [`QuerySessions::sync_bound`]) so the next plan opens
/// a fresh session instead of reusing a dead one (carried from the Task 14 review). A
/// `USE db` the user runs (MySQL, `dnt_core::sql::session_effect`) must update
/// `database` (and `sync_bound`) after it succeeds (review M3), or the tab's next plan
/// would switch back to the database the UI shows.
#[derive(Default)]
pub struct TabSession {
    /// The database the session is on: the one the tab asked for, else the connect
    /// database. Meaningful only while `session` is set.
    pub(crate) database: Option<String>,
    pub(crate) session: Option<Box<dyn Session>>,
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
        if ts.session.is_some() && !ts.closed {
            tabs.bound.insert(tab.to_string(), ts.database.clone());
        } else {
            tabs.bound.remove(tab);
        }
    }

    /// The database `tab`'s session is on; `None` when the tab has no session here.
    fn bound(&self, tab: &str) -> Option<Option<String>> {
        lock(&self.tabs).bound.get(tab).cloned()
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
pub(crate) struct PlanTarget {
    tab: String,
    conn: Uuid,
    /// The runtime the plan was made on (a reopened connection is another one).
    runtime: Weak<ConnectionRuntime>,
    /// The database the tab's session was on.
    database: Option<String>,
}

struct StoredPlan {
    target: PlanTarget,
    plan: ScriptPlan,
    /// When the plan was made.
    at: Instant,
    /// `query_run` took it: it cannot be run again, only single statements of it.
    taken: bool,
}

/// Plans by id. A pending (never taken) plan lives for the plan TTL and only the newest
/// one of a tab is kept. Of the taken plans only the latest of each tab is kept (its
/// results are what the tab shows: Retry / Fetch all use it), with no TTL: it goes with
/// the next take of the tab, or when the tab or its connection closes.
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
    fn insert(&self, ttl: Duration, target: PlanTarget, plan: ScriptPlan) -> Uuid {
        debug_assert!(plan.guard.is_some(), "stored plans must be guarded");
        let id = Uuid::new_v4();
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        plans.retain(|_, p| p.taken || p.target.tab != target.tab);
        plans.insert(
            id,
            StoredPlan {
                target,
                plan,
                at: Instant::now(),
                taken: false,
            },
        );
        id
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
        let taken = (p.target.tab.clone(), p.target.conn, p.plan.clone());
        // Only the latest taken plan of a tab is kept.
        plans.retain(|pid, p| *pid == id || !p.taken || p.target.tab != taken.0);
        Ok(taken)
    }

    fn statement(
        &self,
        ttl: Duration,
        id: Uuid,
        index: u32,
        current: &dyn Fn(&PlanTarget) -> bool,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let mut plans = lock(&self.plans);
        Self::prune(&mut plans, ttl);
        let p = Self::checked(&mut plans, id, current)?;
        let stmt = p
            .plan
            .stmts
            .iter()
            .find(|s| s.index == index)
            .ok_or(DbError::PlanExpired)?;
        // Only the statement: its text is the whole source of the one-statement plan.
        let mut one = stmt.clone();
        one.source_range = (0, one.text.len());
        let plan = ScriptPlan {
            dialect: p.plan.dialect,
            source: one.text.clone(),
            base: one.range.0,
            stmts: vec![one],
            explain: p.plan.explain,
            guard: p.plan.guard,
        };
        Ok((p.target.tab.clone(), p.target.conn, plan))
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
    async fn open_query_session(
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
        let session = ts.session.as_deref().ok_or_else(|| DbError::Internal {
            message: "the tab's session vanished".into(),
        })?;
        let dialect = session.dialect();
        let tx_state = session.tx_state();
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
        let stmts = planned
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
        let plan_id = self.plans.insert(self.plan_ttl, plan_target, planned);
        Ok(PlanView {
            plan_id,
            stmts,
            dialect,
            connection_name: cfg.name,
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
    fn find_tab(&self, tab: &str) -> Option<TabSlot> {
        self.runtimes_snapshot()
            .into_iter()
            .find_map(|(_, rt)| rt.queries.get(tab))
    }

    /// Commits or rolls back the tab's transaction (`execute_one("COMMIT"/"ROLLBACK")`)
    /// and returns the session's new state. A tab without a session is `Idle`.
    pub async fn query_tx(&self, tab: &str, action: TxAction) -> Result<TxState, DbError> {
        validate_tab(tab)?;
        let Some(slot) = self.find_tab(tab) else {
            return Ok(TxState::Idle);
        };
        let ts = slot.try_lock_owned().map_err(|_| DbError::Busy)?;
        let Some(session) = ts.session.as_deref() else {
            return Ok(TxState::Idle);
        };
        execute_quietly(session, action.sql()).await?;
        Ok(session.tx_state())
    }

    /// Closes the tab's session and forgets its plans. Without `force`, a session with a
    /// transaction open is kept and `TransactionOpen` returned, so the UI can ask
    /// Commit / Rollback / Cancel (spec §7.2); with `force` the session is closed anyway
    /// (the server rolls the transaction back). Waits up to [`SLOT_CLOSE_WAIT`] for a
    /// holder of the session: without `force` that ends in `Busy`. Closing an unknown
    /// tab is a no-op.
    pub async fn query_close_tab(&self, tab: &str, force: bool) -> Result<(), DbError> {
        validate_tab(tab)?;
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
    /// The check reads the tab's database without its slot, so Task 15 may take the plan
    /// before or after it locks the slot; after locking it must still compare
    /// `ts.database` (a plan made meanwhile can switch the database in between).
    ///
    /// For Task 15 (review M2): the plan was guarded with the config as it was when it
    /// was made (`plan.guard` is set). `query_run` must re-apply `guard` with
    /// `current_config` before it runs, and refuse `Block` / unconfirmed `Confirm`.
    /// (Review item M7 is also carried to Task 15; see the ledger.)
    pub fn take_plan(&self, plan_id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
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
    /// For Task 15 (review M2): re-apply `guard` with `current_config` before running
    /// the statement, as for `take_plan`.
    pub fn plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plans.statement(self.plan_ttl, plan_id, index, &|t| {
            self.plan_target_current(t)
        })
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
