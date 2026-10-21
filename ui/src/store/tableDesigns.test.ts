import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ColumnDef, DesignOpen, DesignRender, TableDef } from '../bindings';
import { appError, callsOf, Failure, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useConnections } from './connections';
import { describeChanges, useTableDesigns } from './tableDesigns';

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

const lostError = () => new Failure(appError('config', 'This designer is no longer open; open the table again.', { field: 'design_id' }));

describe('previous draft kept after a stale apply', () => {
  it('describes each change of a draft against its base', () => {
    const base: TableDef = {
      ...def,
      columns: [column('id'), column('total'), column('gone')],
      indexes: [{ original: 'ix_total', name: 'ix_total', unique: false, parts: [], method: null, include: [], visible: true, comment: null, constraint: false, locked: false }],
      checks: [{ original: 'ck', name: 'ck', expression: 'total > 0', locked: false }],
    };
    const draft: TableDef = {
      ...base,
      name: 'orders2',
      columns: [{ ...column('total'), name: 'amount' }, { ...column('id'), nullable: false }, { ...column('note'), original: null }],
      primary_key: null,
      indexes: [],
      checks: [{ original: 'ck', name: 'ck', expression: 'total >= 0', locked: false }],
      options: { ...base.options, engine: 'MyISAM' },
      comment: 'hello',
    };
    expect(describeChanges(base, draft)).toEqual([
      'Table renamed to "orders2"',
      'Column "total" renamed to "amount"',
      'Column "id" modified',
      'Column "note" added',
      'Column "gone" removed',
      'Columns reordered',
      'Primary key removed',
      'Index "ix_total" removed',
      'Check "ck" modified',
      'Table options changed',
      'Comment changed',
    ]);
    expect(describeChanges(base, base)).toEqual([]);
  });

  it('keeps the base with the stale draft and forgets both together', async () => {
    mockBackend({ table_design_open: () => opened(), table_design_render: () => rendered, table_design_close: () => null });
    await store().load(TAB, ids.prod, scope, 'orders');
    const draft = addColumn('note')(def);
    store().setStaleDraft(TAB, draft, def);
    expect(state().staleDraft).toEqual(draft);
    expect(state().staleBase).toEqual(def);
    // Reopening keeps them for comparison; the draft starts from the new structure.
    await store().reload(TAB);
    expect(state().staleBase).toEqual(def);
    expect(state().draft).toEqual(def);
    store().setStaleDraft(TAB, null);
    expect(state().staleDraft).toBeNull();
    expect(state().staleBase).toBeNull();
  });
});

