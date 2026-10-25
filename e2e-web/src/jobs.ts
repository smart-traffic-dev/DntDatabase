import type { JobDetail, JobEvent, JobLogLine, JobProgress, JobSummary } from '../../ui/src/bindings';

/** A job as `job_list` and `job://progress` carry it: a running import with no progress yet. */
export function jobSummary(over: Partial<JobSummary> = {}): JobSummary {
  const now = new Date().toISOString();
  return {
    id: 'job-1',
    kind: 'import',
    connection_id: null,
    title: 'Import orders.csv into orders',
    state: 'running',
    progress: jobProgress(),
    created_at: now,
    started_at: now,
    finished_at: null,
    result_path: null,
    summary: null,
    error: null,
    outcome_unknown: false,
    rerunnable: true,
    ...over,
  };
}

export function jobProgress(over: Partial<JobProgress> = {}): JobProgress {
  return { phase: '', done: 0, total: null, unit: 'none', ...over };
}

export function logLine(message: string, level: JobLogLine['level'] = 'info'): JobLogLine {
  return { at: new Date().toISOString(), level, message };
}

/** The payload of a `job://progress` event. */
export function jobEvent(job: JobSummary, newLog: JobLogLine[] = []): JobEvent {
  return { job, new_log: newLog };
}

/** What `job_detail` answers for `job`. */
export function jobDetail(job: JobSummary, log: JobLogLine[] = [], over: Partial<JobDetail> = {}): JobDetail {
  return { job, log, dropped_lines: 0, params: null, ...over };
}
