//! Sessions of the table data view (`table_data` / `table_row_count`).
//!
//! They are separate from both the metadata sessions and the query tabs' sessions:
//! - opened lazily per (connection, database) through the connection's tunnel, with the
//!   same open path as a query tab ([`ConnectionRuntime::open_query_session`], which only
//!   takes the metadata `link` for the moment it reads the address), and used OUTSIDE the
//!   `link` lock, so a slow page or count never holds up the tree, completion or Info;
//! - taken from a small idle pool for one request and given back afterwards (a session
//!   that failed, or was opened through a tunnel that has been replaced, is closed);
//! - every request runs under a caller-chosen key (one per tab and purpose): a new
//!   request under the same key cancels the one still running, and
//!   [`SessionManager::data_cancel`] cancels it on demand, through the driver's cancel
//!   (`KILL QUERY` / PostgreSQL cancel request);
//! - closing the connection cancels the running requests and closes the idle sessions;
//!   an edit apply (which runs on a taken session, see `edit_plans`) is registered with
//!   [`DataSessions::register_apply`] so closing can wait for it (at most
//!   `SessionManager::apply_close_wait`) and then cancel it.
//!
//! # Cancels never reach a reused session
//!
//! The drivers' cancel targets whatever statement runs on the session when the cancel is
//! actually sent (it reads the running generation then, not when it was requested). A
//! session whose request was cancelled or superseded is therefore NEVER given back to the
//! pool: it is closed, even when its statement completed. A late cancel can then only
//! find that request's statement or an idle (closing) session, never another request's.
//! A request cancelled while its session is still opening is dropped before it starts
//! (its fresh session, which ran nothing, does go to the pool).

use crate::session_manager::{
    close_quietly, config_error, is_lost, lock, ConnectionRuntime, SessionManager,
};
use dnt_core::driver::{CancelHandle, ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::DbError;
use dnt_core::sql::{Dialect, StatementKind, TxControl};
use dnt_core::value::{ColumnMeta, Value};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{watch, Notify};
use uuid::Uuid;

/// Statement timeout of a data request when the connection sets no query timeout.
pub const DATA_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace after a timeout / cancel before the driver gives the connection up.
pub(crate) const DATA_CANCEL_GRACE: Duration = Duration::from_secs(5);
/// Idle data sessions kept per database.
const IDLE_PER_DATABASE: usize = 2;

/// A text / JSON cell longer than this is cut (on a char boundary).
pub const MAX_CELL_BYTES: usize = dnt_core::value::PAGE_CELL_CAP;
/// A page stops taking rows once its cells add up to this.
pub const MAX_PAGE_BYTES: usize = 32 * 1024 * 1024;

struct Idle {
    session: Box<dyn Session>,
    tunnel_gen: u64,
}

/// A data session taken for one exclusive use ([`SessionManager::take_data_session`]).
pub(crate) struct TakenSession {
    pub session: Box<dyn Session>,
    /// `ConnectionRuntime::tunnel_gen` when the session was opened.
    pub tunnel_gen: u64,
    database: Option<String>,
    /// It came from the pool (it may have gone stale while idle).
    pub pooled: bool,
}

/// Where the current request of a key is.
enum Phase {
    /// Its session is being taken / opened: a cancel only removes the entry.
    Opening,
    /// Its statement runs: a cancel raises `stop` and sends the driver's cancel.
    Running {
        cancel: CancelHandle,
        stop: StopSignal,
    },
}

struct Request {
    serial: u64,
    phase: Phase,
}

/// How long closing waits, after cancelling a running edit apply, for it to end.
const APPLY_ABORT_WAIT: Duration = Duration::from_secs(1);

/// What closing needs of a running edit apply.
struct ApplyCtl {
    stop: StopSignal,
    cancel: CancelHandle,
    abort: Arc<Notify>,
    /// Its sender is dropped when the apply ends.
    done: watch::Receiver<()>,
}

impl ApplyCtl {
    async fn ended(&mut self) {
        // Nothing is ever sent: `changed` returns once the sender is dropped.
        let _ = self.done.changed().await;
    }
}

#[derive(Default)]
struct Applies {
    serial: u64,
    running: HashMap<u64, ApplyCtl>,
    /// Set by `end_applies`: no apply may start any more.
    closing: bool,
}

/// A registered edit apply; dropping it unregisters it.
pub(crate) struct ApplyGuard<'a> {
    data: &'a DataSessions,
    id: u64,
    /// Notified when closing gives up waiting: the apply must stop now.
    pub(crate) abort: Arc<Notify>,
    _done: watch::Sender<()>,
}

