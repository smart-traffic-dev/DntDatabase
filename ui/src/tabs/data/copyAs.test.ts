import { beforeEach, describe, expect, it } from 'vitest';
import type { ColumnMeta, EditableInfo, ObjectRef, TableDataPage, Value } from '../../bindings';
import { callsOf, ids, mockBackend } from '../../test/backend';
import type { SelectionLike } from '../query/gridSelection';
import { copyAs, copySql, TRUNCATED_COPY } from './copyAs';

const col = (name: string, logical: ColumnMeta['logical'] = 'Text'): ColumnMeta => ({
  name,
  db_type: logical === 'Integer' ? 'bigint' : 'varchar',
  logical,
  nullable: true,
  origin_table: null,
  origin_column: null,
});
const text = (v: string): Value => ({ t: 'Text', v });
const int = (v: number | string): Value => ({ t: 'Int', v });

function page(rows: Value[][], over: Partial<TableDataPage> = {}): TableDataPage {
  return {
    columns: [col('id', 'Integer'), col('name')],
    rows,
    has_more: false,
    truncated_cells: 0,
    truncated: [],
    size_capped: false,
    key_order: [],
    row_versions: null,
    hidden_key: [],
    hidden_key_values: [],
    elapsed_ms: 1,
    ...over,
  };
}

const none = { length: 0, toArray: () => [], first: () => undefined };
const rowsOf = (...r: number[]): SelectionLike => ({
  columns: none,
  rows: { length: r.length, toArray: () => r, first: () => r[0] },
});
const range = (x: number, y: number, width: number, height: number): SelectionLike => ({
  current: { cell: [x, y], range: { x, y, width, height } },
  columns: none,
  rows: none,
});

describe('copyAs', () => {
  it('copy_tsv_with_header', () => {
    const p = page([[int(1), text('a')], [int(2), text('b')]]);
    expect(copyAs('tsv', rowsOf(0, 1), p)).toBe('id\tname\r\n1\ta\r\n2\tb');
    // A range copies the columns it covers, header included.
    expect(copyAs('tsv', range(1, 0, 1, 2), p)).toBe('name\r\na\r\nb');
  });

  it('copy_csv_quotes_and_newlines', () => {
    const p = page([[int(1), text('a,b')], [int(2), text('x"y')], [int(3), text('l1\nl2')], [int(4), { t: 'Null' }]]);
    expect(copyAs('csv', rowsOf(0, 1, 2, 3), p)).toBe('id,name\r\n1,"a,b"\r\n2,"x""y"\r\n3,"l1\nl2"\r\n4,');
  });

  it('copy_json_keeps_big_ints_as_strings', () => {
    const p = page([
      [int('9007199254740993'), text('a')],
      [int(7), { t: 'Null' }],
      [{ t: 'UInt', v: 18446744073709551615n as unknown as number }, text('c')],
    ]);
    const out = copyAs('json', rowsOf(0, 1), p);
    expect(JSON.parse(out)).toEqual([
      { id: '9007199254740993', name: 'a' },
      { id: 7, name: null },
    ]);
    // A number the engine cannot hold exactly is a string too.
    const unsafe = page([[int(2 ** 60), text('x')]]);
    expect(JSON.parse(copyAs('json', rowsOf(0), unsafe))).toEqual([{ id: String(2 ** 60), name: 'x' }]);
  });

  it('copy_markdown_escapes_pipes', () => {
    const p = page([[int(1), text('a|b')], [int(2), text('l1\nl2')]]);
    expect(copyAs('markdown', rowsOf(0, 1), p)).toBe('| id | name |\n| --- | --- |\n| 1 | a\\|b |\n| 2 | l1<br>l2 |');
  });

  it('copies the active cell when nothing is selected', () => {
    const p = page([[int(1), text('a')]]);
    expect(copyAs('tsv', undefined, p, { cell: { row: 0, col: 1 } })).toBe('name\r\na');
  });
});

describe('copySql', () => {
  const object: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
  const info: EditableInfo = {
    editable: false,
    reason: 'read_only_connection',
    key: ['id'],
    hidden_key: [],
    editable_columns: [],
    atomic: 'yes',
    protection: 'snapshot',
  };
  const meta = { connectionId: ids.prod, object, info };

  beforeEach(() => {
    mockBackend({ data_copy_sql: () => 'INSERT INTO `orders` VALUES (1);' });
  });

  it('copy_insert_calls_backend_and_refuses_truncated', async () => {
    const p = page([[int(1), text('a')], [int(2), text('b')]]);
    await expect(copySql('insert', rowsOf(0, 1), p, meta)).resolves.toBe('INSERT INTO `orders` VALUES (1);');
    expect(callsOf('data_copy_sql')).toEqual([
      {
        id: ids.prod,
        obj: object,
        rows: [
          { key: [['id', int(1)]], original: [['name', text('a')]], xmin: null },
          { key: [['id', int(2)]], original: [['name', text('b')]], xmin: null },
        ],
        format: 'insert',
      },
    ]);

    const cut = page([[int(1), text('a')], [int(2), text('b')]], { truncated: [[1, 1]], truncated_cells: 1 });
    await expect(copySql('update', rowsOf(0, 1), cut, meta)).rejects.toThrow(TRUNCATED_COPY);
    expect(TRUNCATED_COPY).toBe('Some cells are truncated; open them first or use Export.');
    expect(callsOf('data_copy_sql')).toHaveLength(1);
    // Rows without a cut cell are copied.
    await expect(copySql('update', rowsOf(0), cut, meta)).resolves.toBeTypeOf('string');
  });

  it('needs the key to find the rows', async () => {
    const p = page([[int(1), text('a')]]);
    await expect(copySql('insert', rowsOf(0), p, { ...meta, info: { ...info, key: [] } })).rejects.toThrow(/key/);
    expect(callsOf('data_copy_sql')).toHaveLength(0);
  });
});
