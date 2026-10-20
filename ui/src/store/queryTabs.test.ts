import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { open, save } from '@tauri-apps/plugin-dialog';
import { createElement } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { QueryTabRecord } from '../bindings';
import { DialogHost } from '../dialogs/DialogHost';
import { QueryTab } from '../tabs/query/QueryTab';
import { onQueryShortcut } from '../components/queryShortcuts';
import { callsOf, calls, config, fail, ids, mockBackend, resetStores } from '../test/backend';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { AUTOSAVE_MS, fromRecord, setTxStateSource, toRecord, useQueryDocs, type QueryDoc } from './queryTabs';
import { useTabs, type QueryTabRef } from './tabs';

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn(), open: vi.fn() }));

const record = (over: Partial<QueryTabRecord> = {}): QueryTabRecord => ({
  id: 'tab-1',
  title: 'Query 1',
  sort_order: 0,
  connection_id: ids.prod,
  database: 'shop',
  sql_text: 'SELECT 1;',
  cursor: 3,
  file_path: null,
  dirty: false,
  continue_on_error: false,
  row_limit: 10000,
  updated_at: '2026-10-20T00:00:00Z',
  ...over,
});

/** The backend of a session whose tabs table holds `rows`. */
function backend(rows: QueryTabRecord[] = [], extra: Record<string, (a: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    tabs_load: () => rows,
    tabs_save: () => null,
    tabs_delete: () => null,
    file_read: () => 'SELECT 42;',
    file_write: () => null,
    query_close_tab: () => null,
    query_tx: () => 'idle',
    ...extra,
  });
}

const doc = (id: string) => useQueryDocs.getState().docs.get(id)!;

beforeEach(() => {
  resetStores();
  setTxStateSource(null);
  vi.mocked(save).mockReset();
  vi.mocked(open).mockReset();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod'), config(ids.analytics, 'analytics', { kind: 'postgres' })]);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('records', () => {
  it('round-trips a document through its record', () => {
    const d: QueryDoc = {
      id: 'x',
      title: 'report.sql',
      connectionId: ids.analytics,
      database: 'stats',
      sql: 'SELECT 1',
      cursor: 4,
      filePath: 'C:\\sql\\report.sql',
      dirty: true,
      continueOnError: true,
      rowLimit: 100000,
      sortOrder: 3,
      dialect: 'postgres',
    };
    expect(toRecord(d)).toMatchObject({ id: 'x', sql_text: 'SELECT 1', file_path: 'C:\\sql\\report.sql', row_limit: 100000, sort_order: 3, continue_on_error: true });
    expect(fromRecord(toRecord(d))).toEqual(d);
  });

  it('an unknown row limit falls back to the default', () => {
    expect(fromRecord(record({ row_limit: 7 })).rowLimit).toBe(10000);
  });
});

describe('autosave', () => {
  it('autosave_debounces_1s', async () => {
    backend();
    vi.useFakeTimers();
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    const update = useQueryDocs.getState().update;
    update(id, { sql: 'S' });
    await vi.advanceTimersByTimeAsync(300);
    update(id, { sql: 'SE' });
    await vi.advanceTimersByTimeAsync(300);
    update(id, { sql: 'SEL' });
    await vi.advanceTimersByTimeAsync(AUTOSAVE_MS - 1);
    expect(callsOf('tabs_save')).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(callsOf('tabs_save')).toHaveLength(1);
    expect(callsOf('tabs_save')[0]!.tab).toMatchObject({ id, sql_text: 'SEL', connection_id: ids.prod, database: 'shop' });
  });

  it('flush saves pending tabs at once', async () => {
    backend();
    vi.useFakeTimers();
    const a = useQueryDocs.getState().newQuery(null, null);
    const b = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(a, { sql: 'A' });
    useQueryDocs.getState().update(b, { sql: 'B' });
    await useQueryDocs.getState().flush();
    expect(callsOf('tabs_save').map((c) => (c.tab as QueryTabRecord).sql_text).sort()).toEqual(['A', 'B']);
    await vi.advanceTimersByTimeAsync(AUTOSAVE_MS * 2);
    expect(callsOf('tabs_save')).toHaveLength(2);
  });
});