impl Drop for ApplyGuard<'_> {
    fn drop(&mut self) {
        lock(&self.data.applies).running.remove(&self.id);
    }
}

/// The data sessions of one connection runtime.
#[derive(Default)]
pub(crate) struct DataSessions {
    idle: Mutex<HashMap<Option<String>, Vec<Idle>>>,
    /// The current request of every key. A request whose entry is gone (or holds another
    /// serial) was cancelled or superseded.
    requests: Mutex<HashMap<String, Request>>,
    serial: AtomicU64,
    /// Edit applies running on this connection.
    applies: Mutex<Applies>,
}

/// Stops a request taken out of the map.
fn stop_request(r: Request) {
    if let Phase::Running { cancel, stop } = r.phase {
        stop.stop();
        tokio::spawn(async move {
            let _ = cancel.cancel().await;
        });
    }
}

impl DataSessions {
    /// Cancels every request and closes the idle sessions (connection close).
    pub(crate) async fn shutdown(&self) {
        let requests: Vec<Request> = lock(&self.requests).drain().map(|(_, r)| r).collect();
        for r in requests {
            stop_request(r);
        }
        let idle: Vec<Idle> = lock(&self.idle).drain().flat_map(|(_, v)| v).collect();
        futures_util::future::join_all(idle.into_iter().map(|i| close_quietly(i.session))).await;
    }

    /// Registers an edit apply about to run on `session` with `stop` in its options.
    /// `None` once the connection is closing (`closed` is the runtime's flag, set before
    /// `end_applies` runs): nothing may be sent then.
    pub(crate) fn register_apply(
        &self,
        closed: &AtomicBool,
        session: &dyn Session,
        stop: &StopSignal,
    ) -> Option<ApplyGuard<'_>> {
        let mut applies = lock(&self.applies);
        if applies.closing || closed.load(Ordering::SeqCst) {
            return None;
        }
        applies.serial += 1;
        let id = applies.serial;
        let abort = Arc::new(Notify::new());
        let (tx, rx) = watch::channel(());
        applies.running.insert(
            id,
            ApplyCtl {
                stop: stop.clone(),
                cancel: session.cancel_handle(),
                abort: Arc::clone(&abort),
                done: rx,
            },
        );
        Some(ApplyGuard {
            data: self,
            id,
            abort,
            _done: tx,
        })
    }

    /// Closing: refuses new applies, waits up to `wait` for each running one, then
    /// cancels it (stop signal, the driver's cancel, and `abort`, which makes the apply
    /// report what is known and close its session) and waits briefly for it to end.
    pub(crate) async fn end_applies(&self, wait: Duration) {
        let running: Vec<ApplyCtl> = {
            let mut applies = lock(&self.applies);
            applies.closing = true;
            applies.running.drain().map(|(_, c)| c).collect()
        };
        futures_util::future::join_all(running.into_iter().map(|mut c| async move {
            if tokio::time::timeout(wait, c.ended()).await.is_ok() {
                return;
            }
            tracing::warn!("an edit apply is still running; cancelling it to close");
            c.stop.stop();
            c.abort.notify_one();
            let cancel = Arc::clone(&c.cancel);
            tokio::spawn(async move {
                let _ = cancel.cancel().await;
            });
            if tokio::time::timeout(APPLY_ABORT_WAIT, c.ended())
                .await
                .is_err()
            {
                tracing::warn!("a cancelled edit apply did not end; closing without it");
            }
        }))
        .await;
    }

    /// Cancels the request of `key` (running or still opening); whether there was one.
    fn cancel(&self, key: &str) -> bool {
        match lock(&self.requests).remove(key) {
            Some(r) => {
                stop_request(r);
                true
            }
            None => false,
        }
    }

    /// Makes request `serial` the current one of `key`, superseding (cancelling) the one
    /// there.
    fn begin(&self, key: &str, serial: u64) {
        let previous = lock(&self.requests).insert(
            key.to_string(),
            Request {
                serial,
                phase: Phase::Opening,
            },
        );
        if let Some(previous) = previous {
            stop_request(previous);
        }
    }

    /// Moves request `serial` of `key` to running on `session`; false when it was
    /// cancelled or superseded meanwhile (it must not start).
    fn start(&self, key: &str, serial: u64, session: &dyn Session, stop: &StopSignal) -> bool {
        let mut requests = lock(&self.requests);
        match requests.get_mut(key) {
            Some(r) if r.serial == serial => {
                r.phase = Phase::Running {
                    cancel: session.cancel_handle(),
                    stop: stop.clone(),
                };
                true
            }
            _ => false,
        }
    }

    /// Ends request `serial` of `key`; true when it was still current, i.e. no cancel was
    /// ever sent for it (only then may its session be reused).
    fn finish(&self, key: &str, serial: u64) -> bool {
        let mut requests = lock(&self.requests);
        if requests.get(key).is_some_and(|r| r.serial == serial) {
            requests.remove(key);
            true
        } else {
            false
        }
    }
}

