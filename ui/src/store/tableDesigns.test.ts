import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ColumnDef, DesignOpen, DesignRender, TableDef } from '../bindings';
import { callsOf, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useTableDesigns } from './tableDesigns';

const column = (name: string): ColumnDef => ({
  original: name,
  name,
  data_type: { kind: 'known', base: 'int', length: null, precision: null, scale: null, unsigned: false, values: [] },
  nullable: true,
  default: null,
  auto_increment: false,
  identity: null,
  on_update: null,
  charset: null,
  collation: null,
  comment: null,
  using: null,
  generated: null,
  locked: false,
});

const def: TableDef = {
  original_name: 'orders',
  name: 'orders',
  columns: [column('id'), column('total')],
  primary_key: { original: 'PRIMARY', name: 'PRIMARY', columns: ['id'], locked: false },
  indexes: [],
  foreign_keys: [],
  checks: [],
  options: { engine: 'InnoDB', charset: null, collation: null, row_format: null, locked: false },
  comment: null,
  read_only_reason: null,
};

const opened = (table: TableDef = def): DesignOpen => ({
  design_id: 'd1',
  def: table,
  locked: [],
  catalog: [],
  server: serverInfo('MySql8'),
  env: null,
  read_only: false,
});
const rendered: DesignRender = { statements: ['ALTER TABLE `orders` ADD COLUMN `note` int'], impacts: [], errors: [], error: null };
const scope = { database: 'shop', schema: null };
const TAB = 't1';
const store = () => useTableDesigns.getState();
const state = () => store().byTab[TAB]!;
const addColumn = (name: string) => (d: TableDef): TableDef => ({ ...d, columns: [...d.columns, { ...column(name), original: null }] });

beforeEach(() => {
  vi.useFakeTimers();
  resetStores();
  mockBackend({ table_design_open: () => opened(), table_design_render: () => rendered, table_design_close: () => null });
});
afterEach(() => vi.useRealTimers());

async function loaded() {
  await store().load(TAB, ids.prod, scope, 'orders');
}

