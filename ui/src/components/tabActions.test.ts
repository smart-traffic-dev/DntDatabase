import { beforeEach, describe, expect, it } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { useQueryDocs } from '../store/queryTabs';
import { CLOSED_STACK_MAX, objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import { useConnections } from '../store/connections';
import { callsOf, config, mockBackend, resetStores } from '../test/backend';
import { closeOtherTabs, closeSavedTabs, closeTab, closeTabsToRight, goToTab, reopenClosedTab, togglePin } from './tabActions';

const objectTab = (name: string): ObjectTabRef => {
  const objectRef = { database: 'shop', schema: null, name, kind: 'Table' as const };
  return { id: objectTabId('c1', objectRef), kind: 'object', connectionId: 'c1', objectRef, title: name };
};
const titles = () => useTabs.getState().tabs.map((t) => t.title);
const tab = (name: string) => useTabs.getState().tabs.find((t) => t.title === name)!;

beforeEach(() => {
  resetStores();
  mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
});

describe('go to tab', () => {
  it('Ctrl+N goes to tab N, Ctrl+9 to the last, and a missing tab is ignored', () => {
    for (const n of ['a', 'b', 'c']) useTabs.getState().open(objectTab(n));
    goToTab(1);
    expect(useTabs.getState().activeId).toBe(tab('a').id);
    goToTab(2);
    expect(useTabs.getState().activeId).toBe(tab('b').id);
    goToTab(8);
    expect(useTabs.getState().activeId).toBe(tab('b').id);
    goToTab(9, true);
    expect(useTabs.getState().activeId).toBe(tab('c').id);
  });
});

describe('pinning', () => {
  it('pinned tabs sort first, in the order they were pinned; unpinning puts a tab after them', () => {
    for (const n of ['a', 'b', 'c', 'd']) useTabs.getState().open(objectTab(n));
    togglePin(tab('c'));
    expect(titles()).toEqual(['c', 'a', 'b', 'd']);
    togglePin(tab('d'));
    expect(titles()).toEqual(['c', 'd', 'a', 'b']);
    expect(useTabs.getState().tabs.filter((t) => t.pinned).map((t) => t.title)).toEqual(['c', 'd']);
    togglePin(tab('c'));
    expect(titles()).toEqual(['d', 'c', 'a', 'b']);
    expect(tab('c').pinned).toBe(false);
  });

  it('a new tab opens after the pinned ones, and the active tab is unchanged by pinning', () => {
    useTabs.getState().open(objectTab('a'));
    useTabs.getState().open(objectTab('b'));
    togglePin(tab('b'));
    expect(useTabs.getState().activeId).toBe(tab('b').id);
    // With a pinned tab active, a new tab opens right after the pinned block (VS Code).
    useTabs.getState().open(objectTab('c'));
    expect(titles()).toEqual(['b', 'c', 'a']);
  });

  it('renaming or rebinding keeps the pin', () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    togglePin(tab('Query 1'));
    useTabs.getState().rename(id, 'daily');
    useTabs.getState().bind(id, 'c9');
    expect(useTabs.getState().tabs[0]).toMatchObject({ title: 'daily', connectionId: 'c9', pinned: true });
  });
});

describe('close others / to the right', () => {
  beforeEach(() => {
    for (const n of ['a', 'b', 'c', 'd', 'e']) useTabs.getState().open(objectTab(n));
  });

  it('close others keeps the tab and the pinned tabs', async () => {
    togglePin(tab('a'));
    await closeOtherTabs(tab('c'));
    expect(titles()).toEqual(['a', 'c']);
  });

  it('close tabs to the right skips pinned tabs and leaves the left ones', async () => {
    togglePin(tab('e'));
    expect(titles()).toEqual(['e', 'a', 'b', 'c', 'd']);
    await closeTabsToRight(tab('b'));
    expect(titles()).toEqual(['e', 'a', 'b']);
  });

  it('closing the active pinned tab with the close action still works', async () => {
    togglePin(tab('b'));
    useTabs.getState().activate(tab('b').id);
    await closeTab(tab('b'));
    expect(titles()).toEqual(['a', 'c', 'd', 'e']);
  });
});

