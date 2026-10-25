//! [`JobManager`] (spec §4.2): runs background jobs with at most N running per connection
//! (R2, FIFO queue), Stop, throttled events (R4), an in-memory log (R6) and SQLite
//! persistence; tracks the streaming exports that keep their own execution path (R3).
//!
//! Concurrency: one std `Mutex<State>` holds every job and the per-connection queues; it
//! is never held across an `.await`. Queued jobs wait on `slot_changed` (a `Notify` woken
//! whenever a slot frees, a job is stopped or a queue head leaves), re-checking their
//! queue position and the limit under the mutex. A job that is stopped while queued never
//! enters its runner: the transition to `running` checks the stop signal under the same
//! mutex that `cancel` raises it under.

use super::model::{
    JobDetail, JobEvent, JobKind, JobLogLevel, JobLogLine, JobParams, JobProgress, JobState,
    JobSummary, ProgressUnit,
};
use super::persist;
use crate::run_events::utc_millis_now;
use crate::session_manager::lock;
use async_trait::async_trait;
use dnt_core::driver::{CancelHandle, StopSignal};
use dnt_core::error::{AppError, DbError};
use dnt_store::jobs::JobRecord;
use dnt_store::repo::ConnectionRepo;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use uuid::Uuid;

/// Log lines memory keeps per job: the first half and the last half (R6).
pub const MAX_LIVE_LOG_LINES: usize = 2000;
const LIVE_HEAD: usize = MAX_LIVE_LOG_LINES / 2;
const LIVE_TAIL: usize = MAX_LIVE_LOG_LINES - LIVE_HEAD;
/// Log lines the store keeps per job (first 250 + last 250, R6).
pub const PERSIST_LOG_LINES: usize = 500;
/// Largest stored log (JSON) of one job.
pub const PERSIST_LOG_BYTES: usize = 256 * 1024;
/// Finished jobs the store keeps.
pub const KEEP_FINISHED: u32 = 500;
/// Least time between two progress/log events of a job (≤ 5 per second, R4).
pub const EVENT_INTERVAL: Duration = Duration::from_millis(200);
/// `app_settings` key of the per-connection limit (R2).
pub const JOBS_PER_CONNECTION_KEY: &str = "jobs_per_connection";
pub const DEFAULT_JOBS_PER_CONNECTION: u32 = 2;
pub const MIN_JOBS_PER_CONNECTION: u32 = 1;
pub const MAX_JOBS_PER_CONNECTION: u32 = 8;
/// Finished jobs whose full detail (complete log, server messages) memory keeps for the
/// rest of the session; older ones are read back from the store.
const FINISHED_IN_MEMORY: usize = 100;
/// Summary of a job the app closed under (R7).
pub const INTERRUPTED_SUMMARY: &str = "The app closed while the job ran; its outcome is unknown.";
const STOPPED_BEFORE_START: &str = "Stopped before it started.";
const PANIC_SUMMARY: &str = "The job failed unexpectedly.";

/// The per-connection limit stored under [`JOBS_PER_CONNECTION_KEY`]: the default when
/// missing or not a number, clamped to 1–8.
pub fn jobs_per_connection(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_JOBS_PER_CONNECTION)
        .clamp(MIN_JOBS_PER_CONNECTION, MAX_JOBS_PER_CONNECTION)
}

/// What a job is (fixed when it is submitted or tracked).
#[derive(Debug, Clone)]
pub struct JobSpec {
    pub kind: JobKind,
    pub connection_id: Option<Uuid>,
    pub title: String,
    /// What "Run again" needs (never a secret).
    pub params: Option<JobParams>,
    pub rerunnable: bool,
}

/// The work of a submitted job. It runs on a tokio task; it checks `ctx.is_stopped()`
/// between batches/statements and never sends a statement once it is raised.
#[async_trait]
pub trait JobRunner: Send + 'static {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    pub summary: String,
    pub result_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobFailure {
    pub error: DbError,
    /// The last write may or may not have been applied.
    pub outcome_unknown: bool,
    pub summary: String,
}

