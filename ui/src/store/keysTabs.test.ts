import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { callsOf, config, fail, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useConnections } from './connections';
import { KEYS_TABS_KEY, persistKeysTabs, restoreKeysTabs, syncKeysTabs, useKeysTabs, valueCounts } from './keysTabs';
import { keysTabId, openKeysTab, useTabs } from './tabs';

let stop = () => {};

beforeEach(() => {
  resetStores();
  useKeysTabs.getState().reset();
  localStorage.removeItem(KEYS_TABS_KEY);
  mockBackend({ tabs_save: () => null });
  useConnections.getState().setStored([], [config(ids.prod, 'cache', { kind: 'redis', port: 6379 }), config(ids.legacy, 'other', { kind: 'redis', port: 6379 })]);
});

afterEach(() => stop());

describe('keys tabs', () => {
  it('one tab per database: opening it again focuses it', () => {
    openKeysTab(ids.prod, '0');
    openKeysTab(ids.prod, '2');
    expect(useTabs.getState().activeId).toBe(keysTabId(ids.prod, '2'));
    openKeysTab(ids.prod, '0');
    const { tabs, activeId } = useTabs.getState();
    expect(tabs).toHaveLength(2);
    expect(activeId).toBe(`keys:${ids.prod}:0`);
    expect(tabs[0]).toMatchObject({ kind: 'keys', connectionId: ids.prod, database: '0', title: 'cache · db0' });
  });

  it('closing the connection closes its keys tabs', () => {
    openKeysTab(ids.prod, '0');
    openKeysTab(ids.legacy, '0');
    useTabs.getState().closeConnection(ids.prod);
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([keysTabId(ids.legacy, '0')]);
  });

  it('keys_tab_restores_after_reload', () => {
    stop = persistKeysTabs();
    openKeysTab(ids.prod, '0');
    openKeysTab(ids.legacy, '3');
    useTabs.getState().setPinned(keysTabId(ids.legacy, '3'), true);
    stop();
    stop = () => {};

    // A restart: the tabs are gone, the stored connections come back without one of them.
    useTabs.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'cache', { kind: 'redis', port: 6379 })]);
    restoreKeysTabs();
    expect(useTabs.getState().tabs).toEqual([{ kind: 'keys', id: keysTabId(ids.prod, '0'), connectionId: ids.prod, database: '0', title: 'cache · db0' }]);

    // Unknown connections (the tree failed to load) keep every tab.
    useTabs.getState().reset();
    restoreKeysTabs({ connectionsKnown: false });
    expect(useTabs.getState().tabs.map((t) => [t.id, !!t.pinned])).toEqual([
      [keysTabId(ids.legacy, '3'), true],
      [keysTabId(ids.prod, '0'), false],
    ]);
  });

  it('a restored keys tab scans again once its connection opens', async () => {
    let open = false;
    mockBackend({
      tabs_save: () => null,
      keys_scan: () => {
        if (!open) throw fail('not_open');
        return { keys: [{ name: { display: 'a', raw: 'YQ==' }, key_type: 'String', ttl_ms: null }], next_cursor: null };
      },
    });
    stop = syncKeysTabs();
    openKeysTab(ids.prod, '0');
    openKeysTab(ids.legacy, '0');
    const tabId = keysTabId(ids.prod, '0');
    const other = keysTabId(ids.legacy, '0');
    // The tabs mount (restored at startup) before their connections are open: the scans fail.
    useKeysTabs.getState().ensure({ id: tabId, connectionId: ids.prod, database: '0' });
    useKeysTabs.getState().ensure({ id: other, connectionId: ids.legacy, database: '0' });
    await vi.waitFor(() => expect(useKeysTabs.getState().byTab[tabId]!.error).not.toBeNull());
    expect(callsOf('keys_scan')).toHaveLength(2);

    open = true;
    useConnections.getState().setInfo(ids.prod, serverInfo('Redis'));
    await vi.waitFor(() => expect(useKeysTabs.getState().byTab[tabId]!.keys).toHaveLength(1));
    // Only the tab of the connection that opened scans again.
    expect(callsOf('keys_scan').map((c) => c.id)).toEqual([ids.prod, ids.legacy, ids.prod]);
    expect(useKeysTabs.getState().byTab[tabId]!.error).toBeNull();

    // A tab that already scanned is left alone when the info changes again.
    useConnections.getState().setInfo(ids.prod, serverInfo('Redis'));
    await Promise.resolve();
    expect(callsOf('keys_scan')).toHaveLength(3);
  });

  it('closing a keys tab drops its state', async () => {
    mockBackend({ tabs_save: () => null, keys_scan: () => ({ keys: [], next_cursor: null }) });
    stop = syncKeysTabs();
    openKeysTab(ids.prod, '0');
    openKeysTab(ids.prod, '1');
    const a = keysTabId(ids.prod, '0');
    const b = keysTabId(ids.prod, '1');
    useKeysTabs.getState().ensure({ id: a, connectionId: ids.prod, database: '0' });
    useKeysTabs.getState().ensure({ id: b, connectionId: ids.prod, database: '1' });
    // Switching tabs keeps both.
    useTabs.getState().activate(a);
    expect(Object.keys(useKeysTabs.getState().byTab).sort()).toEqual([a, b].sort());
    useTabs.getState().close(a);
    expect(Object.keys(useKeysTabs.getState().byTab)).toEqual([b]);
  });

  it('junk in storage restores nothing', () => {
    localStorage.setItem(KEYS_TABS_KEY, '{nope');
    restoreKeysTabs();
    localStorage.setItem(KEYS_TABS_KEY, JSON.stringify([{ connectionId: 1 }, 'x', null]));
    restoreKeysTabs();
    expect(useTabs.getState().tabs).toEqual([]);
  });

  it('valueCounts reports loaded and total items', () => {
    expect(valueCounts({ List: { items: [{ t: 'Text', v: 'a' }], total: 3 } })).toEqual({ loaded: 1, total: 3 });
    expect(valueCounts({ String: { value: { t: 'Text', v: 'a' } } })).toBeNull();
  });
});
