//! Cancelling a running statement: `KILL QUERY <id>` sent over a temporary connection
//! (same `Opts`, so through the same SSH tunnel port when tunneled).
//!
//! Cancels are scoped to one `execute` call by a generation number: `cancel()` targets
//! the call running when it is invoked, does nothing when the session is idle, and does
//! not send the `KILL` if that call has finished by the time the temporary connection is
//! open. A remaining window of one network round trip (the generation check vs. the
//! server running `KILL QUERY`) cannot be closed from the client side.

use crate::map_mysql_error;
use async_trait::async_trait;
use dnt_core::driver::Canceller;
use dnt_core::error::{ConnectStage, DbError};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Bound on opening the temporary connection and running `KILL QUERY`.
pub(crate) const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// State shared between a session and its cancellers. Never touches the session's
/// connection mutex, since `cancel` is called while `execute` holds it.
#[derive(Debug, Default)]
pub(crate) struct CancelState {
    /// Server thread id of the session's current physical connection (updated on reconnect).
    pub(crate) connection_id: AtomicU32,
    /// Last generation handed out to an `execute` call.
    last_gen: AtomicU64,
    /// Generation of the `execute` call holding the connection; 0 when idle.
    running: AtomicU64,
    /// Generation a cancel was requested for. Needed besides the `KILL` because a killed
    /// `SELECT SLEEP(n)` returns a normal row on MySQL instead of error 1317.
    cancelled: AtomicU64,
}

impl CancelState {
    /// Marks a new `execute` call as running (call with the connection lock held).
    /// The returned guard marks the session idle again when dropped.
    pub(crate) fn begin(self: &Arc<Self>) -> RunGuard {
        let gen = self.last_gen.fetch_add(1, Ordering::SeqCst) + 1;
        self.running.store(gen, Ordering::SeqCst);
        RunGuard {
            state: self.clone(),
            gen,
        }
    }

    /// Records a cancel for generation `gen`. Generations only grow, so `fetch_max`
    /// keeps a stale request (one that read an older `running` before a newer click
    /// stored its own) from lowering the value and un-cancelling the newer call.
    pub(crate) fn request_cancel(&self, gen: u64) {
        self.cancelled.fetch_max(gen, Ordering::SeqCst);
    }
}

/// One running `execute` call.
pub(crate) struct RunGuard {
    state: Arc<CancelState>,
    gen: u64,
}

impl RunGuard {
    /// True once a cancel targeted this call.
    pub(crate) fn cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst) == self.gen
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let _ =
            self.state
                .running
                .compare_exchange(self.gen, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

pub(crate) struct MySqlCanceller {
    pub(crate) opts: Opts,
    pub(crate) state: Arc<CancelState>,
}

#[async_trait]
impl Canceller for MySqlCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        let gen = self.state.running.load(Ordering::SeqCst);
        if gen == 0 {
            return Ok(());
        }
        self.state.request_cancel(gen);
        let state = &self.state;
        kill_query(&self.opts, || {
            (state.running.load(Ordering::SeqCst) == gen)
                .then(|| state.connection_id.load(Ordering::SeqCst))
        })
        .await
    }
}

/// Opens a temporary connection, asks `target` for the thread id to kill (`None`: the
/// target is gone, send nothing), runs `KILL QUERY <id>`, then closes the connection.
pub(crate) async fn kill_query(
    opts: &Opts,
    target: impl Fn() -> Option<u32> + Send + Sync,
) -> Result<(), DbError> {
    let work = async {
        let mut conn = Conn::new(opts.clone())
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))?;
        let res = match target() {
            Some(id) => conn
                .query_drop(format!("KILL QUERY {id}"))
                .await
                .map_err(|e| map_mysql_error(e, ConnectStage::Execute)),
            None => Ok(()),
        };
        let _ = conn.disconnect().await;
        res
    };
    tokio::time::timeout(CANCEL_TIMEOUT, work)
        .await
        .map_err(|_| DbError::timeout(ConnectStage::Execute))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_scope_cancel_requests() {
        let state = Arc::new(CancelState::default());
        let a = state.begin();
        assert_eq!(state.running.load(Ordering::SeqCst), 1);
        state.cancelled.store(1, Ordering::SeqCst);
        assert!(a.cancelled());
        drop(a);
        assert_eq!(state.running.load(Ordering::SeqCst), 0);
        // A request aimed at the finished call does not leak into the next one.
        let b = state.begin();
        assert!(!b.cancelled());
        assert_eq!(state.running.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_stale_cancel_never_lowers_the_cancelled_generation() {
        let state = Arc::new(CancelState::default());
        drop(state.begin());
        let b = state.begin();
        // A click for call 2, then a late write from a click that read call 1 earlier.
        state.request_cancel(2);
        state.request_cancel(1);
        assert!(b.cancelled());
        assert_eq!(state.cancelled.load(Ordering::SeqCst), 2);
        // A second click on the same call changes nothing.
        state.request_cancel(2);
        assert!(b.cancelled());
    }
}
