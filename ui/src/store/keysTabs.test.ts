import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { config, ids, mockBackend, resetStores } from '../test/backend';
import { useConnections } from './connections';
import { KEYS_TABS_KEY, persistKeysTabs, restoreKeysTabs, useKeysTabs, valueCounts } from './keysTabs';
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
