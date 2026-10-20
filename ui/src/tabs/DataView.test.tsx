import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ColumnMeta, ObjectRef, TableDataPage, Value } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { nextSort, useTableData } from '../store/tableData';
import { objectTabId, useTabs, type ObjectTabRef as Tab } from '../store/tabs';
import { useAppSettings } from '../store/appSettings';
import { callsOf, config, fail, ids, mockBackend, resetStores, serverInfo, sampleTree } from '../test/backend';
import { DataView, pageSizeOptions, pageToResult, sortMarks, unsortable } from './DataView';

// The grid draws on a canvas: a stand-in with one header button per column (calls
// `onHeaderClicked` like Glide's header click, shows the header mark) and the row numbering.
vi.mock('./query/ResultGrid', () => ({
  ResultGrid: ({
    result,
    onHeaderClicked,
    headerMarks,
    firstRowNumber,
  }: {
    result: { columns: { name: string }[]; rowCount: number };
    onHeaderClicked?: (i: number) => void;
    headerMarks?: Record<number, string>;
    firstRowNumber?: number;
  }) => (
    <div data-testid="grid" data-first-row={firstRowNumber}>
      {result.columns.map((c, i) => (
        <button key={c.name} type="button" data-mark={headerMarks?.[i] ?? ''} onClick={() => onHeaderClicked?.(i)}>
          {c.name}
        </button>
      ))}
      <span data-testid="grid-rows">{result.rowCount}</span>
    </div>
  ),
}));

const col = (name: string, db_type = 'int'): ColumnMeta => ({ name, db_type, logical: 'Integer', nullable: false, origin_table: null, origin_column: null });
const int = (v: number): Value => ({ t: 'Int', v });

/** A page of `n` rows with ids starting at `from`. */
function page(n: number, from = 1, hasMore = false, over: Partial<TableDataPage> = {}): TableDataPage {
  return {
    columns: [col('id'), col('we`ird"col'), col('doc', 'json')],
    rows: Array.from({ length: n }, (_, i) => [int(from + i), int(0), { t: 'Json', v: '{}' }]),
    has_more: hasMore,
    truncated_cells: 0,
    size_capped: false,
    key_order: [],
    elapsed_ms: 2,
    ...over,
  };
}

const objectRef: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };

function makeTab(ref: ObjectRef = objectRef): Tab {
  return { id: objectTabId(ids.prod, ref), kind: 'object', connectionId: ids.prod, objectRef: ref, title: ref.name };
}

const PAGE_KEY = `${makeTab().id}:page`;
const COUNT_KEY = `${makeTab().id}:count`;

function setup(
  handlers: Record<string, (a: Record<string, unknown>) => unknown>,
  { estimate = null, tab = makeTab() }: { estimate?: number | null; tab?: Tab } = {},
) {
  mockBackend({ get_table_page_size: () => 100, table_data_cancel: () => true, ...handlers });
  useTabs.getState().open(tab);
  return { tab, ...render(<DataView tab={tab} estimate={estimate} />) };
}

