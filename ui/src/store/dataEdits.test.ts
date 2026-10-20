import { beforeEach, describe, expect, it } from 'vitest';
import type { CellInput, ColumnMeta, EditableInfo, ObjectRef, TableDataPage, Value } from '../bindings';
import { ONE_AT_A_TIME, readOnlyCopy, rowIdOf, rowRef, useDataEdits } from './dataEdits';

const col = (name: string): ColumnMeta => ({ name, db_type: 'int', logical: 'Integer', nullable: false, origin_table: null, origin_column: null });
const int = (v: number): Value => ({ t: 'Int', v });
const object: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const info = (over: Partial<EditableInfo> = {}): EditableInfo => ({
  editable: true,
  reason: null,
  key: ['id'],
  hidden_key: [],
  editable_columns: ['name', 'qty'],
  atomic: 'yes',
  protection: 'row_version',
  ...over,
});

function page(ids: number[], over: Partial<TableDataPage> = {}): TableDataPage {
  return {
    columns: [col('id'), col('name'), col('qty')],
    rows: ids.map((id) => [int(id), { t: 'Text', v: `n${id}` }, int(id * 10)]),
    has_more: false,
    truncated_cells: 0,
    truncated: [],
    size_capped: false,
    key_order: [],
    row_versions: ids.map((id) => `${id}00`),
    hidden_key: [],
    hidden_key_values: [],
    elapsed_ms: 1,
    ...over,
  };
}

const TAB = 't1';
const edits = () => useDataEdits.getState();
const staged = () => edits().byTab[TAB]!.staged;
const update = (p: TableDataPage, i: number, column: string, input: CellInput) =>
  edits().stage(TAB, { kind: 'update', row: rowIdOf(p, info(), i)!, ref: rowRef(p, info(), i)!, column, input });
const remove = (p: TableDataPage, i: number) =>
  edits().stage(TAB, { kind: 'delete', row: rowIdOf(p, info(), i)!, ref: rowRef(p, info(), i)! });

beforeEach(() => {
  useDataEdits.getState().reset();
  useDataEdits.getState().setInfo(TAB, object, info());
});