impl From<DbError> for JobFailure {
    /// The error's own message as the summary; `outcome_unknown` as the error says.
    fn from(error: DbError) -> Self {
        let app = AppError::from(error.clone());
        JobFailure {
            error,
            outcome_unknown: app.outcome_unknown,
            summary: app.message,
        }
    }
}

/// Receives job events (the app emits them as `job://progress`).
pub trait JobEvents: Send + Sync {
    fn emit(&self, event: &JobEvent);
}

/// What memory keeps of a job's log: the first and last 1,000 lines.
#[derive(Default)]
struct LogBuf {
    head: Vec<JobLogLine>,
    tail: VecDeque<JobLogLine>,
    dropped: u32,
}

impl LogBuf {
    fn push(&mut self, line: JobLogLine) {
        if self.head.len() < LIVE_HEAD {
            self.head.push(line);
            return;
        }
        if self.tail.len() == LIVE_TAIL {
            self.tail.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.tail.push_back(line);
    }

    fn lines(&self) -> Vec<JobLogLine> {
        self.head.iter().chain(self.tail.iter()).cloned().collect()
    }
}

type Kill = Arc<dyn Fn() + Send + Sync>;

struct Live {
    /// Creation order (newest first in lists).
    seq: u64,
    summary: JobSummary,
    params: Option<JobParams>,
    stop: StopSignal,
    cancel: Option<CancelHandle>,
    /// Kills an external process, or (tracked jobs) the export's cancel closure.
    kill: Option<Kill>,
    log: LogBuf,
    /// Log lines not sent in an event yet.
    pending: VecDeque<JobLogLine>,
    /// Something changed since the last event.
    dirty: bool,
    last_emit: Option<Instant>,
    flush_scheduled: bool,
    tmp_dir: Option<PathBuf>,
    /// The runner returned (or the tracked job finished): a Stop no longer changes the
    /// outcome.
    returned: bool,
    /// The final state is persisted and the temporary directory removed.
    settled: bool,
}

impl Live {
    fn new(seq: u64, summary: JobSummary, params: Option<JobParams>) -> Self {
        Live {
            seq,
            summary,
            params,
            stop: StopSignal::new(),
            cancel: None,
            kill: None,
            log: LogBuf::default(),
            pending: VecDeque::new(),
            dirty: true,
            last_emit: None,
            flush_scheduled: false,
            tmp_dir: None,
            returned: false,
            settled: false,
        }
    }

    fn record(&self) -> JobRecord {
        persist::to_record(
            &self.summary,
            self.params.as_ref(),
            &self.log.lines(),
            self.log.dropped,
        )
    }
}

#[derive(Default)]
struct Slots {
    /// Queued jobs of the connection, oldest first.
    queue: VecDeque<Uuid>,
    running: u32,
}

#[derive(Default)]
struct State {
    seq: u64,
    jobs: HashMap<Uuid, Live>,
    conns: HashMap<Uuid, Slots>,
    /// Settled jobs still in memory, oldest first.
    finished: VecDeque<Uuid>,
}

type LimitReader = Arc<dyn Fn() -> u32 + Send + Sync>;

struct Inner {
    repo: Arc<ConnectionRepo>,
    state: Mutex<State>,
    /// A slot freed, a queued job was stopped or left its queue.
    slot_changed: Notify,
    /// A job settled.
    settled: Notify,
    events: RwLock<Option<Arc<dyn JobEvents>>>,
    /// Serialises building and sending events, so a job's events arrive in order.
    emit_lock: Mutex<()>,
    tmp_root: RwLock<Option<PathBuf>>,
    limit: RwLock<LimitReader>,
}

/// How a job ended.
struct Final {
    state: JobState,
    summary: Option<String>,
    result_path: Option<String>,
    error: Option<AppError>,
    outcome_unknown: bool,
}

impl Final {
    fn of(result: Result<JobDone, JobFailure>, stopped: bool) -> Final {
        match result {
            Ok(done) => Final {
                state: JobState::Done,
                summary: Some(done.summary),
                result_path: done.result_path,
                error: None,
                outcome_unknown: false,
            },
            Err(f) => {
                let error = AppError::from(f.error);
                Final {
                    // Stopped before the runner returned: cancelled, whatever error the
                    // stop produced (a driver may report it as a server error).
                    state: if stopped {
                        JobState::Cancelled
                    } else {
                        JobState::Failed
                    },
                    summary: Some(f.summary),
                    result_path: None,
                    outcome_unknown: f.outcome_unknown || error.outcome_unknown,
                    error: Some(error),
                }
            }
        }
    }

