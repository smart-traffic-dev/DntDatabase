import { describe, expect, it } from 'vitest';
import type { Value } from '../../bindings';
import { rowToDocument } from './documents';

describe('rowToDocument', () => {
  it('object_id_to_oid', () => {
    const row: Value[] = [
      { t: 'Other', v: { type_name: 'ObjectId', text: '65f0a1b2c3d4e5f607182930' } },
      { t: 'Other', v: { type_name: 'Decimal128', text: '1.50' } },
    ];
    expect(rowToDocument(['_id', 'price'], row, null)).toEqual({
      _id: { $oid: '65f0a1b2c3d4e5f607182930' },
      price: { $Decimal128: '1.50' },
    });
  });

  it('converts_scalars_json_dates_binary_and_uuid', () => {
    const row: Value[] = [
      { t: 'Json', v: '{"city":"Hue","tags":[1,2]}' },
      { t: 'DateTime', v: '2026-01-02T03:04:05.000Z' },
      { t: 'Bytes', v: { len: 3, preview: [1, 2, 3] } },
      { t: 'Uuid', v: '67e55044-10b1-426f-9247-bb680e5fe0c8' },
      { t: 'Int', v: 7 },
      { t: 'Bool', v: true },
      { t: 'Text', v: 'x' },
      { t: 'Float', v: 1.5 },
    ];
    expect(rowToDocument(['addr', 'at', 'bin', 'u', 'n', 'ok', 's', 'f'], row, null)).toEqual({
      addr: { city: 'Hue', tags: [1, 2] },
      at: { $date: '2026-01-02T03:04:05.000Z' },
      bin: { $binary: { base64: 'AQID', subType: '00' } },
      u: { $uuid: '67e55044-10b1-426f-9247-bb680e5fe0c8' },
      n: 7,
      ok: true,
      s: 'x',
      f: 1.5,
    });
  });

  it('json_view_skips_padding_nulls', () => {
    const columns = ['_id', 'a', 'b'];
    const id = (n: number): Value => ({ t: 'Int', v: n });
    const rows: Value[][] = [
      [id(1), { t: 'Text', v: 'x' }, { t: 'Null' }],
      [id(2), { t: 'Null' }, { t: 'Text', v: 'y' }],
    ];
    const present = [
      [true, true, false],
      [true, false, true],
    ];
    expect(rows.map((row, i) => rowToDocument(columns, row, present[i]!))).toEqual([
      { _id: 1, a: 'x' },
      { _id: 2, b: 'y' },
    ]);
    // Without a mask every Null cell is skipped.
    expect(rowToDocument(columns, rows[0]!, null)).toEqual({ _id: 1, a: 'x' });
  });

  it('a_mask_keeps_a_real_null', () => {
    expect(rowToDocument(['a'], [{ t: 'Null' }], [true])).toEqual({ a: null });
  });
});
