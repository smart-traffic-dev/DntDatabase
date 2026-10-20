import { describe, expect, it, vi } from 'vitest';
import type { Value } from '../../bindings';
import {
  MAX_COL_WIDTH,
  MIN_COL_WIDTH,
  charsForWidth,
  fitCellText,
  fitColumnWidth,
  noLigatures,
  MAX_COPY_CELLS, MEMORY_WARNING_ROWS, cellView, copyText, currentStatement, truncateText } from './format';

describe('column fitting', () => {
  it('fits_header_and_is_clamped', () => {
    expect(fitColumnWidth('id', 'int4', ['1'])).toBe(MIN_COL_WIDTH);
    expect(fitColumnWidth('a_rather_long_column_name', 'text', [])).toBeGreaterThan(150);
    expect(fitColumnWidth('c', 't', ['x'.repeat(500)])).toBe(MAX_COL_WIDTH);
  });

  it('fits_a_full_bigint', () => {
    expect(fitColumnWidth('n', 'int8', ['9223372036854775807'])).toBeGreaterThanOrEqual(19 * 7.9);
  });

  it('chars_for_width_never_zero', () => {
    expect(charsForWidth(10)).toBe(1);
    expect(charsForWidth(200)).toBe(22);
  });

  it('fit_text_keeps_start_with_ellipsis', () => {
    expect(fitCellText({ text: '9223372036854775807', kind: 'number' }, 8)).toBe('9223372…');
    expect(fitCellText({ text: 'abc', kind: 'text' }, 8)).toBe('abc');
  });

  it('fit_text_keeps_byte_count', () => {
    const v = cellView({ t: 'Bytes', v: { len: 3000, preview: Array.from({ length: 32 }, (_, i) => i) } });
    const out = fitCellText(v, 30);
    expect(out.endsWith('(3000 bytes)')).toBe(true);
    expect(out.length).toBeLessThanOrEqual(30);
    expect(fitCellText(v, 5)).toBe('(3000 bytes)');
  });

  it('splits_ligature_pairs', () => {
    expect(noLigatures('a >= 0')).toBe('a >‌= 0');
    expect(noLigatures('plain text')).toBe('plain text');
    expect(noLigatures('1234')).toBe('1234');
  });
});

describe('cellView', () => {
  it('null_is_grey_italic_kind', () => {
    expect(cellView({ t: 'Null' })).toEqual({ text: 'NULL', kind: 'null' });
  });

  it('numbers_right_aligned_kind', () => {
    expect(cellView({ t: 'Int', v: 5 }).kind).toBe('number');
    expect(cellView({ t: 'UInt', v: 5 }).kind).toBe('number');
    expect(cellView({ t: 'Float', v: 1.5 })).toEqual({ text: '1.5', kind: 'number' });
    expect(cellView({ t: 'Decimal', v: '12.50' })).toEqual({ text: '12.50', kind: 'number' });
  });

  it('big_int_string_kept', () => {
    expect(cellView({ t: 'Int', v: '9223372036854775807' })).toEqual({ text: '9223372036854775807', kind: 'number' });
  });

  it('bytes_hex_with_length', () => {
    expect(cellView({ t: 'Bytes', v: { len: 3, preview: [0, 255, 16] } })).toEqual({
      text: '0x00ff10 (3 bytes)',
      kind: 'bytes',
    });
    expect(cellView({ t: 'Bytes', v: { len: 10, preview: [1, 2] } }).text).toBe('0x0102… (10 bytes)');
  });

  it('json_single_line', () => {
    expect(cellView({ t: 'Json', v: '{\n  "a": 1,\n  "b": [1, 2]\n}' })).toEqual({ text: '{"a":1,"b":[1,2]}', kind: 'json' });
    expect(cellView({ t: 'Json', v: '{ not\n   json' }).text).toBe('{ not json');
  });

  it('array_and_other_kinds', () => {
    expect(cellView({ t: 'Array', v: [{ t: 'Int', v: 1 }, { t: 'Text', v: 'x' }, { t: 'Null' }] }).text).toBe('{1,x,NULL}');
    expect(cellView({ t: 'Bool', v: true })).toEqual({ text: 'true', kind: 'bool' });
    expect(cellView({ t: 'Text', v: 'héllo' })).toEqual({ text: 'héllo', kind: 'text' });
    expect(cellView({ t: 'Other', v: { type_name: 'point', text: '(1,2)' } }).text).toBe('(1,2)');
  });
});

describe('cellView limits', () => {
  it('big_json_is_capped_and_parsed_at_most_once', () => {
    const big: Value = { t: 'Json', v: JSON.stringify({ a: 'x'.repeat(100_000) }) };
    const parse = vi.spyOn(JSON, 'parse');
    const t0 = performance.now();
    const v = cellView(big);
    for (let i = 0; i < 50; i++) cellView(big);
    expect(performance.now() - t0).toBeLessThan(200);
    expect(parse.mock.calls.length).toBeLessThanOrEqual(1);
    expect(v.text.length).toBeLessThanOrEqual(10_001);
    expect(v.text.endsWith('…')).toBe(true);
  });

  it('small_json_is_cached_per_value', () => {
    const v: Value = { t: 'Json', v: '{ "a": 1 }' };
    const parse = vi.spyOn(JSON, 'parse');
    cellView(v);
    cellView(v);
    expect(parse.mock.calls.length).toBe(1);
  });

  it('truncate_keeps_code_points', () => {
    const s = 'ab😀cd';
    expect(truncateText(s, 3)).toBe('ab…'); // would split the emoji
    expect(truncateText(s, 4)).toBe('ab😀…');
    expect(truncateText('abc', 3)).toBe('abc');
  });

  it('tsv_replaces_tabs_and_newlines', () => {
    expect(copyText({ t: 'Text', v: 'a\tb' })).toBe('a b');
    expect(copyText({ t: 'Text', v: 'x\r\ny\nz' })).toBe('x  y z');
    expect(copyText({ t: 'Null' })).toBe('NULL');
  });

  it('copy_cap_constant', () => {
    expect(MAX_COPY_CELLS).toBe(100_000);
  });
});
describe('currentStatement', () => {
  // "SELECT 1;\nSELECT 2;\n\nSELECT 3" -> 0..8, 10..18, 21..29 (end-exclusive, as statement_ranges)
  const ranges: [number, number][] = [
    [0, 8],
    [10, 18],
    [21, 29],
  ];

  it('current_statement_rule_matches_backend', () => {
    expect(currentStatement(ranges, 10)).toEqual([10, 18]); // start
    expect(currentStatement(ranges, 8)).toEqual([0, 8]); // end is inclusive
    expect(currentStatement(ranges, 9)).toEqual([0, 8]); // between: nearest ending before
    expect(currentStatement(ranges, 20)).toEqual([10, 18]);
    expect(currentStatement(ranges, 29)).toEqual([21, 29]);
    expect(currentStatement(ranges, 999)).toEqual([21, 29]); // past the end
    expect(currentStatement(ranges, 0)).toEqual([0, 8]);
  });

  it('falls_back_to_first_and_empty', () => {
    expect(currentStatement([[5, 9]], 1)).toEqual([5, 9]);
    expect(currentStatement([], 3)).toBeNull();
  });

  it('memory_warning_constant', () => {
    expect(MEMORY_WARNING_ROWS).toBe(100_000);
  });
});