    fn stopped_before_start() -> Final {
        Final {
            state: JobState::Cancelled,
            summary: Some(STOPPED_BEFORE_START.into()),
            result_path: None,
            error: None,
            outcome_unknown: false,
        }
    }
}

/// Runs and tracks background jobs; `Clone` shares the same manager.
#[derive(Clone)]
pub struct JobManager {
    inner: Arc<Inner>,
}

impl JobManager {
    /// The per-connection limit is read from the repo's `app_settings` each time a job
    /// waits for its slot.
    pub fn new(repo: Arc<ConnectionRepo>) -> Self {
        let reader_repo = Arc::clone(&repo);
        let limit: LimitReader = Arc::new(move || {
            match reader_repo.app_setting(JOBS_PER_CONNECTION_KEY) {
                Ok(raw) => jobs_per_connection(raw.as_deref()),
                Err(e) => {
                    tracing::warn!(error = %e, "could not read the jobs per connection setting");
                    DEFAULT_JOBS_PER_CONNECTION
                }
            }
        });
        JobManager {
            inner: Arc::new(Inner {
                repo,
                state: Mutex::new(State::default()),
                slot_changed: Notify::new(),
                settled: Notify::new(),
                events: RwLock::new(None),
                emit_lock: Mutex::new(()),
                tmp_root: RwLock::new(None),
                limit: RwLock::new(limit),
            }),
        }
    }

    /// Replaces where the per-connection limit comes from (tests). The value is clamped
    /// to 1–8.
    pub fn with_limit_reader(self, f: impl Fn() -> u32 + Send + Sync + 'static) -> Self {
        *self
            .inner
            .limit
            .write()
            .unwrap_or_else(PoisonError::into_inner) =
            Arc::new(move || f().clamp(MIN_JOBS_PER_CONNECTION, MAX_JOBS_PER_CONNECTION));
        self
    }

    pub fn set_events(&self, sink: Arc<dyn JobEvents>) {
        *self
            .inner
            .events
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

    /// Where jobs get their temporary directories (`<data_dir>/job-tmp`).
    pub fn set_tmp_root(&self, dir: PathBuf) {
        *self
            .inner
            .tmp_root
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(dir);
    }

    fn tmp_root(&self) -> Option<PathBuf> {
        self.inner
            .tmp_root
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Startup recovery (R7): deletes every leftover temporary job file and marks jobs
    /// left queued/running as failed with an unknown outcome.
    pub fn recover(&self) -> Result<(), DbError> {
        if let Some(root) = self.tmp_root() {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let removed = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    if let Err(e) = removed {
                        tracing::warn!(error = %e, "could not delete a leftover job file");
                    }
                }
            }
        }
        let n = self
            .inner
            .repo
            .job_mark_interrupted(INTERRUPTED_SUMMARY, &utc_millis_now())?;
        if n > 0 {
            tracing::info!(count = n, "jobs interrupted by the last app exit");
        }
        Ok(())
    }

    fn new_summary(spec: &JobSpec, id: Uuid, state: JobState, now: &str) -> JobSummary {
        JobSummary {
            id,
            kind: spec.kind,
            connection_id: spec.connection_id,
            title: spec.title.clone(),
            state,
            progress: JobProgress::default(),
            created_at: now.to_owned(),
            started_at: (state == JobState::Running).then(|| now.to_owned()),
            finished_at: None,
            result_path: None,
            summary: None,
            error: None,
            outcome_unknown: false,
            rerunnable: spec.rerunnable,
        }
    }

