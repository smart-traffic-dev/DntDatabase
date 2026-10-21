import { act, render, screen, waitFor, within } from '@testing-library/react';
import { DialogHost } from '../dialogs/DialogHost';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { EnvTag } from '../bindings';
import userEvent from '@testing-library/user-event';
import { useConnections } from '../store/connections';
import { emptyRun, useQueryRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs } from '../store/tabs';
import { callsOf, config, fail, ids, mockBackend, resetStores } from '../test/backend';
import { TabBar } from './TabBar';

const orders = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };

function openTab(connectionId: string, title: string) {
  useTabs.getState().open({ id: objectTabId(connectionId, { ...orders, name: title }), kind: 'object', connectionId, objectRef: { ...orders, name: title }, title });
}

describe('TabBar', () => {
  beforeEach(resetStores);

  it('shows an environment badge for each tab connection', () => {
    const tagged = (id: string, name: string, env_tag: EnvTag | null) => config(id, name, { env_tag });
    act(() => {
      useConnections.getState().setStored([], [tagged(ids.prod, 'prod', 'production'), tagged(ids.analytics, 'stats', 'staging'), tagged(ids.legacy, 'crm', null)]);
      openTab(ids.prod, 'p_orders');
      openTab(ids.analytics, 's_orders');
      openTab(ids.legacy, 'c_orders');
    });
    render(<TabBar />);
    const tab = (title: string) => screen.getByRole('tab', { name: new RegExp(title) }).closest('.tab')!;
    expect(tab('p_orders')).toHaveClass('env-production');
    expect(tab('p_orders').querySelector('.env-tag.env-production')).toHaveAttribute('title', 'Environment: production');
    expect(tab('p_orders').querySelector('.env-tag')?.textContent).toBe('P');
    expect(tab('s_orders').querySelector('.env-tag.env-staging')).not.toBeNull();
    expect(tab('s_orders').querySelector('.env-tag')?.textContent).toBe('S');
    expect(tab('c_orders').querySelector('.env-tag')).toBeNull();
    expect(tab('c_orders')).not.toHaveClass('env-production');
  });

  it('closing a query tab removes its doc', async () => {
    mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
    const id = useQueryDocs.getState().newQuery(null, null);
    render(<TabBar />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Close Query 1' }));
    await waitFor(() => expect(useTabs.getState().tabs).toEqual([]));
    expect(useQueryDocs.getState().docs.has(id)).toBe(false);
  });

  it('dirty_dot_only_for_file_tabs', () => {
    mockBackend({ tabs_save: () => null });
    const plain = useQueryDocs.getState().newQuery(null, null);
    const file = useQueryDocs.getState().newQuery(null, null);
    act(() => {
      useQueryDocs.getState().update(plain, { sql: 'edited', dirty: true });
      useQueryDocs.getState().update(file, { filePath: 'C:\\sql\\daily.sql', title: 'daily.sql' });
    });
    render(<TabBar />);
    const dot = (name: RegExp) => screen.getByRole('tab', { name }).querySelector('.tab-dirty');
    expect(dot(/Query 1/)).toBeNull();
    expect(dot(/daily\.sql/)).toBeNull();
    act(() => useQueryDocs.getState().update(file, { sql: 'edited' }));
    expect(dot(/daily\.sql/)).toHaveTextContent('●');
    expect(dot(/daily\.sql/)).toHaveAttribute('title', 'Unsaved changes');
    expect(dot(/Query 1/)).toBeNull();
  });

  it('closing a query tab goes through the close guards', async () => {
    mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
    const id = useQueryDocs.getState().newQuery(null, null);
    render(<TabBar />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Close Query 1' }));
    await waitFor(() => expect(useTabs.getState().tabs).toEqual([]));
    expect(callsOf('query_close_tab')).toEqual([{ tabId: id, force: false }]);
    expect(callsOf('tabs_delete')).toEqual([{ id }]);
  });

  it('closing a tab with an open transaction asks first and Cancel keeps it', async () => {
    mockBackend({
      tabs_save: () => null,
      tabs_delete: () => null,
      query_close_tab: (a) => {
        if (!a.force) throw fail('transaction_open');
        return null;
      },
      query_tx: () => 'idle',
    });
    const id = useQueryDocs.getState().newQuery(null, null);
    render(
      <>
        <TabBar />
        <DialogHost />
      </>,
    );
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Close Query 1' }));
    const dialog = await screen.findByRole('alertdialog');
    expect(dialog).toHaveTextContent(/This tab has an open transaction/);
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toHaveFocus();
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull());
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([id]);
    expect(useQueryDocs.getState().docs.has(id)).toBe(true);
    // Never forced closed, never committed or rolled back behind the user's back.
    expect(callsOf('query_close_tab')).toEqual([{ tabId: id, force: false }]);
    expect(callsOf('query_tx')).toEqual([]);
    expect(callsOf('tabs_delete')).toEqual([]);
  });

  it('query_tab_shows_sql_label', () => {
    act(() => {
      useConnections.getState().setStored([], [config(ids.prod, 'prod', { env_tag: 'production' })]);
      useTabs.getState().open({ id: 'q1', kind: 'query', connectionId: ids.prod, title: 'Query 1' });
      useTabs.getState().open({ id: 'q2', kind: 'query', connectionId: null, title: 'Query 2' });
    });
    render(<TabBar />);
    const q1 = screen.getByRole('tab', { name: /Query 1/ });
    expect(q1).toHaveTextContent('SQL');
    expect(q1.closest('.tab')).toHaveClass('env-production');
    expect(screen.getByRole('tab', { name: /Query 2/ })).toHaveTextContent('SQL');
  });
  it('marks read-only connections and tabs with an open transaction', () => {
    act(() => {
      useConnections.getState().setStored([], [config(ids.prod, 'prod', { read_only: true })]);
      useTabs.getState().open({ id: 'q1', kind: 'query', connectionId: ids.prod, title: 'Query 1' });
      useTabs.getState().open({ id: 'q2', kind: 'query', connectionId: null, title: 'Query 2' });
      useQueryRun.setState({ runs: new Map([['q1', { ...emptyRun, txState: 'active' }]]) });
    });
    render(<TabBar />);
    const q1 = screen.getByRole('tab', { name: /Query 1/ });
    expect(q1.querySelector('.tab-ro')).not.toBeNull();
    expect(q1.querySelector('.tab-tx')).toHaveAttribute('title', 'Open transaction');
    const q2 = screen.getByRole('tab', { name: /Query 2/ });
    expect(q2.querySelector('.tab-ro')).toBeNull();
    expect(q2.querySelector('.tab-tx')).toBeNull();
    expect(q1.closest('.tab')).toHaveAttribute('title', expect.stringContaining('Read-only'));
  });

  it('lists all tabs in an overflow menu and activates the picked one', async () => {
    act(() => {
      useTabs.getState().open({ id: 'q1', kind: 'query', connectionId: null, title: 'Query 1' });
      useTabs.getState().open({ id: 'q2', kind: 'query', connectionId: null, title: 'Query 2' });
    });
    render(<TabBar />);
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'List all tabs' }));
    await user.click(screen.getByRole('menuitemradio', { name: 'Query 1' }));
    expect(useTabs.getState().activeId).toBe('q1');
  });

  it('closes the other tabs from the context menu', async () => {
    mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
    act(() => {
      useQueryDocs.getState().newQuery(null, null);
      useQueryDocs.getState().newQuery(null, null);
      useQueryDocs.getState().newQuery(null, null);
    });
    render(<TabBar />);
    const keep = screen.getByRole('tab', { name: /Query 2/ });
    const user = userEvent.setup();
    await user.pointer({ keys: '[MouseRight]', target: keep });
    await user.click(screen.getByRole('menuitem', { name: 'Close others' }));
    await waitFor(() => expect(useTabs.getState().tabs.map((x) => x.title)).toEqual(['Query 2']));
  });

  it('moves between tabs with the arrow keys, Home and End (one tab stop)', async () => {
    act(() => ['alpha', 'beta', 'gamma'].forEach((title) => openTab(ids.prod, title)));
    const idOf = (title: string) => objectTabId(ids.prod, { ...orders, name: title });
    render(<TabBar />);
    const user = userEvent.setup();
    const tab = (title: string) => screen.getByRole('tab', { name: new RegExp(title) });
    // Only the active tab is in the Tab order.
    expect(tab('gamma')).toHaveAttribute('tabindex', '0');
    expect(tab('alpha')).toHaveAttribute('tabindex', '-1');
    expect(tab('alpha')).toHaveAttribute('aria-controls', 'tab-content');

    act(() => tab('gamma').focus());
    await user.keyboard('{Home}');
    expect(useTabs.getState().activeId).toBe(idOf('alpha'));
    expect(tab('alpha')).toHaveFocus();
    await user.keyboard('{ArrowRight}');
    expect(useTabs.getState().activeId).toBe(idOf('beta'));
    expect(tab('beta')).toHaveFocus();
    await user.keyboard('{End}');
    expect(useTabs.getState().activeId).toBe(idOf('gamma'));
    // Wraps around at both ends.
    await user.keyboard('{ArrowRight}');
    expect(useTabs.getState().activeId).toBe(idOf('alpha'));
    await user.keyboard('{ArrowLeft}');
    expect(useTabs.getState().activeId).toBe(idOf('gamma'));
  });

  it('opens the tab menu from the keyboard (Shift+F10) and its arrows move between items', async () => {
    act(() => ['alpha', 'beta'].forEach((title) => openTab(ids.prod, title)));
    render(<TabBar />);
    const user = userEvent.setup();
    act(() => screen.getByRole('tab', { name: /beta/ }).focus());
    await user.keyboard('{Shift>}{F10}{/Shift}');
    const menu = screen.getByRole('menu', { name: 'Tabs' });
    expect(within(menu).getAllByRole('menuitem')[0]).toHaveTextContent(/^Close/);
    expect(within(menu).getAllByRole('menuitem')[0]).toHaveFocus();
    await user.keyboard('{ArrowDown}');
    expect(within(menu).getByRole('menuitem', { name: 'Close others' })).toHaveFocus();
    // An object tab has no Save.
    expect(within(menu).queryByRole('menuitem', { name: /^Save/ })).toBeNull();
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('menu')).toBeNull();
    expect(screen.getByRole('tab', { name: /beta/ })).toHaveFocus();
  });

  it('closes every unpinned tab with Close all and copies a title', async () => {
    act(() => ['alpha', 'beta', 'gamma'].forEach((title) => openTab(ids.prod, title)));
    const idOf = (title: string) => objectTabId(ids.prod, { ...orders, name: title });
    act(() => useTabs.getState().setPinned(idOf('beta'), true));
    render(<TabBar />);
    // After setup: user-event installs its own clipboard stub.
    const user = userEvent.setup();
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /alpha/ }) });
    await user.click(screen.getByRole('menuitem', { name: 'Copy title' }));
    expect(writeText).toHaveBeenCalledWith('alpha');
    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /alpha/ }) });
    await user.click(screen.getByRole('menuitem', { name: 'Close all' }));
    await waitFor(() => expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([idOf('beta')]));
  });

  it('saves a query tab from its menu', async () => {
    mockBackend({ tabs_save: () => null, file_write: () => null, 'plugin:dialog|save': () => null });
    const id = useQueryDocs.getState().newQuery(null, null);
    const save = vi.spyOn(useQueryDocs.getState(), 'saveFile').mockResolvedValue(undefined as never);
    render(<TabBar />);
    const user = userEvent.setup();
    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /Query 1/ }) });
    await user.click(screen.getByRole('menuitem', { name: /^Save/ }));
    expect(save).toHaveBeenCalledWith(id, false);
    save.mockRestore();
  });

  it('disables context-menu entries that would do nothing and marks the active tab in the overflow list', async () => {
    act(() => {
      useTabs.getState().open({ id: 'q1', kind: 'query', connectionId: null, title: 'Query 1' });
      useTabs.getState().open({ id: 'q2', kind: 'query', connectionId: null, title: 'Query 2' });
    });
    render(<TabBar />);
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'List all tabs' }));
    expect(screen.getByRole('menuitemradio', { name: 'Query 2' })).toHaveAttribute('aria-checked', 'true');
    expect(screen.getByRole('menuitemradio', { name: 'Query 1' })).toHaveAttribute('aria-checked', 'false');
    await user.keyboard('{Escape}');
    await user.pointer({ keys: '[MouseRight]', target: screen.getByRole('tab', { name: /Query 2/ }) });
    expect(screen.getByRole('menuitem', { name: 'Close tabs to the right' })).toBeDisabled();
    expect(screen.getByRole('menuitem', { name: 'Close others' })).toBeEnabled();
  });
});