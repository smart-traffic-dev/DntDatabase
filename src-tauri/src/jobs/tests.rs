//! JobManager tests with an in-memory repo and fake runners driven by `Notify`/channels.

use super::*;
use async_trait::async_trait;
use dnt_core::driver::{CancelHandle, Canceller};
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_store::db::open_in_memory;
use dnt_store::jobs::JobRecord;
use dnt_store::repo::ConnectionRepo;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Recorder(Mutex<Vec<JobEvent>>);

impl JobEvents for Recorder {
    fn emit(&self, event: &JobEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Recorder {
    fn of(&self, id: Uuid) -> Vec<JobEvent> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.job.id == id)
            .cloned()
            .collect()
    }
}

struct Fixture {
    mgr: JobManager,
    repo: Arc<ConnectionRepo>,
    events: Arc<Recorder>,
    limit: Arc<AtomicU32>,
    tmp: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let repo = Arc::new(ConnectionRepo::new(
        open_in_memory().unwrap(),
        Arc::new(MemorySecretStore::default()),
    ));
    let limit = Arc::new(AtomicU32::new(DEFAULT_JOBS_PER_CONNECTION));
    let reader = Arc::clone(&limit);
    let mgr =
        JobManager::new(Arc::clone(&repo)).with_limit_reader(move || reader.load(Ordering::SeqCst));
    let events = Arc::new(Recorder::default());
    mgr.set_events(events.clone());
    let tmp = tempfile::tempdir().unwrap();
    mgr.set_tmp_root(tmp.path().join("job-tmp"));
    Fixture {
        mgr,
        repo,
        events,
        limit,
        tmp,
    }
}

fn spec(conn: Option<Uuid>) -> JobSpec {
    JobSpec {
        kind: JobKind::Dump,
        connection_id: conn,
        title: "Dump shop".into(),
        params: Some(JobParams::Dump(serde_json::json!({ "database": "shop" }))),
        rerunnable: true,
    }
}

type RunFn = Box<dyn FnOnce(JobCtx) -> BoxFuture<'static, Result<JobDone, JobFailure>> + Send>;

struct FnRunner(RunFn);

#[async_trait]
impl JobRunner for FnRunner {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        (self.0)(ctx).await
    }
}

fn runner<F, Fut>(f: F) -> Box<dyn JobRunner>
where
    F: FnOnce(JobCtx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<JobDone, JobFailure>> + Send + 'static,
{
    Box::new(FnRunner(Box::new(move |ctx| f(ctx).boxed())))
}

fn done(summary: &str) -> JobDone {
    JobDone {
        summary: summary.into(),
        result_path: None,
    }
}

fn cancelled_failure() -> JobFailure {
    JobFailure {
        error: DbError::cancelled(),
        outcome_unknown: false,
        summary: "Stopped.".into(),
    }
}

/// A runner that reports `entered` with its id, then waits for `gate` and succeeds.
fn gated(entered: mpsc::UnboundedSender<Uuid>, gate: Arc<Notify>) -> Box<dyn JobRunner> {
    runner(move |ctx| async move {
        entered.send(ctx.id()).unwrap();
        gate.notified().await;
        Ok(done("ok"))
    })
}

/// A runner that waits until it is stopped (its cancel handle fires), then ends cancelled.
fn stoppable(entered: mpsc::UnboundedSender<Uuid>) -> Box<dyn JobRunner> {
    runner(move |ctx| async move {
        let woke = Arc::new(Notify::new());
        ctx.set_cancel(Some(Arc::new(NotifyCanceller(Arc::clone(&woke)))));
        entered.send(ctx.id()).unwrap();
        if !ctx.is_stopped() {
            woke.notified().await;
        }
        Err(cancelled_failure())
    })
}

struct NotifyCanceller(Arc<Notify>);

#[async_trait]
impl Canceller for NotifyCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.notify_one();
        Ok(())
    }
}

struct CountingCanceller {
    calls: Arc<AtomicUsize>,
    woke: Arc<Notify>,
}

#[async_trait]
impl Canceller for CountingCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.woke.notify_one();
        Ok(())
    }
}

async fn recv(rx: &mut mpsc::UnboundedReceiver<Uuid>) -> Uuid {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("runner entered in time")
        .unwrap()
}

/// `rx` stays empty for a short bounded time.
async fn stays_empty(rx: &mut mpsc::UnboundedReceiver<Uuid>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "no runner should have entered"
    );
}

async fn finish(f: &Fixture, id: Uuid) -> JobSummary {
    assert!(f.mgr.wait(id, WAIT).await, "job {id} ended in time");
    f.mgr.detail(id).unwrap().job
}

fn record(f: &Fixture, id: Uuid) -> JobRecord {
    f.repo.job_get(id).unwrap().expect("job is persisted")
}