    /// Stores the job as queued, emits an event and starts a task that waits for a run
    /// slot of its connection, then runs `runner`. Must be called inside a tokio runtime.
    pub fn submit(&self, spec: JobSpec, runner: Box<dyn JobRunner>) -> Result<Uuid, DbError> {
        let id = Uuid::new_v4();
        let summary = Self::new_summary(&spec, id, JobState::Queued, &utc_millis_now());
        let live = {
            let mut st = lock(&self.inner.state);
            st.seq += 1;
            Live::new(st.seq, summary, spec.params)
        };
        self.inner.repo.job_insert(&live.record())?;
        {
            let mut st = lock(&self.inner.state);
            if let Some(conn) = spec.connection_id {
                st.conns.entry(conn).or_default().queue.push_back(id);
            }
            st.jobs.insert(id, live);
        }
        self.emit(id, true);
        let mgr = self.clone();
        tokio::spawn(async move { mgr.drive(id, spec.connection_id, runner).await });
        Ok(id)
    }

    async fn drive(self, id: Uuid, conn: Option<Uuid>, runner: Box<dyn JobRunner>) {
        if let Some(conn) = conn {
            if !self.acquire_slot(conn, id).await {
                self.settle(id, Final::stopped_before_start()).await;
                return;
            }
        }
        let stop = {
            let mut st = lock(&self.inner.state);
            match st.jobs.get_mut(&id) {
                Some(live) if !live.stop.is_stopped() => {
                    live.summary.state = JobState::Running;
                    live.summary.started_at = Some(utc_millis_now());
                    live.dirty = true;
                    Some(live.stop.clone())
                }
                _ => None,
            }
        };
        let Some(stop) = stop else {
            if let Some(conn) = conn {
                self.release_slot(conn);
            }
            self.settle(id, Final::stopped_before_start()).await;
            return;
        };
        self.emit(id, true);
        self.persist(id).await;

        let ctx = JobCtx {
            id,
            stop,
            mgr: self.clone(),
        };
        // A panic becomes `failed` / `internal`, logged with its location only.
        let result = crate::commands::guard(async move { Ok(runner.run(ctx).await) }).await;
        let stopped = {
            let mut st = lock(&self.inner.state);
            st.jobs.get_mut(&id).is_some_and(|live| {
                live.returned = true;
                live.stop.is_stopped()
            })
        };
        let fin = match result {
            Ok(result) => Final::of(result, stopped),
            Err(app) => Final {
                state: JobState::Failed,
                summary: Some(PANIC_SUMMARY.into()),
                result_path: None,
                error: Some(app),
                outcome_unknown: false,
            },
        };
        if let Some(conn) = conn {
            self.release_slot(conn);
        }
        self.settle(id, fin).await;
    }

    async fn read_limit(&self) -> u32 {
        let reader = Arc::clone(
            &self
                .inner
                .limit
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        );
        tokio::task::spawn_blocking(move || reader())
            .await
            .unwrap_or(DEFAULT_JOBS_PER_CONNECTION)
    }

    /// Waits until `id` is the oldest queued job of `conn` and a slot is free; `false`
    /// when it was stopped first (it then left the queue).
    async fn acquire_slot(&self, conn: Uuid, id: Uuid) -> bool {
        loop {
            let limit = self.read_limit().await;
            let changed = self.inner.slot_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut st = lock(&self.inner.state);
                let stopped = st.jobs.get(&id).is_none_or(|live| live.stop.is_stopped());
                let slots = st.conns.entry(conn).or_default();
                if stopped {
                    slots.queue.retain(|q| *q != id);
                    drop(st);
                    self.inner.slot_changed.notify_waiters();
                    return false;
                }
                if slots.queue.front() == Some(&id) && slots.running < limit {
                    slots.queue.pop_front();
                    slots.running += 1;
                    drop(st);
                    // The next job of the queue may fit too.
                    self.inner.slot_changed.notify_waiters();
                    return true;
                }
            }
            changed.await;
        }
    }

    fn release_slot(&self, conn: Uuid) {
        {
            let mut st = lock(&self.inner.state);
            if let Some(slots) = st.conns.get_mut(&conn) {
                slots.running = slots.running.saturating_sub(1);
                if slots.running == 0 && slots.queue.is_empty() {
                    st.conns.remove(&conn);
                }
            }
        }
        self.inner.slot_changed.notify_waiters();
    }

