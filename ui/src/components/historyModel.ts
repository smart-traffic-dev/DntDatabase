import type { HistoryEntry, HistoryStatus } from '../bindings';

/** Error kinds after which the server may or may not have applied the statement. */
const UNKNOWN_OUTCOME_KINDS = new Set(['timeout', 'connection_lost']);

/**
 * The status of a row. A statement stopped by a timeout, a lost connection or a cancel may
 * have taken effect, so it never reads as a plain error ("not applied"). A row the backend marked
 * outcome_unknown (any status: an unanswered COMMIT leaves ok statements unknown) reads the same.
 */
export function statusOf(row: Pick<HistoryEntry, 'status' | 'error_kind' | 'error_code' | 'outcome_unknown'>): {
  text: string;
  tone: HistoryStatus | 'unknown';
  hint: string | undefined;
} {
  const code = row.error_code ? ` (${row.error_code})` : '';
  if (row.outcome_unknown && row.status === 'ok') {
    return {
      text: 'Outcome unknown',
      tone: 'unknown',
      hint: 'The server did not confirm the commit: this statement may or may not have been applied. Check the data before running it again.',
    };
  }
  if (row.status === 'ok') return { text: 'OK', tone: 'ok', hint: undefined };
  if (row.outcome_unknown || (row.error_kind !== null && UNKNOWN_OUTCOME_KINDS.has(row.error_kind))) {
    const why = row.error_kind === 'timeout' ? 'timed out' : row.error_kind === 'cancelled' ? 'was cancelled' : 'lost its connection';
    return {
      text: `Outcome unknown${code}`,
      tone: 'unknown',
      hint: `The statement ${why} before the server confirmed it: it may have been applied. Check the data before running it again.`,
    };
  }
  if (row.status === 'cancelled' || row.error_kind === 'cancelled') {
    return {
      text: `Cancelled — may have been applied${code}`,
      tone: 'cancelled',
      hint: 'The statement was cancelled while it ran: the server may have applied it before the cancel arrived.',
    };
  }
  return {
    text: `Error${code}`,
    tone: 'error',
    hint: row.error_kind ? `Failed (${row.error_kind}${row.error_code ? `, code ${row.error_code}` : ''}).` : undefined,
  };
}

/** Why a data-edit row is not inserted into the editor. */
export const NOT_RE_RUNNABLE = "This statement was shortened or redacted for history and can't be re-run.";

/**
 * Whether a row's text may go back into the editor. A data edit marked redacted holds a
 * shortened literal (`'abc…' /* N chars *\/`) or a placeholder: valid SQL, but not what ran.
 */
export const canReRun = (row: Pick<HistoryEntry, 'origin' | 'redacted'>) => !(row.origin === 'data_edit' && row.redacted);

export const firstLine = (sql: string) => sql.split(/\r?\n/, 1)[0] ?? '';

/**
 * An Explain run is stored as its wrapper (`EXPLAIN [ANALYZE] [FORMAT=JSON | (ANALYZE, FORMAT JSON)] <stmt>`,
 * or MariaDB's `ANALYZE [FORMAT=JSON] <stmt>`): the statement inside, and whether it was an Analyze.
 */
export function explainOf(row: Pick<HistoryEntry, 'sql_text'>): { inner: string; analyze: boolean } | null {
  const m = /^\s*(EXPLAIN|ANALYZE)\s+(?:FORMAT\s*=\s*\w+\s+|\(([^)]*)\)\s+|(ANALYZE\s+)(?:FORMAT\s*=\s*\w+\s+)?)?/i.exec(row.sql_text);
  if (!m) return null;
  const analyze = m[1]!.toUpperCase() === 'ANALYZE' || !!m[3] || /\bANALYZE\b/i.test(m[2] ?? '');
  return { inner: row.sql_text.slice(m[0].length), analyze };
}

export type StatusFilter = 'all' | 'ok' | 'errors';

/** All, only the clean successes, or every other outcome (errors, cancels, unknown outcomes). */
export function filterByStatus(rows: HistoryEntry[], filter: StatusFilter): HistoryEntry[] {
  if (filter === 'all') return rows;
  return rows.filter((r) => (statusOf(r).tone === 'ok') === (filter === 'ok'));
}

export interface HistoryGroup {
  label: 'Today' | 'Yesterday' | 'Older';
  rows: HistoryEntry[];
}

const dayStart = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
const DAY_MS = 86_400_000;

/** How many calendar days (local time) before `now` a time is; null when it does not parse. */
function daysAgo(iso: string, now: Date): number | null {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return null;
  return Math.round((dayStart(now) - dayStart(d)) / DAY_MS);
}

/** The rows (newest first, as listed) under Today / Yesterday / Older; empty groups are left out. */
export function groupByDay(rows: HistoryEntry[], now: Date = new Date()): HistoryGroup[] {
  const groups: HistoryGroup[] = [
    { label: 'Today', rows: [] },
    { label: 'Yesterday', rows: [] },
    { label: 'Older', rows: [] },
  ];
  for (const row of rows) {
    const ago = daysAgo(row.started_at, now);
    groups[ago === 0 ? 0 : ago === 1 ? 1 : 2]!.rows.push(row);
  }
  return groups.filter((g) => g.rows.length > 0);
}

const pad = (n: number) => String(n).padStart(2, '0');
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** `14:03:09` for today and yesterday, `16 Oct 14:03` for older entries. */
export function timeText(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const ago = daysAgo(iso, now);
  if (ago === 0 || ago === 1) return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  return `${pad(d.getDate())} ${MONTHS[d.getMonth()]} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** The row's result in a few words: the row count, the affected rows, or the failure. */
export function detailOf(row: HistoryEntry): string {
  const status = statusOf(row);
  if (status.tone !== 'ok') return status.text;
  if (row.row_count !== null) return `${row.row_count} ${row.row_count === 1 ? 'row' : 'rows'}`;
  if (row.affected !== null) return `${row.affected} affected`;
  return '';
}
