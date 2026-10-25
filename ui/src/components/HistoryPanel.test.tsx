import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { HistoryEntry } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { registerEditor, unregisterEditor, type EditorLike } from '../store/runQuery';
import { useTabs } from '../store/tabs';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { HistoryPanel } from './HistoryPanel';

const today = (h = 9) => {
  const d = new Date();
  d.setHours(h, 5, 7, 0);
  return d.toISOString();
};
const daysAgo = (n: number) => {
  const d = new Date();
  d.setDate(d.getDate() - n);
  d.setHours(9, 5, 7, 0);
  return d.toISOString();
};

const entry = (id: number, patch: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id,
  connection_id: ids.prod,
  database: 'shop',
  sql_text: `SELECT ${id}\nFROM t`,
  redacted: false,
  kind: 'read',
  started_at: today(),
  elapsed_ms: 12,
  status: 'ok',
  row_count: 1,
  affected: null,
  error_kind: null,
  error_code: null,
  origin: 'query',
  outcome_unknown: false,
  ...patch,
});

let rows: HistoryEntry[];
const backend = () =>
  mockBackend({
    history_list: () => rows,
    history_delete: (a) => {
      const gone = a.ids as number[];
      rows = rows.filter((r) => !gone.includes(r.id));
      return null;
    },
    history_clear: () => {
      rows = [];
      return null;
    },
    query_plan: () => ({ plan_id: 'p', stmts: [], dialect: { engine: 'mysql' }, connection_name: 'x', env_tag: null, read_only: false, tx_state: 'idle' }),
    tabs_save: () => null,
  });

let tab: string;
beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod'), config(ids.analytics, 'shop-analytics')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  rows = [entry(1), entry(2, { redacted: true, sql_text: "CREATE USER a IDENTIFIED BY '[REDACTED]'" })];
  backend();
});
afterEach(() => vi.useRealTimers());

const rowsOf = () => screen.findAllByTestId('history-row');
const editorWith = (executeEdits = vi.fn(() => true)): EditorLike => ({
  getModel: () => ({ getOffsetAt: () => 0 }),
  getSelection: () => ({
    isEmpty: () => true,
    getStartPosition: () => ({ lineNumber: 2, column: 5 }),
    getEndPosition: () => ({ lineNumber: 2, column: 5 }),
  }),
  getPosition: () => ({ lineNumber: 2, column: 5 }),
  executeEdits,
});

describe('HistoryPanel filters', () => {
  it('lists the focused tab connection by default, with All connections as the other choice', async () => {
    const user = userEvent.setup();
    render(<HistoryPanel />);
    expect(await rowsOf()).toHaveLength(2);
    expect(callsOf('history_list')[0]).toMatchObject({ filter: { connection_id: ids.prod, search: null } });
    const select = screen.getByTestId('history-connection');
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual(['shop-prod', 'All connections']);
    await user.selectOptions(select, '');
    await waitFor(() => expect(callsOf('history_list').at(-1)).toMatchObject({ filter: { connection_id: null } }));
  });

  it('follows the focused tab: another connection, then no connection at all', async () => {
    render(<HistoryPanel />);
    await rowsOf();
    act(() => {
      useQueryDocs.getState().newQuery(ids.analytics, null);
    });
    await waitFor(() => expect(callsOf('history_list').at(-1)).toMatchObject({ filter: { connection_id: ids.analytics } }));
    act(() => {
      useQueryDocs.getState().newQuery(null, null);
    });
    await waitFor(() => expect(callsOf('history_list').at(-1)).toMatchObject({ filter: { connection_id: null } }));
    expect(within(screen.getByTestId('history-connection')).getAllByRole('option').map((o) => o.textContent)).toEqual(['All connections']);
  });

  it('search is debounced 300 ms', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<HistoryPanel />);
    await rowsOf();
    const before = callsOf('history_list').length;
    await user.type(screen.getByTestId('history-search'), 'abc');
    expect(callsOf('history_list')).toHaveLength(before);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(300);
    });
    await waitFor(() => expect(callsOf('history_list')).toHaveLength(before + 1));
    expect(callsOf('history_list').at(-1)).toMatchObject({ filter: { search: 'abc' } });
  });

  it('the status segment narrows to OK or to Errors (cancels and unknown outcomes count as errors)', async () => {
    const user = userEvent.setup();
    rows = [
      entry(1),
      entry(2, { status: 'error', error_kind: 'query', error_code: '1062' }),
      entry(3, { status: 'cancelled', error_kind: 'cancelled' }),
    ];
    render(<HistoryPanel />);
    expect(await rowsOf()).toHaveLength(3);
    const segment = screen.getByRole('radiogroup', { name: 'Status' });
    expect(within(segment).getAllByRole('radio').map((r) => r.textContent)).toEqual(['All', 'OK', 'Errors']);
    expect(within(segment).getByRole('radio', { name: 'All' })).toBeChecked();
    await user.click(within(segment).getByRole('radio', { name: 'OK' }));
    expect(screen.getAllByTestId('history-row')).toHaveLength(1);
    await user.click(within(segment).getByRole('radio', { name: 'Errors' }));
    expect(screen.getAllByTestId('history-row')).toHaveLength(2);
    expect(within(segment).getByRole('radio', { name: 'Errors' })).toBeChecked();
  });
});

