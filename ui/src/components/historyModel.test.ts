import { describe, expect, it } from 'vitest';
import type { HistoryEntry } from '../bindings';
import { detailOf, filterByStatus, groupByDay, timeText } from './historyModel';

const entry = (id: number, startedAt: string, patch: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id,
  connection_id: 'c1',
  database: 'shop',
  sql_text: 'SELECT 1',
  redacted: false,
  kind: 'read',
  started_at: startedAt,
  elapsed_ms: 12,
  status: 'ok',
  row_count: 3,
  affected: null,
  error_kind: null,
  error_code: null,
  origin: 'query',
  outcome_unknown: false,
  ...patch,
});

// Local noon, so a few hours either way stays on the same calendar day in any time zone.
const now = new Date(2026, 9, 21, 12, 0, 0);
const at = (daysAgo: number, hour = 9) => new Date(2026, 9, 21 - daysAgo, hour, 5, 7).toISOString();

describe('groupByDay', () => {
  it('splits into Today / Yesterday / Older, keeping order and dropping empty groups', () => {
    const rows = [entry(1, at(0, 11)), entry(2, at(0, 8)), entry(3, at(1)), entry(4, at(2)), entry(5, at(40))];
    const groups = groupByDay(rows, now);
    expect(groups.map((g) => [g.label, g.rows.map((r) => r.id)])).toEqual([
      ['Today', [1, 2]],
      ['Yesterday', [3]],
      ['Older', [4, 5]],
    ]);
    expect(groupByDay([entry(1, at(3))], now).map((g) => g.label)).toEqual(['Older']);
    expect(groupByDay([], now)).toEqual([]);
  });

  it('treats an unparsable time as Older', () => {
    expect(groupByDay([entry(1, 'never')], now)[0]!.label).toBe('Older');
  });
});

describe('filterByStatus', () => {
  const rows = [
    entry(1, at(0)),
    entry(2, at(0), { status: 'error', error_kind: 'query', error_code: '1062' }),
    entry(3, at(0), { status: 'cancelled', error_kind: 'cancelled' }),
    entry(4, at(0), { outcome_unknown: true }),
  ];
  it('All keeps everything, OK only clean successes, Errors every other outcome', () => {
    expect(filterByStatus(rows, 'all').map((r) => r.id)).toEqual([1, 2, 3, 4]);
    expect(filterByStatus(rows, 'ok').map((r) => r.id)).toEqual([1]);
    expect(filterByStatus(rows, 'errors').map((r) => r.id)).toEqual([2, 3, 4]);
  });
});

describe('timeText', () => {
  it('shows the clock for today and yesterday, the date too for older entries', () => {
    expect(timeText(at(0, 9), now)).toBe('09:05:07');
    expect(timeText(at(1, 9), now)).toBe('09:05:07');
    expect(timeText(at(5, 9), now)).toMatch(/^16 Oct 09:05$/);
    expect(timeText('nope', now)).toBe('nope');
  });
});

describe('detailOf', () => {
  it('reads rows for a read, affected rows for a write, the code for a failure', () => {
    expect(detailOf(entry(1, at(0)))).toBe('3 rows');
    expect(detailOf(entry(1, at(0), { row_count: 1 }))).toBe('1 row');
    expect(detailOf(entry(1, at(0), { row_count: null, affected: 5 }))).toBe('5 affected');
    expect(detailOf(entry(1, at(0), { row_count: null, affected: null }))).toBe('');
    expect(detailOf(entry(1, at(0), { status: 'error', error_kind: 'query', error_code: '1062' }))).toBe('Error (1062)');
  });
});
