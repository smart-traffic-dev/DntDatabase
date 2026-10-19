//! Cancelling a running statement: `KILL QUERY <id>` sent over a temporary connection
//! (same `Opts`, so through the same SSH tunnel port when tunneled).

use crate::map_mysql_error;
use async_trait::async_trait;
use dnt_core::driver::Canceller;
use dnt_core::error::{ConnectStage, DbError};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// Set by [`MySqlCanceller::cancel`]; `execute` clears it when it starts and reports
    /// `Cancelled` once it sees it. Needed because a killed `SELECT SLEEP(n)` returns a
    /// normal row on MySQL instead of error 1317.
    pub(crate) requested: AtomicBool,
}

pub(crate) struct MySqlCanceller {
    pub(crate) opts: Opts,
    pub(crate) state: Arc<CancelState>,
}

#[async_trait]
impl Canceller for MySqlCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        self.state.requested.store(true, Ordering::SeqCst);
        kill_query(&self.opts, self.state.connection_id.load(Ordering::SeqCst)).await
    }
}

/// Runs `KILL QUERY <id>` on a temporary connection, then closes it.
pub(crate) async fn kill_query(opts: &Opts, id: u32) -> Result<(), DbError> {
    let work = async {
        let mut conn = Conn::new(opts.clone())
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::ConnectDb))?;
        let res = conn
            .query_drop(format!("KILL QUERY {id}"))
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::Execute));
        let _ = conn.disconnect().await;
        res
    };
    tokio::time::timeout(CANCEL_TIMEOUT, work)
        .await
        .map_err(|_| DbError::Timeout {
            stage: ConnectStage::Execute,
        })?
}