#[tokio::test]
async fn runs_and_persists_done() {
    let f = fixture();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let g = Arc::clone(&gate);
    let tmp_seen = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&tmp_seen);
    let id = f
        .mgr
        .submit(
            spec(Some(Uuid::new_v4())),
            runner(move |ctx| async move {
                let dir = ctx.tmp_dir().unwrap();
                std::fs::write(dir.join("part"), b"x").unwrap();
                *seen.lock().unwrap() = Some(dir);
                ctx.progress("Tables", 3, Some(4), ProgressUnit::Statements);
                tx.send(ctx.id()).unwrap();
                g.notified().await;
                Ok(JobDone {
                    summary: "4 tables dumped".into(),
                    result_path: Some("C:\\out\\shop.sql".into()),
                })
            }),
        )
        .unwrap();
    // Inserted on the blocking pool (as queued; the job may have started meanwhile).
    f.mgr.stored(id).await;
    let r = record(&f, id);
    assert!(r.state == "queued" || r.state == "running", "{}", r.state);
    assert_eq!(r.kind, "dump");
    assert!(r.rerunnable);
    assert!(r.params_json.unwrap().contains("shop"));

    assert_eq!(recv(&mut rx).await, id);
    let r = record(&f, id);
    assert_eq!(r.state, "running");
    assert!(r.started_at.is_some());
    assert_eq!(f.mgr.active_count(), 1);

    gate.notify_one();
    let s = finish(&f, id).await;
    assert_eq!(s.state, JobState::Done);
    assert_eq!(s.progress.done, 3.0);
    let r = record(&f, id);
    assert_eq!(r.state, "done");
    assert_eq!(r.summary.as_deref(), Some("4 tables dumped"));
    assert_eq!(r.result_path.as_deref(), Some("C:\\out\\shop.sql"));
    assert!(r.finished_at.is_some());
    assert!(r.error_json.is_none());
    assert_eq!(f.mgr.active_count(), 0);
    // The job's temporary directory is gone.
    let dir = tmp_seen.lock().unwrap().clone().unwrap();
    assert!(dir.starts_with(f.tmp.path()));
    assert!(!dir.exists());
    // Events: queued first, done last.
    let ev = f.events.of(id);
    assert_eq!(ev.first().unwrap().job.state, JobState::Queued);
    assert_eq!(ev.last().unwrap().job.state, JobState::Done);
}

#[tokio::test]
async fn failure_is_persisted_with_error_kind() {
    let f = fixture();
    let id = f
        .mgr
        .submit(
            spec(None),
            runner(|_ctx| async move {
                Err(JobFailure {
                    error: DbError::Server {
                        code: Some("1062".into()),
                        sqlstate: Some("23000".into()),
                        message: "Duplicate entry 'alice@example.com'".into(),
                        position: None,
                    },
                    outcome_unknown: true,
                    summary: "Stopped at batch 3.".into(),
                })
            }),
        )
        .unwrap();
    let s = finish(&f, id).await;
    assert_eq!(s.state, JobState::Failed);
    assert!(s.outcome_unknown);
    let e = s.error.unwrap();
    assert_eq!(e.kind, "server");
    // The full server text is shown while the app runs …
    assert!(e.message.contains("alice@example.com"));
    // … but never persisted (R6).
    let r = record(&f, id);
    assert_eq!(r.state, "failed");
    assert!(r.outcome_unknown);
    assert_eq!(r.summary.as_deref(), Some("Stopped at batch 3."));
    let json = r.error_json.unwrap();
    assert!(json.contains("\"kind\":\"server\""), "{json}");
    assert!(json.contains("Server error 1062"), "{json}");
    assert!(!json.contains("alice"), "{json}");
}

#[tokio::test]
async fn panic_becomes_failed_internal() {
    let f = fixture();
    let id = f
        .mgr
        .submit(
            spec(Some(Uuid::new_v4())),
            runner(|_ctx| async move {
                if true {
                    panic!("boom with user data");
                }
                Ok(done("unreachable"))
            }),
        )
        .unwrap();
    let s = finish(&f, id).await;
    assert_eq!(s.state, JobState::Failed);
    assert_eq!(s.error.unwrap().kind, "internal");
    let r = record(&f, id);
    assert_eq!(r.state, "failed");
    assert!(!r.error_json.unwrap().contains("boom"));
    assert_eq!(f.mgr.active_count(), 0);
}