/** A promise the test resolves (or rejects) by hand. */
function deferred<T>() {
  let resolve: (v: T) => void = () => undefined;
  let reject: (e: unknown) => void = () => undefined;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const grid = () => within(screen.getByTestId('grid'));

beforeEach(() => {
  resetStores();
});

describe('DataView', () => {
  it('loads the first page with the settings default and pages with has_more', async () => {
    const user = userEvent.setup();
    setup({
      get_table_page_size: () => 10,
      table_data: (a) => ((a.offset as number) === 0 ? page(10, 1, true) : page(3, 11, false)),
    });
    expect(await screen.findByText('Rows 1–10')).toBeInTheDocument();
    expect(callsOf('table_data')).toEqual([{ id: ids.prod, obj: objectRef, limit: 10, offset: 0, order: null, requestKey: PAGE_KEY }]);
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
    expect(screen.getByTestId('grid')).toHaveAttribute('data-first-row', '1');

    await user.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByText('Rows 11–13')).toBeInTheDocument();
    expect(callsOf('table_data')[1]).toMatchObject({ limit: 10, offset: 10 });
    // Row numbers continue from the offset.
    expect(screen.getByTestId('grid')).toHaveAttribute('data-first-row', '11');
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();

    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    expect(await screen.findByText('Rows 1–10')).toBeInTheDocument();
    expect(callsOf('table_data')[2]).toMatchObject({ offset: 0 });
  });

  it('defaults to 100 rows when the setting cannot be read', async () => {
    setup({
      get_table_page_size: () => {
        throw fail('connection_lost');
      },
      table_data: () => page(3),
    });
    expect(await screen.findByText('Rows 1–3')).toBeInTheDocument();
    expect(callsOf('table_data')[0]).toMatchObject({ limit: 100 });
  });

  it('header clicks sort server-side: asc, desc, none; the arrow is a header mark only', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3) });
    await screen.findByText('Rows 1–3');
    const header = () => grid().getByRole('button', { name: 'we`ird"col' });

    await user.click(header());
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    expect(callsOf('table_data')[1]).toMatchObject({ offset: 0, order: { column: 'we`ird"col', dir: 'asc' } });
    // The column keeps its name (copy / export use it); the arrow is drawn in the header.
    await waitFor(() => expect(header()).toHaveAttribute('data-mark', '↑'));
    expect(screen.getByText(/Sorted by we`ird"col ↑/)).toBeInTheDocument();

    await user.click(header());
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(3));
    expect(callsOf('table_data')[2]).toMatchObject({ order: { column: 'we`ird"col', dir: 'desc' } });
    await waitFor(() => expect(header()).toHaveAttribute('data-mark', '↓'));

    await user.click(header());
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(4));
    expect(callsOf('table_data')[3]).toMatchObject({ order: null });
    await waitFor(() => expect(header()).toHaveAttribute('data-mark', ''));
    expect(screen.queryByText(/Sorted by/)).not.toBeInTheDocument();
  });

  it('postgres json columns are not sortable: a friendly note, no request', async () => {
    const user = userEvent.setup();
    useConnections.getState().setStored([], [config(ids.prod, 'c', { kind: 'postgres' })]);
    const ref: ObjectRef = { database: 'shop', schema: 'public', name: 'docs', kind: 'Table' };
    setup({ table_data: () => page(2) }, { tab: makeTab(ref) });
    await screen.findByText('Rows 1–2');
    await user.click(grid().getByRole('button', { name: 'doc' }));
    expect(await screen.findByText("Can't sort by doc: values of type json have no order.")).toBeInTheDocument();
    expect(callsOf('table_data')).toHaveLength(1);
  });

  it('without a sort, the primary key order is hinted', async () => {
    setup({ table_data: () => page(2, 1, false, { key_order: ['id'] }) });
    expect(await screen.findByText('By primary key')).toBeInTheDocument();
  });

  it('sorting goes back to the first page; Clear sort drops it', async () => {
    const user = userEvent.setup();
    setup({ get_table_page_size: () => 10, table_data: (a) => page(10, (a.offset as number) + 1, true) });
    await screen.findByText('Rows 1–10');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('Rows 11–20');
    await user.click(grid().getByRole('button', { name: 'id' }));
    await screen.findByText('Rows 1–10');
    expect(callsOf('table_data').at(-1)).toMatchObject({ offset: 0, order: { column: 'id', dir: 'asc' } });
    await user.click(screen.getByRole('button', { name: 'Clear sort' }));
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ order: null }));
  });

  it('the page size select changes only this tab', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3) });
    await screen.findByText('Rows 1–3');
    const select = screen.getByRole('combobox', { name: 'Page size' });
    expect(select).toHaveValue('100');
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual(['100', '500', '1,000']);
    await user.selectOptions(select, '500');
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ limit: 500, offset: 0 }));
    expect(useAppSettings.getState().tablePageSize).toBe(100);
    expect(callsOf('set_table_page_size')).toEqual([]);
  });

  it('an error shows its hint and Retry loads again', async () => {
    const user = userEvent.setup();
    let attempts = 0;
    setup({
      table_data: () => {
        if (attempts++ === 0) throw fail('server_db_missing');
        return page(1);
      },
    });
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent("Unknown database 'shop'");
    expect(alert).toHaveTextContent('Database does not exist.');
    await user.click(within(alert).getByRole('button', { name: 'Retry' }));
    expect(await screen.findByText('Rows 1–1')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('a lost connection offers Reconnect', async () => {
    const user = userEvent.setup();
    let attempts = 0;
    setup({
      table_data: () => {
        if (attempts++ === 0) throw fail('connection_lost');
        return page(2);
      },
      open_connection: () => ({ info: serverInfo('MySql8'), timings: [] }),
      list_tree: () => sampleTree(),
    });
    const alert = await screen.findByRole('alert');
    await user.click(within(alert).getByRole('button', { name: 'Reconnect' }));
    expect(await screen.findByText('Rows 1–2')).toBeInTheDocument();
    expect(callsOf('open_connection')).toEqual([{ id: ids.prod, prompt: null }]);
  });

  it('a slow page can be cancelled', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => new Promise(() => undefined) });
    await user.click(await screen.findByRole('button', { name: 'Cancel' }));
    expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: PAGE_KEY }]);
    expect(screen.queryByRole('button', { name: 'Refresh' })).not.toBeInTheDocument();
  });

  it('a cancelled page shows Retry', async () => {
    setup({
      table_data: () => {
        throw fail('cancelled');
      },
    });
    const alert = await screen.findByRole('alert');
    expect(within(alert).getByRole('button', { name: 'Retry' })).toBeInTheDocument();
  });

  it('an empty table says so', async () => {
    setup({ table_data: () => page(0) });
    expect(await screen.findByText('This table has no rows.')).toBeInTheDocument();
    expect(screen.getByText('No rows')).toBeInTheDocument();
    expect(screen.queryByTestId('grid')).not.toBeInTheDocument();
  });

  it('truncated values and a size-capped page are reported; Next continues after the last row', async () => {
    const user = userEvent.setup();
    setup({
      table_data: (a) =>
        (a.offset as number) === 0 ? page(40, 1, true, { truncated_cells: 2, size_capped: true }) : page(5, 41),
    });
    expect(await screen.findByText(/Some values are truncated; open in query to see full values\./)).toBeInTheDocument();
    expect(screen.getByText(/This page stopped early to stay small/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ offset: 40 }));
    expect(await screen.findByText('Rows 41–45')).toBeInTheDocument();
    expect(screen.queryByText(/truncated/)).not.toBeInTheDocument();
  });

  it('shows the estimate, and Count fetches the exact count', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3), table_row_count: () => ({ rows: 12_400, elapsed_ms: 5 }) }, { estimate: 12_345 });
    expect(await screen.findByText('~12,345 rows')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Count' }));
    expect(await screen.findByText('12,400 rows')).toBeInTheDocument();
    expect(callsOf('table_row_count')).toEqual([{ id: ids.prod, obj: objectRef, requestKey: COUNT_KEY }]);
  });

  it('refresh or sorting clears the exact count; paging keeps it', async () => {
    const user = userEvent.setup();
    setup(
      { table_data: (a) => page(3, (a.offset as number) + 1, true), table_row_count: () => ({ rows: 999, elapsed_ms: 5 }) },
      { estimate: 12_345 },
    );
    await screen.findByText('Rows 1–3');
    await user.click(screen.getByRole('button', { name: 'Count' }));
    expect(await screen.findByText('999 rows')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('Rows 4–6');
    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    await screen.findByText('Rows 1–3');
    expect(screen.getByText('999 rows')).toBeInTheDocument();
    expect(callsOf('table_row_count')).toHaveLength(1);

    for (const act of [
      () => user.click(screen.getByRole('button', { name: 'Refresh' })),
      () => user.click(grid().getByRole('button', { name: 'id' })),
    ]) {
      if (!screen.queryByText('999 rows')) {
        await user.click(screen.getByRole('button', { name: 'Count' }));
        expect(await screen.findByText('999 rows')).toBeInTheDocument();
      }
      await act();
      expect(await screen.findByText('~12,345 rows')).toBeInTheDocument();
      expect(screen.queryByText('999 rows')).not.toBeInTheDocument();
    }
  });

  it('Prev after a page that stopped early returns to the exact previous offset', async () => {
    const user = userEvent.setup();
    // Pages of 10, but the first stops after 4 rows (byte budget).
    setup({
      get_table_page_size: () => 10,
      table_data: (a) => {
        const offset = a.offset as number;
        return offset === 0 ? page(4, 1, true, { size_capped: true }) : page(10, offset + 1, true);
      },
    });
    await screen.findByText('Rows 1–4');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('Rows 5–14');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('Rows 15–24');
    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    await screen.findByText('Rows 5–14');
    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    await screen.findByText('Rows 1–4');
    expect(callsOf('table_data').map((c) => c.offset)).toEqual([0, 4, 14, 4, 0]);
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
  });

  it('a count answered after the page changed is dropped', async () => {
    const user = userEvent.setup();
    const slow = deferred<{ rows: number; elapsed_ms: number }>();
    setup({ table_data: () => page(3), table_row_count: () => slow.promise }, { estimate: 7 });
    await screen.findByText('Rows 1–3');
    await user.click(screen.getByRole('button', { name: 'Count' }));
    expect(await screen.findByRole('button', { name: 'Cancel count' })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    // The running count was cancelled with the old page.
    await waitFor(() => expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: COUNT_KEY }]));
    slow.resolve({ rows: 123, elapsed_ms: 1 });
    await screen.findByText('Rows 1–3');
    expect(screen.queryByText('123 rows')).not.toBeInTheDocument();
    expect(screen.getByText('~7 rows')).toBeInTheDocument();
  });

  it('Cancel count stops the count', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3), table_row_count: () => new Promise(() => undefined) });
    await screen.findByText('Rows 1–3');
    await user.click(screen.getByRole('button', { name: 'Count' }));
    await user.click(await screen.findByRole('button', { name: 'Cancel count' }));
    expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: COUNT_KEY }]);
    expect(screen.getByRole('button', { name: 'Count' })).toBeInTheDocument();
  });

  it('Open in query fills a query tab with the page SELECT', async () => {
    const user = userEvent.setup();
    useConnections.getState().setStored([], [config(ids.prod, 'c', { kind: 'postgres' })]);
    const ref: ObjectRef = { database: 'shop', schema: 'public', name: 'đơn_hàng', kind: 'Table' };
    setup({ get_table_page_size: () => 50, table_data: (a) => page(50, (a.offset as number) + 1, true) }, { tab: makeTab(ref) });
    await screen.findByText('Rows 1–50');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('Rows 51–100');
    await user.click(grid().getByRole('button', { name: 'id' }));
    await screen.findByText('Rows 1–50');
    await user.click(screen.getByRole('button', { name: 'Open in query' }));
    const doc = [...useQueryDocs.getState().docs.values()].at(-1)!;
    expect(doc).toMatchObject({
      connectionId: ids.prod,
      database: 'shop',
      sql: 'SELECT * FROM "public"."đơn_hàng" ORDER BY "id" ASC LIMIT 50;',
    });
  });

  it('keeps its page across remounts; closing the tab stops its running load', async () => {
    const { tab, unmount } = setup({ table_data: () => page(2) });
    await screen.findByText('Rows 1–2');
    unmount();
    render(<DataView tab={tab} estimate={null} />);
    expect(await screen.findByText('Rows 1–2')).toBeInTheDocument();
    expect(callsOf('table_data')).toHaveLength(1);

    mockBackend({ table_data: () => new Promise(() => undefined), table_data_cancel: () => true });
    void useTableData.getState().load(tab);
    useTabs.getState().close(tab.id);
    expect(useTableData.getState().byTab[tab.id]).toBeUndefined();
    await waitFor(() => expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: PAGE_KEY }]));
  });
});

