import { describe, expect, it } from 'vitest';
import type { TableDataPage, Value } from '../../bindings';
import { editText } from '../query/format';
import { isCutCell, isCutValue } from './cutCells';

const bytes = (len: number, shown: number): Value => ({ t: 'Bytes', v: { len, preview: Array.from({ length: shown }, () => 0) } });

function page(rows: Value[][], truncated: [number, number][] = []): TableDataPage {
  return {
    columns: [],
    rows,
    has_more: false,
    truncated_cells: truncated.length,
    truncated,
    size_capped: false,
    key_order: [],
    row_versions: null,
    hidden_key: [],
    hidden_key_values: [],
    elapsed_ms: 1,
  };
}

describe('cut cells', () => {
  it('a binary preview shorter than its value is cut, also inside arrays', () => {
    expect(isCutValue(bytes(1000, 64))).toBe(true);
    expect(isCutValue(bytes(3, 3))).toBe(false);
    expect(isCutValue({ t: 'Array', v: [{ t: 'Int', v: 1 }, bytes(100, 64)] })).toBe(true);
    expect(isCutValue({ t: 'Text', v: 'x' })).toBe(false);
  });

  it('a cell is cut when the page lists it or its value is a short preview', () => {
    const p = page([[{ t: 'Text', v: 'abc' }, bytes(1000, 64), bytes(2, 2)]], [[0, 0]]);
    expect(isCutCell(p, 0, 0)).toBe(true);
    expect(isCutCell(p, 0, 1)).toBe(true);
    expect(isCutCell(p, 0, 2)).toBe(false);
    expect(isCutCell(p, 5, 0)).toBe(false);
  });

  it('only cells shown whole are edited in place, as their stored text', () => {
    expect(editText({ t: 'Json', v: '{"n": 12345678901234567890, "x": 1.10}' })).toBe('{"n": 12345678901234567890, "x": 1.10}');
    expect(editText({ t: 'Text', v: 'a'.repeat(10_001) })).toBeNull();
    expect(editText({ t: 'Json', v: `"${'a'.repeat(10_000)}"` })).toBeNull();
    expect(editText(bytes(3, 3))).toBeNull();
    expect(editText({ t: 'Array', v: [{ t: 'Int', v: 1 }] })).toBeNull();
    expect(editText({ t: 'Int', v: 7 })).toBe('7');
  });
});
