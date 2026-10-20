import { describe, expect, it } from 'vitest';
import type { Value } from '../../bindings';
import { MEMORY_WARNING_ROWS, cellView, currentStatement, toTsv } from './format';

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

describe('toTsv', () => {
  it('tsv_replaces_tabs_and_newlines', () => {
    const rows: Value[][] = [
      [{ t: 'Text', v: 'a\tb' }, { t: 'Null' }, { t: 'Text', v: 'x\r\ny\nz' }],
      [{ t: 'Int', v: 1 }, { t: 'Text', v: 'q' }, { t: 'Text', v: 'unused' }],
    ];
    expect(toTsv(rows, [0, 1, 2])).toBe('a b\tNULL\tx  y z\n1\tq\tunused');
    expect(toTsv(rows, [1])).toBe('NULL\nq');
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