describe('HistoryPanel rows', () => {
  it('groups by Today / Yesterday / Older', async () => {
    rows = [entry(1), entry(2, { started_at: daysAgo(1) }), entry(3, { started_at: daysAgo(9) })];
    render(<HistoryPanel />);
    await rowsOf();
    expect(screen.getAllByRole('group').map((g) => g.getAttribute('aria-label'))).toEqual(['Today', 'Yesterday', 'Older']);
    expect(within(screen.getByRole('group', { name: 'Yesterday' })).getAllByTestId('history-row')).toHaveLength(1);
  });

  it('a row shows time, duration, rows or the error, database and only the first SQL line', async () => {
    rows = [
      entry(1),
      entry(2, { status: 'error', elapsed_ms: null, error_kind: 'syntax', error_code: '42601' }),
      entry(3, { row_count: null, affected: 4, database: null }),
    ];
    render(<HistoryPanel />);
    const list = await rowsOf();
    expect(list[0]).toHaveTextContent('09:05:07');
    expect(list[0]).toHaveTextContent('12 ms');
    expect(list[0]).toHaveTextContent('1 row');
    expect(list[0]).toHaveTextContent('shop');
    expect(list[0]).toHaveTextContent('SELECT 1');
    expect(list[0]).not.toHaveTextContent('FROM t');
    expect(list[1]).toHaveTextContent('Error (42601)');
    expect(list[2]).toHaveTextContent('4 affected');
  });

  it('the status dot says what happened, and a cancel or timeout never reads as not applied', async () => {
    rows = [
      entry(1),
      entry(2, { status: 'error', error_kind: 'timeout' }),
      entry(3, { status: 'error', error_kind: 'connection_lost', error_code: '2013' }),
      entry(4, { status: 'cancelled', error_kind: 'cancelled' }),
      entry(5, { status: 'error', error_kind: 'query', error_code: '1062' }),
      entry(6, { origin: 'data_edit', outcome_unknown: true }),
    ];
    render(<HistoryPanel />);
    await rowsOf();
    const dots = screen.getAllByTestId('history-status');
    expect(dots.map((d) => d.getAttribute('aria-label'))).toEqual([
      'OK',
      'Outcome unknown',
      'Outcome unknown (2013)',
      'Cancelled — may have been applied',
      'Error (1062)',
      'Outcome unknown',
    ]);
    expect(dots[1]!.getAttribute('title')).toMatch(/timed out/);
    expect(dots[2]!.getAttribute('title')).toMatch(/connection/);
    expect(dots[3]!.getAttribute('title')).toMatch(/may have applied/);
    expect(dots[5]!.getAttribute('title')).toMatch(/may or may not have been applied/);
    expect(screen.getAllByTestId('history-row')[5]).toHaveTextContent('Outcome unknown');
  });

  it('marks data edits, explains and redacted rows', async () => {
    rows = [
      entry(1, { sql_text: 'EXPLAIN FORMAT=JSON SELECT * FROM orders' }),
      entry(2, { origin: 'data_edit', sql_text: 'UPDATE t SET a = 1' }),
      entry(3, { sql_text: 'EXPLAIN (ANALYZE, FORMAT JSON) SELECT 3 FROM c' }),
      entry(4, { redacted: true }),
    ];
    render(<HistoryPanel />);
    const list = await rowsOf();
    expect(within(list[0]!).getByTestId('history-explain')).toHaveTextContent('Explain');
    expect(list[0]).toHaveTextContent('SELECT * FROM orders');
    expect(list[0]).not.toHaveTextContent('FORMAT=JSON');
    expect(within(list[1]!).getByTestId('history-data-edit')).toHaveTextContent('Data edit');
    expect(within(list[2]!).getByTestId('history-explain')).toHaveTextContent('Analyze');
    expect(within(list[3]!).getByText('redacted')).toBeInTheDocument();
    expect(within(list[0]!).queryByText('redacted')).toBeNull();
  });

  it('marks import and restore jobs', async () => {
    rows = [
      entry(1, { origin: 'import', kind: 'write', sql_text: '-- Import a.csv into t: append, 3 rows, 1 batches' }),
      entry(2, { origin: 'restore', kind: 'write', sql_text: '-- Restore a.sql (abc123) into shop: 4 statements' }),
      entry(3),
    ];
    render(<HistoryPanel />);
    const list = await rowsOf();
    expect(within(list[0]!).getByTestId('history-import')).toHaveTextContent('Import');
    expect(within(list[1]!).getByTestId('history-restore')).toHaveTextContent('Restore');
    expect(within(list[2]!).queryByTestId('history-import')).toBeNull();
    expect(within(list[2]!).queryByTestId('history-restore')).toBeNull();
  });

  it('the all-connections scope names the connection of each row', async () => {
    const user = userEvent.setup();
    rows = [entry(1), entry(2, { connection_id: 'gone-conn', database: null })];
    render(<HistoryPanel />);
    await rowsOf();
    expect(screen.queryByTestId('history-origin')).toBeNull();
    await user.selectOptions(screen.getByTestId('history-connection'), '');
    const origins = await screen.findAllByTestId('history-origin');
    expect(origins[0]).toHaveTextContent('shop-prod');
    expect(origins[1]).toHaveTextContent('gone-conn');
  });

  it('shows an empty state', async () => {
    rows = [];
    render(<HistoryPanel />);
    expect(await screen.findByText('No history.')).toBeInTheDocument();
  });
});