describe('restore', () => {
  it('restore_does_not_connect', async () => {
    backend([record({ id: 'b', title: 'Query 2', sort_order: 1 }), record({ id: 'a', sort_order: 0 })]);
    await useQueryDocs.getState().restore();
    expect(calls()).toEqual(['tabs_load']);
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual(['a', 'b']);
    expect(doc('a')).toMatchObject({ connectionId: ids.prod, database: 'shop', sql: 'SELECT 1;', cursor: 3, dialect: 'mysql' });
    expect(useTabs.getState().activeId).toBe('a');
    // A new query after a restore continues the numbering.
    useQueryDocs.getState().newQuery(null, null);
    expect(useTabs.getState().tabs.at(-1)!.title).toBe('Query 3');
  });

  it('restores_tab_whose_connection_was_deleted', async () => {
    const gone = '99999999-9999-4999-8999-999999999999';
    backend([record({ id: 'orphan', connection_id: gone, database: 'old' })]);
    await useQueryDocs.getState().restore();
    expect(calls()).toEqual(['tabs_load']);
    expect(doc('orphan')).toMatchObject({ connectionId: null, sql: 'SELECT 1;' });
    const tab = useTabs.getState().tabs[0] as QueryTabRef;
    expect(tab.connectionId).toBeNull();
    render(createElement(QueryTab, { tab }));
    const select = screen.getByTestId('query-connection') as HTMLSelectElement;
    expect(select.selectedOptions[0]).toHaveTextContent('Choose a connection');
    expect(screen.getByTestId('query-run')).toBeDisabled();
  });
});

describe('files', () => {
  it('editing a file tab marks it dirty; a tab without a file never is', () => {
    backend();
    const plain = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(plain, { sql: 'x', dirty: true });
    expect(doc(plain).dirty).toBe(false);
    useQueryDocs.getState().update(plain, { filePath: 'C:\\a.sql' });
    useQueryDocs.getState().update(plain, { sql: 'xy' });
    expect(doc(plain).dirty).toBe(true);
  });

  it('save_writes_file_and_clears_dirty', async () => {
    backend();
    vi.mocked(save).mockResolvedValue('C:\\work\\report.sql');
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    useQueryDocs.getState().update(id, { sql: 'SELECT 7;' });
    await useQueryDocs.getState().saveFile(id);
    expect(save).toHaveBeenCalledWith({ filters: [{ name: 'SQL', extensions: ['sql'] }] });
    expect(callsOf('file_write')).toEqual([{ path: 'C:\\work\\report.sql', text: 'SELECT 7;' }]);
    expect(doc(id)).toMatchObject({ filePath: 'C:\\work\\report.sql', dirty: false, title: 'report.sql' });
    expect(useTabs.getState().tabs[0]!.title).toBe('report.sql');

    // Saving again writes to the same file without asking.
    useQueryDocs.getState().update(id, { sql: 'SELECT 8;' });
    expect(doc(id).dirty).toBe(true);
    await useQueryDocs.getState().saveFile(id);
    expect(save).toHaveBeenCalledTimes(1);
    expect(callsOf('file_write')[1]).toEqual({ path: 'C:\\work\\report.sql', text: 'SELECT 8;' });
    expect(doc(id).dirty).toBe(false);

    // Save as always asks.
    vi.mocked(save).mockResolvedValue('C:\\work\\copy.sql');
    await useQueryDocs.getState().saveFile(id, true);
    expect(save).toHaveBeenCalledTimes(2);
    expect(doc(id)).toMatchObject({ filePath: 'C:\\work\\copy.sql', title: 'copy.sql' });
  });

  it('a cancelled save dialog writes nothing', async () => {
    backend();
    vi.mocked(save).mockResolvedValue(null);
    const id = useQueryDocs.getState().newQuery(null, null);
    await useQueryDocs.getState().saveFile(id);
    expect(callsOf('file_write')).toHaveLength(0);
    expect(doc(id).filePath).toBeNull();
  });

  it('open_creates_tab_from_file', async () => {
    backend();
    vi.mocked(open).mockResolvedValue('D:\\scripts\\daily report.sql');
    await useQueryDocs.getState().openFile();
    expect(callsOf('file_read')).toEqual([{ path: 'D:\\scripts\\daily report.sql' }]);
    const tab = useTabs.getState().tabs[0]!;
    expect(tab.title).toBe('daily report.sql');
    expect(useTabs.getState().activeId).toBe(tab.id);
    expect(doc(tab.id)).toMatchObject({ sql: 'SELECT 42;', filePath: 'D:\\scripts\\daily report.sql', dirty: false, title: 'daily report.sql' });

    // The same file again activates its tab.
    useQueryDocs.getState().newQuery(null, null);
    await useQueryDocs.getState().openFile();
    expect(useTabs.getState().tabs).toHaveLength(2);
    expect(useTabs.getState().activeId).toBe(tab.id);
  });
});

