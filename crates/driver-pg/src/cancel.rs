//! Cancelling a running statement with PostgreSQL's cancel request
//! (`CancelToken::cancel_query`), sent over a new socket to the same address — through the
//! same SSH tunnel port when tunnelled — with the session's TLS settings.
//!
//! Cancels are scoped to one `execute` call by a generation number: `cancel()` targets the
//! call running when it is invoked, does nothing when the session is idle, and is skipped if
//! that call finished before the request could be sent. The server applies a cancel request
//! to whatever the backend runs when it arrives, so a window of one network round trip
//! remains that cannot be closed from the client side.

use crate::map_pg_error;
use crate::tls::Tls;
use async_trait::async_trait;
use dnt_core::driver::Canceller;
use dnt_core::error::{ConnectStage, DbError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_postgres::CancelToken;

/// Bound on sending a cancel request.
pub(crate) const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// State shared between a session and its cancellers. Never touches the session's
/// connection mutex, since `cancel` is called while `execute` holds it.
pub(crate) struct CancelState {
    /// Cancel key of the session's current physical connection (replaced on reconnect).
    token: Mutex<CancelToken>,
    gens: Arc<Generations>,
}

impl CancelState {
    pub(crate) fn new(token: CancelToken) -> Self {
        CancelState {
            token: Mutex::new(token),
            gens: Arc::default(),
        }
    }

    pub(crate) fn token(&self) -> CancelToken {
        self.token.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(crate) fn set_token(&self, token: CancelToken) {
        *self.token.lock().unwrap_or_else(|p| p.into_inner()) = token;
    }

    /// Marks a new `execute` call as running (call with the connection lock held).
    /// The returned guard marks the session idle again when dropped.
    pub(crate) fn begin(&self) -> RunGuard {
        self.gens.begin()
    }
}

/// Generation bookkeeping for `execute` calls.
#[derive(Debug, Default)]
struct Generations {
    /// Last generation handed out to an `execute` call.
    last: AtomicU64,
    /// Generation of the `execute` call holding the connection; 0 when idle.
    running: AtomicU64,
    /// Generation a cancel was requested for.
    cancelled: AtomicU64,
}

impl Generations {
    fn begin(self: &Arc<Self>) -> RunGuard {
        let gen = self.last.fetch_add(1, Ordering::SeqCst) + 1;
        self.running.store(gen, Ordering::SeqCst);
        RunGuard {
            gens: self.clone(),
            gen,
        }
    }

    /// Records a cancel for the running call; returns its generation (`None` when idle).
    fn request(&self) -> Option<u64> {
        let gen = self.running.load(Ordering::SeqCst);
        if gen == 0 {
            return None;
        }
        self.cancelled.store(gen, Ordering::SeqCst);
        Some(gen)
    }

    fn is_running(&self, gen: u64) -> bool {
        self.running.load(Ordering::SeqCst) == gen
    }
}

/// One running `execute` call.
pub(crate) struct RunGuard {
    gens: Arc<Generations>,
    gen: u64,
}

impl RunGuard {
    /// True once a cancel targeted this call.
    pub(crate) fn cancelled(&self) -> bool {
        self.gens.cancelled.load(Ordering::SeqCst) == self.gen
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let _ = self
            .gens
            .running
            .compare_exchange(self.gen, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

pub(crate) struct PgCanceller {
    pub(crate) tls: Tls,
    pub(crate) state: Arc<CancelState>,
}

#[async_trait]
impl Canceller for PgCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        let Some(gen) = self.state.gens.request() else {
            return Ok(());
        };
        let token = self.state.token();
        if !self.state.gens.is_running(gen) {
            return Ok(());
        }
        send_cancel(&token, &self.tls).await
    }
}

/// Sends one cancel request for `token`'s backend.
pub(crate) async fn send_cancel(token: &CancelToken, tls: &Tls) -> Result<(), DbError> {
    tokio::time::timeout(CANCEL_TIMEOUT, token.cancel_query(tls.connector.clone()))
        .await
        .map_err(|_| DbError::timeout(ConnectStage::Execute))?
        .map_err(|e| map_pg_error(e, ConnectStage::Execute))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_scope_cancel_requests() {
        let gens = Arc::new(Generations::default());
        assert_eq!(gens.request(), None, "idle: nothing to cancel");
        let a = gens.begin();
        assert!(gens.is_running(1));
        assert_eq!(gens.request(), Some(1));
        assert!(a.cancelled());
        drop(a);
        assert!(gens.is_running(0));
        // A request aimed at the finished call does not leak into the next one.
        let b = gens.begin();
        assert!(!b.cancelled());
        assert!(gens.is_running(2));
        // A guard of an older call never clears a newer one.
        drop(RunGuard {
            gens: gens.clone(),
            gen: 1,
        });
        assert!(gens.is_running(2));
    }
}