#[tokio::test]
async fn limit_per_connection_queues_fifo() {
    let f = fixture();
    f.limit.store(2, Ordering::SeqCst);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let gates: Vec<Arc<Notify>> = (0..5).map(|_| Arc::new(Notify::new())).collect();
    let ids: Vec<Uuid> = (0..4)
        .map(|i| {
            f.mgr
                .submit(spec(Some(a)), gated(tx.clone(), Arc::clone(&gates[i])))
                .unwrap()
        })
        .collect();
    let mut first = [recv(&mut rx).await, recv(&mut rx).await];
    first.sort();
    let mut expected = [ids[0], ids[1]];
    expected.sort();
    assert_eq!(first, expected);
    stays_empty(&mut rx).await;
    let d = f.mgr.detail(ids[2]).unwrap();
    assert_eq!(d.job.state, JobState::Queued);
    assert!(d.job.started_at.is_none());
    assert_eq!(f.mgr.active_count(), 4);

    // Another connection is not limited by A's jobs.
    let on_b = f
        .mgr
        .submit(spec(Some(b)), gated(tx.clone(), Arc::clone(&gates[4])))
        .unwrap();
    assert_eq!(recv(&mut rx).await, on_b);

    // One of A's jobs ends: the oldest queued one (FIFO) takes its slot, not the newest.
    gates[0].notify_one();
    finish(&f, ids[0]).await;
    assert_eq!(recv(&mut rx).await, ids[2]);
    stays_empty(&mut rx).await;
    gates[1].notify_one();
    assert_eq!(recv(&mut rx).await, ids[3]);
    for g in &gates {
        g.notify_one();
    }
    for id in ids.iter().chain([&on_b]) {
        assert_eq!(finish(&f, *id).await.state, JobState::Done);
    }
}

#[tokio::test]
async fn stop_queued_job_never_runs() {
    let f = fixture();
    f.limit.store(1, Ordering::SeqCst);
    let conn = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let first = f
        .mgr
        .submit(spec(Some(conn)), gated(tx, Arc::clone(&gate)))
        .unwrap();
    recv(&mut rx).await;
    let entered = Arc::new(AtomicBool::new(false));
    let e = Arc::clone(&entered);
    let queued = f
        .mgr
        .submit(
            spec(Some(conn)),
            runner(move |_ctx| async move {
                e.store(true, Ordering::SeqCst);
                Ok(done("ran"))
            }),
        )
        .unwrap();
    assert_eq!(f.mgr.detail(queued).unwrap().job.state, JobState::Queued);
    f.mgr.stored(queued).await;
    assert_eq!(record(&f, queued).state, "queued");
    assert!(f.mgr.cancel(queued).await);
    let s = finish(&f, queued).await;
    assert_eq!(s.state, JobState::Cancelled);
    assert!(s.started_at.is_none());
    assert_eq!(record(&f, queued).state, "cancelled");
    // Freeing the slot afterwards does not start it either.
    gate.notify_one();
    finish(&f, first).await;
    assert!(!entered.load(Ordering::SeqCst));
    // A job that is no longer live cannot be cancelled.
    assert!(!f.mgr.cancel(queued).await);
}