    /// Writes the job's current row (blocking pool); a failure is logged, never fatal.
    async fn persist(&self, id: Uuid) {
        let record = lock(&self.inner.state).jobs.get(&id).map(Live::record);
        if let Some(record) = record {
            self.store(record, false).await;
        }
    }

    async fn store(&self, record: JobRecord, prune: bool) {
        let repo = Arc::clone(&self.inner.repo);
        let saved = tokio::task::spawn_blocking(move || {
            repo.job_update(&record)?;
            if prune {
                repo.job_prune(KEEP_FINISHED)?;
            }
            Ok::<_, DbError>(())
        })
        .await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "could not save a job"),
            Err(e) => tracing::warn!(error = %e, "could not save a job"),
        }
    }

    /// Records the final state, emits it, persists it, removes the temporary directory
    /// and marks the job settled.
    async fn settle(&self, id: Uuid, fin: Final) {
        let (record, tmp) = {
            let mut st = lock(&self.inner.state);
            let Some(live) = st.jobs.get_mut(&id) else {
                return;
            };
            let s = &mut live.summary;
            s.state = fin.state;
            s.finished_at = Some(utc_millis_now());
            s.summary = fin.summary;
            s.result_path = fin.result_path;
            s.error = fin.error;
            s.outcome_unknown = fin.outcome_unknown;
            live.returned = true;
            live.cancel = None;
            live.kill = None;
            live.dirty = true;
            (live.record(), live.tmp_dir.take())
        };
        self.emit(id, true);
        self.store(record, true).await;
        if let Some(dir) = tmp {
            let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
            if let Ok(Err(e)) = removed {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, "could not delete a job's temporary files");
                }
            }
        }
        {
            let mut st = lock(&self.inner.state);
            if let Some(live) = st.jobs.get_mut(&id) {
                live.settled = true;
                st.finished.push_back(id);
            }
            while st.finished.len() > FINISHED_IN_MEMORY {
                if let Some(old) = st.finished.pop_front() {
                    st.jobs.remove(&old);
                }
            }
        }
        self.inner.settled.notify_waiters();
    }

    /// Sends the job's state with the log lines added since its last event. `force`:
    /// a state change (always sent); otherwise only when something changed.
    fn emit(&self, id: Uuid, force: bool) {
        let sink = self
            .inner
            .events
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let _order = lock(&self.inner.emit_lock);
        let event = {
            let mut st = lock(&self.inner.state);
            let Some(live) = st.jobs.get_mut(&id) else {
                return;
            };
            if !force && !live.dirty {
                return;
            }
            live.dirty = false;
            live.last_emit = Some(Instant::now());
            JobEvent {
                job: live.summary.clone(),
                new_log: live.pending.drain(..).collect(),
            }
        };
        if let Some(sink) = sink {
            sink.emit(&event);
        }
    }

    /// Emits now when the last event is at least [`EVENT_INTERVAL`] old, else once at
    /// the end of the interval (coalesced: the latest progress wins, log lines add up).
    fn changed(&self, id: Uuid) {
        enum Next {
            Now,
            At(Instant),
            Nothing,
        }
        let next = {
            let mut st = lock(&self.inner.state);
            match st.jobs.get_mut(&id) {
                Some(live) => match live.last_emit {
                    None => Next::Now,
                    Some(t) if t.elapsed() >= EVENT_INTERVAL => Next::Now,
                    Some(_) if live.flush_scheduled => Next::Nothing,
                    Some(t) => {
                        live.flush_scheduled = true;
                        Next::At(t + EVENT_INTERVAL)
                    }
                },
                None => Next::Nothing,
            }
        };
        match next {
            Next::Now => self.emit(id, false),
            Next::At(at) => match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    let mgr = self.clone();
                    rt.spawn(async move {
                        tokio::time::sleep_until(at.into()).await;
                        if let Some(live) = lock(&mgr.inner.state).jobs.get_mut(&id) {
                            live.flush_scheduled = false;
                        }
                        mgr.emit(id, false);
                    });
                }
                Err(_) => {
                    if let Some(live) = lock(&self.inner.state).jobs.get_mut(&id) {
                        live.flush_scheduled = false;
                    }
                    self.emit(id, false);
                }
            },
            Next::Nothing => {}
        }
    }

    fn set_progress(&self, id: Uuid, progress: JobProgress) {
        {
            let mut st = lock(&self.inner.state);
            let Some(live) = st.jobs.get_mut(&id) else {
                return;
            };
            live.summary.progress = progress;
            live.dirty = true;
        }
        self.changed(id);
    }

    fn push_log(&self, id: Uuid, line: JobLogLine) {
        {
            let mut st = lock(&self.inner.state);
            let Some(live) = st.jobs.get_mut(&id) else {
                return;
            };
            if live.pending.len() == MAX_LIVE_LOG_LINES {
                live.pending.pop_front();
            }
            live.pending.push_back(line.clone());
            live.log.push(line);
            live.dirty = true;
        }
        self.changed(id);
    }

    /// Registers a job that runs on its own execution path (R3): `running` at once, no
    /// slot; Stop calls `cancel`. Report progress and the outcome on the returned handle.
    pub fn track(&self, spec: JobSpec, cancel: Box<dyn Fn() + Send + Sync>) -> TrackedJob {
        let id = Uuid::new_v4();
        let summary = Self::new_summary(&spec, id, JobState::Running, &utc_millis_now());
        let mut live = {
            let mut st = lock(&self.inner.state);
            st.seq += 1;
            Live::new(st.seq, summary, spec.params)
        };
        live.kill = Some(Arc::from(cancel));
        if let Err(e) = self.inner.repo.job_insert(&live.record()) {
            tracing::warn!(error = %e, "could not save a tracked job");
        }
        lock(&self.inner.state).jobs.insert(id, live);
        self.emit(id, true);
        TrackedJob {
            mgr: self.clone(),
            id,
            finished: std::sync::atomic::AtomicBool::new(false),
        }
    }

    async fn finish_tracked(&self, id: Uuid, outcome: Result<JobDone, DbError>) {
        let stopped = {
            let mut st = lock(&self.inner.state);
            match st.jobs.get_mut(&id) {
                Some(live) if !live.returned => {
                    live.returned = true;
                    live.stop.is_stopped()
                }
                _ => return,
            }
        };
        let stopped = stopped
            || matches!(&outcome, Err(e) if matches!(e.unstaged(), DbError::Cancelled { .. }));
        self.settle(id, Final::of(outcome.map_err(JobFailure::from), stopped))
            .await;
    }

    /// Stops a queued or running job: raises its stop signal, then calls its statement
    /// canceller and/or kill closure. `false` when the job is not live.
    pub async fn cancel(&self, id: Uuid) -> bool {
        let (handle, kill) = {
            let mut st = lock(&self.inner.state);
            let Some(live) = st.jobs.get_mut(&id) else {
                return false;
            };
            if !live.summary.state.is_live() {
                return false;
            }
            if live.returned {
                // Already ending; a Stop no longer changes anything.
                return true;
            }
            live.stop.stop();
            (live.cancel.clone(), live.kill.clone())
        };
        self.inner.slot_changed.notify_waiters();
        if let Some(kill) = kill {
            kill();
        }
        if let Some(handle) = handle {
            if let Err(e) = handle.cancel().await {
                tracing::warn!(error = %e, "could not cancel a job's statement");
            }
        }
        true
    }

    fn live_ids(&self, filter: impl Fn(&Live) -> bool) -> Vec<Uuid> {
        lock(&self.inner.state)
            .jobs
            .iter()
            .filter(|(_, live)| live.summary.state.is_live() && filter(live))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Stops every queued/running job of `conn` (R9), tracked exports included.
    pub async fn cancel_connection(&self, conn: Uuid) {
        for id in self.live_ids(|live| live.summary.connection_id == Some(conn)) {
            self.cancel(id).await;
        }
    }

    /// Stops every job and waits up to `wait` for them to settle (R8).
    pub async fn stop_all(&self, wait: Duration) {
        for id in self.live_ids(|_| true) {
            self.cancel(id).await;
        }
        let _ = tokio::time::timeout(wait, async {
            loop {
                let settled = self.inner.settled.notified();
                tokio::pin!(settled);
                settled.as_mut().enable();
                if lock(&self.inner.state).jobs.values().all(|l| l.settled) {
                    return;
                }
                settled.await;
            }
        })
        .await;
    }

    /// Waits up to `timeout` for job `id` to settle (final state persisted). `true` when
    /// it did, or when no such job is in memory.
    pub async fn wait(&self, id: Uuid, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let settled = self.inner.settled.notified();
                tokio::pin!(settled);
                settled.as_mut().enable();
                if lock(&self.inner.state)
                    .jobs
                    .get(&id)
                    .is_none_or(|l| l.settled)
                {
                    return;
                }
                settled.await;
            }
        })
        .await
        .is_ok()
    }

    /// Queued + running jobs (tracked ones included).
    pub fn active_count(&self) -> u32 {
        let n = lock(&self.inner.state)
            .jobs
            .values()
            .filter(|l| l.summary.state.is_live())
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Live jobs (newest first), then stored ones (newest created first); at most
    /// `limit`. A finished job still in memory shows its full detail (server messages).
    pub fn list(&self, limit: u32) -> Result<Vec<JobSummary>, DbError> {
        let limit = limit as usize;
        let (mut live, finished) = {
            let st = lock(&self.inner.state);
            let mut live: Vec<(u64, JobSummary)> = Vec::new();
            let mut finished: HashMap<Uuid, JobSummary> = HashMap::new();
            for l in st.jobs.values() {
                if l.summary.state.is_live() {
                    live.push((l.seq, l.summary.clone()));
                } else {
                    finished.insert(l.summary.id, l.summary.clone());
                }
            }
            (live, finished)
        };
        live.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        let mut out: Vec<JobSummary> = live.into_iter().map(|(_, s)| s).take(limit).collect();
        if out.len() == limit {
            return Ok(out);
        }
        let live_ids: Vec<Uuid> = out.iter().map(|s| s.id).collect();
        let fetch = u32::try_from(limit + live_ids.len()).unwrap_or(u32::MAX);
        for r in self.inner.repo.job_list(fetch)? {
            if out.len() == limit {
                break;
            }
            if live_ids.contains(&r.id) {
                continue;
            }
            if let Some(s) = finished.get(&r.id) {
                out.push(s.clone());
            } else if let Some(d) = persist::from_record(r) {
                out.push(d.job);
            }
        }
        Ok(out)
    }

    /// A job with its log and parameters: from memory while the app runs, else from the
    /// store (persisted log, R6).
    pub fn detail(&self, id: Uuid) -> Result<JobDetail, DbError> {
        if let Some(live) = lock(&self.inner.state).jobs.get(&id) {
            return Ok(JobDetail {
                job: live.summary.clone(),
                log: live.log.lines(),
                dropped_lines: live.log.dropped,
                params: live.params.clone(),
            });
        }
        self.inner
            .repo
            .job_get(id)?
            .and_then(persist::from_record)
            .ok_or_else(|| DbError::Config {
                field: Some("id".into()),
                message: "this job no longer exists".into(),
            })
    }

    /// Deletes every finished job (store and memory); live jobs stay.
    pub fn clear_finished(&self) -> Result<(), DbError> {
        self.inner.repo.job_clear_finished()?;
        let mut st = lock(&self.inner.state);
        st.jobs
            .retain(|_, l| l.summary.state.is_live() || !l.settled);
        st.finished.clear();
        Ok(())
    }

    fn create_tmp_dir(&self, id: Uuid) -> Result<PathBuf, DbError> {
        let root = self.tmp_root().ok_or_else(|| DbError::Internal {
            message: "the job temporary directory is not set".into(),
        })?;
        let dir = root.join(id.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| DbError::Internal {
            message: format!("could not create the job's temporary directory: {e}"),
        })?;
        if let Some(live) = lock(&self.inner.state).jobs.get_mut(&id) {
            live.tmp_dir = Some(dir.clone());
        }
        Ok(dir)
    }
}

