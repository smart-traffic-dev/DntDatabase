//! Cancelling a running statement with PostgreSQL's cancel request
//! (`CancelToken::cancel_query_raw`), sent over a new socket to the same address — through
//! the same SSH tunnel port when tunnelled — with the session's TLS settings.
//!
//! Cancels are scoped to one `execute` call by a generation number: `cancel()` targets the
//! call running when it is invoked, does nothing when the session is idle, and is skipped if
//! that call finished before the request could be sent.
//!
//! # When the signal lands
//!
//! The request is asynchronous: the server process that reads it signals the backend
//! (`SIGINT`) and only then exits, closing the request's connection. tokio-postgres
//! returns as soon as the request is written, so the signal could still be on its way when
//! the next statement is sent and cancel that one instead (a spurious `57014`, which also
//! aborts an open transaction block). [`send_cancel`] therefore waits for the server to
//! close the connection. Once it has, the signal is already pending on the backend, which
//! handles it before it can read another command. A cancel handled while the backend
//! waits for a command is discarded ("If we are reading a command from the client, just
//! ignore the cancel request", `ProcessInterrupts`). So after a confirmed request, the
//! signal either stopped the statement it was aimed at or is dropped harmlessly; it never
//! reaches a statement sent later. A reset counts as a close too: only the server side
//! closing produces one, and that happens after the signal (or when the request was
//! rejected, with no signal at all). This assumes the address reaches PostgreSQL itself,
//! directly or through a plain TCP forwarder (SSH tunnel, Docker port proxy); a pooler
//! that answers cancel requests on its own may close before the server acted.
//!
//! A request that was written but never confirmed (the close did not come within
//! [`CANCEL_TIMEOUT`], or the socket failed) may still signal the backend at any time:
//! [`CancelFailed::maybe_sent`]. A user's Cancel click that lands while the call is
//! finishing keeps a window of one network round trip that the client cannot close.

use crate::tls::Tls;
use crate::{map_pg_error, ConnectParams};
use async_trait::async_trait;
use dnt_core::driver::Canceller;
use dnt_core::error::{ConnectStage, DbError};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_postgres::config::Host;
use tokio_postgres::tls::MakeTlsConnect;
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
        self.mark_cancelled(gen);
        Some(gen)
    }

    /// Records a cancel for generation `gen`. `fetch_max`: a cancel that read an older
    /// generation before a newer call began must never lower the mark and so undo the
    /// newer call's cancel.
    fn mark_cancelled(&self, gen: u64) {
        self.cancelled.fetch_max(gen, Ordering::SeqCst);
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
    pub(crate) target: CancelTarget,
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
        send_cancel(&token, &self.target).await.map_err(|f| f.error)
    }
}

/// Where cancel requests go: the session's address (the SSH tunnel's local port when
/// tunnelled) and TLS settings. Holds no credentials.
#[derive(Clone)]
pub(crate) struct CancelTarget {
    host: String,
    port: u16,
    tls: Tls,
}

impl CancelTarget {
    pub(crate) fn new(params: &ConnectParams) -> Self {
        #[allow(unreachable_patterns)] // `Host::Unix` exists on Unix only.
        let host = match params.config.get_hosts().first() {
            Some(Host::Tcp(h)) => h.clone(),
            _ => String::new(),
        };
        CancelTarget {
            host,
            port: params.config.get_ports().first().copied().unwrap_or(5432),
            tls: params.tls.clone(),
        }
    }
}

/// A cancel request that was not confirmed (see the module docs).
#[derive(Debug)]
pub(crate) struct CancelFailed {
    pub(crate) error: DbError,
    /// The request may have reached the server, so the backend may still be signalled
    /// at any time. False when no connection to the server was ever made.
    pub(crate) maybe_sent: bool,
}

/// Sends one cancel request for `token`'s backend and waits (within [`CANCEL_TIMEOUT`])
/// until the server closed the request's connection: `Ok` means any signal it causes is
/// already pending on the backend (see the module docs).
pub(crate) async fn send_cancel(
    token: &CancelToken,
    target: &CancelTarget,
) -> Result<(), CancelFailed> {
    let mut connected = false;
    let attempt = async {
        let tls = MakeTlsConnect::<AwaitClose<TcpStream>>::make_tls_connect(
            &mut target.tls.connector.clone(),
            &target.host,
        )
        .map_err(|e| DbError::Tls {
            message: e.to_string(),
        })?;
        let socket = TcpStream::connect((target.host.as_str(), target.port))
            .await
            .map_err(|e| DbError::Connect {
                message: e.to_string(),
            })?;
        connected = true;
        token
            .cancel_query_raw(AwaitClose::new(socket), tls)
            .await
            .map_err(|e| map_pg_error(e, ConnectStage::Execute))
    };
    let res = match tokio::time::timeout(CANCEL_TIMEOUT, attempt).await {
        Ok(res) => res,
        Err(_) => Err(DbError::timeout(ConnectStage::Execute)),
    };
    res.map_err(|error| CancelFailed {
        error,
        maybe_sent: connected,
    })
}

/// A socket whose shutdown waits for the peer to close the connection, reading (and
/// discarding) until end of stream. tokio-postgres shuts the stream down right after
/// writing the request (through the TLS layer, which passes the shutdown on), so
/// `cancel_query_raw` returns only once the server is done.
///
/// Our side is never half-closed: a forwarder in between (Docker's port proxy, an SSH
/// tunnel, a pooler) may answer a FIN by closing both directions at once, an end of
/// stream that would prove nothing about the server. Only the server, having read the
/// whole request, ends the connection; the socket is closed when dropped.
struct AwaitClose<S> {
    inner: S,
}

impl<S> AwaitClose<S> {
    fn new(inner: S) -> Self {
        AwaitClose { inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for AwaitClose<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for AwaitClose<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self.inner).poll_flush(cx))?;
        let mut scratch = [0u8; 256];
        loop {
            let mut buf = ReadBuf::new(&mut scratch);
            match ready!(Pin::new(&mut self.inner).poll_read(cx, &mut buf)) {
                Ok(()) if buf.filled().is_empty() => return Poll::Ready(Ok(())),
                Ok(()) => {}
                Err(e) if peer_closed(&e) => return Poll::Ready(Ok(())),
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

/// The server closed the connection with data of ours still unread (TLS `close_notify`).
fn peer_closed(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
    )
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

    #[test]
    fn a_stale_cancel_never_lowers_the_cancelled_generation() {
        let gens = Arc::new(Generations::default());
        drop(gens.begin());
        let second = gens.begin();
        assert_eq!(gens.request(), Some(2));
        // A click that read generation 1 before it ended lands late.
        gens.mark_cancelled(1);
        assert!(second.cancelled(), "the newer call lost its cancel");
    }
}