#[tokio::test]
async fn stop_running_job_raises_signal_and_calls_cancel_handle() {
    let f = fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let kills = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (c, k) = (Arc::clone(&calls), Arc::clone(&kills));
    let id = f
        .mgr
        .submit(
            spec(Some(Uuid::new_v4())),
            runner(move |ctx| async move {
                let woke = Arc::new(Notify::new());
                let handle: CancelHandle = Arc::new(CountingCanceller {
                    calls: c,
                    woke: Arc::clone(&woke),
                });
                ctx.set_cancel(Some(handle));
                ctx.set_kill(Some(Box::new(move || {
                    k.fetch_add(1, Ordering::SeqCst);
                })));
                let stop = ctx.stop();
                tx.send(ctx.id()).unwrap();
                woke.notified().await;
                assert!(stop.is_stopped());
                Err(JobFailure {
                    error: DbError::Server {
                        code: Some("1317".into()),
                        sqlstate: None,
                        message: "Query execution was interrupted".into(),
                        position: None,
                    },
                    outcome_unknown: false,
                    summary: "Stopped.".into(),
                })
            }),
        )
        .unwrap();
    recv(&mut rx).await;
    assert!(f.mgr.cancel(id).await);
    let s = finish(&f, id).await;
    // Stopped before the runner returned: cancelled, whatever error it returned.
    assert_eq!(s.state, JobState::Cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(kills.load(Ordering::SeqCst), 1);
    assert_eq!(record(&f, id).state, "cancelled");
}

#[tokio::test]
async fn stop_between_batches_sends_nothing_more() {
    let f = fixture();
    let sends = Arc::new(AtomicUsize::new(0));
    let after_three = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let (s, a, r) = (
        Arc::clone(&sends),
        Arc::clone(&after_three),
        Arc::clone(&resume),
    );
    let id = f
        .mgr
        .submit(
            spec(Some(Uuid::new_v4())),
            runner(move |ctx| async move {
                for batch in 1..=1000u64 {
                    if ctx.is_stopped() {
                        return Err(cancelled_failure());
                    }
                    s.fetch_add(1, Ordering::SeqCst); // the fake "send"
                    ctx.progress("Rows", batch * 100, None, ProgressUnit::Rows);
                    if batch == 3 {
                        a.notify_one();
                        r.notified().await;
                    }
                }
                Ok(done("all sent"))
            }),
        )
        .unwrap();
    tokio::time::timeout(WAIT, after_three.notified())
        .await
        .unwrap();
    let at_cancel = sends.load(Ordering::SeqCst);
    assert_eq!(at_cancel, 3);
    assert!(f.mgr.cancel(id).await);
    resume.notify_one();
    let s = finish(&f, id).await;
    assert_eq!(s.state, JobState::Cancelled);
    assert_eq!(sends.load(Ordering::SeqCst) - at_cancel, 0);
}

#[tokio::test]
async fn events_are_throttled_but_final_state_always_sent() {
    let f = fixture();
    let took = Arc::new(Mutex::new(Duration::ZERO));
    let t = Arc::clone(&took);
    let id = f
        .mgr
        .submit(
            spec(Some(Uuid::new_v4())),
            runner(move |ctx| async move {
                let started = Instant::now();
                ctx.log(JobLogLevel::Info, "start");
                for i in 0..1000u64 {
                    ctx.progress("Rows", i, Some(1000), ProgressUnit::Rows);
                }
                ctx.log(JobLogLevel::Info, "middle");
                ctx.log(JobLogLevel::Warn, "end");
                *t.lock().unwrap() = started.elapsed();
                Ok(done("ok"))
            }),
        )
        .unwrap();
    finish(&f, id).await;
    // At most one progress event per interval, plus the first one, whatever the speed
    // of the machine (normally the loop takes a few ms: no progress event at all).
    let intervals = (took.lock().unwrap().as_millis() / EVENT_INTERVAL.as_millis()) as usize;
    let max_progress = 2 + intervals;
    let ev = f.events.of(id);
    let states: Vec<JobState> = ev.iter().map(|e| e.job.state).collect();
    assert_eq!(states.first(), Some(&JobState::Queued));
    let last = ev.last().unwrap();
    assert_eq!(last.job.state, JobState::Done);
    assert_eq!(last.job.progress.done, 999.0);
    assert_eq!(last.job.progress.total, Some(1000.0));
    // One event for the running transition plus at most two progress events.
    let running = states.iter().filter(|s| **s == JobState::Running).count();
    assert!((1..=1 + max_progress).contains(&running), "{states:?}");
    assert!(ev.len() <= 3 + max_progress, "{states:?}");
    // Every log line reaches the UI exactly once.
    let lines: Vec<String> = ev
        .iter()
        .flat_map(|e| e.new_log.iter().map(|l| l.message.clone()))
        .collect();
    assert_eq!(lines, ["start", "middle", "end"]);
}

#[tokio::test]
async fn log_is_trimmed_head_and_tail() {
    let f = fixture();
    let id = f
        .mgr
        .submit(
            spec(Some(Uuid::new_v4())),
            runner(|ctx| async move {
                for i in 0..3000 {
                    if i == 0 || i == 2999 {
                        ctx.log_server(
                            JobLogLevel::Error,
                            format!("Duplicate entry 'secret-{i}'"),
                            Some("1062"),
                        );
                    } else {
                        ctx.log(JobLogLevel::Info, format!("line {i}"));
                    }
                }
                Ok(done("ok"))
            }),
        )
        .unwrap();
    finish(&f, id).await;
    let d = f.mgr.detail(id).unwrap();
    assert_eq!(d.log.len(), MAX_LIVE_LOG_LINES);
    assert_eq!(d.dropped_lines, 1000);
    assert_eq!(d.log[0].message, "Duplicate entry 'secret-0'");
    assert_eq!(d.log[999].message, "line 999");
    assert_eq!(d.log[1000].message, "line 2000");
    assert_eq!(d.log[1999].message, "Duplicate entry 'secret-2999'");

    let r = record(&f, id);
    let persisted: Vec<JobLogLine> = serde_json::from_str(&r.log_json).unwrap();
    assert_eq!(persisted.len(), PERSIST_LOG_LINES);
    assert_eq!(r.dropped_lines, 2500);
    assert_eq!(persisted[0].message, "Server error 1062");
    assert_eq!(persisted[0].level, JobLogLevel::Error);
    assert_eq!(persisted[249].message, "line 249");
    assert_eq!(persisted[250].message, "line 2750");
    assert_eq!(persisted[499].message, "Server error 1062");
    assert!(!r.log_json.contains("secret"));
    assert!(r.log_json.len() <= PERSIST_LOG_BYTES);
}

#[tokio::test]
async fn persisted_log_respects_byte_cap() {
    let f = fixture();
    let id = f
        .mgr
        .submit(
            spec(None),
            runner(|ctx| async move {
                for i in 0..400 {
                    ctx.log(JobLogLevel::Info, format!("{i:04} {}", "x".repeat(2000)));
                }
                Ok(done("ok"))
            }),
        )
        .unwrap();
    finish(&f, id).await;
    let r = record(&f, id);
    assert!(
        r.log_json.len() <= PERSIST_LOG_BYTES,
        "{}",
        r.log_json.len()
    );
    let persisted: Vec<JobLogLine> = serde_json::from_str(&r.log_json).unwrap();
    assert!(!persisted.is_empty());
    assert!(persisted[0].message.starts_with("0000 "));
    assert!(persisted.last().unwrap().message.starts_with("0399 "));
    assert_eq!(persisted.len() as u32 + r.dropped_lines, 400);
}

#[tokio::test]
async fn recover_marks_interrupted_and_cleans_tmp() {
    let f = fixture();
    let rec = |state: &str| JobRecord {
        id: Uuid::new_v4(),
        kind: "restore".into(),
        connection_id: None,
        title: "Restore".into(),
        params_json: None,
        rerunnable: true,
        state: state.into(),
        created_at: "2026-10-25T10:00:00.000Z".into(),
        started_at: None,
        finished_at: None,
        result_path: None,
        summary: None,
        error_json: None,
        outcome_unknown: false,
        log_json: "[]".into(),
        dropped_lines: 0,
    };
    let running = rec("running");
    let queued = rec("queued");
    let finished = rec("done");
    for r in [&running, &queued, &finished] {
        f.repo.job_insert(r).unwrap();
    }
    let root = f.tmp.path().join("job-tmp");
    // An instance that is gone: its lock file is not locked.
    let gone = root.join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(gone.join(Uuid::new_v4().to_string())).unwrap();
    std::fs::write(gone.join("owner.lock"), b"").unwrap();
    std::fs::write(root.join("stray.cnf"), b"secret").unwrap();
    // A young folder without a lock file may be an instance that is starting: kept.
    let young = root.join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&young).unwrap();

    f.mgr.recover().unwrap();

    for id in [running.id, queued.id] {
        let r = record(&f, id);
        assert_eq!(r.state, "failed");
        assert!(r.outcome_unknown);
        assert_eq!(
            r.summary.as_deref(),
            Some("The app closed while the job ran; its outcome is unknown.")
        );
        assert!(r.finished_at.is_some());
    }
    assert_eq!(record(&f, finished.id).state, "done");
    assert!(!gone.exists() && !root.join("stray.cnf").exists());
    assert!(young.exists());
    // Left: this instance's own folder and the young one.
    let left = std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 2);
    // The recovered job shows in the list as failed with an unknown outcome.
    let listed = f.mgr.list(10).unwrap();
    let r = listed.iter().find(|s| s.id == running.id).unwrap();
    assert_eq!(r.state, JobState::Failed);
    assert!(r.outcome_unknown);
    assert_eq!(r.kind, JobKind::Restore);
}