describe('HistoryPanel actions', () => {
  it('a click expands the full SQL with its actions; clicking again folds it', async () => {
    const user = userEvent.setup();
    render(<HistoryPanel />);
    const list = await rowsOf();
    expect(screen.queryByTestId('history-detail')).toBeNull();
    await user.click(list[0]!);
    const detail = screen.getByTestId('history-detail');
    expect(list[0]).toHaveAttribute('aria-expanded', 'true');
    expect(detail).toHaveTextContent('SELECT 1 FROM t');
    expect(detail.querySelector('pre')!.textContent).toBe('SELECT 1\nFROM t');
    expect(within(detail).getAllByRole('button').map((b) => b.textContent)).toEqual(['Insert', 'Open in new tab', 'Run again', 'Copy']);
    // Another row takes over.
    await user.click(list[1]!);
    expect(screen.getAllByTestId('history-detail')).toHaveLength(1);
    await user.click(list[1]!);
    expect(screen.queryByTestId('history-detail')).toBeNull();
  });

  it('Insert puts the statement at the caret of the focused editor', async () => {
    const user = userEvent.setup();
    const executeEdits = vi.fn(() => true);
    const editor = editorWith(executeEdits);
    registerEditor(tab, editor);
    render(<HistoryPanel />);
    await user.click((await rowsOf())[0]!);
    await user.click(screen.getByRole('button', { name: 'Insert' }));
    const [, edits] = executeEdits.mock.calls[0] as unknown as [string, { range: object; text: string }[]];
    expect(edits[0]!.text).toBe('SELECT 1\nFROM t');
    expect(edits[0]!.range).toEqual({ startLineNumber: 2, startColumn: 5, endLineNumber: 2, endColumn: 5 });
    unregisterEditor(tab, editor);
  });

  it('double click inserts as well, without an editor at the stored cursor', async () => {
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: 'ab', cursor: 1 });
    render(<HistoryPanel />);
    await user.dblClick((await rowsOf())[0]!);
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('aSELECT 1\nFROM tb');
  });

  it('Insert is unavailable when the focused tab is not a query tab', async () => {
    const user = userEvent.setup();
    act(() => {
      useTabs.getState().open({
        id: 'obj1',
        kind: 'object',
        connectionId: ids.prod,
        objectRef: { database: 'shop', schema: null, name: 'orders', kind: 'Table' },
        title: 'orders',
      });
    });
    render(<HistoryPanel />);
    const list = await rowsOf();
    await user.click(list[0]!);
    expect(screen.getByRole('button', { name: 'Insert' })).toBeDisabled();
    await user.dblClick(list[0]!);
    expect(screen.getByRole('status')).toHaveTextContent(/query tab/i);
    // The object tab's connection still filters the list.
    expect(callsOf('history_list')[0]).toMatchObject({ filter: { connection_id: ids.prod } });
  });

  it('Open in new tab opens a query on the entry connection and database', async () => {
    const user = userEvent.setup();
    rows = [entry(1, { connection_id: ids.analytics, database: 'audit' })];
    render(<HistoryPanel />);
    await user.selectOptions(screen.getByTestId('history-connection'), '');
    await user.click((await rowsOf())[0]!);
    const before = useQueryDocs.getState().docs.size;
    await user.click(screen.getByRole('button', { name: 'Open in new tab' }));
    expect(useQueryDocs.getState().docs.size).toBe(before + 1);
    const doc = [...useQueryDocs.getState().docs.values()].at(-1)!;
    expect(doc).toMatchObject({ connectionId: ids.analytics, database: 'audit', sql: 'SELECT 1\nFROM t' });
    expect(useTabs.getState().activeId).toBe(doc.id);
  });

  it('Run again goes through the normal run path in a new tab (plan first, guards apply)', async () => {
    const user = userEvent.setup();
    render(<HistoryPanel />);
    await user.click((await rowsOf())[0]!);
    await user.click(screen.getByRole('button', { name: 'Run again' }));
    await waitFor(() => expect(callsOf('query_plan')).toHaveLength(1));
    expect(callsOf('query_plan')[0]).toMatchObject({ connId: ids.prod, database: 'shop', text: 'SELECT 1\nFROM t', target: 'All' });
    expect(callsOf('query_run')).toHaveLength(0);
  });

  it('Copy writes the statement to the clipboard', async () => {
    const user = userEvent.setup();
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    render(<HistoryPanel />);
    await user.click((await rowsOf())[0]!);
    await user.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith('SELECT 1\nFROM t');
  });

  it('a shortened data-edit statement is refused for Insert and Run again, with a notice', async () => {
    const shortened = "UPDATE `t` SET `note` = 'aaaa…' /* 1048576 chars */ WHERE `id` = 1";
    rows = [entry(1, { origin: 'data_edit', kind: 'write', redacted: true, sql_text: shortened }), entry(2, { origin: 'data_edit', kind: 'write', sql_text: 'UPDATE t SET a = 1' })];
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: '', cursor: 0 });
    render(<HistoryPanel />);
    const list = await rowsOf();
    await user.dblClick(list[0]!);
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('');
    expect(screen.getByRole('status')).toHaveTextContent("This statement was shortened or redacted for history and can't be re-run.");
    expect(screen.getByRole('button', { name: 'Run again' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Insert' })).toBeDisabled();
    list[0]!.focus();
    await user.keyboard('{Enter}');
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('');
    // A whole data-edit statement goes in as before, and the notice goes.
    await user.dblClick(list[1]!);
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('UPDATE t SET a = 1');
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });

  it('the context menu offers the same actions plus Delete', async () => {
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: '', cursor: 0 });
    render(<HistoryPanel />);
    const list = await rowsOf();
    await user.pointer({ keys: '[MouseRight]', target: list[0]! });
    const menu = screen.getByRole('menu', { name: 'History entry actions' });
    expect(within(menu).getAllByRole('menuitem').map((i) => i.querySelector('.menu-label')?.textContent)).toEqual([
      'Insert',
      'Open in new tab',
      'Run again',
      'Copy',
      'Delete',
    ]);
    await user.click(within(menu).getByRole('menuitem', { name: /Insert/ }));
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('SELECT 1\nFROM t');
    await user.pointer({ keys: '[MouseRight]', target: list[0]! });
    await user.click(within(screen.getByRole('menu')).getByRole('menuitem', { name: /Delete/ }));
    expect(callsOf('history_delete')[0]).toEqual({ ids: [1] });
    await waitFor(() => expect(screen.getAllByTestId('history-row')).toHaveLength(1));
  });

  it('keyboard: arrows move a roving focus, Enter inserts, Right/Left unfold/fold, Delete deletes', async () => {
    rows = [entry(1), entry(2), entry(3)];
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: '', cursor: 0 });
    render(<HistoryPanel />);
    const list = await rowsOf();
    expect(list.map((r) => r.tabIndex)).toEqual([0, -1, -1]);
    list[0]!.focus();
    await user.keyboard('{ArrowDown}');
    expect(document.activeElement).toBe(list[1]);
    expect(list[1]).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{End}');
    expect(document.activeElement).toBe(list[2]);
    await user.keyboard('{Home}');
    expect(document.activeElement).toBe(list[0]);
    await user.keyboard('{ArrowRight}');
    expect(list[0]).toHaveAttribute('aria-expanded', 'true');
    await user.keyboard('{ArrowLeft}');
    expect(list[0]).toHaveAttribute('aria-expanded', 'false');

    await user.keyboard('{Enter}');
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('SELECT 1\nFROM t');

    await user.keyboard('{ArrowDown}{Delete}');
    expect(callsOf('history_delete')[0]).toEqual({ ids: [2] });
    await waitFor(() => expect(screen.getAllByTestId('history-row')).toHaveLength(2));
  });
});