/// What a runner gets: its id, its stop signal and the reporting calls.
#[derive(Clone)]
pub struct JobCtx {
    id: Uuid,
    stop: StopSignal,
    mgr: JobManager,
}

fn line(level: JobLogLevel, message: String) -> JobLogLine {
    JobLogLine::new(utc_millis_now(), level, message)
}

impl JobCtx {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn stop(&self) -> StopSignal {
        self.stop.clone()
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.is_stopped()
    }

    pub fn progress(&self, phase: &str, done: u64, total: Option<u64>, unit: ProgressUnit) {
        self.mgr.set_progress(
            self.id,
            JobProgress {
                phase: phase.to_owned(),
                done: done as f64,
                total: total.map(|t| t as f64),
                unit,
            },
        );
    }

    /// A log line the app wrote (never cell values).
    pub fn log(&self, level: JobLogLevel, message: impl Into<String>) {
        self.mgr.push_log(self.id, line(level, message.into()));
    }

    /// A log line carrying a server message: shown in full while the app runs, stored
    /// as `Server error <code>` only (R6).
    pub fn log_server(&self, level: JobLogLevel, message: impl Into<String>, code: Option<&str>) {
        let mut l = line(level, message.into());
        l.server = true;
        l.server_code = code.map(str::to_owned);
        self.mgr.push_log(self.id, l);
    }

