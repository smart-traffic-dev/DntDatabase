import { describe, expect, it, vi } from 'vitest';
import type { ResultSet } from '../../store/queryRun';
import { copyOutcome, emptyPages, initialWidths, overridesFor, pageNeedsRows, sampleRows, storePage, touchPage, withResize, selectionCells, tooLargeMessage } from './ResultGrid';

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

describe('sampling for column fit', () => {
  const many = (n: number, widest: number): ResultSet => ({
    ...result,
    chunks: Array.from({ length: Math.ceil(n / 500) }, (_, c) =>
      Array.from({ length: Math.min(500, n - c * 500) }, (_, i) => [
        { t: 'Text' as const, v: c * 500 + i === widest ? 'w'.repeat(30) : 'x' },
        { t: 'Null' as const },
      ]),
    ),
    rowCount: n,
  });

  it('caps_the_sample_and_spreads_it_over_all_loaded_rows', () => {
    const rows = sampleRows(many(2000, -1), 200);
    expect(rows).toHaveLength(200);
    expect(sampleRows(many(5, -1), 200)).toHaveLength(5);
    expect(sampleRows(many(0, -1), 200)).toHaveLength(0);
  });

  it('sees_a_wide_value_beyond_the_first_rows_when_it_falls_on_the_stride', () => {
    // Row 100 is the first strided row after the 100 head rows of a 200 sample.
    const [a] = initialWidths(many(200, 150), undefined, { width: (t) => t.length * 8 });
    expect(a).toBe(30 * 8 + 20);
  });
});

describe('copyOutcome', () => {
  const opts = { engine: 'mysql' as const, cellsCapped: false, firstRowNumber: 1 };
  const sel = (x: number, y: number, width: number, height: number) =>
    ({
      current: { cell: [x, y], range: { x, y, width, height }, rangeStack: [] },
      rows: { length: 0, toArray: () => [], first: () => undefined },
      columns: { length: 0, toArray: () => [], first: () => undefined },
    }) as never;

  it('copies_the_range_as_tsv_with_null_empty', () => {
    const r = copyOutcome('tsv', sel(0, 0, 2, 2), result, opts);
    expect(r).toEqual({ ok: true, text: '"a\tb"\t\r\n2\tz', message: 'Copied 2 rows' });
  });

  it('adds_headers_and_makes_csv', () => {
    const r = copyOutcome('tsv-headers', sel(1, 1, 1, 1), result, opts);
    expect(r).toMatchObject({ ok: true, text: 'c1\r\nz', message: 'Copied 1 cell with headers' });
    expect(copyOutcome('csv', sel(0, 1, 2, 1), result, opts)).toMatchObject({ ok: true, text: 'c0,c1\r\n2,z' });
  });

  it('makes_insert_statements_of_whole_rows_with_a_placeholder_table', () => {
    const r = copyOutcome('insert', sel(0, 1, 1, 1), result, opts);
    expect(r).toMatchObject({ ok: true, text: "INSERT INTO table_name (`c0`, `c1`) VALUES (2, 'z');" });
  });

  it('uses_the_table_ref_then_the_single_source_table', () => {
    const ref = { database: 'shop', schema: null, name: 'orders', kind: 'Table' } as never;
    expect(copyOutcome('insert', sel(0, 1, 1, 1), result, { ...opts, tableRef: ref })).toMatchObject({
      text: "INSERT INTO `shop`.`orders` (`c0`, `c1`) VALUES (2, 'z');",
    });
    const origin = { ...result, columns: result.columns.map((c) => ({ ...c, origin_table: ref })) };
    expect(copyOutcome('insert', sel(0, 1, 1, 1), origin, { ...opts, engine: 'postgres' })).toMatchObject({
      text: 'INSERT INTO "orders" ("c0", "c1") VALUES (2, \'z\');',
    });
  });

  it('refuses_insert_for_binary_and_names_the_row_number', () => {
    const bin: ResultSet = { ...result, chunks: [[[{ t: 'Int', v: 1 }, { t: 'Bytes', v: { len: 2, preview: [1, 2] } }]]], rowCount: 1 };
    const r = copyOutcome('insert', sel(0, 0, 1, 1), bin, { ...opts, firstRowNumber: 501 });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.message).toMatch(/column c1, row 501/);
  });

  it('copies_the_column_name_of_the_clicked_or_current_column', () => {
    expect(copyOutcome('column', sel(1, 0, 1, 1), result, opts)).toMatchObject({ ok: true, text: 'c1' });
    expect(copyOutcome('column', sel(0, 0, 1, 1), result, { ...opts, cellColumn: 1 })).toMatchObject({ text: 'c1' });
    expect(copyOutcome('column', undefined, result, opts).ok).toBe(false);
  });

  it('refuses_over_the_cap_and_empty_selections', () => {
    expect(copyOutcome('tsv', sel(0, 0, 2, 50_001), result, opts)).toEqual({ ok: false, message: tooLargeMessage(100_002) });
    expect(copyOutcome('tsv', undefined, result, opts)).toEqual({ ok: false, message: 'Select cells first' });
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

describe('stored result pages', () => {
  const rows = (n: number) => Array.from({ length: n }, (_, i) => [{ t: 'Int' as const, v: i }]);

  it('evicts the least recently used pages beyond the byte budget, never the new one', () => {
    let p = emptyPages();
    p = storePage(p, 0, rows(500), 10, 25);
    p = storePage(p, 1, rows(500), 10, 25);
    p = touchPage(p, 0);
    p = storePage(p, 2, rows(500), 10, 25);
    expect([...p.pages.keys()].sort()).toEqual([0, 2]);
    expect(p.chunks[1]).toBeUndefined();
    expect(p.chunks[0]).toHaveLength(500);
    // A page larger than the budget alone is still kept.
    p = storePage(p, 3, rows(1), 100, 25);
    expect([...p.pages.keys()]).toEqual([3]);
  });

  it('a partly filled page is fetched again once more rows were committed into it', () => {
    expect(pageNeedsRows(120, 0, 120)).toBe(false);
    expect(pageNeedsRows(120, 0, 121)).toBe(true);
    expect(pageNeedsRows(500, 0, 10_000)).toBe(false);
    expect(pageNeedsRows(3, 2, 1003)).toBe(false);
    expect(pageNeedsRows(3, 2, 1004)).toBe(true);
  });
});