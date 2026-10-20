import { describe, expect, it, vi } from 'vitest';
import type { ResultSet } from '../../store/queryRun';
import { initialWidths, overridesFor, withResize, selectionCells, tooLargeMessage } from './ResultGrid';

const result: ResultSet = {
  id: 1,
  index: 0,
  columns: [0, 1].map((i) => ({ name: `c${i}`, db_type: 'text', logical: 'Text' as const, nullable: true, origin_table: null, origin_column: null })),
  chunks: [[[{ t: 'Text', v: 'a\tb' }, { t: 'Null' }], [{ t: 'Int', v: 2 }, { t: 'Text', v: 'z' }]]],
  rowCount: 2,
  truncated: false,
  affected: null,
  elapsedMs: 1,
  done: true,
};

describe('initialWidths', () => {
  it('fits_each_column_to_its_sampled_values', () => {
    const wide: ResultSet = {
      ...result,
      chunks: [[[{ t: 'Int', v: '9223372036854775807' }, { t: 'Null' }]]],
      rowCount: 1,
    };
    const [a, b] = initialWidths(wide);
    expect(a).toBeGreaterThanOrEqual(19 * 7.9);
    expect(b).toBe(60);
  });
});

describe('resize overrides', () => {
  it('kept_for_the_result_and_reset_for_a_new_one', () => {
    const r = withResize({ cols: result.columns, w: {} }, result.columns, 1, 250);
    expect(overridesFor(r, result.columns)).toEqual({ 1: 250 });
    expect(withResize(r, result.columns, 0, 10).w).toEqual({ 0: 60, 1: 250 });
    const other = [...result.columns];
    expect(overridesFor(r, other)).toEqual({});
    expect(withResize(r, other, 0, 100).w).toEqual({ 0: 100 });
  });
});

describe('selectionCells', () => {
  it('copies_cells_with_sanitized_text', () => {
    const cells = selectionCells(result, { x: 0, y: 0, width: 2, height: 2 }, vi.fn());
    expect(cells.map((r) => r.map((c) => (c as { copyData: string }).copyData))).toEqual([
      ['a b', 'NULL'],
      ['2', 'z'],
    ]);
  });

  it('refuses_more_than_100k_cells', () => {
    const onLarge = vi.fn();
    const cells = selectionCells(result, { x: 0, y: 0, width: 2, height: 50_001 }, onLarge);
    expect(cells).toEqual([]);
    expect(onLarge).toHaveBeenCalledWith(100_002);
    expect(tooLargeMessage(100_002)).toBe('Selection too large to copy (100002 cells); narrow the selection');
  });

  it('allows_exactly_100k_cells', () => {
    const onLarge = vi.fn();
    selectionCells(result, { x: 0, y: 0, width: 2, height: 50_000 }, onLarge);
    expect(onLarge).not.toHaveBeenCalled();
  });
});
