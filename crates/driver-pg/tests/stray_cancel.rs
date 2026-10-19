//! A cancel request sent at a statement's timeout reaches the backend only later (the
//! postmaster signals it after reading the request). These tests hold the request back
//! in a local proxy, so the late signal is deterministic rather than a timing accident.

use dnt_core::driver::{ExecOptions, ExecSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_pg::{PgDriver, PgSession};
use dnt_testkit::{require_db, Target, FIXTURE_DB};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// `CancelRequest`: length 16, request code 80877102.
const CANCEL_HEADER: [u8; 8] = [0, 0, 0, 16, 0x04, 0xd2, 0x16, 0x2e];

#[derive(Default)]
struct Collect {
    results: Vec<Vec<Vec<Value>>>,
}

impl RowSink for Collect {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {
        self.results.push(Vec::new());
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.results.last_mut().unwrap().extend(batch);
    }
    fn result_end(&mut self, _affected: Option<u64>, _elapsed: Duration) {}
}

async fn run_with(
    s: &PgSession,
    sql: &str,
    opts: ExecOptions,
) -> Result<(ExecSummary, Collect), DbError> {
    let mut sink = Collect::default();
    let summary = s.execute(sql, opts, &mut sink).await?;
    Ok((summary, sink))
}

async fn run(s: &PgSession, sql: &str) -> Result<(ExecSummary, Collect), DbError> {
    run_with(s, sql, ExecOptions::default()).await
}

async fn scalar(s: &PgSession, sql: &str) -> Value {
    let (_, c) = run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    c.results.last().unwrap()[0][0].clone()
}

/// A cancel request the proxy holds back until `release` fires.
struct HeldCancel {
    release: oneshot::Sender<()>,
}

/// A TCP proxy to `upstream` on 127.0.0.1 that forwards every connection, except that
/// the first cancel request is reported on the returned channel and forwarded only once
/// released. Later ones (a statement that overran its own timeout on a loaded machine)
/// go straight through, so they show up as what they are rather than as a hang.
async fn proxy(upstream: u16) -> (u16, mpsc::UnboundedReceiver<HeldCancel>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (held_tx, held_rx) = mpsc::unbounded_channel();
    let held_one = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let held_tx = held_tx.clone();
            let held_one = held_one.clone();
            tokio::spawn(async move {
                let mut head = [0u8; 8];
                if client.read_exact(&mut head).await.is_err() {
                    return;
                }
                if head == CANCEL_HEADER && !held_one.swap(true, Ordering::SeqCst) {
                    let (release, wait) = oneshot::channel();
                    let _ = held_tx.send(HeldCancel { release });
                    if wait.await.is_err() {
                        return;
                    }
                }
                let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
                    return;
                };
                if server.write_all(&head).await.is_err() {
                    return;
                }
                // Half-closes are forwarded, so the client sees the server's EOF.
                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
            });
        }
    });
    (port, held_rx)
}

/// What the backend with `pid` is doing: (`state`, `query`).
async fn activity(admin: &PgSession, pid: &Value) -> (String, String) {
    let Value::Int(pid) = pid else {
        panic!("not a pid: {pid:?}")
    };
    let (_, c) = run(
        admin,
        &format!("SELECT state, query FROM pg_stat_activity WHERE pid = {pid}"),
    )
    .await
    .unwrap();
    let row = &c.results[0][0];
    match (&row[0], &row[1]) {
        (Value::Text(state), Value::Text(query)) => (state.clone(), query.clone()),
        other => panic!("{other:?}"),
    }
}