/// Approximate size of a cell in a page (what it costs to send).
fn cell_bytes(v: &Value) -> usize {
    match v {
        Value::Text(s) | Value::Json(s) | Value::Decimal(s) | Value::Uuid(s) => s.len(),
        Value::Date(s) | Value::Time(s) | Value::DateTime(s) | Value::TimestampTz(s) => s.len(),
        Value::Other { type_name, text } => type_name.len() + text.len(),
        Value::Bytes { preview, .. } => preview.len(),
        Value::Array(items) => items.iter().map(cell_bytes).sum::<usize>() + 8,
        _ => 8,
    }
}

/// `s` cut to at most `max` bytes on a char boundary; whether it was cut.
fn cut(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    true
}

/// `v` with long text cut to `max` bytes; whether anything was cut.
fn truncate_cell(v: &mut Value, max: usize) -> bool {
    match v {
        Value::Text(s) | Value::Json(s) => cut(s, max),
        Value::Other { text, .. } => cut(text, max),
        // Every item is cut (no short circuit).
        Value::Array(items) => {
            items
                .iter_mut()
                .map(|i| truncate_cell(i, max))
                .filter(|cut| *cut)
                .count()
                > 0
        }
        _ => false,
    }
}

/// Collects the first result of a data request, bounded: long cells are cut to
/// `cell_cap` bytes and rows stop once the page holds `page_cap` bytes.
#[derive(Debug)]
pub struct BoundedPage {
    cell_cap: usize,
    page_cap: usize,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    pub bytes: usize,
    /// Cells of the kept rows that were cut.
    pub truncated_cells: u32,
    /// Where those cells are: `(row, column)` indexes into `rows`.
    pub truncated: Vec<(u32, u32)>,
    /// Rows were left out because the page reached `page_cap`.
    pub capped: bool,
    results: usize,
}

impl BoundedPage {
    pub fn new(cell_cap: usize, page_cap: usize) -> Self {
        BoundedPage {
            cell_cap,
            page_cap,
            columns: Vec::new(),
            rows: Vec::new(),
            bytes: 0,
            truncated_cells: 0,
            truncated: Vec::new(),
            capped: false,
            results: 0,
        }
    }
}