describe('HistoryPanel delete and clear', () => {
  it('Delete removes the selected rows', async () => {
    const user = userEvent.setup();
    render(<HistoryPanel />);
    const list = await rowsOf();
    expect(screen.getByTestId('history-delete')).toBeDisabled();
    await user.click(list[0]!);
    await user.click(screen.getByTestId('history-delete'));
    expect(callsOf('history_delete')[0]).toEqual({ ids: [1] });
    await waitFor(() => expect(screen.getAllByTestId('history-row')).toHaveLength(1));
  });

  it('Clear asks first; Cancel keeps the rows', async () => {
    const user = userEvent.setup();
    render(<HistoryPanel />);
    await rowsOf();
    expect(screen.getByTestId('history-clear')).toHaveAccessibleName('Clear history');
    await user.click(screen.getByTestId('history-clear'));
    expect(callsOf('history_clear')).toHaveLength(0);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.getAllByTestId('history-row')).toHaveLength(2);

    await user.click(screen.getByTestId('history-clear'));
    await user.click(screen.getByTestId('history-clear-confirm'));
    expect(callsOf('history_clear')).toHaveLength(1);
    await waitFor(() => expect(screen.queryAllByTestId('history-row')).toHaveLength(0));
    expect(screen.getByTestId('history-view')).toHaveTextContent(/no history/i);
  });
});
