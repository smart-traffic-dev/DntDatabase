import { listen } from '@tauri-apps/api/event';
import { create } from 'zustand';
import { api } from '../api';
import { commands, type JobDetail, type JobEvent, type JobState, type JobSummary } from '../bindings';

/** Poll interval of `job_list` while a job is queued or running (R4: a missed event only delays the panel). */
export const POLL_MS = 2000;
/** The app event the backend emits for every job change (throttled, final state never dropped). */
export const JOB_EVENT = 'job://progress';
/** Jobs asked for per list (the backend's default). */
const LIST_LIMIT = 200;

export const isLive = (state: JobState) => state === 'queued' || state === 'running';

/** Live jobs first, then the newest created. */
function sortOrder(jobs: Record<string, JobSummary>): string[] {
  return Object.values(jobs)
    .sort((a, b) => {
      const live = Number(isLive(b.state)) - Number(isLive(a.state));
      if (live !== 0) return live;
      return a.created_at < b.created_at ? 1 : a.created_at > b.created_at ? -1 : 0;
    })
    .map((j) => j.id);
}

interface JobsState {
  jobs: Record<string, JobSummary>;
  /** Ids in display order: live first, then newest. */
  order: string[];
  /** A `job_list` answer has arrived at least once. */
  loaded: boolean;
  /** The job whose detail the Jobs panel shows (null: the list). */
  openId: string | null;
  /** Shows `id`'s detail in the Jobs panel (e.g. the import just started); null returns to the list. */
  openJob(id: string | null): void;
  /** Reads `job_list` (failures are ignored: the next poll or event catches up). */
  load(): Promise<void>;
  /** Listens to `job://progress` (ignored outside Tauri) and polls while jobs are live; returns the unsubscribe. */
  subscribe(): () => void;
  /** Applies one `job://progress` payload. */
  apply(ev: JobEvent): void;
  /** Stops a job (`job_cancel`); rejects with the backend error. */
  cancel(id: string): Promise<void>;
  detail(id: string): Promise<JobDetail>;
  /** Deletes the finished jobs; live ones stay. */
  clearFinished(): Promise<void>;
  /** Queued + running jobs known to the UI. */
  activeCount(): number;
  reset(): void;
}

/**
 * The backend's count of queued and running jobs (`job_active_count`, tracked exports included),
 * for the close guard; what the UI knows when the backend cannot answer.
 */
export async function liveJobCount(): Promise<number> {
  try {
    const result = await commands.jobActiveCount();
    if (result.status === 'ok') return result.data;
  } catch {
    // No backend answer: fall through to the UI's own view.
  }
  return useJobs.getState().activeCount();
}

let pollTimer: ReturnType<typeof setInterval> | null = null;
let subscribers = 0;
/** `load` calls sent / the newest one applied: an older answer arriving later is dropped. */
let loadSeq = 0;
let loadApplied = 0;

/** A final state never goes back to live (an answer or event sent before the final one arrived late). */
const merge = (known: JobSummary | undefined, incoming: JobSummary) =>
  known && !isLive(known.state) && isLive(incoming.state) ? known : incoming;

function stopPolling() {
  if (pollTimer !== null) clearInterval(pollTimer);
  pollTimer = null;
}

/** Starts or stops the poll so it runs exactly while something is live and someone subscribed. */
function syncPolling() {
  const want = subscribers > 0 && useJobs.getState().activeCount() > 0;
  if (want && pollTimer === null) pollTimer = setInterval(() => void useJobs.getState().load(), POLL_MS);
  else if (!want) stopPolling();
}

export const useJobs = create<JobsState>()((set, get) => ({
  jobs: {},
  order: [],
  loaded: false,
  openId: null,
  openJob: (openId) => set({ openId }),
  async load() {
    const seq = ++loadSeq;
    // Straight to the bindings: a poll that fails every 2 s must not raise the internal-error toast.
    let result;
    try {
      result = await commands.jobList(LIST_LIMIT);
    } catch {
      return;
    }
    if (result.status !== 'ok') return;
    // Overlapping loads: a newer answer was applied already.
    if (seq < loadApplied) return;
    loadApplied = seq;
    const current = get().jobs;
    const jobs: Record<string, JobSummary> = {};
    // An answer sent before a final event must not bring the job back to life.
    for (const job of result.data) jobs[job.id] = merge(current[job.id], job);
    // A job that just ended may have been sent by event but not be in this answer yet.
    for (const [id, known] of Object.entries(current)) if (!(id in jobs) && isLive(known.state)) jobs[id] = known;
    set({ jobs, order: sortOrder(jobs), loaded: true });
    syncPolling();
  },
  subscribe() {
    subscribers++;
    let unlisten: (() => void) | null = null;
    let closed = false;
    try {
      void Promise.resolve(listen<JobEvent>(JOB_EVENT, (e) => get().apply(e.payload)))
        .then((f) => {
          if (closed) f();
          else unlisten = f;
        })
        .catch(() => {
          // No Tauri event bridge (plain browser): polling alone keeps the panel current.
        });
    } catch {
      // Same: `listen` threw synchronously outside Tauri.
    }
    void get().load();
    return () => {
      if (closed) return;
      closed = true;
      subscribers--;
      unlisten?.();
      syncPolling();
    };
  },
  apply(ev) {
    const current = get().jobs;
    const jobs = { ...current, [ev.job.id]: merge(current[ev.job.id], ev.job) };
    set({ jobs, order: sortOrder(jobs) });
    syncPolling();
  },
  async cancel(id) {
    await api.jobCancel(id);
  },
  detail: (id) => api.jobDetail(id),
  async clearFinished() {
    await api.jobClearHistory();
    const jobs = Object.fromEntries(Object.entries(get().jobs).filter(([, j]) => isLive(j.state)));
    set({ jobs, order: sortOrder(jobs) });
    await get().load();
  },
  activeCount() {
    let n = 0;
    for (const j of Object.values(get().jobs)) if (isLive(j.state)) n++;
    return n;
  },
  reset() {
    stopPolling();
    subscribers = 0;
    loadSeq = 0;
    loadApplied = 0;
    set({ jobs: {}, order: [], loaded: false, openId: null });
  },
}));