describe('data view helpers', () => {
  it('nextSort cycles asc, desc, none and restarts on another column', () => {
    expect(nextSort(null, 'a')).toEqual({ column: 'a', dir: 'asc' });
    expect(nextSort({ column: 'a', dir: 'asc' }, 'a')).toEqual({ column: 'a', dir: 'desc' });
    expect(nextSort({ column: 'a', dir: 'desc' }, 'a')).toBeNull();
    expect(nextSort({ column: 'a', dir: 'desc' }, 'b')).toEqual({ column: 'b', dir: 'asc' });
  });

  it('page sizes include the settings default and the current size', () => {
    expect(pageSizeOptions(100, 100)).toEqual([100, 500, 1000]);
    expect(pageSizeOptions(250, 2000)).toEqual([100, 250, 500, 1000, 2000]);
    expect(pageSizeOptions(null, 100)).toEqual([100, 500, 1000]);
  });

  it('pageToResult chunks rows and keeps the column names', () => {
    const big = page(1200);
    const r = pageToResult(big);
    expect(r.chunks.map((c) => c.length)).toEqual([500, 500, 200]);
    expect(r.rowCount).toBe(1200);
    expect(r.columns.map((c) => c.name)).toEqual(['id', 'we`ird"col', 'doc']);
    expect(sortMarks(big, { column: 'doc', dir: 'desc' })).toEqual({ 2: '↓' });
    expect(sortMarks(big, null)).toEqual({});
  });

  it('unsortable matches the backend list', () => {
    for (const t of ['json', 'JSON', 'xml', 'point', 'polygon', 'json[]', 'pg_catalog.json']) expect(unsortable('postgres', t)).toBe(true);
    for (const t of ['jsonb', 'text', 'integer', 'int[]']) expect(unsortable('postgres', t)).toBe(false);
    expect(unsortable('mysql', 'json')).toBe(false);
  });
});