/// Two app instances share the data folder: a starting one never deletes the running
/// one's temporary files nor marks its live jobs interrupted.
#[tokio::test]
async fn recover_leaves_a_running_instance_alone() {
    let f = fixture();
    // The other instance: same store and base folder, its own locked folder and a job
    // with a temporary directory.
    let other = JobManager::new(Arc::clone(&f.repo));
    other.set_tmp_root(f.tmp.path().join("job-tmp"));
    let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
    let gate = Arc::new(Notify::new());
    let release = Arc::clone(&gate);
    let id = other
        .submit(
            spec(None),
            Box::new(FnRunner(Box::new(move |ctx: JobCtx| {
                Box::pin(async move {
                    let dir = ctx.tmp_dir().unwrap();
                    std::fs::write(dir.join("pgpass.conf"), b"x").unwrap();
                    tx.send(dir).unwrap();
                    release.notified().await;
                    Ok(done("ok"))
                })
            }))),
        )
        .unwrap();
    let dir = rx.recv().await.unwrap();
    other.stored(id).await;
    assert_eq!(record(&f, id).state, "running");

    f.mgr.recover().unwrap();

    assert!(
        dir.join("pgpass.conf").exists(),
        "the other instance's files stay"
    );
    assert_eq!(
        record(&f, id).state,
        "running",
        "its live job is not marked"
    );
    gate.notify_one();
    // Once it is gone (lock released), a later start cleans up after it.
    let other_root = dir.parent().unwrap().to_path_buf();
    assert!(other.wait(id, WAIT).await);
    drop(other);
    // A coalesced event flush may hold the manager a moment longer.
    tokio::time::sleep(EVENT_INTERVAL + Duration::from_millis(100)).await;
    let again = JobManager::new(Arc::clone(&f.repo));
    again.set_tmp_root(f.tmp.path().join("job-tmp"));
    again.recover().unwrap();
    assert!(!other_root.exists());
}

