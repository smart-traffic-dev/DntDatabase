// Typing must stay cheap: a keystroke replaces the query docs Map, and the tab strip must not
// re-render for it (it only needs to know which tabs show the unsaved dot / open transaction).
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { Profiler } from 'react';
import { beforeEach, describe, expect, it } from 'vitest';
import { emptyRun, useQueryRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import { mockBackend, resetStores } from '../test/backend';
import { TabBar } from './TabBar';

beforeEach(() => {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
  mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
});

const type = (id: string, n: number) => {
  for (let i = 1; i <= n; i++) act(() => useQueryDocs.getState().update(id, { sql: 'x'.repeat(i) }));
};

function profiled() {
  let commits = 0;
  render(
    <Profiler id="tabbar" onRender={() => void commits++}>
      <TabBar />
    </Profiler>,
  );
  return () => commits;
}

describe('TabBar renders', () => {
  it('fifty_keystrokes_do_not_re_render_the_tab_strip', () => {
    const a = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().newQuery(null, null);
    const commits = profiled();
    const before = commits();
    type(a, 50);
    expect(useQueryDocs.getState().docs.get(a)?.sql).toHaveLength(50);
    expect(commits() - before).toBe(0);
  });

  it('a file tab re-renders once when the unsaved dot appears, then not per keystroke', () => {
    const a = useQueryDocs.getState().newQuery(null, null);
    act(() => useQueryDocs.getState().update(a, { filePath: 'C:\\sql\\daily.sql', title: 'daily.sql' }));
    const commits = profiled();
    const before = commits();
    type(a, 1);
    expect(commits() - before).toBe(1);
    expect(screen.getByRole('tab', { name: /daily\.sql/ }).querySelector('.tab-dirty')).not.toBeNull();
    type(a, 50);
    expect(commits() - before).toBe(1);
  });

  it('still follows an open transaction', () => {
    const a = useQueryDocs.getState().newQuery(null, null);
    profiled();
    expect(screen.getByRole('tab').querySelector('.tab-tx')).toBeNull();
    act(() => useQueryRun.setState({ runs: new Map([[a, { ...emptyRun, txState: 'active' }]]) }));
    expect(screen.getByRole('tab').querySelector('.tab-tx')).not.toBeNull();
  });
});

const objectTab = (name: string): ObjectTabRef => {
  const objectRef = { database: 'shop', schema: null, name, kind: 'Table' as const };
  return { id: objectTabId('c1', objectRef), kind: 'object', connectionId: 'c1', objectRef, title: name };
};
const menu = (name: string) => {
  fireEvent.contextMenu(within(screen.getByRole('tablist', { name: 'Open tabs' })).getByRole('tab', { name: new RegExp(`${name}$`) }));
  return screen.getByRole('menu', { name: 'Tabs' });
};

describe('TabBar context menu', () => {
  beforeEach(() => {
    for (const n of ['a', 'b', 'c']) act(() => useTabs.getState().open(objectTab(n)));
  });

  it('offers Close others, Close tabs to the right, Pin and Reopen closed tab with the right enablement', () => {
    render(<TabBar />);
    const m = menu('c');
    const item = (label: string) => screen.getByRole('menuitem', { name: new RegExp(`^${label}`) });
    expect(item('Close others')).toBeEnabled();
    // The last tab has nothing to its right; nothing was closed yet.
    expect(item('Close tabs to the right')).toBeDisabled();
    expect(item('Reopen closed tab')).toBeDisabled();
    expect(item('Pin')).toBeEnabled();
    expect(m).toBeInTheDocument();
    fireEvent.keyDown(document, { key: 'Escape' });
    menu('a');
    expect(item('Close tabs to the right')).toBeEnabled();
  });

  it('Pin moves the tab first with a pin glyph; Unpin takes it away', () => {
    render(<TabBar />);
    menu('c');
    fireEvent.click(screen.getByRole('menuitem', { name: /^Pin/ }));
    const strip = screen.getByRole('tablist', { name: 'Open tabs' });
    expect(within(strip).getAllByRole('tab').map((t) => t.textContent)).toEqual(['c', 'a', 'b']);
    expect(screen.getByRole('tab', { name: /c$/ }).querySelector('.tab-pin')).not.toBeNull();
    menu('c');
    expect(screen.getByRole('menuitem', { name: /^Unpin/ })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('menuitem', { name: /^Unpin/ }));
    expect(screen.getByRole('tab', { name: /c$/ }).querySelector('.tab-pin')).toBeNull();
  });

  it('Close others leaves pinned tabs; Reopen closed tab brings one back', async () => {
    render(<TabBar />);
    menu('a');
    fireEvent.click(screen.getByRole('menuitem', { name: /^Pin/ }));
    menu('b');
    fireEvent.click(screen.getByRole('menuitem', { name: /^Close others/ }));
    await waitFor(() => expect(useTabs.getState().tabs.map((t) => t.title)).toEqual(['a', 'b']));
    menu('b');
    expect(screen.getByRole('menuitem', { name: /^Reopen closed tab/ })).toBeEnabled();
    fireEvent.click(screen.getByRole('menuitem', { name: /^Reopen closed tab/ }));
    expect(useTabs.getState().tabs.map((t) => t.title)).toEqual(['a', 'b', 'c']);
  });
});