/// An index as a page coordinate (pages are far smaller than `u32::MAX`).
fn coord(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl RowSink for BoundedPage {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.results += 1;
        if self.results == 1 {
            self.columns = cols.to_vec();
        }
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.results != 1 {
            return;
        }
        for mut row in batch {
            if self.capped {
                return;
            }
            let mut size = 0;
            let mut cut = Vec::new();
            for (col, cell) in row.iter_mut().enumerate() {
                if truncate_cell(cell, self.cell_cap) {
                    cut.push(coord(col));
                }
                size += cell_bytes(cell);
            }
            // The first row is always kept, so a page never comes back empty for size.
            if !self.rows.is_empty() && self.bytes + size > self.page_cap {
                self.capped = true;
                return;
            }
            let r = coord(self.rows.len());
            self.truncated_cells = self.truncated_cells.saturating_add(coord(cut.len()));
            self.truncated.extend(cut.into_iter().map(|c| (r, c)));
            self.bytes += size;
            self.rows.push(row);
        }
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Builds a read from the dialect of the session it runs on (a raw `WHERE` must be
/// lexed the way that session's server will read it). An error is returned before
/// anything is sent.
pub type BuildRead = dyn Fn(Dialect) -> Result<String, DbError> + Send + Sync;

/// A read built around the user's raw `WHERE` text (see [`DataRequest::raw`]).
#[derive(Clone)]
pub struct RawRead(pub Arc<BuildRead>);

impl fmt::Debug for RawRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RawRead(..)")
    }
}

/// One statement for [`SessionManager::data_exec`].
#[derive(Debug, Clone, Default)]
pub struct DataRequest {
    /// ONE read statement: built by the backend, or a user statement the plan / guard
    /// classified as a read (export). Ignored when `raw` is set.
    pub sql: String,
    /// A read around the user's raw `WHERE` text, built with the session's dialect, then
    /// run inside a read-only transaction that is always rolled back: PostgreSQL
    /// `BEGIN READ ONLY` (after checking `standard_conforming_strings` is on, which the
    /// raw text was lexed under), MySQL `START TRANSACTION READ ONLY`.
    pub raw: Option<RawRead>,
    /// Bound values (see `ExecOptions::params`).
    pub params: Vec<String>,
    pub max_rows: usize,
    /// Every byte of binary values (export).
    pub full_bytes: bool,
    /// No statement timeout (an export streams for as long as it takes; it is
    /// cancelled through its key).
    pub no_timeout: bool,
}

/// Forwards to a sink and remembers whether anything reached it (a request may only be
/// sent again on a fresh session when nothing did).
struct Tracked<'a> {
    inner: &'a mut dyn RowSink,
    touched: bool,
}

impl RowSink for Tracked<'_> {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.touched = true;
        self.inner.result_start(cols);
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.touched = true;
        self.inner.rows(batch);
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.inner.result_end(summary);
    }
    fn progress(&mut self, p: dnt_core::driver::ExecProgress) {
        self.inner.progress(p);
    }
    fn server_notices(&mut self, notices: dnt_core::driver::ServerNotices) {
        self.inner.server_notices(notices);
    }
}

impl SessionManager {
    /// Runs ONE `SELECT` built by the backend on a data session for `db` of connection
    /// `id`, under request `key` (see the module docs), and returns its first result,
    /// bounded (see [`BoundedPage`]). The statement timeout is the connection's query
    /// timeout, else [`DATA_READ_TIMEOUT`].
    pub async fn data_read(
        &self,
        id: Uuid,
        db: Option<&str>,
        key: &str,
        sql: &str,
        max_rows: usize,
    ) -> Result<BoundedPage, DbError> {
        self.data_read_params(id, db, key, sql, Vec::new(), max_rows)
            .await
    }

    /// [`Self::data_read`] with bound values (the data view's filter).
    pub async fn data_read_params(
        &self,
        id: Uuid,
        db: Option<&str>,
        key: &str,
        sql: &str,
        params: Vec<String>,
        max_rows: usize,
    ) -> Result<BoundedPage, DbError> {
        let request = DataRequest {
            sql: sql.to_string(),
            params,
            max_rows,
            ..DataRequest::default()
        };
        self.data_read_request(id, db, key, &request).await
    }

    /// [`Self::data_read`] of any [`DataRequest`] (e.g. a raw `WHERE` read).
    pub async fn data_read_request(
        &self,
        id: Uuid,
        db: Option<&str>,
        key: &str,
        request: &DataRequest,
    ) -> Result<BoundedPage, DbError> {
        let mut page = BoundedPage::new(self.data_cell_cap, self.data_page_cap);
        self.data_exec(id, db, key, request, &mut page).await?;
        Ok(page)
    }