/// A statement that outlives its timeout but completes during the grace period (the
/// cancel request is still on its way) keeps its result, and the late signal never
/// reaches the next statement of the script: the open transaction survives and the
/// whole script succeeds.
///
/// The proxy holds the cancel request until the backend has either finished the first
/// statement and gone idle (the request is then harmless) or started the second one (it
/// would then be cancelled), so the outcome does not depend on timing.
#[tokio::test]
async fn late_cancel_after_grace_success_spares_the_next_statement() {
    require_db!();
    for t in Target::pg() {
        let (port, mut held) = proxy(t.host_port()).await;
        let mut cfg = t.resolved(Some(FIXTURE_DB));
        cfg.port = port;
        let s = PgDriver
            .connect_session(&cfg)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let admin = PgDriver
            .connect_session(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let pid = scalar(&s, "SELECT pg_backend_pid()").await;
        run(&s, "CREATE TEMP TABLE m1_late (v int)").await.unwrap();
        run(&s, "BEGIN; INSERT INTO m1_late VALUES (1)")
            .await
            .unwrap();
        assert!(s.in_transaction(), "{t:?}");

        // Statement 1 outlives the 4 s timeout by 0.5 s. Statement 2 stays far within it,
        // even on a loaded machine, yet runs long enough for a released signal to land
        // on it.
        let script = "SELECT pg_sleep(4.5), 'm1_first'; \
                      INSERT INTO m1_late SELECT 2 FROM pg_sleep(0.5) AS m1_second";
        let opts = ExecOptions {
            timeout: Some(Duration::from_secs(4)),
            ..ExecOptions::default()
        };
        let (res, ()) = tokio::join!(run_with(&s, script, opts), async {
            let cancel = tokio::time::timeout(Duration::from_secs(10), held.recv())
                .await
                .unwrap_or_else(|_| panic!("{t:?}: no cancel request after the timeout"))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let (state, query) = activity(&admin, &pid).await;
                let first_done = state == "idle in transaction" && query.contains("m1_first");
                let second_running = state == "active" && query.contains("m1_second");
                if first_done || second_running {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{t:?}: backend stuck in {state:?}: {query}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = cancel.release.send(());
        });
        let (summary, c) =
            res.unwrap_or_else(|e| panic!("{t:?}: a late cancel hit the script: {e:?}"));
        assert_eq!(summary.results.len(), 2, "{t:?}");
        assert_eq!(c.results[0][0][1], Value::Text("m1_first".into()), "{t:?}");
        assert!(s.in_transaction(), "{t:?}: the transaction was lost");
        run(&s, "COMMIT")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        assert!(!s.in_transaction(), "{t:?}");
        assert_eq!(
            scalar(
                &s,
                "SELECT string_agg(v::text, ',' ORDER BY v) FROM m1_late"
            )
            .await,
            Value::Text("1,2".into()),
            "{t:?}"
        );
        // Nothing stray is left for the next call either.
        run(&s, "SELECT pg_sleep(0.3)")
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
    }
}

/// A cancel request the server never confirms (the proxy swallows it) may still signal
/// the backend at any time: once the statement ends during the grace period, its
/// connection is given up rather than risk the signal hitting the next statement. The
/// rest of the script never runs, the lost transaction is reported, and the session works
/// again on a new backend.
#[tokio::test]
async fn unconfirmed_cancel_gives_up_the_connection() {
    require_db!();
    for t in Target::pg() {
        let (port, mut held) = proxy(t.host_port()).await;
        let mut cfg = t.resolved(Some(FIXTURE_DB));
        cfg.port = port;
        let s = PgDriver
            .connect_session(&cfg)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {e}"));
        let pid = scalar(&s, "SELECT pg_backend_pid()").await;
        run(&s, "BEGIN").await.unwrap();
        let script = "SELECT pg_sleep(1.5), 'm1_u1'; SELECT 'm1_u2'";
        let opts = ExecOptions {
            timeout: Some(Duration::from_secs(1)),
            ..ExecOptions::default()
        };
        let started = Instant::now();
        let (res, swallowed) = tokio::join!(run_with(&s, script, opts), held.recv());
        match &res {
            Err(DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: false,
                message,
            }) => {
                assert!(
                    message.contains("could not be confirmed"),
                    "{t:?}: {message}"
                );
                assert!(message.contains("rolled back"), "{t:?}: {message}");
            }
            other => panic!("{t:?}: {:?}", other.as_ref().map(|(s, _)| s)),
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{t:?}");
        drop(swallowed);
        assert!(!s.in_transaction(), "{t:?}");
        assert_ne!(scalar(&s, "SELECT pg_backend_pid()").await, pid, "{t:?}");
    }
}
