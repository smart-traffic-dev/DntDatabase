import { describe, expect, it } from 'vitest';
import type { Value } from '../../bindings';
import { CHUNK } from '../../store/queryRun';
import { MAX_MATCHES, findMatches, firstMatchFrom, parseGoToRow, stepIndex } from './gridFind';

const t = (v: string): Value => ({ t: 'Text', v });
const cols = [0, 1].map((i) => ({ name: `c${i}` }) as never);
const make = (rows: Value[][]) => ({
  columns: cols,
  rowCount: rows.length,
  chunks: Array.from({ length: Math.ceil(rows.length / CHUNK) }, (_, i) => rows.slice(i * CHUNK, (i + 1) * CHUNK)),
});

describe('findMatches', () => {
  const data = make([
    [t('Alpha'), t('beta')],
    [t('gamma'), { t: 'Int', v: 42 }],
    [{ t: 'Null' }, t('ALPHA beta')],
  ]);

  it('is_case_insensitive_and_returns_cells_in_reading_order', () => {
    expect(findMatches(data, 'alpha').matches).toEqual([
      { row: 0, col: 0 },
      { row: 2, col: 1 },
    ]);
  });

  it('searches_numbers_and_null_text', () => {
    expect(findMatches(data, '42').matches).toEqual([{ row: 1, col: 1 }]);
    expect(findMatches(data, 'null').matches).toEqual([{ row: 2, col: 0 }]);
  });

  it('matches_nothing_for_an_empty_query', () => {
    expect(findMatches(data, '')).toEqual({ matches: [], capped: false });
  });

  it('reads_across_chunks', () => {
    const rows = Array.from({ length: CHUNK + 5 }, (_, i) => [t(i === CHUNK + 2 ? 'needle' : 'hay'), t('x')]);
    expect(findMatches(make(rows), 'NEEDLE').matches).toEqual([{ row: CHUNK + 2, col: 0 }]);
  });

  it('matches_unicode_case_insensitively', () => {
    expect(findMatches(make([[t('ÉCOLE'), t('x')]]), 'école').matches).toHaveLength(1);
  });

  it('stops_collecting_at_the_cap', () => {
    const rows = Array.from({ length: MAX_MATCHES + 10 }, () => [t('a'), t('b')]);
    const r = findMatches(make(rows), 'a');
    expect(r.matches).toHaveLength(MAX_MATCHES);
    expect(r.capped).toBe(true);
  });

  it('skips_rows_that_are_not_loaded', () => {
    expect(findMatches({ columns: cols, rowCount: 3, chunks: [[[t('a'), t('b')]]] }, 'a').matches).toHaveLength(1);
  });
});

describe('stepIndex', () => {
  it('wraps_in_both_directions_and_starts_at_the_ends', () => {
    expect(stepIndex(null, 1, 3)).toBe(0);
    expect(stepIndex(null, -1, 3)).toBe(2);
    expect(stepIndex(2, 1, 3)).toBe(0);
    expect(stepIndex(0, -1, 3)).toBe(2);
    expect(stepIndex(0, 1, 0)).toBe(-1);
  });
});

describe('firstMatchFrom', () => {
  const ms = [
    { row: 0, col: 1 },
    { row: 3, col: 0 },
    { row: 3, col: 2 },
  ];
  it('picks_the_first_match_at_or_after_the_cell_else_wraps', () => {
    expect(firstMatchFrom(ms, 0, 0)).toBe(0);
    expect(firstMatchFrom(ms, 0, 2)).toBe(1);
    expect(firstMatchFrom(ms, 3, 1)).toBe(2);
    expect(firstMatchFrom(ms, 9, 0)).toBe(0);
  });
});

describe('parseGoToRow', () => {
  it('maps_a_one_based_number_to_a_row', () => {
    expect(parseGoToRow('1', 100)).toEqual({ ok: true, row: 0 });
    expect(parseGoToRow(' 100 ', 100)).toEqual({ ok: true, row: 99 });
    expect(parseGoToRow('1,000', 5000)).toEqual({ ok: true, row: 999 });
  });

  it('respects_the_page_offset', () => {
    expect(parseGoToRow('501', 500, 501)).toEqual({ ok: true, row: 0 });
    expect(parseGoToRow('1000', 500, 501)).toEqual({ ok: true, row: 499 });
    expect(parseGoToRow('500', 500, 501).ok).toBe(false);
    expect(parseGoToRow('1001', 500, 501).ok).toBe(false);
  });

  it('rejects_junk_and_out_of_range', () => {
    expect(parseGoToRow('', 10).ok).toBe(false);
    expect(parseGoToRow('abc', 10).ok).toBe(false);
    expect(parseGoToRow('-3', 10).ok).toBe(false);
    expect(parseGoToRow('1.5', 10).ok).toBe(false);
    expect(parseGoToRow('0', 10).ok).toBe(false);
    expect(parseGoToRow('11', 10)).toEqual({ ok: false, message: 'Out of range: rows 1–10' });
    expect(parseGoToRow('1', 0)).toEqual({ ok: false, message: 'No rows' });
  });
});