#[tokio::test]
async fn tmp_dir_is_refused_after_the_job_returned() {
    let f = fixture();
    let (tx, mut rx) = mpsc::unbounded_channel::<JobCtx>();
    let id = f
        .mgr
        .submit(
            spec(None),
            Box::new(FnRunner(Box::new(move |ctx: JobCtx| {
                Box::pin(async move {
                    tx.send(ctx).unwrap();
                    Ok(done("ok"))
                })
            }))),
        )
        .unwrap();
    let ctx = rx.recv().await.unwrap();
    finish(&f, id).await;
    assert!(ctx.tmp_dir().is_err());
    let own = std::fs::read_dir(f.tmp.path().join("job-tmp"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(!own.join(id.to_string()).exists());
}

#[tokio::test]
async fn cancel_connection_stops_only_that_connection() {
    let f = fixture();
    f.limit.store(1, Ordering::SeqCst);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let a_running = f.mgr.submit(spec(Some(a)), stoppable(tx.clone())).unwrap();
    recv(&mut rx).await;
    let a_queued = f.mgr.submit(spec(Some(a)), stoppable(tx.clone())).unwrap();
    let gate = Arc::new(Notify::new());
    let on_b = f
        .mgr
        .submit(spec(Some(b)), gated(tx.clone(), Arc::clone(&gate)))
        .unwrap();
    assert_eq!(recv(&mut rx).await, on_b);
    let cancelled = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&cancelled);
    let mut tracked_spec = spec(Some(a));
    tracked_spec.kind = JobKind::Export;
    let tracked = f.mgr.track(
        tracked_spec,
        Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }),
    );

    f.mgr.cancel_connection(a).await;

    assert_eq!(finish(&f, a_running).await.state, JobState::Cancelled);
    assert_eq!(finish(&f, a_queued).await.state, JobState::Cancelled);
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    tracked.finish(Err(DbError::cancelled())).await;
    assert_eq!(
        f.mgr.detail(tracked.id()).unwrap().job.state,
        JobState::Cancelled
    );
    stays_empty(&mut rx).await; // the queued job never ran
    assert_eq!(f.mgr.detail(on_b).unwrap().job.state, JobState::Running);
    gate.notify_one();
    assert_eq!(finish(&f, on_b).await.state, JobState::Done);
}

