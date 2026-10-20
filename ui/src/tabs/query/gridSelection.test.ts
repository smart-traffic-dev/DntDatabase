import { describe, expect, it } from 'vitest';
import type { Value } from '../../bindings';
import { blockValues, cellValue, currentCol, currentRow, selectionBlock, selectionSize, type IndexSet, type SelectionLike } from './gridSelection';

const set = (...items: number[]): IndexSet => ({ length: items.length, toArray: () => items, first: () => items[0] });
const empty = set();
const range = (x: number, y: number, width: number, height: number): SelectionLike => ({
  current: { cell: [x, y], range: { x, y, width, height } },
  rows: empty,
  columns: empty,
});

describe('selectionBlock', () => {
  it('lists_the_rows_and_columns_of_a_range', () => {
    expect(selectionBlock(range(1, 2, 2, 3), 10, 5, 100)).toEqual({ ok: true, block: { rows: [2, 3, 4], cols: [1, 2] } });
  });

  it('clips_a_range_to_the_result', () => {
    expect(selectionBlock(range(0, 8, 2, 5), 10, 5, 100)).toEqual({ ok: true, block: { rows: [8, 9], cols: [0, 1] } });
  });

  it('takes_all_columns_for_selected_rows_and_all_rows_for_selected_columns', () => {
    expect(selectionBlock({ rows: set(1, 4), columns: empty }, 10, 3, 100)).toEqual({ ok: true, block: { rows: [1, 4], cols: [0, 1, 2] } });
    expect(selectionBlock({ rows: empty, columns: set(2) }, 3, 4, 100)).toEqual({ ok: true, block: { rows: [0, 1, 2], cols: [2] } });
  });

  it('refuses_over_the_cap_without_building_it', () => {
    expect(selectionBlock({ rows: empty, columns: set(0) }, 1_000_000, 3, 100_000)).toEqual({ ok: false, cells: 1_000_000 });
  });

  it('is_empty_without_a_selection', () => {
    expect(selectionBlock(undefined, 10, 3, 100)).toEqual({ ok: true, block: { rows: [], cols: [] } });
    expect(selectionSize(undefined, 10, 3)).toBe(0);
  });
});

describe('current position', () => {
  it('prefers_selected_rows_then_the_active_cell', () => {
    expect(currentRow({ rows: set(7), columns: empty, current: range(1, 2, 1, 1).current })).toBe(7);
    expect(currentRow(range(1, 2, 1, 1))).toBe(2);
    expect(currentRow(undefined)).toBeNull();
    expect(currentCol(range(1, 2, 1, 1))).toBe(1);
    expect(currentCol({ rows: empty, columns: set(3) })).toBe(3);
  });
});

describe('values', () => {
  const chunks: Value[][][] = [[[{ t: 'Int', v: 1 }, { t: 'Int', v: 2 }]]];
  it('reads_loaded_cells_and_nulls_for_the_rest', () => {
    expect(cellValue({ chunks }, 0, 1)).toEqual({ t: 'Int', v: 2 });
    expect(cellValue({ chunks }, 5, 0)).toEqual({ t: 'Null' });
    expect(blockValues({ chunks }, { rows: [0], cols: [1, 0] })).toEqual([[{ t: 'Int', v: 2 }, { t: 'Int', v: 1 }]]);
  });
});
