import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { JobEvent, JobSummary } from '../bindings';
import { callsOf, mockBackend, resetStores } from '../test/backend';
import { jobSummary } from '../test/jobs';
import { POLL_MS, useJobs } from './jobs';

const listeners = vi.hoisted(() => new Map<string, (e: { payload: unknown }) => void>());
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (name: string, handler: (e: { payload: unknown }) => void) => {
    listeners.set(name, handler);
    return () => listeners.delete(name);
  }),
}));

const event = (job: JobSummary, newLog: JobEvent['new_log'] = []): JobEvent => ({ job, new_log: newLog });

beforeEach(() => {
  resetStores();
  listeners.clear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('useJobs', () => {
  it('applies events: a new job, its progress and its final state', () => {
    const { apply } = useJobs.getState();
    apply(event(jobSummary({ id: 'a', state: 'queued' })));
    expect(useJobs.getState().order).toEqual(['a']);
    expect(useJobs.getState().activeCount()).toBe(1);

    apply(event(jobSummary({ id: 'a', state: 'running', progress: { phase: 'Rows', done: 10, total: 100, unit: 'rows' } })));
    expect(useJobs.getState().jobs.a!.progress.done).toBe(10);

    apply(event(jobSummary({ id: 'a', state: 'done', summary: '100 rows' })));
    expect(useJobs.getState().jobs.a!.state).toBe('done');
    expect(useJobs.getState().activeCount()).toBe(0);
  });

  it('orders live jobs first, then the newest', () => {
    const { apply } = useJobs.getState();
    apply(event(jobSummary({ id: 'old', state: 'done', created_at: '2026-10-25T10:00:00.000Z' })));
    apply(event(jobSummary({ id: 'new', state: 'failed', created_at: '2026-10-25T11:00:00.000Z' })));
    apply(event(jobSummary({ id: 'live', state: 'running', created_at: '2026-10-25T09:00:00.000Z' })));
    expect(useJobs.getState().order).toEqual(['live', 'new', 'old']);
  });

  it('a poll answer that is older than a final event does not bring the job back to running', async () => {
    useJobs.getState().apply(event(jobSummary({ id: 'a', state: 'cancelled' })));
    mockBackend({ job_list: () => [jobSummary({ id: 'a', state: 'running' }), jobSummary({ id: 'b', state: 'done' })] });
    await useJobs.getState().load();
    expect(useJobs.getState().jobs.a!.state).toBe('cancelled');
    expect(useJobs.getState().order).toEqual(['a', 'b']);
    expect(useJobs.getState().loaded).toBe(true);
  });

  it('listens to job://progress and polls job_list every 2 s only while a job is live', async () => {
    vi.useFakeTimers();
    let list: JobSummary[] = [jobSummary({ id: 'a', state: 'running' })];
    mockBackend({ job_list: () => list });
    const stop = useJobs.getState().subscribe();
    await vi.advanceTimersByTimeAsync(0);
    expect(callsOf('job_list')).toHaveLength(1);
    expect(listeners.has('job://progress')).toBe(true);

    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(callsOf('job_list')).toHaveLength(2);

    // The event ends the job: polling stops.
    list = [jobSummary({ id: 'a', state: 'done' })];
    listeners.get('job://progress')!({ payload: event(jobSummary({ id: 'a', state: 'done' })) });
    expect(useJobs.getState().jobs.a!.state).toBe('done');
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(callsOf('job_list')).toHaveLength(2);

    // A new live job starts it again.
    listeners.get('job://progress')!({ payload: event(jobSummary({ id: 'b', state: 'queued' })) });
    list = [jobSummary({ id: 'b', state: 'queued' })];
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(callsOf('job_list')).toHaveLength(3);

    stop();
    expect(listeners.has('job://progress')).toBe(false);
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(callsOf('job_list')).toHaveLength(3);
  });

  it('works without Tauri events (only polling)', async () => {
    const { listen } = await import('@tauri-apps/api/event');
    vi.mocked(listen).mockImplementationOnce(() => {
      throw new Error('no tauri');
    });
    mockBackend({ job_list: () => [] });
    const stop = useJobs.getState().subscribe();
    await vi.waitFor(() => expect(useJobs.getState().loaded).toBe(true));
    stop();
  });

  it('cancel calls job_cancel; detail and clearFinished call the backend', async () => {
    mockBackend({
      job_cancel: () => true,
      job_detail: (a) => ({ job: jobSummary({ id: String(a.id) }), log: [], dropped_lines: 0, params: null }),
      job_clear_history: () => null,
      job_list: () => [jobSummary({ id: 'live', state: 'running' })],
    });
    useJobs.getState().apply(event(jobSummary({ id: 'x', state: 'done' })));
    await useJobs.getState().cancel('a');
    expect(callsOf('job_cancel')).toEqual([{ id: 'a' }]);
    expect((await useJobs.getState().detail('b')).job.id).toBe('b');
    await useJobs.getState().clearFinished();
    expect(callsOf('job_clear_history')).toHaveLength(1);
    expect(useJobs.getState().order).toEqual(['live']);
  });
});
