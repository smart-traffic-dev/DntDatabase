import type { JobKind, JobProgress, JobSummary } from '../bindings';
import { formatCount } from '../lib/plural';

/** Texts of the Jobs panel: kinds, states, progress and elapsed time. */

/** Base-1024 size with one decimal ("1.5 KB"), as the object tab shows sizes. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (Number(value.toFixed(1)) >= 1024 && unit < units.length - 1);
  return `${value.toFixed(1)} ${units[unit]}`;
}

export const KIND_LABEL: Record<JobKind, string> = {
  import: 'Import',
  import_dry_run: 'Import dry run',
  dump: 'Dump',
  tool_dump: 'Dump (external tool)',
  restore: 'Restore',
  tool_restore: 'Restore (external tool)',
  export: 'Export',
};

/** The tone of a job's state chip and progress (CSS modifier). */
export type JobTone = 'live' | 'ok' | 'error' | 'warn';

/** "Queued", "Running", "Done", "Failed", "Cancelled", or "Outcome unknown" when a failure may have written. */
export function stateLabel(job: Pick<JobSummary, 'state' | 'outcome_unknown'>): string {
  if (job.outcome_unknown && job.state !== 'done') return 'Outcome unknown';
  switch (job.state) {
    case 'queued':
      return 'Queued';
    case 'running':
      return 'Running';
    case 'done':
      return 'Done';
    case 'failed':
      return 'Failed';
    case 'cancelled':
      return 'Cancelled';
  }
}

export function toneOf(job: Pick<JobSummary, 'state' | 'outcome_unknown'>): JobTone {
  if (job.outcome_unknown && job.state !== 'done') return 'warn';
  switch (job.state) {
    case 'queued':
    case 'running':
      return 'live';
    case 'done':
      return 'ok';
    case 'failed':
      return 'error';
    case 'cancelled':
      return 'warn';
  }
}

function amount(n: number, unit: JobProgress['unit']): string {
  return unit === 'bytes' ? formatBytes(n) : formatCount(n);
}

/**
 * "Exporting · 1,200 / 5,000 rows", "1.5 KB", "Starting": phase, then done (/ total) in its unit.
 * A phase that only names the unit ("Rows") is not repeated before it.
 */
export function progressText(p: JobProgress): string {
  const parts: string[] = [];
  const word = p.unit === 'rows' ? 'rows' : p.unit === 'statements' ? 'statements' : '';
  if (p.phase && !(word && p.phase.trim().toLowerCase() === word)) parts.push(p.phase);
  if (p.unit !== 'none') {
    const total = p.total !== null ? ` / ${amount(p.total, p.unit)}` : '';
    parts.push(`${amount(p.done, p.unit)}${total}${word ? ` ${word}` : ''}`);
  }
  return parts.join(' · ');
}

/** The share done, 0..100 (whole percent), or `null` when the total is unknown. */
export function progressPercent(p: JobProgress): number | null {
  if (p.total === null || p.total <= 0) return null;
  return Math.round(Math.min(1, Math.max(0, p.done / p.total)) * 100);
}

/** Milliseconds the job has run: from start to its end (or `now` while it runs); `null` before it starts. */
export function elapsedMs(job: Pick<JobSummary, 'started_at' | 'finished_at'>, now: number): number | null {
  if (!job.started_at) return null;
  const start = Date.parse(job.started_at);
  if (Number.isNaN(start)) return null;
  const end = job.finished_at ? Date.parse(job.finished_at) : now;
  return Number.isNaN(end) ? null : Math.max(0, end - start);
}