describe('shortcuts', () => {
  const press = (key: string, over: Partial<KeyboardEventInit> = {}) => {
    const e = new KeyboardEvent('keydown', { key, ctrlKey: true, cancelable: true, ...over });
    onQueryShortcut(e);
    return e;
  };

  it('Ctrl+T opens a new query, Ctrl+S saves the active query tab, Ctrl+O opens a file', async () => {
    backend();
    vi.mocked(save).mockResolvedValue('C:\\a.sql');
    vi.mocked(open).mockResolvedValue(null);
    expect(press('t').defaultPrevented).toBe(true);
    expect(useTabs.getState().tabs.map((t) => t.title)).toEqual(['Query 1']);
    expect(press('s').defaultPrevented).toBe(true);
    await waitFor(() => expect(callsOf('file_write')).toHaveLength(1));
    expect(press('o').defaultPrevented).toBe(true);
    await waitFor(() => expect(open).toHaveBeenCalled());
    // Ctrl+Shift+S is Save as.
    press('S', { shiftKey: true });
    await waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    // Other keys pass through.
    expect(press('a').defaultPrevented).toBe(false);
    expect(press('t', { ctrlKey: false }).defaultPrevented).toBe(false);
  });

  it('inputs outside the editor keep their own keys', () => {
    backend();
    const input = document.body.appendChild(document.createElement('input'));
    const e = new KeyboardEvent('keydown', { key: 't', ctrlKey: true, cancelable: true, bubbles: true });
    input.addEventListener('keydown', onQueryShortcut);
    input.dispatchEvent(e);
    expect(e.defaultPrevented).toBe(false);
    expect(useTabs.getState().tabs).toHaveLength(0);
    input.remove();

    // The editor's own textarea still gets the shortcuts.
    const editor = document.body.appendChild(document.createElement('div'));
    editor.className = 'sql-editor';
    const area = editor.appendChild(document.createElement('textarea'));
    const e2 = new KeyboardEvent('keydown', { key: 't', ctrlKey: true, cancelable: true, bubbles: true });
    area.addEventListener('keydown', onQueryShortcut);
    area.dispatchEvent(e2);
    expect(e2.defaultPrevented).toBe(true);
    expect(useTabs.getState().tabs).toHaveLength(1);
    editor.remove();
  });

  it('Ctrl+S without an active query tab does nothing', () => {
    backend();
    expect(press('s').defaultPrevented).toBe(true);
    expect(save).not.toHaveBeenCalled();
  });
});