describe('reopen closed tab', () => {
  it('reopens an object tab by reference, most recent first', async () => {
    for (const n of ['a', 'b']) useTabs.getState().open(objectTab(n));
    await closeTab(tab('a'));
    await closeTab(tab('b'));
    expect(useTabs.getState().closed.map((c) => (c.kind === 'object' ? c.tab.title : ''))).toEqual(['a', 'b']);
    expect(reopenClosedTab()).toBe(true);
    expect(titles()).toEqual(['b']);
    expect(useTabs.getState().activeId).toBe(tab('b').id);
    expect(reopenClosedTab()).toBe(true);
    expect(titles()).toEqual(['b', 'a']);
    expect(reopenClosedTab()).toBe(false);
  });

  it('a pinned object tab comes back unpinned', async () => {
    useTabs.getState().open(objectTab('a'));
    useTabs.getState().open(objectTab('b'));
    togglePin(tab('b'));
    await closeTab(tab('b'));
    reopenClosedTab();
    expect(tab('b').pinned).toBeFalsy();
  });

  it('reopens a query tab with its text, file and settings, and saves it again', async () => {
    useConnections.getState().setStored([], [config('c1', 'shop-prod')]);
    const id = useQueryDocs.getState().newQuery('c1', 'shop');
    useQueryDocs.getState().update(id, { sql: 'SELECT 42;', cursor: 5, rowLimit: 1000, continueOnError: true });
    expect(await closeTab(tab('Query 1'))).toBe(true);
    expect(useQueryDocs.getState().docs.has(id)).toBe(false);
    expect(callsOf('tabs_delete')).toHaveLength(1);
    expect(useTabs.getState().closed).toHaveLength(1);

    expect(reopenClosedTab()).toBe(true);
    const doc = useQueryDocs.getState().docs.get(id);
    expect(doc).toMatchObject({ sql: 'SELECT 42;', cursor: 5, rowLimit: 1000, continueOnError: true, database: 'shop' });
    expect(useTabs.getState().activeId).toBe(id);
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([id]);
    expect(useTabs.getState().closed).toEqual([]);
  });

  it('a query tab whose connection was deleted meanwhile comes back unbound', async () => {
    const id = useQueryDocs.getState().newQuery('gone', 'shop');
    await closeTab(tab('Query 1'));
    // 'gone' is not a stored connection.
    reopenClosedTab();
    expect(useQueryDocs.getState().docs.get(id)).toMatchObject({ connectionId: null, database: null });
    expect(tab('Query 1')).toMatchObject({ connectionId: null });
  });

  it('a close that is cancelled leaves the stack alone', async () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { filePath: 'C:\\x.sql', dirty: true, sql: 'x' });
    // The discard question comes up; answering "keep" cancels the close.
    const closing = closeTab(tab('Query 1'));
    await Promise.resolve();
    const q = useDialogs.getState().question;
    expect(q?.kind).toBe('discard');
    if (q?.kind === 'discard') q.done(false);
    expect(await closing).toBe(false);
    expect(useTabs.getState().closed).toEqual([]);
  });

  it(`keeps only the last ${CLOSED_STACK_MAX} closed tabs`, async () => {
    for (let i = 0; i < 13; i++) useTabs.getState().open(objectTab(`t${i}`));
    for (let i = 0; i < 13; i++) await closeTab(tab(`t${i}`));
    const closed = useTabs.getState().closed;
    expect(closed).toHaveLength(CLOSED_STACK_MAX);
    expect(closed[0]?.kind === 'object' && closed[0].tab.title).toBe('t3');
    expect(closed.at(-1)?.kind === 'object' && (closed.at(-1) as { tab: ObjectTabRef }).tab.title).toBe('t12');
  });

  it('closing the connection (deleting it) does not fill the stack', () => {
    useTabs.getState().open(objectTab('a'));
    useTabs.getState().closeConnection('c1');
    expect(useTabs.getState().closed).toEqual([]);
  });
});

describe('closeSavedTabs', () => {
  beforeEach(resetStores);

  it('closes the tabs of the focused group that have nothing unsaved', async () => {
    mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
    useQueryDocs.getState().newQuery(null, null);
    const file = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(file, { filePath: 'C:\\sql\\daily.sql', title: 'daily.sql' });
    useQueryDocs.getState().update(file, { sql: 'edited' });
    useTabs.getState().open(objectTab('orders'));
    await closeSavedTabs();
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([file]);
  });
});