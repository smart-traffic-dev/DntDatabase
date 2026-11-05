import { describe, expect, it } from 'vitest';
import { draftToFilter, emptyDraft, newFilterRow, type FilterDraft } from './tableData';

const raw = (rawText: string): FilterDraft => ({ ...emptyDraft(), rawMode: true, rawText });

describe('draftToFilter', () => {
  it('command_raw_filter_sends_raw_json', () => {
    expect(draftToFilter(raw('  { "status": "paid" } '), 'command')).toEqual({
      filter: { mode: 'and', conditions: [], raw_json: '{ "status": "paid" }' },
      error: null,
    });
    expect(draftToFilter(raw('   '), 'command')).toEqual({ filter: null, error: null });
  });

  it('sql_raw_filter_unchanged', () => {
    const where = { filter: { mode: 'and', conditions: [], raw_where: "status = 'paid'" }, error: null };
    expect(draftToFilter(raw(" status = 'paid' "), 'sql')).toEqual(where);
    expect(draftToFilter(raw(" status = 'paid' "))).toEqual(where);
    // The condition rows are the same structured filter for both families.
    const rows: FilterDraft = { mode: 'or', rows: [newFilterRow('status', 'eq', 'paid')] };
    expect(draftToFilter(rows, 'command')).toEqual(draftToFilter(rows, 'sql'));
  });
});