    /// The running statement's canceller (`None` between statements). Set after a Stop,
    /// it is called at once.
    pub fn set_cancel(&self, handle: Option<CancelHandle>) {
        let late = {
            let mut st = lock(&self.mgr.inner.state);
            match st.jobs.get_mut(&self.id) {
                Some(live) => {
                    live.cancel = handle.clone();
                    live.stop.is_stopped()
                }
                None => false,
            }
        };
        if let (true, Some(handle)) = (late, handle) {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    let _ = handle.cancel().await;
                });
            }
        }
    }

    /// Kills an external process on Stop (`None` once it exited). Set after a Stop, it
    /// is called at once.
    pub fn set_kill(&self, kill: Option<Box<dyn Fn() + Send + Sync>>) {
        let kill: Option<Kill> = kill.map(Arc::from);
        let late = {
            let mut st = lock(&self.mgr.inner.state);
            match st.jobs.get_mut(&self.id) {
                Some(live) => {
                    live.kill = kill.clone();
                    live.stop.is_stopped()
                }
                None => false,
            }
        };
        if let (true, Some(kill)) = (late, kill) {
            kill();
        }
    }

    /// `<tmp_root>/<job id>/`, created on first use and removed when the job ends.
    pub fn tmp_dir(&self) -> Result<PathBuf, DbError> {
        self.mgr.create_tmp_dir(self.id)
    }
}