describe('table design fix round', () => {
  it('a failed reload keeps the draft and the live session, and sets the error', async () => {
    let fail = false;
    mockBackend({
      table_design_open: () => {
        if (fail) throw new Failure(appError('internal', 'server gone'));
        return opened();
      },
      table_design_render: () => rendered,
      table_design_close: () => null,
    });
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(300);
    fail = true;
    await store().reload(TAB);
    expect(state().open!.design_id).toBe('d1');
    expect(state().draft!.columns).toHaveLength(3);
    expect(state().past).toHaveLength(1);
    expect(state().loading).toBe(false);
    expect(state().error).toContain('server gone');
    expect(callsOf('table_design_close')).toEqual([]);
  });

  it('a reload keeps the old design shown while the new one opens', async () => {
    const slow: { resolve?: (o: DesignOpen) => void } = {};
    let first = true;
    mockBackend({
      table_design_open: () => {
        if (first) {
          first = false;
          return opened();
        }
        return new Promise<DesignOpen>((r) => { slow.resolve = r; });
      },
      table_design_close: () => null,
      table_design_render: () => rendered,
    });
    await loaded();
    store().edit(TAB, addColumn('a'));
    const reloading = store().reload(TAB);
    expect(state().draft!.columns).toHaveLength(3);
    expect(state().loading).toBe(true);
    slow.resolve!({ ...opened(), design_id: 'd2' });
    await reloading;
    expect(state().open!.design_id).toBe('d2');
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }]);
  });

  function slowReload() {
    const slow: { resolve?: (o: DesignOpen) => void } = {};
    let first = true;
    mockBackend({
      table_design_open: () => {
        if (first) {
          first = false;
          return opened();
        }
        return new Promise<DesignOpen>((r) => { slow.resolve = r; });
      },
      table_design_close: () => null,
      table_design_render: () => rendered,
    });
    return slow;
  }

  it('edits, undo, redo and a discard during a pending reload do not cancel it', async () => {
    const slow = slowReload();
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(300);
    const reloading = store().reload(TAB);
    expect(state().loading).toBe(true);
    // Edits are ignored while the reload runs, so nothing is scheduled or superseded.
    store().edit(TAB, addColumn('b'));
    store().undo(TAB);
    store().redo(TAB);
    expect(state().draft!.columns).toHaveLength(3);
    expect(state().past).toHaveLength(1);
    store().discard(TAB);
    await vi.advanceTimersByTimeAsync(1000);
    slow.resolve!({ ...opened(), design_id: 'd2' });
    await reloading;
    expect(state().loading).toBe(false);
    expect(state().open!.design_id).toBe('d2');
    expect(state().draft).toEqual(def);
    expect(state().error).toBeNull();
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }]);
  });

  it('a render answer in flight when the reload lands is dropped', async () => {
    const slow = slowReload();
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(300);
    const reloading = store().reload(TAB);
    slow.resolve!({ ...opened(), design_id: 'd2' });
    await reloading;
    expect(state().render).toBeNull();
    expect(state().loading).toBe(false);
  });

  it('markLost during a pending reload keeps the draft as lostDraft and the reload still completes', async () => {
    const slow = slowReload();
    await loaded();
    store().edit(TAB, addColumn('a'));
    const reloading = store().reload(TAB);
    store().markLost(TAB);
    expect(state().lostDraft!.columns).toHaveLength(3);
    slow.resolve!({ ...opened(), design_id: 'd2' });
    await reloading;
    expect(state().loading).toBe(false);
    expect(state().open!.design_id).toBe('d2');
    expect(state().lostDraft!.columns).toHaveLength(3);
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }]);
  });

  it('a newer open supersedes an older one: the older session is closed, loading ends once', async () => {
    const resolvers: ((o: DesignOpen) => void)[] = [];
    let first = true;
    mockBackend({
      table_design_open: () => {
        if (first) {
          first = false;
          return opened();
        }
        return new Promise<DesignOpen>((r) => resolvers.push(r));
      },
      table_design_close: () => null,
    });
    await loaded();
    const a = store().reload(TAB);
    const b = store().reload(TAB);
    resolvers[0]!({ ...opened(), design_id: 'd2' });
    await a;
    expect(state().loading).toBe(true);
    resolvers[1]!({ ...opened(), design_id: 'd3' });
    await b;
    expect(state().loading).toBe(false);
    expect(state().open!.design_id).toBe('d3');
    expect(callsOf('table_design_close').map((c) => c.designId).sort()).toEqual(['d1', 'd2']);
  });

  it('a render that finds the design gone marks it lost and keeps the draft', async () => {
    mockBackend({ table_design_open: () => opened(), table_design_render: () => { throw lostError(); }, table_design_close: () => null });
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(300);
    expect(state().lostDraft!.columns).toHaveLength(3);
    expect(state().error).toBeNull();
    expect(store().isDirty(TAB)).toBe(true);
    expect(store().changeCount(TAB)).toBe(1);
    // A lost design takes no more edits.
    store().edit(TAB, addColumn('b'));
    expect(state().draft!.columns).toHaveLength(3);
  });

  it('a render that fails otherwise only sets the error', async () => {
    mockBackend({ table_design_open: () => opened(), table_design_render: () => { throw new Failure(appError('internal', 'oops')); }, table_design_close: () => null });
    await loaded();
    store().edit(TAB, addColumn('a'));
    await vi.advanceTimersByTimeAsync(300);
    expect(state().lostDraft).toBeNull();
    expect(state().error).toContain('oops');
  });

  it('a connection that closes marks its designs lost', async () => {
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    await loaded();
    store().edit(TAB, addColumn('a'));
    useConnections.getState().clearInfo(ids.prod);
    expect(state().lostDraft!.columns).toHaveLength(3);
  });

  it('reopen opens a fresh session, keeps the lost draft until restored, and restore is undoable', async () => {
    let n = 0;
    mockBackend({
      table_design_open: () => ({ ...opened(), design_id: `d${++n}` }),
      table_design_render: () => rendered,
      table_design_close: () => null,
    });
    await loaded();
    store().edit(TAB, addColumn('a'));
    store().markLost(TAB);
    await store().reopen(TAB);
    expect(state().open!.design_id).toBe('d2');
    expect(state().draft).toEqual(def);
    expect(state().lostDraft!.columns).toHaveLength(3);
    expect(store().isDirty(TAB)).toBe(true);
    store().restoreLostDraft(TAB);
    expect(state().lostDraft).toBeNull();
    expect(state().draft!.columns).toHaveLength(3);
    store().undo(TAB);
    expect(state().draft).toEqual(def);
  });

  it('a failed reopen keeps the lost draft', async () => {
    await loaded();
    store().edit(TAB, addColumn('a'));
    store().markLost(TAB);
    mockBackend({ table_design_open: () => { throw new Failure(appError('not_open', 'This connection is not open.')); } });
    await store().reopen(TAB);
    expect(state().lostDraft!.columns).toHaveLength(3);
    expect(state().error).toContain('not open');
  });

  it('discard clears a lost draft', async () => {
    await loaded();
    store().edit(TAB, addColumn('a'));
    store().markLost(TAB);
    store().discard(TAB);
    expect(state().lostDraft).toBeNull();
    expect(store().isDirty(TAB)).toBe(false);
  });

  it('the undo stack is capped at 100 steps', async () => {
    await loaded();
    for (let i = 0; i < 120; i++) store().edit(TAB, (d) => ({ ...d, comment: `c${i}` }));
    expect(state().past).toHaveLength(100);
  });

  it('counts a table rename and a comment change separately', async () => {
    await loaded();
    store().edit(TAB, (d) => ({ ...d, name: 'orders2', comment: 'hi' }));
    expect(store().changeCount(TAB)).toBe(2);
  });
});

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