    /// Runs `request` on a data session for `db` of connection `id`, under request
    /// `key` (see the module docs: supersede, cancel, pooling), streaming every result
    /// into `sink` unbounded (export). The statement timeout is none with
    /// `no_timeout`, else the connection's query timeout, else [`DATA_READ_TIMEOUT`].
    pub async fn data_exec(
        &self,
        id: Uuid,
        db: Option<&str>,
        key: &str,
        request: &DataRequest,
        sink: &mut dyn RowSink,
    ) -> Result<(), DbError> {
        let rt = self.runtime(id)?;
        let data = &rt.data;
        let serial = data.serial.fetch_add(1, Ordering::SeqCst) + 1;
        // Ours from now on: the previous request of this key is cancelled at once.
        data.begin(key, serial);
        let database = db.map(str::to_string);
        let timeout = match (request.no_timeout, rt.cfg.options.query_timeout_s) {
            (true, _) => None,
            (false, 0) => Some(self.data_timeout),
            (false, s) => Some(Duration::from_secs(s.into())),
        };
        let mut sink = Tracked {
            inner: sink,
            touched: false,
        };
        let mut pooled = take_idle(&rt, &database);
        loop {
            let from_pool = pooled.is_some();
            let (session, tunnel_gen) = match pooled.take() {
                Some(idle) => (idle.session, idle.tunnel_gen),
                None => match rt.open_query_session(database.clone()).await {
                    Ok(opened) => opened,
                    Err(e) => {
                        data.finish(key, serial);
                        return Err(e);
                    }
                },
            };
            let reusable = |rt: &ConnectionRuntime| {
                !rt.closed.load(Ordering::SeqCst)
                    && !rt.tunnel_dead()
                    && tunnel_gen == rt.tunnel_gen.load(Ordering::SeqCst)
            };
            // A fresh stop signal per attempt: a raised one stays raised.
            let stop = StopSignal::new();
            if !data.start(key, serial, session.as_ref(), &stop) {
                // Cancelled (or superseded) while the session was being taken or opened:
                // nothing ran on it, so it may serve the next request.
                if reusable(&rt) {
                    give_back(
                        &rt,
                        database,
                        Idle {
                            session,
                            tunnel_gen,
                        },
                    );
                } else {
                    tokio::spawn(close_quietly(session));
                }
                return Err(DbError::cancelled());
            }
            let opts = ExecOptions {
                max_rows: request.max_rows,
                timeout,
                cancel_grace: DATA_CANCEL_GRACE,
                stop,
                params: request.params.clone(),
                full_bytes: request.full_bytes,
                ..ExecOptions::default()
            };
            let outcome = run_request(session.as_ref(), request, &opts, &mut sink).await;
            // A pooled session that went stale while idle (server restart, idle timeout):
            // one more attempt on a fresh session, unless the request was cancelled or
            // something already reached the sink.
            if from_pool && !sink.touched && outcome.as_ref().is_err_and(is_lost) {
                tokio::spawn(close_quietly(session));
                continue;
            }
            // Only a session whose request was never cancelled goes back to the pool: a
            // cancel sent for it may still land on whatever runs there next. A raw
            // `WHERE` read never does either: the user's condition may have left session
            // state that its ROLLBACK does not undo (a MySQL `GET_LOCK`, a PostgreSQL
            // session advisory lock, a sequence's `currval`).
            let current = data.finish(key, serial);
            if outcome.is_ok()
                && current
                && request.raw.is_none()
                && reusable(&rt)
                && !session.in_transaction()
            {
                give_back(
                    &rt,
                    database,
                    Idle {
                        session,
                        tunnel_gen,
                    },
                );
            } else {
                tokio::spawn(close_quietly(session));
            }
            return outcome;
        }
    }

    /// Cancels the data request running under `key` on connection `id`; whether one was
    /// running. Its call then fails with `Cancelled`.
    pub fn data_cancel(&self, id: Uuid, key: &str) -> bool {
        self.runtime_opt(id).is_some_and(|rt| rt.data.cancel(key))
    }