#[tokio::test]
async fn stop_all_waits_bounded() {
    let f = fixture();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let polite = f
        .mgr
        .submit(spec(Some(Uuid::new_v4())), stoppable(tx.clone()))
        .unwrap();
    let gate = Arc::new(Notify::new());
    let stubborn = f
        .mgr
        .submit(spec(Some(Uuid::new_v4())), gated(tx, Arc::clone(&gate)))
        .unwrap();
    recv(&mut rx).await;
    recv(&mut rx).await;

    let started = Instant::now();
    f.mgr.stop_all(Duration::from_millis(300)).await;
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!(f.mgr.detail(polite).unwrap().job.state, JobState::Cancelled);
    assert_eq!(f.mgr.active_count(), 1);

    gate.notify_one();
    // A runner that ignores Stop and completes its work ends done, not cancelled.
    assert_eq!(finish(&f, stubborn).await.state, JobState::Done);

    // With nothing running, stop_all returns at once.
    let started = Instant::now();
    f.mgr.stop_all(Duration::from_secs(5)).await;
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn tracked_job_progress_finish_and_cancel_closure() {
    let f = fixture();
    let cancels = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&cancels);
    let mut s = spec(Some(Uuid::new_v4()));
    s.kind = JobKind::Export;
    s.rerunnable = false;
    s.params = None;
    s.title = "Export orders to orders.csv".into();
    let t = f.mgr.track(
        s.clone(),
        Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let d = f.mgr.detail(t.id()).unwrap();
    assert_eq!(d.job.state, JobState::Running);
    assert!(d.job.started_at.is_some());
    assert_eq!(f.mgr.active_count(), 1);
    f.mgr.stored(t.id()).await;
    assert_eq!(record(&f, t.id()).state, "running");
    assert_eq!(record(&f, t.id()).kind, "export");

    t.progress(500);
    let d = f.mgr.detail(t.id()).unwrap();
    assert_eq!(d.job.progress.done, 500.0);
    assert_eq!(d.job.progress.unit, ProgressUnit::Rows);

    assert!(f.mgr.cancel(t.id()).await);
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
    t.finish(Err(DbError::cancelled())).await;
    assert_eq!(f.mgr.detail(t.id()).unwrap().job.state, JobState::Cancelled);
    assert_eq!(record(&f, t.id()).state, "cancelled");
    assert_eq!(f.mgr.active_count(), 0);
    // A second finish is ignored.
    t.finish(Ok(done("late"))).await;
    assert_eq!(f.mgr.detail(t.id()).unwrap().job.state, JobState::Cancelled);

    let ok = f.mgr.track(s.clone(), Box::new(|| {}));
    ok.progress(10);
    let p = f.mgr.detail(ok.id()).unwrap().job.progress;
    assert_eq!(
        (p.phase.as_str(), p.done, p.unit),
        ("Exporting", 10.0, ProgressUnit::Rows)
    );
    ok.finish(Ok(JobDone {
        summary: "10 rows exported".into(),
        result_path: Some("C:\\out\\orders.csv".into()),
    }))
    .await;
    let r = record(&f, ok.id());
    assert_eq!(r.state, "done");
    assert_eq!(r.summary.as_deref(), Some("10 rows exported"));
    assert_eq!(r.result_path.as_deref(), Some("C:\\out\\orders.csv"));

    let failed = f.mgr.track(s, Box::new(|| {}));
    failed
        .finish(Err(DbError::Internal {
            message: "disk full".into(),
        }))
        .await;
    assert_eq!(record(&f, failed.id()).state, "failed");
    assert!(!f.mgr.cancel(failed.id()).await);
}

#[tokio::test]
async fn list_merges_live_and_history_newest_first() {
    let f = fixture();
    let old = JobRecord {
        id: Uuid::new_v4(),
        kind: "import".into(),
        connection_id: None,
        title: "Old import".into(),
        params_json: None,
        rerunnable: true,
        state: "done".into(),
        created_at: "2020-01-01T00:00:00.000Z".into(),
        started_at: Some("2020-01-01T00:00:00.000Z".into()),
        finished_at: Some("2020-01-01T00:00:01.000Z".into()),
        result_path: None,
        summary: Some("5 rows".into()),
        error_json: None,
        outcome_unknown: false,
        log_json: "[]".into(),
        dropped_lines: 0,
    };
    f.repo.job_insert(&old).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let live = f
        .mgr
        .submit(spec(Some(Uuid::new_v4())), gated(tx, Arc::clone(&gate)))
        .unwrap();
    recv(&mut rx).await;
    let finished = f
        .mgr
        .submit(spec(None), runner(|_ctx| async { Ok(done("ok")) }))
        .unwrap();
    finish(&f, finished).await;

    let list = f.mgr.list(10).unwrap();
    let ids: Vec<Uuid> = list.iter().map(|s| s.id).collect();
    assert_eq!(ids, [live, finished, old.id]);
    assert_eq!(list[0].state, JobState::Running);
    assert_eq!(list[2].kind, JobKind::Import);
    assert_eq!(list[2].summary.as_deref(), Some("5 rows"));
    assert_eq!(f.mgr.list(2).unwrap().len(), 2);
    // A persisted job's detail comes from the store.
    let d = f.mgr.detail(old.id).unwrap();
    assert_eq!(d.job.title, "Old import");
    assert!(d.log.is_empty());
    assert!(f.mgr.detail(Uuid::new_v4()).is_err());

    // Clearing keeps the live job.
    f.mgr.clear_finished().unwrap();
    let ids: Vec<Uuid> = f.mgr.list(10).unwrap().iter().map(|s| s.id).collect();
    assert_eq!(ids, [live]);
    gate.notify_one();
    finish(&f, live).await;
}

const ALICE: &str = "alice@example.com";

fn duplicate() -> DbError {
    DbError::Server {
        code: Some("1062".into()),
        sqlstate: Some("23000".into()),
        message: format!("Duplicate entry '{ALICE}' for key 'email'"),
        position: Some(3),
    }
}

async fn failed_with(f: &Fixture, failure: JobFailure) -> (JobSummary, JobRecord) {
    let id = f
        .mgr
        .submit(spec(None), runner(move |_ctx| async move { Err(failure) }))
        .unwrap();
    let s = finish(f, id).await;
    (s, record(f, id))
}

#[tokio::test]
async fn server_text_is_never_persisted() {
    let f = fixture();
    // `From<DbError>` gives a generic summary for a server error.
    let (s, r) = failed_with(&f, JobFailure::from(duplicate())).await;
    assert_eq!(s.summary.as_deref(), Some("Server error 1062"));
    assert!(s.error.unwrap().message.contains(ALICE)); // shown while the app runs
    assert_eq!(r.summary.as_deref(), Some("Server error 1062"));
    let json = r.error_json.unwrap();
    assert!(!json.contains(ALICE), "{json}");
    assert!(json.contains("\"server_code\":\"1062\""), "{json}");

    // A runner summary that repeats the server text is redacted when stored.
    let (_, r) = failed_with(
        &f,
        JobFailure {
            error: duplicate(),
            outcome_unknown: false,
            summary: format!("Batch 3 failed: Duplicate entry '{ALICE}' for key 'email'"),
        },
    )
    .await;
    assert_eq!(r.summary.as_deref(), Some("Server error 1062"));
    // One that does not is kept.
    let (_, r) = failed_with(
        &f,
        JobFailure {
            error: duplicate(),
            outcome_unknown: false,
            summary: "Stopped at batch 3; 200 rows written.".into(),
        },
    )
    .await;
    assert_eq!(
        r.summary.as_deref(),
        Some("Stopped at batch 3; 200 rows written.")
    );

    // Auth, connect and connection-lost errors keep only kind and flags.
    for (error, kind) in [
        (
            DbError::Auth {
                message: format!("Access denied for user '{ALICE}'"),
            },
            "auth",
        ),
        (
            DbError::Connect {
                message: format!("host {ALICE} unreachable"),
            },
            "connect",
        ),
        (
            DbError::ConnectionLost {
                was_in_tx: true,
                outcome_unknown: true,
                message: format!("reset while sending '{ALICE}'"),
            },
            "connection_lost",
        ),
    ] {
        let (s, r) = failed_with(&f, JobFailure::from(error)).await;
        let json = r.error_json.unwrap();
        assert!(json.contains(&format!("\"kind\":\"{kind}\"")), "{json}");
        assert!(!json.contains(ALICE), "{json}");
        assert!(!r.summary.unwrap().contains(ALICE));
        assert_eq!(r.outcome_unknown, s.outcome_unknown);
    }
    let (s, r) = failed_with(
        &f,
        JobFailure::from(DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: true,
            message: ALICE.into(),
        }),
    )
    .await;
    assert!(s.outcome_unknown && r.outcome_unknown);
    assert!(r.error_json.unwrap().contains("\"outcome_unknown\":true"));
}

