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
//! - closing the connection cancels the running requests and closes the idle sessions.

use crate::session_manager::{close_quietly, lock, ConnectionRuntime, SessionManager};
use dnt_core::driver::{CancelHandle, ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::DbError;
use dnt_core::sql::StatementKind;
use dnt_core::value::{ColumnMeta, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;

/// Statement timeout of a data request when the connection sets no query timeout.
pub const DATA_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace after a timeout / cancel before the driver gives the connection up.
const DATA_CANCEL_GRACE: Duration = Duration::from_secs(5);
/// Idle data sessions kept per database.
const IDLE_PER_DATABASE: usize = 2;

/// A text / JSON cell longer than this is cut (on a char boundary).
pub const MAX_CELL_BYTES: usize = 64 * 1024;
/// A page stops taking rows once its cells add up to this.
pub const MAX_PAGE_BYTES: usize = 32 * 1024 * 1024;

struct Idle {
    session: Box<dyn Session>,
    tunnel_gen: u64,
}

struct Running {
    serial: u64,
    cancel: CancelHandle,
    stop: StopSignal,
}

/// The data sessions of one connection runtime.
#[derive(Default)]
pub(crate) struct DataSessions {
    idle: Mutex<HashMap<Option<String>, Vec<Idle>>>,
    running: Mutex<HashMap<String, Running>>,
    serial: AtomicU64,
}

fn cancel_running(r: Running) {
    r.stop.stop();
    tokio::spawn(async move {
        let _ = r.cancel.cancel().await;
    });
}

impl DataSessions {
    /// Cancels every running request and closes the idle sessions (connection close).
    pub(crate) async fn shutdown(&self) {
        let running: Vec<Running> = lock(&self.running).drain().map(|(_, r)| r).collect();
        for r in running {
            cancel_running(r);
        }
        let idle: Vec<Idle> = lock(&self.idle).drain().flat_map(|(_, v)| v).collect();
        futures_util::future::join_all(idle.into_iter().map(|i| close_quietly(i.session))).await;
    }

    /// Cancels the request running under `key`; whether there was one.
    fn cancel(&self, key: &str) -> bool {
        match lock(&self.running).remove(key) {
            Some(r) => {
                cancel_running(r);
                true
            }
            None => false,
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
    /// Cells that were cut.
    pub truncated_cells: u32,
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
            capped: false,
            results: 0,
        }
    }
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
            for cell in &mut row {
                if truncate_cell(cell, self.cell_cap) {
                    self.truncated_cells = self.truncated_cells.saturating_add(1);
                }
                size += cell_bytes(cell);
            }
            // The first row is always kept, so a page never comes back empty for size.
            if !self.rows.is_empty() && self.bytes + size > self.page_cap {
                self.capped = true;
                return;
            }
            self.bytes += size;
            self.rows.push(row);
        }
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
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
        let rt = self.runtime(id)?;
        let data = &rt.data;
        let serial = data.serial.fetch_add(1, Ordering::SeqCst) + 1;
        let stop = StopSignal::new();
        // Supersede the previous request of this key at once (even before ours starts).
        data.cancel(key);
        let database = db.map(str::to_string);
        let (session, tunnel_gen) = match take_idle(&rt, &database) {
            Some(idle) => (idle.session, idle.tunnel_gen),
            None => rt.open_query_session(database.clone()).await?,
        };
        {
            let mut running = lock(&data.running);
            if let Some(previous) = running.insert(
                key.to_string(),
                Running {
                    serial,
                    cancel: session.cancel_handle(),
                    stop: stop.clone(),
                },
            ) {
                // A newer request of this key started meanwhile only if it has a higher
                // serial; ours is newest otherwise.
                if previous.serial > serial {
                    let ours = running.insert(key.to_string(), previous);
                    drop(running);
                    if let Some(ours) = ours {
                        ours.stop.stop();
                    }
                } else {
                    cancel_running(previous);
                }
            }
        }
        let timeout = match rt.cfg.options.query_timeout_s {
            0 => self.data_timeout,
            s => Duration::from_secs(s.into()),
        };
        let opts = ExecOptions {
            max_rows,
            timeout: Some(timeout),
            cancel_grace: DATA_CANCEL_GRACE,
            stop,
            ..ExecOptions::default()
        };
        let mut page = BoundedPage::new(self.data_cell_cap, self.data_page_cap);
        let outcome = session
            .execute_one(sql, StatementKind::Read, &opts, &mut page)
            .await;
        {
            let mut running = lock(&data.running);
            if running.get(key).is_some_and(|r| r.serial == serial) {
                running.remove(key);
            }
        }
        let reusable = outcome.is_ok()
            && !rt.closed.load(Ordering::SeqCst)
            && !rt.tunnel_dead()
            && tunnel_gen == rt.tunnel_gen.load(Ordering::SeqCst);
        if reusable {
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
        outcome.map(|_| page)
    }

    /// Cancels the data request running under `key` on connection `id`; whether one was
    /// running. Its call then fails with `Cancelled`.
    pub fn data_cancel(&self, id: Uuid, key: &str) -> bool {
        self.runtime_opt(id).is_some_and(|rt| rt.data.cancel(key))
    }

    /// Overrides [`DATA_READ_TIMEOUT`] and the page bounds (tests).
    pub fn with_data_limits(mut self, timeout: Duration, cell_cap: usize, page_cap: usize) -> Self {
        self.data_timeout = timeout;
        self.data_cell_cap = cell_cap;
        self.data_page_cap = page_cap;
        self
    }
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
    fn later_results_are_ignored() {
        let mut page = BoundedPage::new(100, 100);
        page.result_start(&[]);
        page.rows(vec![vec![Value::Int(1)]]);
        page.result_start(&[]);
        page.rows(vec![vec![Value::Int(2)]]);
        assert_eq!(page.rows, [vec![Value::Int(1)]]);
    }
}