/// A job registered with [`JobManager::track`]. Dropped without [`TrackedJob::finish`],
/// it ends `failed`.
pub struct TrackedJob {
    mgr: JobManager,
    id: Uuid,
    finished: std::sync::atomic::AtomicBool,
}

impl TrackedJob {
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Rows written so far.
    pub fn progress(&self, rows: u64) {
        self.mgr.set_progress(
            self.id,
            JobProgress {
                phase: "Rows".into(),
                done: rows as f64,
                total: None,
                unit: ProgressUnit::Rows,
            },
        );
    }

    pub fn log(&self, level: JobLogLevel, message: impl Into<String>) {
        self.mgr.push_log(self.id, line(level, message.into()));
    }

    /// Records the outcome (the first call wins): `Ok` → done; an error after a Stop or
    /// a `Cancelled` error → cancelled; else failed.
    pub async fn finish(&self, outcome: Result<JobDone, DbError>) {
        if self
            .finished
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.mgr.finish_tracked(self.id, outcome).await;
    }
}

impl Drop for TrackedJob {
    fn drop(&mut self) {
        if self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let mgr = self.mgr.clone();
            let id = self.id;
            rt.spawn(async move {
                mgr.finish_tracked(
                    id,
                    Err(DbError::Internal {
                        message: "the job ended without reporting its outcome".into(),
                    }),
                )
                .await;
            });
        }
    }
}
