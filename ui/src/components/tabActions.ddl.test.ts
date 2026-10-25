import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DdlEditOpen } from '../bindings';
import { useDdlEdits } from '../store/ddlEdits';
import { useDialogs } from '../store/dialogs';
import { installCloseGuard, type CloseGuardWindow } from '../store/queryTabs';
import { useTabs, type DdlTabRef } from '../store/tabs';
import { callsOf, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { closeSavedTabs, closeTab, confirmDiscardConnection, confirmDiscardForImport } from './tabActions';

const scope = { database: 'shop', schema: null };
const tab: DdlTabRef = { id: 'ddl/one', kind: 'ddl', connectionId: ids.prod, objectKind: 'View', title: 'v' };
const open: DdlEditOpen = {
  edit_id: 'e1',
  text: 'CREATE VIEW v AS select 1',
  target: { kind: 'View', database: 'shop', schema: null, name: 'v', table: null },
  object: { database: 'shop', schema: null, name: 'v', kind: 'View' },
  server: serverInfo('MySql8'),
  env: null,
  read_only: false,
  strategies: [],
  strategy: null,
  blocked_reason: null,
  notes: [],
};

const question = () => {
  const q = useDialogs.getState().question;
  if (q?.kind !== 'discardEdits') throw new Error('expected the discardEdits question');
  return q;
};

async function dirtyDdl() {
  useTabs.getState().open(tab);
  await useDdlEdits.getState().load(tab.id, { connectionId: ids.prod, scope, kind: 'View', name: 'v', table: null });
  useDdlEdits.getState().setText(tab.id, 'CREATE VIEW v AS select 2');
}

function closeGuard() {
  let handler: ((e: { preventDefault(): void }) => unknown) | undefined;
  const win: CloseGuardWindow = {
    onCloseRequested: vi.fn(async (h) => {
      handler = h;
      return () => undefined;
    }),
    destroy: vi.fn(async () => undefined),
  };
  return { win, quit: () => handler!({ preventDefault: () => undefined }) };
}

beforeEach(() => {
  resetStores();
  mockBackend({ ddl_edit_open: () => open, ddl_edit_close: () => null, tabs_save: () => null, query_close_tab: () => null });
});

describe('DDL editor tab close guards', () => {
  it('closing_dirty_ddl_tab_asks', async () => {
    await dirtyDdl();
    const closing = closeTab(tab);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    expect(question().count).toBe(1);
    question().done(false);
    expect(await closing).toBe(false);
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useDdlEdits.getState().isDirty(tab.id)).toBe(true);

    // Close saved leaves a dirty DDL tab open.
    await closeSavedTabs();
    expect(useTabs.getState().tabs).toHaveLength(1);

    const again = closeTab(tab);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    expect(await again).toBe(true);
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useDdlEdits.getState().byTab[tab.id]).toBeUndefined();
    expect(callsOf('ddl_edit_close')).toEqual([{ editId: 'e1' }]);
  });

  it('a clean DDL tab closes without asking', async () => {
    useTabs.getState().open(tab);
    await useDdlEdits.getState().load(tab.id, { connectionId: ids.prod, scope, kind: 'View', name: 'v', table: null });
    expect(await closeTab(tab)).toBe(true);
    expect(useDialogs.getState().question).toBeNull();
  });

  it('deleting the connection asks, and closeConnection closes the DDL tab', async () => {
    await dirtyDdl();
    const asking = confirmDiscardConnection(ids.prod);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(false);
    expect(await asking).toBe(false);
    const again = confirmDiscardConnection(ids.prod);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    expect(await again).toBe(true);
    useTabs.getState().closeConnection(ids.prod);
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useDdlEdits.getState().byTab[tab.id]).toBeUndefined();
  });

  it('an import over the connection counts dirty DDL tabs', async () => {
    await dirtyDdl();
    const asking = confirmDiscardForImport([ids.prod]);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardForImport'));
    const q = useDialogs.getState().question;
    expect(q).toMatchObject({ kind: 'discardForImport', count: 1 });
    if (q?.kind === 'discardForImport') q.done(false);
    expect(await asking).toBe(false);
  });

  it('quitting asks about dirty DDL text, and waits while a DDL edit is applied', async () => {
    await dirtyDdl();
    const { win, quit } = closeGuard();
    await installCloseGuard(win);

    useDialogs.setState({ ddlEditRunning: true });
    await quit();
    expect(useDialogs.getState().question).toBeNull();
    expect(win.destroy).not.toHaveBeenCalled();
    useDialogs.setState({ ddlEditRunning: false });

    const first = quit();
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(false);
    await first;
    expect(win.destroy).not.toHaveBeenCalled();

    const second = quit();
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    await second;
    expect(win.destroy).toHaveBeenCalled();
  });
});