    /// A data session of `db` for one exclusive use outside the request keys (an edit
    /// preview or apply): an idle pooled one, else a fresh one. Nothing is retried here;
    /// give it back with [`Self::return_data_session`].
    pub(crate) async fn take_data_session(
        &self,
        rt: &ConnectionRuntime,
        db: Option<&str>,
    ) -> Result<TakenSession, DbError> {
        let database = db.map(str::to_string);
        let (session, tunnel_gen, pooled) = match take_idle(rt, &database) {
            Some(idle) => (idle.session, idle.tunnel_gen, true),
            None => {
                let (session, gen) = rt.open_query_session(database.clone()).await?;
                (session, gen, false)
            }
        };
        Ok(TakenSession {
            session,
            tunnel_gen,
            database,
            pooled,
        })
    }

    /// Gives a taken session back to the pool when `clean` (it ran to the end, nothing
    /// failed on it), its transaction is closed and its connection still current;
    /// otherwise closes it.
    pub(crate) fn return_data_session(
        &self,
        rt: &ConnectionRuntime,
        taken: TakenSession,
        clean: bool,
    ) {
        let reusable = clean
            && !taken.session.in_transaction()
            && !rt.closed.load(Ordering::SeqCst)
            && !rt.tunnel_dead()
            && taken.tunnel_gen == rt.tunnel_gen.load(Ordering::SeqCst);
        if reusable {
            give_back(
                rt,
                taken.database,
                Idle {
                    session: taken.session,
                    tunnel_gen: taken.tunnel_gen,
                },
            );
        } else {
            tokio::spawn(close_quietly(taken.session));
        }
    }

    /// Overrides how long closing a connection waits for a running edit apply before it
    /// cancels it (default `SLOT_CLOSE_WAIT`; tests).
    pub fn with_apply_close_wait(mut self, wait: Duration) -> Self {
        self.apply_close_wait = wait;
        self
    }

    /// Overrides [`DATA_READ_TIMEOUT`] and the page bounds (tests).
    pub fn with_data_limits(mut self, timeout: Duration, cell_cap: usize, page_cap: usize) -> Self {
        self.data_timeout = timeout;
        self.data_cell_cap = cell_cap;
        self.data_page_cap = page_cap;
        self
    }
}

/// Keeps the first cell of a result (`SHOW …`); drops everything else.
#[derive(Default)]
pub(crate) struct FirstCell(pub Option<Value>);