struct SlowCanceller(Arc<Notify>);

#[async_trait]
impl Canceller for SlowCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        tokio::time::sleep(Duration::from_millis(800)).await;
        self.0.notify_one();
        Ok(())
    }
}

/// Waits for its (slow) canceller, then ends cancelled.
fn slow_to_cancel(entered: mpsc::UnboundedSender<Uuid>) -> Box<dyn JobRunner> {
    runner(move |ctx| async move {
        let woke = Arc::new(Notify::new());
        ctx.set_cancel(Some(Arc::new(SlowCanceller(Arc::clone(&woke)))));
        entered.send(ctx.id()).unwrap();
        woke.notified().await;
        Err(cancelled_failure())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_all_and_cancel_connection_do_not_wait_for_slow_cancellers() {
    let f = fixture();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let ids: Vec<Uuid> = (0..3)
        .map(|_| {
            f.mgr
                .submit(spec(Some(Uuid::new_v4())), slow_to_cancel(tx.clone()))
                .unwrap()
        })
        .collect();
    for _ in 0..3 {
        recv(&mut rx).await;
    }
    let started = Instant::now();
    f.mgr.stop_all(Duration::from_millis(300)).await;
    let took = started.elapsed();
    assert!(took < Duration::from_millis(600), "{took:?}");
    // The cancellers ran concurrently: all three end ~800 ms after the stop.
    for id in &ids {
        assert_eq!(finish(&f, *id).await.state, JobState::Cancelled);
    }
    assert!(started.elapsed() < Duration::from_millis(2000));

    let conn = Uuid::new_v4();
    let id = f.mgr.submit(spec(Some(conn)), slow_to_cancel(tx)).unwrap();
    recv(&mut rx).await;
    let started = Instant::now();
    f.mgr.cancel_connection(conn).await;
    assert!(started.elapsed() < Duration::from_millis(300));
    assert_eq!(finish(&f, id).await.state, JobState::Cancelled);
}

#[tokio::test]
async fn limit_changed_wakes_queued_jobs() {
    let f = fixture();
    f.limit.store(1, Ordering::SeqCst);
    let conn = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let first = f
        .mgr
        .submit(spec(Some(conn)), gated(tx.clone(), Arc::clone(&gate)))
        .unwrap();
    assert_eq!(recv(&mut rx).await, first);
    let second = f
        .mgr
        .submit(spec(Some(conn)), gated(tx, Arc::clone(&gate)))
        .unwrap();
    stays_empty(&mut rx).await;
    f.limit.store(2, Ordering::SeqCst);
    f.mgr.limit_changed();
    assert_eq!(recv(&mut rx).await, second);
    gate.notify_waiters();
    finish(&f, first).await;
    finish(&f, second).await;
}

#[tokio::test]
async fn progress_and_log_after_return_are_ignored() {
    let f = fixture();
    let kept: Arc<Mutex<Option<JobCtx>>> = Arc::default();
    let k = Arc::clone(&kept);
    let id = f
        .mgr
        .submit(
            spec(None),
            runner(move |ctx| async move {
                ctx.progress("Rows", 5, None, ProgressUnit::Rows);
                *k.lock().unwrap() = Some(ctx);
                Ok(done("ok"))
            }),
        )
        .unwrap();
    finish(&f, id).await;
    let events = f.events.of(id).len();
    let ctx = kept.lock().unwrap().take().unwrap();
    ctx.progress("Rows", 99, None, ProgressUnit::Rows);
    ctx.log(JobLogLevel::Info, "late");
    let d = f.mgr.detail(id).unwrap();
    assert_eq!(d.job.progress.done, 5.0);
    assert!(d.log.is_empty());
    assert_eq!(f.events.of(id).len(), events);
}
