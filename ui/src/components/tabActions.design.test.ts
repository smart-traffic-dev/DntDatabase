import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { installCloseGuard, setRunningSource, useQueryDocs, type CloseGuardWindow } from '../store/queryTabs';
import { useTableDesigns } from '../store/tableDesigns';
import { designTabId, useTabs, type DesignTabRef } from '../store/tabs';
import type { DesignOpen, TableDef } from '../bindings';
import { callsOf, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { closeAllTabs, closeOtherTabs, closeSavedTabs, closeTab, confirmDiscardConnection, confirmDiscardForImport } from './tabActions';

const scope = { database: 'shop', schema: null };
const tab: DesignTabRef = { id: designTabId(ids.prod, scope, 'orders'), kind: 'design', connectionId: ids.prod, scope, table: 'orders', title: 'orders' };
const def = {
  original_name: 'orders',
  name: 'orders',
  columns: [],
  primary_key: null,
  indexes: [],
  foreign_keys: [],
  checks: [],
  options: { engine: null, charset: null, collation: null, row_format: null, locked: false },
  comment: null,
  read_only_reason: null,
} satisfies TableDef;
const open: DesignOpen = { design_id: 'd1', def, locked: [], catalog: [], server: serverInfo('MySql8'), env: null, read_only: false };

const question = () => {
  const q = useDialogs.getState().question;
  if (q?.kind !== 'discardEdits') throw new Error('expected the discardEdits question');
  return q;
};

async function dirtyDesign() {
  useTabs.getState().open(tab);
  await useTableDesigns.getState().load(tab.id, ids.prod, scope, 'orders');
  useTableDesigns.getState().edit(tab.id, (d) => ({ ...d, comment: 'changed' }));
}

beforeEach(() => {
  resetStores();
  mockBackend({ table_design_open: () => open, table_design_render: () => ({ statements: [], impacts: [], errors: [], error: null }), table_design_close: () => null });
});

describe('design tabs with connections, bulk closes and quitting', () => {
  it('deleting a connection asks about its dirty designs, and closing it closes the tab and session', async () => {
    await dirtyDesign();
    const asking = confirmDiscardConnection(ids.prod);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(false);
    expect(await asking).toBe(false);
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);

    const again = confirmDiscardConnection(ids.prod);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    expect(await again).toBe(true);
    useTabs.getState().closeConnection(ids.prod);
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useTableDesigns.getState().byTab[tab.id]).toBeUndefined();
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }]);
  });

  it('a clean design does not make a connection delete ask', async () => {
    useTabs.getState().open(tab);
    await useTableDesigns.getState().load(tab.id, ids.prod, scope, 'orders');
    expect(await confirmDiscardConnection(ids.prod)).toBe(true);
    expect(useDialogs.getState().question).toBeNull();
  });

  it('an import that replaces the connection counts dirty designs in its question', async () => {
    await dirtyDesign();
    const asking = confirmDiscardForImport([ids.prod]);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardForImport'));
    const q = useDialogs.getState().question;
    expect(q).toMatchObject({ kind: 'discardForImport', count: 1 });
    if (q?.kind === 'discardForImport') q.done(false);
    expect(await asking).toBe(false);
    expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);
  });

  it('Close others and Close all ask about a dirty design; Close saved leaves it open', async () => {
    const other: DesignTabRef = { ...tab, id: designTabId(ids.prod, scope, 'other'), table: 'other', title: 'other' };
    useTabs.getState().open(other);
    await dirtyDesign();
    await closeSavedTabs();
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([tab.id]);
    useTabs.getState().open(other);

    const closing = closeOtherTabs(other);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(false);
    await closing;
    expect(useTabs.getState().tabs.map((t) => t.id)).toContain(tab.id);

    const all = closeAllTabs();
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    await all;
    expect(useTabs.getState().tabs).toEqual([]);
  });

  it('quitting keeps the drafts when a later quit question is cancelled', async () => {
    await dirtyDesign();
    mockBackend({ tabs_save: () => null, query_close_tab: () => null, table_design_close: () => null });
    useQueryDocs.getState().newQuery(null, null);
    setRunningSource(() => true);
    try {
      let handler: ((e: { preventDefault(): void }) => unknown) | undefined;
      const win: CloseGuardWindow = {
        onCloseRequested: vi.fn(async (h) => {
          handler = h;
          return () => undefined;
        }),
        destroy: vi.fn(async () => undefined),
      };
      await installCloseGuard(win);
      const quitting = handler!({ preventDefault: () => undefined });
      await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
      question().done(true);
      await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('appClose'));
      const q = useDialogs.getState().question;
      if (q?.kind === 'appClose') q.done();
      await quitting;
      expect(win.destroy).not.toHaveBeenCalled();
      expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);
      expect(useTableDesigns.getState().byTab[tab.id]?.draft?.comment).toBe('changed');
    } finally {
      setRunningSource(null);
    }
  });
});

describe('design tab close guards', () => {
  it('a clean design tab closes without asking and closes its session', async () => {
    useTabs.getState().open(tab);
    await useTableDesigns.getState().load(tab.id, ids.prod, scope, 'orders');
    expect(await closeTab(tab)).toBe(true);
    expect(useDialogs.getState().question).toBeNull();
    expect(useTabs.getState().tabs).toEqual([]);
    expect(callsOf('table_design_close')).toEqual([{ designId: 'd1' }]);
  });

  it('closing_dirty_design_tab_asks', async () => {
    await dirtyDesign();
    const closing = closeTab(tab);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    expect(question().count).toBe(1);
    question().done(false);
    expect(await closing).toBe(false);
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useTableDesigns.getState().isDirty(tab.id)).toBe(true);

    const again = closeTab(tab);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    expect(await again).toBe(true);
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useTableDesigns.getState().byTab[tab.id]).toBeUndefined();
  });

  it('the app close guard asks about a dirty design too', async () => {
    await dirtyDesign();
    let handler: ((e: { preventDefault(): void }) => unknown) | undefined;
    const win: CloseGuardWindow = {
      onCloseRequested: vi.fn(async (h) => {
        handler = h;
        return () => undefined;
      }),
      destroy: vi.fn(async () => undefined),
    };
    await installCloseGuard(win);
    mockBackend({ tabs_save: () => null, query_close_tab: () => null, table_design_close: () => null });

    const first = handler!({ preventDefault: () => undefined });
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(false);
    await first;
    expect(win.destroy).not.toHaveBeenCalled();

    const second = handler!({ preventDefault: () => undefined });
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    await second;
    expect(win.destroy).toHaveBeenCalled();
  });
});
