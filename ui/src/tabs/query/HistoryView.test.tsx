import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { HistoryEntry } from '../../bindings';
import { useConnections } from '../../store/connections';
import { useQueryDocs } from '../../store/queryTabs';
import { registerEditor, unregisterEditor, type EditorLike } from '../../store/runQuery';
import { callsOf, config, ids, mockBackend, resetStores } from '../../test/backend';
import { HistoryView } from './HistoryView';

const entry = (id: number, patch: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id,
  connection_id: ids.prod,
  database: 'shop',
  sql_text: `SELECT ${id}\nFROM t`,
  redacted: false,
  kind: 'read',
  started_at: '2026-10-20T08:15:30Z',
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
  });

let tab: string;
beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  rows = [entry(1), entry(2, { redacted: true, sql_text: "CREATE USER a IDENTIFIED BY '[REDACTED]'" })];
  backend();
});
afterEach(() => vi.useRealTimers());

describe('HistoryView', () => {
  it('history_filters_by_tab_connection_by_default', async () => {
    const user = userEvent.setup();
    render(<HistoryView tabId={tab} />);
    expect(await screen.findAllByTestId('history-row')).toHaveLength(2);
    expect(callsOf('history_list')[0]).toMatchObject({ filter: { connection_id: ids.prod, search: null } });
    await user.selectOptions(screen.getByTestId('history-connection'), '');
    await waitFor(() => expect(callsOf('history_list').at(-1)).toMatchObject({ filter: { connection_id: null } }));
  });

  it('search_is_debounced_300ms', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<HistoryView tabId={tab} />);
    await screen.findAllByTestId('history-row');
    const before = callsOf('history_list').length;
    await user.type(screen.getByTestId('history-search'), 'abc');
    expect(callsOf('history_list')).toHaveLength(before);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(300);
    });
    await waitFor(() => expect(callsOf('history_list')).toHaveLength(before + 1));
    expect(callsOf('history_list').at(-1)).toMatchObject({ filter: { search: 'abc' } });
  });

  it('rows_show_status_elapsed_and_first_line', async () => {
    rows = [
      entry(1),
      entry(2, { status: 'cancelled', elapsed_ms: null }),
      entry(3, { status: 'error', error_kind: 'syntax', error_code: '42601' }),
    ];
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    expect(list[0]).toHaveTextContent('SELECT 1');
    expect(list[0]).not.toHaveTextContent('FROM t');
    expect(list[0]).toHaveTextContent('12 ms');
    // A cancelled statement may have taken effect: never shown as "not applied".
    expect(list[1]).toHaveTextContent(/cancelled/i);
    expect(list[1]).not.toHaveTextContent(/not applied/i);
    expect(list[2]).toHaveTextContent(/error/i);
  });

  it('timeouts, lost connections and cancels never read as not applied', async () => {
    rows = [
      entry(1, { status: 'error', error_kind: 'timeout', error_code: null }),
      entry(2, { status: 'error', error_kind: 'connection_lost', error_code: '2013' }),
      entry(3, { status: 'cancelled', error_kind: 'cancelled' }),
      entry(4, { status: 'error', error_kind: 'query', error_code: '1062' }),
      entry(5, { status: 'cancelled', error_kind: 'timeout' }),
    ];
    render(<HistoryView tabId={tab} />);
    await screen.findAllByTestId('history-row');
    const status = screen.getAllByTestId('history-status');
    expect(status.map((s) => s.textContent)).toEqual([
      'Outcome unknown',
      'Outcome unknown (2013)',
      'Cancelled — may have been applied',
      'Error (1062)',
      'Outcome unknown',
    ]);
    for (const s of status.slice(0, 3)) {
      expect(s).not.toHaveTextContent(/^Error/);
      expect(s.getAttribute('title')).toMatch(/may have applied|may have been applied/);
    }
    expect(status[0]!.getAttribute('title')).toMatch(/timed out/);
    expect(status[1]!.getAttribute('title')).toMatch(/connection/);
  });

  it('data_edit_rows_are_labelled_and_unknown_outcomes_are_flagged', async () => {
    rows = [
      entry(1),
      entry(2, { origin: 'data_edit', sql_text: 'UPDATE t SET a = 1' }),
      // An unanswered COMMIT: the statement itself ran fine.
      entry(3, { origin: 'data_edit', outcome_unknown: true }),
      entry(4, { origin: 'data_edit', status: 'error', error_kind: 'connection_lost', outcome_unknown: true }),
    ];
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    expect(within(list[0]!).queryByTestId('history-data-edit')).toBeNull();
    expect(within(list[1]!).getByTestId('history-data-edit')).toHaveTextContent('Data edit');
    const status = screen.getAllByTestId('history-status');
    expect(status.map((s) => s.textContent)).toEqual(['OK', 'OK', 'Outcome unknown', 'Outcome unknown']);
    expect(status[2]!.getAttribute('title')).toMatch(/may or may not have been applied/);
    expect(status[2]).not.toHaveTextContent(/^OK/);
    expect(callsOf('history_list')[0]).toMatchObject({ filter: { origin: null } });
  });

  it('keyboard: arrows move a roving focus, Enter inserts, Delete deletes', async () => {
    rows = [entry(1), entry(2), entry(3)];
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: '', cursor: 0 });
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    // One tab stop for the whole list.
    expect(list.map((r) => r.tabIndex)).toEqual([0, -1, -1]);
    list[0]!.focus();
    await user.keyboard('{ArrowDown}');
    expect(document.activeElement).toBe(list[1]);
    expect(list[1]).toHaveAttribute('aria-selected', 'true');
    expect(screen.getAllByTestId('history-row').map((r) => r.tabIndex)).toEqual([-1, 0, -1]);
    await user.keyboard('{End}');
    expect(document.activeElement).toBe(list[2]);
    await user.keyboard('{ArrowDown}');
    expect(document.activeElement).toBe(list[2]);
    await user.keyboard('{Home}{ArrowUp}');
    expect(document.activeElement).toBe(list[0]);

    await user.keyboard('{Enter}');
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('SELECT 1\nFROM t');

    await user.keyboard('{ArrowDown}{Delete}');
    expect(callsOf('history_delete')[0]).toEqual({ ids: [2] });
    await waitFor(() => expect(screen.getAllByTestId('history-row')).toHaveLength(2));
  });

  it('redacted_badge', async () => {
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    expect(within(list[0]!).queryByText('redacted')).not.toBeInTheDocument();
    expect(within(list[1]!).getByText('redacted')).toBeInTheDocument();
  });

  it('double_click_inserts_at_cursor', async () => {
    const user = userEvent.setup();
    const executeEdits = vi.fn(() => true);
    const editor: EditorLike = {
      getModel: () => ({ getOffsetAt: () => 0 }),
      getSelection: () => ({
        isEmpty: () => true,
        getStartPosition: () => ({ lineNumber: 2, column: 5 }),
        getEndPosition: () => ({ lineNumber: 2, column: 5 }),
      }),
      getPosition: () => ({ lineNumber: 2, column: 5 }),
      executeEdits,
    };
    registerEditor(tab, editor);
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    await user.dblClick(list[1]!);
    expect(executeEdits).toHaveBeenCalledTimes(1);
    const [, edits] = executeEdits.mock.calls[0] as unknown as [string, { range: object; text: string }[]];
    expect(edits[0]!.text).toBe("CREATE USER a IDENTIFIED BY '[REDACTED]'");
    expect(edits[0]!.range).toEqual({ startLineNumber: 2, startColumn: 5, endLineNumber: 2, endColumn: 5 });
    unregisterEditor(tab, editor);
  });

  it('a shortened data-edit statement is never inserted: Enter and double click are refused with a notice', async () => {
    const shortened = "UPDATE `t` SET `note` = 'aaaa…' /* 1048576 chars */ WHERE `id` = 1";
    rows = [entry(1, { origin: 'data_edit', kind: 'write', redacted: true, sql_text: shortened }), entry(2, { origin: 'data_edit', kind: 'write', sql_text: 'UPDATE t SET a = 1' })];
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: '', cursor: 0 });
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    await user.dblClick(list[0]!);
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('');
    expect(screen.getByRole('status')).toHaveTextContent("This statement was shortened or redacted for history and can't be re-run.");
    list[0]!.focus();
    await user.keyboard('{Enter}');
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('');
    // A whole data-edit statement is inserted as before, and the notice goes.
    await user.dblClick(list[1]!);
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('UPDATE t SET a = 1');
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });

  it('double_click_without_an_editor_inserts_at_the_stored_cursor', async () => {
    const user = userEvent.setup();
    useQueryDocs.getState().update(tab, { sql: 'ab', cursor: 1 });
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    await user.dblClick(list[0]!);
    expect(useQueryDocs.getState().docs.get(tab)!.sql).toBe('aSELECT 1\nFROM tb');
  });

  it('delete_and_clear', async () => {
    const user = userEvent.setup();
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    expect(screen.getByTestId('history-delete')).toBeDisabled();
    await user.click(list[0]!);
    await user.click(screen.getByTestId('history-delete'));
    expect(callsOf('history_delete')[0]).toEqual({ ids: [1] });
    await waitFor(() => expect(screen.getAllByTestId('history-row')).toHaveLength(1));

    // Clear asks first; Cancel keeps the rows.
    await user.click(screen.getByTestId('history-clear'));
    expect(callsOf('history_clear')).toHaveLength(0);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.getAllByTestId('history-row')).toHaveLength(1);

    await user.click(screen.getByTestId('history-clear'));
    await user.click(screen.getByTestId('history-clear-confirm'));
    expect(callsOf('history_clear')).toHaveLength(1);
    await waitFor(() => expect(screen.queryAllByTestId('history-row')).toHaveLength(0));
    expect(screen.getByTestId('history-view')).toHaveTextContent(/no history/i);
  });

  it('all_scope_shows_connection_and_database', async () => {
    const user = userEvent.setup();
    rows = [entry(1), entry(2, { connection_id: 'gone-conn', database: null })];
    render(<HistoryView tabId={tab} />);
    await screen.findAllByTestId('history-row');
    expect(screen.queryByTestId('history-origin')).toBeNull();
    await user.selectOptions(screen.getByTestId('history-connection'), '');
    const origins = await screen.findAllByTestId('history-origin');
    expect(origins[0]).toHaveTextContent('shop-prod');
    expect(origins[0]).toHaveTextContent('shop');
    expect(origins[1]).toHaveTextContent('gone-conn');
  });

  it('explain_rows_are_marked_and_show_the_inner_statement', async () => {
    rows = [entry(1, { sql_text: 'EXPLAIN FORMAT=JSON SELECT * FROM orders' }), entry(2)];
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    expect(within(list[0]!).getByTestId('history-explain')).toHaveTextContent('Explain');
    expect(list[0]).toHaveTextContent('SELECT * FROM orders');
    expect(list[0]).not.toHaveTextContent('FORMAT=JSON');
    expect(within(list[1]!).queryByTestId('history-explain')).toBeNull();
  });

  it('explain_variants_of_postgres_and_mariadb', async () => {
    rows = [
      entry(1, { sql_text: 'EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1 FROM a' }),
      entry(2, { sql_text: 'ANALYZE FORMAT=JSON SELECT 2 FROM b' }),
      entry(3, { sql_text: 'EXPLAIN ANALYZE SELECT 3 FROM c' }),
    ];
    render(<HistoryView tabId={tab} />);
    const list = await screen.findAllByTestId('history-row');
    expect(within(list[0]!).getByTestId('history-explain')).toHaveTextContent('Analyze');
    expect(list[0]).toHaveTextContent('SELECT 1 FROM a');
    expect(within(list[1]!).getByTestId('history-explain')).toHaveTextContent('Analyze');
    expect(list[1]).toHaveTextContent('SELECT 2 FROM b');
    expect(list[1]).not.toHaveTextContent('FORMAT');
    expect(within(list[2]!).getByTestId('history-explain')).toHaveTextContent('Analyze');
    expect(list[2]).toHaveTextContent('SELECT 3 FROM c');
  });
});
