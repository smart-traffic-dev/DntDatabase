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
});
