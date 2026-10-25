import type { JobDetail, JobSummary } from '../bindings';

/** A job summary for tests: a running export on no connection unless overridden. */
export function jobSummary(over: Partial<JobSummary> = {}): JobSummary {
  return {
    id: 'job-1',
    kind: 'export',
    connection_id: null,
    title: 'Export orders to orders.csv',
    state: 'running',
    progress: { phase: '', done: 0, total: null, unit: 'none' },
    created_at: '2026-10-25T10:00:00.000Z',
    started_at: '2026-10-25T10:00:00.000Z',
    finished_at: null,
    result_path: null,
    summary: null,
    error: null,
    outcome_unknown: false,
    rerunnable: false,
    ...over,
  };
}

/** A job detail around `jobSummary(over)`. */
export function jobDetail(over: Partial<JobSummary> = {}, rest: Partial<Omit<JobDetail, 'job'>> = {}): JobDetail {
  return { job: jobSummary(over), log: [], dropped_lines: 0, params: null, ...rest };
}
