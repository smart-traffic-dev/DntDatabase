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
    close_quietly, closed_error, connect_db, with_database, ConnState, ConnectionRuntime,
    SessionManager,
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
use std::sync::{Arc, Mutex, PoisonError};
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

/// A tab's query session.
#[derive(Default)]
pub struct TabSession {
    /// The database the session is on: the one the tab asked for, else the connect
    /// database. Meaningful only while `session` is set.
    pub(crate) database: Option<String>,
    pub(crate) session: Option<Box<dyn Session>>,
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

/// The query tabs of one connection.
#[derive(Default)]
pub(crate) struct QuerySessions {
    tabs: Mutex<HashMap<String, TabSlot>>,
}

impl QuerySessions {
    fn get(&self, tab: &str) -> Option<TabSlot> {
        lock(&self.tabs).get(tab).cloned()
    }

    /// The slot of `tab`, created on first use. Refused once `closed` (the runtime's
    /// flag, which `shutdown` sets before it drains the slots) is set.
    fn get_or_create(&self, tab: &str, closed: &AtomicBool) -> Result<TabSlot, DbError> {
        let mut tabs = lock(&self.tabs);
        if closed.load(Ordering::SeqCst) {
            return Err(closed_error());
        }
        Ok(Arc::clone(tabs.entry(tab.to_string()).or_default()))
    }

    /// Forgets `tab` if `slot` is still its slot.
    fn remove_if(&self, tab: &str, slot: &TabSlot) {
        let mut tabs = lock(&self.tabs);
        if tabs.get(tab).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            tabs.remove(tab);
        }
    }

    pub(crate) fn drain(&self) -> Vec<(String, TabSlot)> {
        lock(&self.tabs).drain().collect()
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

struct StoredPlan {
    tab: String,
    conn: Uuid,
    plan: ScriptPlan,
    /// When the plan was made, or taken.
    at: Instant,
    /// `query_run` took it: it cannot be run again, only single statements of it.
    taken: bool,
}

/// Plans by id, kept for the manager's plan TTL.
#[derive(Default)]
pub(crate) struct PlanStore {
    plans: Mutex<HashMap<Uuid, StoredPlan>>,
}

impl PlanStore {
    fn live(plans: &mut HashMap<Uuid, StoredPlan>, ttl: Duration) {
        plans.retain(|_, p| p.at.elapsed() < ttl);
    }

    /// Stores a guarded plan. The tab's plans that were never taken (a dialog the user
    /// abandoned) are dropped: only the newest pending plan of a tab can run.
    fn insert(&self, ttl: Duration, tab: &str, conn: Uuid, plan: ScriptPlan) -> Uuid {
        debug_assert!(plan.guard.is_some(), "stored plans must be guarded");
        let id = Uuid::new_v4();
        let mut plans = lock(&self.plans);
        Self::live(&mut plans, ttl);
        plans.retain(|_, p| p.taken || p.tab != tab);
        plans.insert(
            id,
            StoredPlan {
                tab: tab.to_string(),
                conn,
                plan,
                at: Instant::now(),
                taken: false,
            },
        );
        id
    }

    fn take(&self, ttl: Duration, id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let mut plans = lock(&self.plans);
        Self::live(&mut plans, ttl);
        let p = plans
            .get_mut(&id)
            .filter(|p| !p.taken)
            .ok_or(DbError::PlanExpired)?;
        p.taken = true;
        p.at = Instant::now();
        Ok((p.tab.clone(), p.conn, p.plan.clone()))
    }

    fn statement(
        &self,
        ttl: Duration,
        id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        let mut plans = lock(&self.plans);
        Self::live(&mut plans, ttl);
        let p = plans.get(&id).ok_or(DbError::PlanExpired)?;
        let stmt = p
            .plan
            .stmts
            .iter()
            .find(|s| s.index == index)
            .ok_or(DbError::PlanExpired)?;
        let one = ScriptPlan {
            dialect: p.plan.dialect,
            source: p.plan.source.clone(),
            base: p.plan.base,
            stmts: vec![stmt.clone()],
            explain: p.plan.explain,
            guard: p.plan.guard,
        };
        Ok((p.tab.clone(), p.conn, one))
    }

    pub(crate) fn drop_tab(&self, tab: &str) {
        lock(&self.plans).retain(|_, p| p.tab != tab);
    }

    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.conn != conn);
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
    /// the tunnel first when it died (as metadata calls do).
    async fn open_query_session(
        &self,
        database: Option<String>,
    ) -> Result<Box<dyn Session>, DbError> {
        let resolved = {
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
            with_database(&link.resolved, database)
        };
        connect_db(self.driver.as_ref(), &resolved).await
    }

    /// Puts `ts` on the database `database` resolves to: opens the session if there is
    /// none; otherwise switches (refused with an open transaction): MySQL runs `USE`,
    /// PostgreSQL opens a session on the new database and closes the old one.
    async fn ensure_tab_session(
        &self,
        ts: &mut TabSession,
        database: Option<String>,
    ) -> Result<(), DbError> {
        let target = self.query_database(database);
        let Some(session) = ts.session.as_deref() else {
            ts.session = Some(self.open_query_session(target.clone()).await?);
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
                let fresh = self.open_query_session(target.clone()).await?;
                if let Some(old) = ts.session.replace(fresh) {
                    close_quietly(old).await;
                }
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

    /// Plans `target` of `text` on tab `tab`, bound to connection `conn` and `database`
    /// (`None`: the connect database), and judges every statement with the guard. The
    /// tab's session opens on first use; see the module docs for switching connection or
    /// database. Errors: `NotOpen`, `Busy` (the tab is running), `TransactionOpen`
    /// (switching away from an open transaction), connect errors.
    pub async fn query_plan(
        &self,
        tab: &str,
        conn: Uuid,
        database: Option<String>,
        text: &str,
        target: Target,
    ) -> Result<PlanView, DbError> {
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
        rt.ensure_tab_session(&mut ts, database).await?;
        let session = ts.session.as_deref().ok_or_else(|| DbError::Internal {
            message: "the tab's session vanished".into(),
        })?;
        let dialect = session.dialect();
        let tx_state = session.tx_state();
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
        let plan_id = self.plans.insert(self.plan_ttl, tab, conn, planned);
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
    /// unknown, expired, replaced or already taken plans are `PlanExpired`. The plan is
    /// guarded (`plan.guard` is set); `query_run` must still re-check its verdicts.
    pub fn take_plan(&self, plan_id: Uuid) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plans.take(self.plan_ttl, plan_id)
    }

    /// Statement `index` of plan `plan_id` (taken or not) as a one-statement plan, for
    /// Retry and Fetch all: the stored statement text runs verbatim, never re-planned
    /// (so its `DELIMITER` context is kept). Keeps the plan's guard context and the
    /// statement's index and range. `PlanExpired` when the plan or index is unknown.
    pub fn plan_statement(
        &self,
        plan_id: Uuid,
        index: u32,
    ) -> Result<(String, Uuid, ScriptPlan), DbError> {
        self.plans.statement(self.plan_ttl, plan_id, index)
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