describe('dataEdits loaded values', () => {
  it('a fetched original joins the staged row and later edits keep it', () => {
    const p = page([1]);
    const withToken = (column: string, token: string) => ({ ...rowRef(p, info(), 0)!, loaded: [[column, token]] as [string, string][] });
    const id = rowIdOf(p, info(), 0)!;
    edits().stage(TAB, { kind: 'update', row: id, ref: withToken('name', 'tok-a'), column: 'name', input: { kind: 'text', text: 'x' } });
    // A second edit of the row captures nothing new but adds the token of its own column.
    edits().stage(TAB, { kind: 'update', row: id, ref: withToken('qty', 'tok-b'), column: 'qty', input: { kind: 'text', text: '5' } });
    edits().stage(TAB, { kind: 'update', row: id, ref: rowRef(p, info(), 0)!, column: 'qty', input: { kind: 'text', text: '6' } });
    const u = staged().updates.get(id)!;
    expect(u.ref.loaded).toEqual([['name', 'tok-a'], ['qty', 'tok-b']]);
    expect(u.ref.original).toEqual(rowRef(p, info(), 0)!.original);
  });
});
describe('dataEdits store', () => {
  it('stage_update_then_undo', () => {
    const p = page([1, 2]);
    const id1 = rowIdOf(p, info(), 0)!;
    expect(edits().hasStaged(TAB)).toBe(false);
    expect(update(p, 0, 'name', { kind: 'text', text: 'x' })).toEqual({ ok: true });
    expect(edits().hasStaged(TAB)).toBe(true);
    expect(staged().updates.get(id1)?.cells.get('name')).toEqual({ kind: 'text', text: 'x' });
    // A second cell of the same row is the same change.
    update(p, 0, 'qty', { kind: 'null' });
    expect(edits().changeCount(TAB)).toBe(1);
    edits().undo(TAB);
    expect(staged().updates.get(id1)?.cells.has('qty')).toBe(false);
    edits().undo(TAB);
    expect(edits().hasStaged(TAB)).toBe(false);
    remove(p, 0);
    edits().discard(TAB);
    expect(edits().hasStaged(TAB)).toBe(false);
  });

  it('row_id_survives_reload', () => {
    const before = page([1, 2, 3]);
    const after = page([3, 1]); // another order, a row gone
    const id = rowIdOf(before, info(), 2)!;
    expect(rowIdOf(after, info(), 0)).toBe(id);
    expect(rowIdOf(after, info(), 1)).not.toBe(id);
    // A hidden key column joins the id.
    const hidden = page([1], { hidden_key: [col('hid')], hidden_key_values: [[int(9)]] });
    const withHidden = info({ key: ['id', 'hid'], hidden_key: ['hid'] });
    expect(JSON.parse(rowIdOf(hidden, withHidden, 0)!)).toEqual([
      ['id', int(1)],
      ['hid', int(9)],
    ]);
  });

  it('to_edit_set_includes_xmin_and_originals', () => {
    const p = page([1, 2, 3]);
    update(p, 0, 'name', { kind: 'text', text: 'new' });
    remove(p, 2);
    edits().stage(TAB, { kind: 'insert' });
    const seq = staged().inserts[0]!.id;
    edits().stage(TAB, { kind: 'insertCell', insert: seq, column: 'qty', input: { kind: 'text', text: '4' } });
    const set = edits().toEditSet(TAB);
    expect(set.object).toEqual(object);
    expect(set.overwrite).toBe(false);
    expect(set.edits).toEqual([
      {
        op: 'update',
        row: { key: [['id', int(1)]], original: [['name', { t: 'Text', v: 'n1' }], ['qty', int(10)]], xmin: '100' },
        set: [['name', { kind: 'text', text: 'new' }]],
      },
      { op: 'delete', row: { key: [['id', int(3)]], original: [['name', { t: 'Text', v: 'n3' }], ['qty', int(30)]], xmin: '300' } },
      { op: 'insert', values: [['qty', { kind: 'text', text: '4' }]] },
    ]);
    expect(edits().toEditSet(TAB, true).overwrite).toBe(true);
  });

  it('the row is captured by its first edit and later edits of the row do not replace it', () => {
    const first = page([1]);
    const reloaded = page([1], { rows: [[int(1), { t: 'Text', v: 'changed on server' }, int(99)]], row_versions: ['777'] });
    update(first, 0, 'name', { kind: 'text', text: 'a' });
    update(reloaded, 0, 'qty', { kind: 'null' });
    const [edit] = edits().toEditSet(TAB).edits;
    expect(edit).toMatchObject({ op: 'update', row: { xmin: '100', original: [['name', { t: 'Text', v: 'n1' }], ['qty', int(10)]] } });
    expect(edit).toMatchObject({ set: [['name', { kind: 'text', text: 'a' }], ['qty', { kind: 'null' }]] });
  });

  it('only restricts the edit set to one row', () => {
    const p = page([1, 2]);
    update(p, 0, 'name', { kind: 'null' });
    update(p, 1, 'name', { kind: 'null' });
    edits().stage(TAB, { kind: 'insert' });
    const only = edits().toEditSet(TAB, true, rowIdOf(p, info(), 1));
    expect(only.edits).toHaveLength(1);
    expect(only.edits[0]).toMatchObject({ op: 'update', row: { key: [['id', int(2)]] } });
  });

  it('a table that cannot roll back a batch takes one change at a time', () => {
    useDataEdits.getState().setInfo(TAB, object, info({ atomic: 'no' }));
    const p = page([1, 2]);
    expect(update(p, 0, 'name', { kind: 'null' })).toEqual({ ok: true });
    expect(update(p, 0, 'qty', { kind: 'null' })).toEqual({ ok: true });
    expect(update(p, 1, 'name', { kind: 'null' })).toEqual({ ok: false, message: ONE_AT_A_TIME });
    expect(edits().stage(TAB, { kind: 'insert' })).toEqual({ ok: false, message: ONE_AT_A_TIME });
    expect(edits().changeCount(TAB)).toBe(1);
    expect(ONE_AT_A_TIME).toBe("This table can't roll back a batch: apply this change first.");
  });

  it('deleting a row drops its staged updates; a deleted row takes no more edits', () => {
    const p = page([1]);
    const id = rowIdOf(p, info(), 0)!;
    update(p, 0, 'name', { kind: 'null' });
    remove(p, 0);
    expect(staged().updates.size).toBe(0);
    expect(staged().deletes.has(id)).toBe(true);
    expect(update(p, 0, 'name', { kind: 'null' }).ok).toBe(false);
  });

  it('maps a machine-readable reason to the read-only copy', () => {
    expect(readOnlyCopy('no_key')).toBe('Read-only: this table has no primary key or unique NOT NULL index.');
    expect(readOnlyCopy('read_only_connection')).toBe('Read-only connection.');
    expect(readOnlyCopy('not_a_table')).toBe('Views are read-only.');
    expect(readOnlyCopy('unsupported:generated columns')).toBe('Read-only: generated columns');
    expect(readOnlyCopy(null)).toBe('Read-only.');
  });
});