impl RowSink for FirstCell {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.0.is_none() {
            self.0 = batch.into_iter().next().and_then(|r| r.into_iter().next());
        }
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Runs `request` on `session` (see [`DataRequest::raw`]).
async fn run_request(
    session: &dyn Session,
    request: &DataRequest,
    opts: &ExecOptions,
    sink: &mut dyn RowSink,
) -> Result<(), DbError> {
    let Some(raw) = &request.raw else {
        return session
            .execute_one(&request.sql, StatementKind::Read, opts, sink)
            .await
            .map(|_| ());
    };
    let dialect = session.dialect();
    // Checked and built before anything is sent.
    let sql = (raw.0)(dialect)?;
    let aux = ExecOptions {
        max_rows: 1,
        params: Vec::new(),
        ..opts.clone()
    };
    let begin = match dialect {
        Dialect::Postgres => {
            // The raw text was lexed with standard strings (`'\'` is a whole string).
            let mut scs = FirstCell::default();
            session
                .execute_one(
                    "SHOW standard_conforming_strings",
                    StatementKind::Read,
                    &aux,
                    &mut scs,
                )
                .await?;
            match scs.0 {
                Some(Value::Text(s)) if s.eq_ignore_ascii_case("on") => {}
                _ => {
                    return Err(config_error(
                        "filter",
                        "the server has standard_conforming_strings off: a raw WHERE filter \
                         cannot be checked safely on this connection",
                    ))
                }
            }
            "BEGIN READ ONLY"
        }
        Dialect::MySql(_) => "START TRANSACTION READ ONLY",
    };
    session
        .execute_one(
            begin,
            StatementKind::Transaction(TxControl::Begin),
            &aux,
            &mut FirstCell::default(),
        )
        .await?;
    let read = session
        .execute_one(&sql, StatementKind::Read, opts, sink)
        .await;
    // Always rolled back: nothing it did may stay. A stopped request's session is closed
    // instead (it is never pooled), which ends the transaction as well.
    let rollback = if opts.stop.is_stopped() {
        Ok(())
    } else {
        session
            .execute_one(
                "ROLLBACK",
                StatementKind::Transaction(TxControl::Rollback),
                &aux,
                &mut FirstCell::default(),
            )
            .await
            .map(|_| ())
    };
    read?;
    rollback
}

/// An idle session of `database` that is still usable (others are closed).
fn take_idle(rt: &ConnectionRuntime, database: &Option<String>) -> Option<Idle> {
    let current = rt.tunnel_gen.load(Ordering::SeqCst);
    let dead = rt.tunnel_dead();
    let mut idle = lock(&rt.data.idle);
    let pool = idle.get_mut(database)?;
    while let Some(i) = pool.pop() {
        if !dead && i.tunnel_gen == current {
            return Some(i);
        }
        tokio::spawn(close_quietly(i.session));
    }
    None
}

fn give_back(rt: &ConnectionRuntime, database: Option<String>, session: Idle) {
    let mut idle = lock(&rt.data.idle);
    let pool = idle.entry(database).or_default();
    if pool.len() < IDLE_PER_DATABASE {
        pool.push(session);
    } else {
        drop(idle);
        tokio::spawn(close_quietly(session.session));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> Value {
        Value::Text("é".repeat(n))
    }

    #[test]
    fn long_cells_are_cut_on_a_char_boundary() {
        let mut page = BoundedPage::new(5, 1000);
        page.result_start(&[]);
        // "é" is 2 bytes: 5 bytes keeps 2 chars.
        page.rows(vec![vec![
            text(10),
            Value::Json("{\"a\":1}".into()),
            Value::Int(1),
        ]]);
        assert_eq!(page.rows[0][0], Value::Text("éé".into()));
        assert_eq!(page.rows[0][1], Value::Json("{\"a\":".into()));
        assert_eq!(page.truncated_cells, 2);
        assert!(!page.capped);
    }

    #[test]
    fn rows_stop_at_the_page_cap_but_the_first_is_kept() {
        let mut page = BoundedPage::new(100, 25);
        page.result_start(&[]);
        page.rows(vec![
            vec![Value::Text("x".repeat(30))],
            vec![Value::Text("y".repeat(5))],
        ]);
        assert_eq!(page.rows.len(), 1);
        assert!(page.capped);
        page.rows(vec![vec![Value::Text("z".into())]]);
        assert_eq!(page.rows.len(), 1);

        let mut page = BoundedPage::new(100, 25);
        page.result_start(&[]);
        page.rows(vec![
            vec![Value::Text("a".repeat(10))],
            vec![Value::Text("b".repeat(10))],
            vec![Value::Text("c".repeat(10))],
        ]);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.bytes, 20);
        assert!(page.capped);
    }

    #[test]
    fn bounded_page_records_truncated_cells() {
        let mut page = BoundedPage::new(4, 1000);
        page.result_start(&[]);
        page.rows(vec![
            vec![
                Value::Int(1),
                Value::Text("short".into()),
                Value::Text("ab".into()),
            ],
            vec![
                Value::Int(2),
                Value::Text("ok".into()),
                Value::Json("[1,2,3]".into()),
            ],
        ]);
        page.rows(vec![vec![
            Value::Text("longer".into()),
            Value::Null,
            Value::Array(vec![Value::Text("abcdef".into())]),
        ]]);
        assert_eq!(page.truncated, [(0, 1), (1, 2), (2, 0), (2, 2)]);
        assert_eq!(page.truncated_cells, 4);

        // A row left out by the page cap records nothing.
        let mut page = BoundedPage::new(4, 5);
        page.result_start(&[]);
        page.rows(vec![
            vec![Value::Text("abcdefgh".into())],
            vec![Value::Text("ijklmnop".into())],
        ]);
        assert_eq!(page.rows.len(), 1);
        assert!(page.capped);
        assert_eq!(page.truncated, [(0, 0)]);
        assert_eq!(page.truncated_cells, 1);
    }

    #[test]
    fn later_results_are_ignored() {
        let mut page = BoundedPage::new(100, 100);
        page.result_start(&[]);
        page.rows(vec![vec![Value::Int(1)]]);
        page.result_start(&[]);
        page.rows(vec![vec![Value::Int(2)]]);
        assert_eq!(page.rows, [vec![Value::Int(1)]]);
    }
}