describe('table design: undo coalescing, lost design id, sections', () => {
  const rename = (name: string) => (d: TableDef): TableDef => ({ ...d, name });

  it('edits with the same coalesce key make one undo step', async () => {
    await loaded();
    store().edit(TAB, rename('o'), 'k1');
    store().edit(TAB, rename('or'), 'k1');
    store().edit(TAB, rename('ord'), 'k1');
    expect(state().past).toHaveLength(1);
    store().edit(TAB, rename('ord2'), 'k2');
    store().edit(TAB, rename('ord3'));
    expect(state().past).toHaveLength(3);
    store().undo(TAB);
    store().undo(TAB);
    expect(state().draft!.name).toBe('ord');
    store().undo(TAB);
    expect(state().draft!.name).toBe('orders');
  });

  it('an undo ends the coalescing: the next edit with the old key is a new step', async () => {
    await loaded();
    store().edit(TAB, rename('a'), 'k');
    store().edit(TAB, rename('ab'), 'k');
    store().undo(TAB);
    store().edit(TAB, rename('x'), 'k');
    store().edit(TAB, rename('xy'), 'k');
    expect(state().past).toHaveLength(1);
    expect(state().future).toEqual([]);
    store().undo(TAB);
    expect(state().draft!.name).toBe('orders');
  });

  it('markLost records the open design; reopening gives a new one; restore and discard clear it', async () => {
    let n = 0;
    mockBackend({ table_design_open: () => ({ ...opened(), design_id: `d${++n}` }), table_design_render: () => rendered, table_design_close: () => null });
    await loaded();
    store().edit(TAB, addColumn('a'));
    store().markLost(TAB);
    expect(state().lostDesignId).toBe('d1');
    await store().reopen(TAB);
    expect(state().open!.design_id).toBe('d2');
    expect(state().lostDesignId).toBe('d1');
    store().restoreLostDraft(TAB);
    expect(state().lostDesignId).toBeNull();
    store().markLost(TAB);
    expect(state().lostDesignId).toBe('d2');
    store().discard(TAB);
    expect(state().lostDesignId).toBeNull();
    expect(state().lostDraft).toBeNull();
  });

  it('setSubTab keeps the section per tab, through a reload', async () => {
    await loaded();
    expect(state().subTab).toBe('columns');
    store().setSubTab(TAB, 'indexes');
    expect(state().subTab).toBe('indexes');
    await store().reload(TAB);
    expect(state().subTab).toBe('indexes');
  });
});