describe('closing a tab', () => {
  it('close_tab_with_tx_asks_commit_rollback_cancel', async () => {
    let txOpen = true;
    backend([], {
      query_close_tab: (a) => {
        if (txOpen && !a.force) throw fail('transaction_open');
        return null;
      },
      query_tx: () => {
        txOpen = false;
        return 'idle';
      },
    });
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(createElement(DialogHost));
    const user = userEvent.setup();

    // Cancel leaves the tab.
    let closing = useQueryDocs.getState().closeTab(id);
    const dialog = await screen.findByRole('alertdialog');
    expect(dialog).toHaveTextContent(/This tab has an open transaction/);
    for (const name of ['Commit', 'Rollback', 'Cancel']) expect(screen.getByRole('button', { name })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(await closing).toBe(false);
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([id]);
    expect(callsOf('query_tx')).toHaveLength(0);
    expect(callsOf('tabs_delete')).toHaveLength(0);

    // Rollback, then the session closes and the tab goes away.
    closing = useQueryDocs.getState().closeTab(id);
    await user.click(await screen.findByRole('button', { name: 'Rollback' }));
    expect(await closing).toBe(true);
    expect(callsOf('query_tx')).toEqual([{ tabId: id, action: 'Rollback' }]);
    const order = calls().filter((c) => c === 'query_tx' || c === 'query_close_tab' || c === 'tabs_delete');
    expect(order.slice(-3)).toEqual(['query_tx', 'query_close_tab', 'tabs_delete']);
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useQueryDocs.getState().docs.has(id)).toBe(false);
    expect(screen.queryByRole('alertdialog')).toBeNull();
  });

  it('a tab the run store reports in a transaction asks before calling the backend', async () => {
    backend();
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    setTxStateSource((tab) => (tab === id ? 'active' : 'idle'));
    render(createElement(DialogHost));
    const closing = useQueryDocs.getState().closeTab(id);
    await userEvent.setup().click(await screen.findByRole('button', { name: 'Commit' }));
    expect(await closing).toBe(true);
    expect(calls().filter((c) => c.startsWith('query_'))).toEqual(['query_tx', 'query_close_tab']);
    expect(callsOf('query_tx')[0]).toEqual({ tabId: id, action: 'Commit' });
  });

  it('a failed commit keeps the dialog open with the error', async () => {
    backend([], { query_tx: () => { throw fail('connection_lost'); } });
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    setTxStateSource(() => 'active');
    render(createElement(DialogHost));
    void useQueryDocs.getState().closeTab(id);
    await userEvent.setup().click(await screen.findByRole('button', { name: 'Commit' }));
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    expect(screen.getByRole('alertdialog')).toBeInTheDocument();
    expect(callsOf('query_close_tab')).toHaveLength(0);
  });

  it('a dirty file tab asks before discarding', async () => {
    backend();
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { filePath: 'C:\\sql\\daily.sql', title: 'daily.sql' });
    useQueryDocs.getState().update(id, { sql: 'changed' });
    render(createElement(DialogHost));
    const user = userEvent.setup();

    let closing = useQueryDocs.getState().closeTab(id);
    expect(await screen.findByText('Discard unsaved changes to daily.sql?')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(await closing).toBe(false);
    expect(useTabs.getState().tabs).toHaveLength(1);

    closing = useQueryDocs.getState().closeTab(id);
    await user.click(await screen.findByRole('button', { name: 'Discard' }));
    expect(await closing).toBe(true);
    expect(callsOf('query_close_tab')).toEqual([{ tabId: id, force: false }]);
    expect(callsOf('tabs_delete')).toEqual([{ id }]);
    expect(useTabs.getState().tabs).toHaveLength(0);
  });

  it('closing an idle tab drops its pending autosave', async () => {
    backend();
    vi.useFakeTimers();
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { sql: 'x' });
    await act(async () => {
      expect(await useQueryDocs.getState().closeTab(id)).toBe(true);
    });
    await vi.advanceTimersByTimeAsync(AUTOSAVE_MS * 2);
    expect(callsOf('tabs_save')).toHaveLength(0);
    expect(callsOf('tabs_delete')).toEqual([{ id }]);
  });

  it('discard is asked before the transaction is settled', async () => {
    backend();
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    useQueryDocs.getState().update(id, { filePath: 'C:\\sql\\daily.sql', title: 'daily.sql' });
    useQueryDocs.getState().update(id, { sql: 'changed' });
    setTxStateSource(() => 'active');
    render(createElement(DialogHost));
    const user = userEvent.setup();

    // Cancelling the discard leaves the transaction alone.
    let closing = useQueryDocs.getState().closeTab(id);
    expect(await screen.findByText('Discard unsaved changes to daily.sql?')).toBeInTheDocument();
    expect(screen.queryByText('This tab has an open transaction.')).toBeNull();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(await closing).toBe(false);
    expect(callsOf('query_tx')).toHaveLength(0);

    closing = useQueryDocs.getState().closeTab(id);
    await user.click(await screen.findByRole('button', { name: 'Discard' }));
    await user.click(await screen.findByRole('button', { name: 'Commit' }));
    expect(await closing).toBe(true);
    expect(callsOf('query_tx')).toEqual([{ tabId: id, action: 'Commit' }]);
  });

  it('flush reports the tabs it could not save', async () => {
    backend([], { tabs_save: () => { throw fail('internal'); } });
    const id = useQueryDocs.getState().newQuery(null, null);
    expect(await useQueryDocs.getState().flush()).toEqual([id]);
  });

  it('a busy session keeps the tab and says why', async () => {
    backend([], { query_close_tab: () => { throw fail('busy'); } });
    const id = useQueryDocs.getState().newQuery(null, null);
    expect(await useQueryDocs.getState().closeTab(id)).toBe(false);
    await waitFor(() => expect(useTabs.getState().tabs).toHaveLength(1));
    expect(callsOf('tabs_delete')).toHaveLength(0);
    expect(useDialogs.getState().notice).toMatch(/^Could not close Query 1: /);
  });
});