describe('table design draft store', () => {
  it('load opens the design and starts the draft from its definition', async () => {
    await loaded();
    expect(callsOf('table_design_open')[0]).toMatchObject({ connectionId: ids.prod, scope, table: 'orders' });
    expect(state().draft).toEqual(def);
    expect(state().loading).toBe(false);
  });

  it('a load that fails keeps the message and no draft', async () => {
    mockBackend({ table_design_open: () => Promise.reject(new Error('boom')) });
    await loaded();
    expect(state().draft).toBeNull();
    expect(state().error).toBeTruthy();
  });

  it('edit_pushes_undo_and_renders_debounced', async () => {
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(100);
    store().edit(TAB, addColumn('b'));
    await vi.advanceTimersByTimeAsync(100);
    store().edit(TAB, addColumn('c'));
    expect(state().past).toHaveLength(3);
    expect(callsOf('table_design_render')).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(299);
    expect(callsOf('table_design_render')).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(callsOf('table_design_render')).toHaveLength(1);
    expect(callsOf('table_design_render')[0]).toMatchObject({ designId: 'd1' });
    expect((callsOf('table_design_render')[0]!.draft as TableDef).columns).toHaveLength(5);
    expect(state().render).toEqual(rendered);
  });

  it('undo_redo', async () => {
    await loaded();
    store().edit(TAB, addColumn('a'));
    store().edit(TAB, addColumn('b'));
    store().undo(TAB);
    expect(state().draft!.columns.map((c) => c.name)).toEqual(['id', 'total', 'a']);
    store().undo(TAB);
    expect(state().draft).toEqual(def);
    store().undo(TAB);
    expect(state().draft).toEqual(def);
    store().redo(TAB);
    expect(state().draft!.columns).toHaveLength(3);
    // A new edit clears the redo stack.
    store().edit(TAB, addColumn('z'));
    expect(state().future).toEqual([]);
    store().redo(TAB);
    expect(state().draft!.columns.map((c) => c.name)).toEqual(['id', 'total', 'a', 'z']);
  });

  it('dirty_and_change_count', async () => {
    await loaded();
    expect(store().isDirty(TAB)).toBe(false);
    expect(store().changeCount(TAB)).toBe(0);
    store().edit(TAB, addColumn('a'));
    expect(store().isDirty(TAB)).toBe(true);
    expect(store().changeCount(TAB)).toBe(1);
    // Modified (renamed keeps `original`), and removed.
    store().edit(TAB, (d) => ({ ...d, columns: [{ ...d.columns[0]!, name: 'pk' }, ...d.columns.slice(2)] }));
    expect(store().changeCount(TAB)).toBe(3);
    // Back to the opened definition: clean again, even through edits.
    store().edit(TAB, () => def);
    expect(store().isDirty(TAB)).toBe(false);
    expect(store().changeCount(TAB)).toBe(0);
  });

  it('counts a table rename, a dropped primary key and a column move', async () => {
    await loaded();
    store().edit(TAB, (d) => ({ ...d, name: 'orders2', primary_key: null }));
    expect(store().changeCount(TAB)).toBe(2);
    store().discard(TAB);
    store().edit(TAB, (d) => ({ ...d, columns: [...d.columns].reverse() }));
    expect(store().isDirty(TAB)).toBe(true);
    expect(store().changeCount(TAB)).toBe(1);
  });

  it('a new table opens with null table and counts its elements as added', async () => {
    const fresh: TableDef = { ...def, original_name: null, name: 'new_table', columns: [], primary_key: null };
    mockBackend({ table_design_open: () => opened(fresh), table_design_close: () => null, table_design_render: () => rendered });
    await store().load(TAB, ids.prod, scope, null);
    expect(callsOf('table_design_open')[0]).toMatchObject({ table: null });
    expect(store().isDirty(TAB)).toBe(false);
    store().edit(TAB, addColumn('id'));
    expect(store().changeCount(TAB)).toBe(1);
  });

  it('discard_restores_open_def', async () => {
    await loaded();
    store().edit(TAB, addColumn('a'));
    store().select(TAB, { kind: 'column', name: 'a' });
    store().discard(TAB);
    expect(state().draft).toEqual(def);
    expect(state().past).toEqual([]);
    expect(state().selection).toBeNull();
    expect(store().isDirty(TAB)).toBe(false);
    // The pending render of the discarded edit is cancelled.
    await vi.advanceTimersByTimeAsync(1000);
    expect(callsOf('table_design_render')).toHaveLength(0);
  });

  it('only the newest render answer is kept', async () => {
    const slow: { resolve?: (r: DesignRender) => void } = {};
    mockBackend({
      table_design_open: () => opened(),
      table_design_close: () => null,
      table_design_render: () => new Promise<DesignRender>((r) => { slow.resolve = r; }),
    });
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(300);
    store().edit(TAB, addColumn('b'));
    const first = slow.resolve!;
    await vi.advanceTimersByTimeAsync(300);
    first({ ...rendered, statements: ['stale'] });
    await vi.advanceTimersByTimeAsync(0);
    expect(state().render).toBeNull();
    slow.resolve!(rendered);
    await vi.advanceTimersByTimeAsync(0);
    expect(state().render).toEqual(rendered);
  });

  it('reload opens a fresh session and closes the old one; dispose closes the session', async () => {
    let n = 0;
    mockBackend({
      table_design_open: () => ({ ...opened(), design_id: `d${++n}` }),
      table_design_close: () => null,
      table_design_render: () => rendered,
    });
    await loaded();
    store().edit(TAB, addColumn('a'));
    await store().reload(TAB);
    expect(state().open!.design_id).toBe('d2');
    expect(store().isDirty(TAB)).toBe(false);
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }]);
    store().dispose(TAB);
    expect(store().byTab[TAB]).toBeUndefined();
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }, { designId: 'd2' }]);
  });
});
