import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent } from '@testing-library/react';
import type { ApplyReport, ColumnMeta, EditPreview, EditableInfo, ObjectRef, TableDataPage, Value } from '../bindings';
import { closeTab } from '../components/tabActions';
import { DialogHost } from '../dialogs/DialogHost';
import { ONE_AT_A_TIME, useDataEdits } from '../store/dataEdits';
import { openValueEditor } from './data/openValueEditor';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { draftToFilter, newFilterRow, nextSortKeys, splitList, useTableData } from '../store/tableData';
import { useDialogs } from '../store/dialogs';
import { toggleFilterBar } from '../dialogs/resultExport';
import { objectTabId, useTabs, type ObjectTabRef as Tab } from '../store/tabs';
import { useAppSettings } from '../store/appSettings';
import { appError, callsOf, config, fail, ids, mockBackend, resetStores, serverInfo, sampleTree } from '../test/backend';
import { DataActions, DataPager, DataView, dataCellMenu, filterValueText, pageSizeOptions, pageToResult, sortMarks, unsortable } from './DataView';

// The grid draws on a canvas: a stand-in with one header button per column (calls
// `onHeaderClicked` like Glide's header click, shows the header mark) and the row numbering.
vi.mock('./query/ResultGrid', () => ({
  ResultGrid: ({
    result,
    onHeaderClicked,
    headerMarks,
    firstRowNumber,
    editing,
  }: {
    result: { columns: { name: string }[]; rowCount: number };
    onHeaderClicked?: (i: number) => void;
    headerMarks?: Record<number, string>;
    firstRowNumber?: number;
    editing?: {
      isEditable(row: number, col: number): boolean;
      status(row: number, col: number): string | null;
      onEdit(row: number, col: number, text: string): void;
      onActivate?(row: number, col: number): void;
      onKey?(e: unknown, cell: { row: number; col: number } | null): boolean;
    };
  }) => (
    <div
      data-testid="grid"
      data-first-row={firstRowNumber}
      onKeyDown={(e) => {
        // The real grid reports the selected cell; the stand-in pretends row 0, column 1.
        if (editing?.onKey?.(e, { row: 0, col: 1 })) e.stopPropagation();
      }}
    >
      {result.columns.map((c, i) => (
        <button key={c.name} type="button" data-mark={headerMarks?.[i] ?? ''} onClick={() => onHeaderClicked?.(i)}>
          {c.name}
        </button>
      ))}
      {editing
        ? Array.from({ length: result.rowCount }).flatMap((_, r) =>
            result.columns.map((_c, ci) => (
              <button
                key={`${r},${ci}`}
                type="button"
                aria-label={`cell ${r},${ci}`}
                data-status={editing.status(r, ci) ?? ''}
                data-editable={String(editing.isEditable(r, ci))}
                // The text the overlay commits: `data-next` when a test sets it, else "edited".
                onClick={(e) => editing.onEdit(r, ci, e.currentTarget.getAttribute('data-next') ?? 'edited')}
                onDoubleClick={() => editing.onActivate?.(r, ci)}
              />
            )),
          )
        : null}
      <span data-testid="grid-rows">{result.rowCount}</span>
    </div>
  ),
}));

vi.mock('./data/openValueEditor', async (importOriginal) => {
  const original = await importOriginal<typeof import('./data/openValueEditor')>();
  return {
    ...original,
    openValueEditor: vi.fn(original.openValueEditor),
  };
});

const col = (name: string, db_type = 'int'): ColumnMeta => ({ name, db_type, logical: 'Integer', nullable: false, origin_table: null, origin_column: null });
const int = (v: number): Value => ({ t: 'Int', v });

/** A page of `n` rows with ids starting at `from`. */
function page(n: number, from = 1, hasMore = false, over: Partial<TableDataPage> = {}): TableDataPage {
  return {
    columns: [col('id'), col('we`ird"col'), col('doc', 'json')],
    rows: Array.from({ length: n }, (_, i) => [int(from + i), int(0), { t: 'Json', v: '{}' }]),
    has_more: hasMore,
    truncated_cells: 0,
    truncated: [],
    size_capped: false,
    key_order: [],
    row_versions: null,
    hidden_key: [],
    hidden_key_values: [],
    elapsed_ms: 2,
    ...over,
  };
}

const objectRef: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };

const readOnlyInfo: EditableInfo = {
  editable: false,
  reason: 'read_only_connection',
  key: [],
  hidden_key: [],
  editable_columns: [],
  atomic: 'yes',
  protection: 'row_version',
};
const editableInfo = (over: Partial<EditableInfo> = {}): EditableInfo => ({
  editable: true,
  reason: null,
  key: ['id'],
  hidden_key: [],
  editable_columns: ['we`ird"col', 'doc'],
  atomic: 'yes',
  protection: 'row_version',
  ...over,
});
const previewOf = (over: Partial<EditPreview> = {}): EditPreview => ({
  plan_id: 'p1',
  statements: ['UPDATE `orders` SET `we``ird"col` = \'edited\' WHERE `id` = 1'],
  verdict: 'allow',
  atomic: 'yes',
  protections: [{ level: 'full' }],
  reason: null,
  ...over,
});
const reportOf = (over: Partial<ApplyReport> = {}): ApplyReport => ({
  outcome: { outcome: 'committed' },
  statements: [{ index: 0, affected: 1, error: null, conflict: false }],
  inserted: [],
  error: null,
  ...over,
});

function makeTab(ref: ObjectRef = objectRef): Tab {
  return { id: objectTabId(ids.prod, ref), kind: 'object', connectionId: ids.prod, objectRef: ref, title: ref.name };
}

const PAGE_KEY = `${makeTab().id}:page`;
const COUNT_KEY = `${makeTab().id}:count`;

function setup(
  handlers: Record<string, (a: Record<string, unknown>) => unknown>,
  { estimate = null, tab = makeTab() }: { estimate?: number | null; tab?: Tab } = {},
) {
  mockBackend({
    get_table_page_size: () => 100,
    table_data_cancel: () => true,
    data_editable: () => readOnlyInfo,
    refresh_node: () => null,
    ...handlers,
  });
  useTabs.getState().open(tab);
  return { tab, ...render(withControls(tab, estimate)) };
}

/** The data view with its controls: the strip's actions (Filter, Refresh, ⋯) and the sub-tab row's pager. */
function withControls(tab: Tab, estimate: number | null = null) {
  return (
    <>
      <div role="toolbar" aria-label="Object">
        <DataActions tab={tab} />
      </div>
      <DataPager tab={tab} estimate={estimate} />
      <DataView tab={tab} />
    </>
  );
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

type User = ReturnType<typeof userEvent.setup>;
/** Opens the data toolbar's `⋯` menu (page size, count). */
const openMore = (user: User) => user.click(screen.getByRole('button', { name: 'More data actions' }));
/** Runs an item of the `⋯` menu. */
async function dataAction(user: User, name: string) {
  await openMore(user);
  await user.click(screen.getByRole('menuitem', { name }));
}
const UNKNOWN_TEXT = 'Outcome unknown: the change may have been applied. Reload to check.';

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
    expect(await screen.findByText('1–10')).toBeInTheDocument();
    expect(callsOf('table_data')).toEqual([{ id: ids.prod, obj: objectRef, limit: 10, offset: 0, sort: [], filter: null, requestKey: PAGE_KEY }]);
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
    expect(screen.getByTestId('grid')).toHaveAttribute('data-first-row', '1');

    await user.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByText('11–13')).toBeInTheDocument();
    expect(callsOf('table_data')[1]).toMatchObject({ limit: 10, offset: 10 });
    // Row numbers continue from the offset.
    expect(screen.getByTestId('grid')).toHaveAttribute('data-first-row', '11');
    expect(screen.getByRole('button', { name: 'Next page' })).toBeDisabled();

    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    expect(await screen.findByText('1–10')).toBeInTheDocument();
    expect(callsOf('table_data')[2]).toMatchObject({ offset: 0 });
  });

  it('the strip actions are Filter, Refresh and ⋯; the pager holds the sort chip, the range, Previous and Next', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3, 1, false, { key_order: ['id'] }) }, { estimate: 3412 });
    await screen.findByText('1–3');
    const actions = screen.getByRole('toolbar', { name: 'Object' });
    const names = (el: HTMLElement) => within(el).getAllByRole('button').map((b) => b.getAttribute('aria-label') ?? b.textContent);
    expect(names(actions)).toEqual(['Filter', 'Refresh', 'More data actions']);
    // Icon-only buttons are named and have a tooltip (with the shortcut where there is one).
    for (const name of ['Filter', 'Refresh', 'More data actions']) {
      expect(within(actions).getByRole('button', { name })).toHaveAttribute('title');
    }
    expect(within(actions).getByRole('button', { name: 'Filter' })).toHaveAttribute('title', 'Filter rows (F3)');
    const pager = screen.getByRole('group', { name: 'Pages' });
    expect(names(pager)).toEqual(['Previous page', 'Next page']);
    expect(within(pager).getByTestId('data-pager')).toHaveTextContent('1–3 / ~3,412');
    expect(within(pager).getByText('By primary key')).toBeInTheDocument();

    await user.click(grid().getByRole('button', { name: 'id' }));
    const chip = await within(pager).findByTitle('Sorted by id ↑');
    expect(names(pager)[0]).toBe('Clear sort');
    expect(chip).toContainElement(within(pager).getByRole('button', { name: 'Clear sort' }));
  });

  it('Refresh reloads the rows and the table metadata', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3) });
    await screen.findByText('1–3');
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    expect(callsOf('refresh_node')).toHaveLength(1);
  });

  it('a read-only connection adds no note: the header badge says so', async () => {
    setup({ table_data: () => page(3) });
    await screen.findByText('1–3');
    await waitFor(() => expect(callsOf('data_editable')).toHaveLength(1));
    await new Promise((r) => setTimeout(r, 10));
    expect(screen.queryByText('Read-only connection.')).toBeNull();
  });

  it('defaults to 100 rows when the setting cannot be read', async () => {
    setup({
      get_table_page_size: () => {
        throw fail('connection_lost');
      },
      table_data: () => page(3),
    });
    expect(await screen.findByText('1–3')).toBeInTheDocument();
    expect(callsOf('table_data')[0]).toMatchObject({ limit: 100 });
  });

  it('header clicks sort server-side: asc, desc, none; the arrow is a header mark only', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3) });
    await screen.findByText('1–3');
    const header = () => grid().getByRole('button', { name: 'we`ird"col' });

    await user.click(header());
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    expect(callsOf('table_data')[1]).toMatchObject({ offset: 0, sort: [{ column: 'we`ird"col', dir: 'asc' }] });
    // The column keeps its name (copy / export use it); the arrow is drawn in the header.
    await waitFor(() => expect(header()).toHaveAttribute('data-mark', '↑'));
    expect(screen.getByTitle(/Sorted by we`ird"col ↑/)).toHaveTextContent('we`ird"col ↑');

    await user.click(header());
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(3));
    expect(callsOf('table_data')[2]).toMatchObject({ sort: [{ column: 'we`ird"col', dir: 'desc' }] });
    await waitFor(() => expect(header()).toHaveAttribute('data-mark', '↓'));

    await user.click(header());
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(4));
    expect(callsOf('table_data')[3]).toMatchObject({ sort: [] });
    await waitFor(() => expect(header()).toHaveAttribute('data-mark', ''));
    expect(screen.queryByTitle(/Sorted by/)).not.toBeInTheDocument();
  });

  it('postgres json columns are not sortable: a friendly note, no request', async () => {
    const user = userEvent.setup();
    useConnections.getState().setStored([], [config(ids.prod, 'c', { kind: 'postgres' })]);
    const ref: ObjectRef = { database: 'shop', schema: 'public', name: 'docs', kind: 'Table' };
    setup({ table_data: () => page(2) }, { tab: makeTab(ref) });
    await screen.findByText('1–2');
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
    await screen.findByText('1–10');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('11–20');
    await user.click(grid().getByRole('button', { name: 'id' }));
    await screen.findByText('1–10');
    expect(callsOf('table_data').at(-1)).toMatchObject({ offset: 0, sort: [{ column: 'id', dir: 'asc' }] });
    await user.click(screen.getByRole('button', { name: 'Clear sort' }));
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ sort: [] }));
  });

  it('the page size (in the ⋯ menu) changes only this tab', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3) });
    await screen.findByText('1–3');
    await openMore(user);
    const sizes = screen.getAllByRole('menuitemradio');
    expect(sizes.map((o) => o.textContent)).toEqual(['✓100 rows per page', '500 rows per page', '1,000 rows per page']);
    expect(screen.getByRole('menuitemradio', { name: '100 rows per page' })).toHaveAttribute('aria-checked', 'true');
    await user.click(screen.getByRole('menuitemradio', { name: '500 rows per page' }));
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ limit: 500, offset: 0 }));
    await openMore(user);
    expect(screen.getByRole('menuitemradio', { name: '500 rows per page' })).toHaveAttribute('aria-checked', 'true');
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
    expect(await screen.findByText('1–1')).toBeInTheDocument();
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
    expect(await screen.findByText('1–2')).toBeInTheDocument();
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
    expect(await screen.findByText('41–45')).toBeInTheDocument();
    expect(screen.queryByText(/truncated/)).not.toBeInTheDocument();
  });

  it('shows the estimate, and Count fetches the exact count', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3), table_row_count: () => ({ rows: 12_400, elapsed_ms: 5 }) }, { estimate: 12_345 });
    await waitFor(() => expect(screen.getByTestId('data-total')).toHaveTextContent(/^~12,345$/));
    await dataAction(user, 'Count rows');
    await waitFor(() => expect(screen.getByTestId('data-total')).toHaveTextContent(/^12,400$/));
    expect(callsOf('table_row_count')).toEqual([{ id: ids.prod, obj: objectRef, filter: null, requestKey: COUNT_KEY }]);
  });

  it('refresh clears the exact count; paging and sorting keep it', async () => {
    const user = userEvent.setup();
    setup(
      { table_data: (a) => page(3, (a.offset as number) + 1, true), table_row_count: () => ({ rows: 999, elapsed_ms: 5 }) },
      { estimate: 12_345 },
    );
    await screen.findByText('1–3');
    await dataAction(user, 'Count rows');
    await waitFor(() => expect(screen.getByTestId('data-total')).toHaveTextContent(/^999$/));
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('4–6');
    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    await screen.findByText('1–3');
    expect(screen.getByTestId('data-total')).toHaveTextContent(/^999$/);
    // Sorting does not change how many rows there are.
    await user.click(grid().getByRole('button', { name: 'id' }));
    await waitFor(() => expect(callsOf('table_data').at(-1)).toMatchObject({ sort: [{ column: 'id', dir: 'asc' }] }));
    await waitFor(() => expect(screen.getByTestId('data-total')).toHaveTextContent(/^999$/));
    expect(callsOf('table_row_count')).toHaveLength(1);

    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(screen.getByTestId('data-total')).toHaveTextContent(/^~12,345$/));
    expect(screen.queryByTestId('data-total')?.textContent).not.toBe('999');
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
    await screen.findByText('1–4');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('5–14');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('15–24');
    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    await screen.findByText('5–14');
    await user.click(screen.getByRole('button', { name: 'Previous page' }));
    await screen.findByText('1–4');
    expect(callsOf('table_data').map((c) => c.offset)).toEqual([0, 4, 14, 4, 0]);
    expect(screen.getByRole('button', { name: 'Previous page' })).toBeDisabled();
  });

  it('a count answered after the page changed is dropped', async () => {
    const user = userEvent.setup();
    const slow = deferred<{ rows: number; elapsed_ms: number }>();
    setup({ table_data: () => page(3), table_row_count: () => slow.promise }, { estimate: 7 });
    await screen.findByText('1–3');
    await dataAction(user, 'Count rows');
    await openMore(user);
    expect(await screen.findByRole('menuitem', { name: 'Cancel count' })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    // The running count was cancelled with the old page.
    await waitFor(() => expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: COUNT_KEY }]));
    slow.resolve({ rows: 123, elapsed_ms: 1 });
    await screen.findByText('1–3');
    expect(screen.queryByTestId('data-total')?.textContent).not.toBe('123');
    expect(screen.getByTestId('data-total')).toHaveTextContent(/^~7$/);
  });

  it('Cancel count stops the count', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3), table_row_count: () => new Promise(() => undefined) });
    await screen.findByText('1–3');
    await dataAction(user, 'Count rows');
    await dataAction(user, 'Cancel count');
    expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: COUNT_KEY }]);
    await openMore(user);
    expect(screen.getByRole('menuitem', { name: 'Count rows' })).toBeInTheDocument();
  });

  it('Open in query fills a query tab with the page SELECT', async () => {
    const user = userEvent.setup();
    useConnections.getState().setStored([], [config(ids.prod, 'c', { kind: 'postgres' })]);
    const ref: ObjectRef = { database: 'shop', schema: 'public', name: 'đơn_hàng', kind: 'Table' };
    setup({ get_table_page_size: () => 50, table_data: (a) => page(50, (a.offset as number) + 1, true) }, { tab: makeTab(ref) });
    await screen.findByText('1–50');
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('51–100');
    await user.click(grid().getByRole('button', { name: 'id' }));
    await screen.findByText('1–50');
    await dataAction(user, 'Open in query');
    const doc = [...useQueryDocs.getState().docs.values()].at(-1)!;
    expect(doc).toMatchObject({
      connectionId: ids.prod,
      database: 'shop',
      sql: 'SELECT * FROM "public"."đơn_hàng" ORDER BY "id" ASC LIMIT 50;',
    });
  });

  it('keeps its page across remounts; closing the tab stops its running load', async () => {
    const { tab, unmount } = setup({ table_data: () => page(2) });
    await screen.findByText('1–2');
    unmount();
    render(withControls(tab));
    expect(await screen.findByText('1–2')).toBeInTheDocument();
    expect(callsOf('table_data')).toHaveLength(1);

    mockBackend({ table_data: () => new Promise(() => undefined), table_data_cancel: () => true });
    void useTableData.getState().load(tab);
    useTabs.getState().close(tab.id);
    expect(useTableData.getState().byTab[tab.id]).toBeUndefined();
    await waitFor(() => expect(callsOf('table_data_cancel')).toEqual([{ id: ids.prod, requestKey: PAGE_KEY }]));
  });
});

describe('data view helpers', () => {
  it('a header click cycles asc, desc, none on one key and restarts on another column', () => {
    expect(nextSortKeys([], 'a')).toEqual([{ column: 'a', dir: 'asc' }]);
    expect(nextSortKeys([{ column: 'a', dir: 'asc' }], 'a')).toEqual([{ column: 'a', dir: 'desc' }]);
    expect(nextSortKeys([{ column: 'a', dir: 'desc' }], 'a')).toEqual([]);
    expect(nextSortKeys([{ column: 'a', dir: 'desc' }], 'b')).toEqual([{ column: 'b', dir: 'asc' }]);
  });

  it('Shift+click adds a key, cycles it in place and drops it after descending', () => {
    const a = { column: 'a', dir: 'asc' } as const;
    expect(nextSortKeys([a], 'b', true)).toEqual([a, { column: 'b', dir: 'asc' }]);
    expect(nextSortKeys([a, { column: 'b', dir: 'asc' }], 'b', true)).toEqual([a, { column: 'b', dir: 'desc' }]);
    expect(nextSortKeys([a, { column: 'b', dir: 'desc' }], 'b', true)).toEqual([a]);
    expect(nextSortKeys([a, { column: 'b', dir: 'desc' }], 'a', true)).toEqual([
      { column: 'a', dir: 'desc' },
      { column: 'b', dir: 'desc' },
    ]);
    // A plain click replaces every key.
    expect(nextSortKeys([a, { column: 'b', dir: 'desc' }], 'b')).toEqual([{ column: 'b', dir: 'asc' }]);
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
    expect(sortMarks(big, [{ column: 'doc', dir: 'desc' }])).toEqual({ 2: '↓' });
    expect(sortMarks(big, [])).toEqual({});
    // Several keys are numbered.
    expect(
      sortMarks(big, [
        { column: 'doc', dir: 'desc' },
        { column: 'id', dir: 'asc' },
      ]),
    ).toEqual({ 2: '↓1', 0: '↑2' });
  });

  it('unsortable matches the backend list', () => {
    for (const t of ['json', 'JSON', 'xml', 'point', 'polygon', 'json[]', 'pg_catalog.json']) expect(unsortable('postgres', t)).toBe(true);
    for (const t of ['jsonb', 'text', 'integer', 'int[]']) expect(unsortable('postgres', t)).toBe(false);
    expect(unsortable('mysql', 'json')).toBe(false);
  });

  it('draftToFilter parses lists and pairs; rows without a column are skipped', () => {
    const rows = [
      { ...newFilterRow('a', 'in'), value: ' 1, 2 ,,3 ' },
      { ...newFilterRow('b', 'between'), value: 'x', value2: 'y' },
      { ...newFilterRow('c', 'is_null'), value: 'ignored' },
      { ...newFilterRow('', 'eq'), value: 'skipped' },
      { ...newFilterRow('d', 'eq'), value: '' },
    ];
    expect(draftToFilter({ mode: 'or', rows })).toEqual({
      filter: {
        mode: 'or',
        conditions: [
          { column: 'a', op: 'in', values: ['1', '2', '3'] },
          { column: 'b', op: 'between', values: ['x', 'y'] },
          { column: 'c', op: 'is_null', values: [] },
          // An empty string is a value like any other.
          { column: 'd', op: 'eq', values: [''] },
        ],
      },
      error: null,
    });
    expect(draftToFilter({ mode: 'and', rows: [] })).toEqual({ filter: null, error: null });
    expect(draftToFilter({ mode: 'and', rows: [{ ...newFilterRow('a', 'in'), value: ' , ' }] }).error).toMatch(/IN on a needs/);
    expect(splitList('a,b')).toEqual(['a', 'b']);
  });

  it('filterValueText matches what the backend binds', () => {
    expect(filterValueText({ t: 'Int', v: 7 }, 'mysql')).toBe('7');
    expect(filterValueText({ t: 'Bool', v: true }, 'mysql')).toBe('1');
    expect(filterValueText({ t: 'Bool', v: false }, 'postgres')).toBe('false');
    expect(filterValueText({ t: 'DateTime', v: '2024-01-01 10:00:00' }, 'mysql')).toBe('2024-01-01 10:00:00');
    expect(filterValueText({ t: 'Other', v: { type_name: 'inet', text: '10.0.0.1' } }, 'postgres')).toBe('10.0.0.1');
    expect(filterValueText({ t: 'Bytes', v: { len: 2, preview: [1, 2] } }, 'mysql')).toBeNull();
    expect(filterValueText({ t: 'Array', v: [] }, 'postgres')).toBeNull();
    expect(filterValueText({ t: 'Text', v: 'x'.repeat(65_536) }, 'mysql')).toBeNull();
  });
});

describe('DataView filter and multi-sort', () => {
  const lastData = () => callsOf('table_data').at(-1)!;

  it('the filter bar sends a structured filter; Apply reads from the first page and Count uses it; Clear drops it', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3), table_row_count: () => ({ rows: 2, elapsed_ms: 1 }) }, { estimate: 99 });
    await screen.findByText('1–3');
    await user.click(screen.getByRole('button', { name: 'Filter' }));
    const bar = screen.getByRole('search', { name: 'Filter' });
    // Opening an empty bar offers a first condition on the first column.
    expect(within(bar).getByRole('combobox', { name: 'Column 1' })).toHaveValue('id');
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Column 1' }), 'we`ird"col');
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 1' }), 'contains');
    await user.type(within(bar).getByRole('textbox', { name: 'Value 1' }), "50%'; DROP");
    await user.click(within(bar).getByRole('button', { name: 'Apply' }));
    const filter = { mode: 'and', conditions: [{ column: 'we`ird"col', op: 'contains', values: ["50%'; DROP"] }] };
    await waitFor(() => expect(lastData()).toMatchObject({ offset: 0, filter }));
    expect(await screen.findByText('Filtered (1)')).toBeInTheDocument();
    // The estimate is of the whole table: not shown under a filter.
    expect(screen.queryByTestId('data-total')?.textContent).not.toBe('~99');
    await dataAction(user, 'Count rows');
    await waitFor(() => expect(screen.getByTestId('data-total')).toHaveTextContent(/^2$/));
    expect(callsOf('table_row_count').at(-1)).toMatchObject({ filter });

    // A second row: BETWEEN takes two values; OR for the bar.
    await user.click(within(bar).getByRole('button', { name: '+ Condition' }));
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 2' }), 'between');
    await user.type(within(bar).getByRole('textbox', { name: 'From 2' }), '1');
    await user.type(within(bar).getByRole('textbox', { name: 'To 2' }), '9{Enter}');
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Match conditions' }), 'or');
    await user.click(within(bar).getByRole('button', { name: 'Apply' }));
    await waitFor(() =>
      expect(lastData()).toMatchObject({
        filter: { mode: 'or', conditions: [filter.conditions[0], { column: 'id', op: 'between', values: ['1', '9'] }] },
      }),
    );

    // IS NULL takes no value; IN needs at least one.
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 2' }), 'in');
    await user.clear(within(bar).getByRole('textbox', { name: 'Value 2' }));
    await user.click(within(bar).getByRole('button', { name: 'Apply' }));
    expect(within(bar).getByRole('alert')).toHaveTextContent('IN on id needs at least one value.');
    await user.selectOptions(within(bar).getByRole('combobox', { name: 'Operator 2' }), 'is_null');
    expect(within(bar).queryByRole('textbox', { name: /2$/ })).toBeNull();
    await user.click(within(bar).getByRole('button', { name: 'Remove condition 2' }));
    expect(within(bar).getAllByTestId('filter-row')).toHaveLength(1);

    await user.click(within(bar).getByRole('button', { name: 'Clear' }));
    await waitFor(() => expect(lastData()).toMatchObject({ filter: null, offset: 0 }));
    expect(screen.queryByText(/Filtered/)).not.toBeInTheDocument();
    expect(within(bar).queryAllByTestId('filter-row')).toHaveLength(0);
  });

  it('F3 opens the filter bar and focuses it; the filter is kept per tab', async () => {
    const user = userEvent.setup();
    const { tab, unmount } = setup({ table_data: () => page(2) });
    await screen.findByText('1–2');
    // F3 anywhere in the view (here: a grid header).
    grid().getByRole('button', { name: 'id' }).focus();
    await user.keyboard('{F3}');
    const value = await screen.findByRole('textbox', { name: 'Value 1' });
    await waitFor(() => expect(value).toHaveFocus());
    await user.type(value, '5{Enter}');
    await waitFor(() => expect(lastData()).toMatchObject({ filter: { conditions: [{ column: 'id', op: 'eq', values: ['5'] }] } }));
    unmount();
    render(withControls(tab));
    expect(await screen.findByRole('textbox', { name: 'Value 1' })).toHaveValue('5');
    expect(screen.getByText('Filtered (1)')).toBeInTheDocument();
  });

  it('an empty filtered result says no rows match', async () => {
    const user = userEvent.setup();
    setup({ table_data: (a) => (a.filter ? page(0) : page(2)) });
    await screen.findByText('1–2');
    await user.click(screen.getByRole('button', { name: 'Filter' }));
    await user.type(screen.getByRole('textbox', { name: 'Value 1' }), 'x{Enter}');
    expect(await screen.findByText('No rows match the filter.')).toBeInTheDocument();
  });

  it('Shift+click adds a sort key; the header marks are numbered', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3) });
    await screen.findByText('1–3');
    await user.click(grid().getByRole('button', { name: 'id' }));
    await waitFor(() => expect(lastData()).toMatchObject({ sort: [{ column: 'id', dir: 'asc' }] }));
    await user.keyboard('{Shift>}');
    await user.click(grid().getByRole('button', { name: 'doc' }));
    await user.keyboard('{/Shift}');
    await waitFor(() =>
      expect(lastData()).toMatchObject({
        sort: [
          { column: 'id', dir: 'asc' },
          { column: 'doc', dir: 'asc' },
        ],
      }),
    );
    expect(await screen.findByTitle(/Sorted by id ↑, doc ↑/)).toBeInTheDocument();
    await waitFor(() => expect(grid().getByRole('button', { name: 'doc' })).toHaveAttribute('data-mark', '↑2'));
    // A plain click starts over with one key.
    await user.click(grid().getByRole('button', { name: 'doc' }));
    await waitFor(() => expect(lastData()).toMatchObject({ sort: [{ column: 'doc', dir: 'asc' }] }));
  });

  it('"Filter = this value" from the grid menu adds and applies a condition', async () => {
    setup({ table_data: () => page(3) });
    await screen.findByText('1–3');
    const tab = makeTab();
    const menu = dataCellMenu(tab);
    const entries = menu({ row: 0, col: 0, value: int(7), column: col('id') });
    expect(entries.map((e) => ('label' in e ? e.label : '-'))).toEqual(['Filter: id = 7', 'Filter: id <> 7']);
    (entries[0] as { run(): void }).run();
    await waitFor(() =>
      expect(lastData()).toMatchObject({ offset: 0, filter: { mode: 'and', conditions: [{ column: 'id', op: 'eq', values: ['7'] }] } }),
    );
    expect(await screen.findByRole('search', { name: 'Filter' })).toBeInTheDocument();
    // A second value is ANDed with the first.
    const nulls = menu({ row: 0, col: 1, value: { t: 'Null' }, column: col('we`ird"col') });
    expect(nulls.map((e) => ('label' in e ? e.label : '-'))).toEqual(['Filter: we`ird"col IS NULL', 'Filter: we`ird"col IS NOT NULL']);
    (nulls[0] as { run(): void }).run();
    await waitFor(() =>
      expect(lastData()).toMatchObject({
        filter: {
          conditions: [
            { column: 'id', op: 'eq', values: ['7'] },
            { column: 'we`ird"col', op: 'is_null', values: [] },
          ],
        },
      }),
    );
    expect(menu({ row: 0, col: 2, value: { t: 'Bytes', v: { len: 1, preview: [1] } }, column: col('b') })).toEqual([]);
  });

  it('Export… asks for the export dialog with the page, the sort and the filter', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => page(3, 1, false, { truncated_cells: 1 }) });
    await screen.findByText('1–3');
    await user.click(grid().getByRole('button', { name: 'id' }));
    await waitFor(() => expect(lastData()).toMatchObject({ sort: [{ column: 'id', dir: 'asc' }] }));
    await screen.findByTitle(/Sorted by id/);
    await dataAction(user, 'Export…');
    const request = useDialogs.getState().request;
    expect(request?.kind).toBe('exportResult');
    if (request?.kind !== 'exportResult') return;
    expect(request.request).toMatchObject({
      title: 'Export orders',
      engine: 'mysql',
      tableName: 'orders',
      loadedCount: 3,
      all: { kind: 'table', connectionId: ids.prod, obj: objectRef, sort: [{ column: 'id', dir: 'asc' }], filter: null },
    });
    expect(request.request.loadedNote).toMatch(/1 value of this page/);
    expect(request.request.rows()).toHaveLength(3);
  });

  it('toggleFilterBar (menu hook) toggles the bar of the active object tab', async () => {
    setup({ table_data: () => page(2) });
    await screen.findByText('1–2');
    toggleFilterBar();
    expect(await screen.findByRole('search', { name: 'Filter' })).toBeInTheDocument();
    toggleFilterBar();
    await waitFor(() => expect(screen.queryByRole('search', { name: 'Filter' })).not.toBeInTheDocument());
  });
});

describe('DataView editing', () => {
  const lastCall = (cmd: string) => callsOf(cmd).at(-1)!;
  const cell = (r: number, c: number) => screen.getByRole('button', { name: `cell ${r},${c}` });
  const rows3 = (over: Partial<TableDataPage> = {}) => page(3, 1, false, { row_versions: ['5', '6', '7'], ...over });

  /** An editable table of 3 rows; the first cell of the first row is edited when `stage`. */
  async function editable(
    handlers: Record<string, (a: Record<string, unknown>) => unknown> = {},
    { info = editableInfo(), stage = true, pageOver = {} as Partial<TableDataPage> } = {},
  ) {
    const user = userEvent.setup();
    const view = setup({ table_data: () => rows3(pageOver), data_editable: () => info, ...handlers });
    await screen.findByText('1–3');
    await waitFor(() => expect(cell(0, 1)).toHaveAttribute('data-editable', String(info.editable)));
    if (stage) await user.click(cell(0, 1));
    return { user, ...view };
  }

  it('says why a table is read-only and edits nothing', async () => {
    const user = userEvent.setup();
    setup({ table_data: () => rows3(), data_editable: () => ({ ...readOnlyInfo, reason: 'no_key' }) });
    expect(await screen.findByText('Read-only: this table has no primary key or unique NOT NULL index.')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add row' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'cell 0,1' })).not.toBeInTheDocument();
    void user;
  });

  it('maps the other reasons', async () => {
    setup({ table_data: () => rows3(), data_editable: () => ({ ...readOnlyInfo, reason: 'not_a_table' }) });
    expect(await screen.findByText('Views are read-only.')).toBeInTheDocument();
  });

  it('only the editable columns of a row take edits', async () => {
    await editable({}, { stage: false });
    expect(cell(0, 0)).toHaveAttribute('data-editable', 'false');
    expect(cell(0, 1)).toHaveAttribute('data-editable', 'true');
  });

  it('truncated_cell_opens_value_editor', async () => {
    vi.mocked(openValueEditor).mockClear();
    const { user } = await editable({}, { stage: false, pageOver: { truncated: [[0, 2]], truncated_cells: 1 } });
    expect(cell(0, 2)).toHaveAttribute('data-editable', 'false');
    fireEvent.doubleClick(cell(0, 2));
    expect(openValueEditor).toHaveBeenCalledWith(expect.objectContaining({ tabId: expect.any(String), row: 0, col: 2, column: 'doc' }));
    // Typing into it stages nothing.
    await user.click(cell(0, 2));
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
  });

  const binCol: ColumnMeta = { name: 'doc', db_type: 'blob', logical: 'Binary', nullable: true, origin_table: null, origin_column: null };
  const bytes = (len: number, shown = len): Value => ({ t: 'Bytes', v: { len, preview: Array.from({ length: shown }, (_, i) => i % 256) } });
  /** Row 0's `doc` is `doc0`; the others hold 3 whole bytes. */
  const binaryPage = (doc0: Value, over: Partial<TableDataPage> = {}) =>
    rows3({
      columns: [col('id'), col('we`ird"col'), binCol],
      rows: [0, 1, 2].map((i) => [int(i + 1), int(0), i === 0 ? doc0 : bytes(3)]),
      ...over,
    });

  it('a 1000-byte binary cell shown by its preview is fetched in full before it can be edited', async () => {
    const full = deferred<unknown>();
    const { user } = await editable(
      { table_data: () => binaryPage(bytes(1000, 64)), data_cell_fetch: () => full.promise },
      { stage: false },
    );
    // Not editable in place, whether or not the page listed it as truncated.
    expect(cell(0, 2)).toHaveAttribute('data-editable', 'false');
    await user.click(cell(0, 2));
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
    fireEvent.doubleClick(cell(0, 2));
    expect(await screen.findByText('Loading the full value…')).toBeInTheDocument();
    expect(lastCall('data_cell_fetch')).toEqual({ id: ids.prod, obj: objectRef, key: [['id', int(1)]], column: 'doc' });
    // Until the full value is here nothing can be saved or set.
    expect(screen.getByRole('textbox', { name: 'Hex' })).toHaveAttribute('readonly');
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Set NULL' })).toBeDisabled();
    await act(async () => full.resolve({ token: 'tok-b', payload: { kind: 'base64', data: btoa('\x01'.repeat(1000)) } }));
    await waitFor(() => expect(screen.getByText(/^1,000 bytes/)).toBeInTheDocument());
    expect(screen.getByRole('textbox', { name: 'Hex' })).not.toHaveAttribute('readonly');
  });

  it('a binary cell is never edited in place, even a whole one; the value editor opens instead', async () => {
    vi.mocked(openValueEditor).mockClear();
    const { user } = await editable({ table_data: () => binaryPage(bytes(3)) }, { stage: false });
    expect(cell(0, 2)).toHaveAttribute('data-editable', 'false');
    cell(0, 2).setAttribute('data-next', 'deadbeef');
    await user.click(cell(0, 2));
    expect(openValueEditor).toHaveBeenCalledWith(expect.objectContaining({ row: 0, col: 2, column: 'doc' }));
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
    expect(useDataEdits.getState().hasStaged(makeTab().id)).toBe(false);
  });

  it('the truncation note points binary-only pages to the value editor', async () => {
    await editable(
      { table_data: () => binaryPage(bytes(1000, 64), { truncated: [[0, 2]], truncated_cells: 1 }) },
      { stage: false },
    );
    expect(
      await screen.findByText('Some binary values are shown as a preview; open them in the value editor (Shift+Enter) to see all bytes.'),
    ).toBeInTheDocument();
    expect(screen.queryByText(/open in query/)).not.toBeInTheDocument();
  });

  it('the truncation note keeps the query hint when a text cell is cut too', async () => {
    await editable(
      { table_data: () => binaryPage(bytes(1000, 64), { truncated: [[0, 2], [1, 1]], truncated_cells: 2 }) },
      { stage: false },
    );
    expect(await screen.findByText(/Some values are truncated; open in query to see full values\./)).toBeInTheDocument();
  });

  it('a binary cell the backend lists as truncated is not editable in place', async () => {
    await editable({ table_data: () => binaryPage(bytes(1000, 64), { truncated: [[0, 2]], truncated_cells: 1 }) }, { stage: false });
    expect(cell(0, 2)).toHaveAttribute('data-editable', 'false');
    expect(cell(1, 2)).toHaveAttribute('data-editable', 'false');
  });

  it('an inline JSON edit stages the stored text exactly (20-digit numbers, 1.10 kept)', async () => {
    const raw = '{"big": 12345678901234567890, "x": 1.10}';
    const jsonCol: ColumnMeta = { name: 'doc', db_type: 'json', logical: 'Json', nullable: true, origin_table: null, origin_column: null };
    const { user } = await editable(
      {
        table_data: () => rows3({ columns: [col('id'), col('we`ird"col'), jsonCol], rows: [1, 2, 3].map((i) => [int(i), int(0), { t: 'Json', v: raw }]) }),
        data_preview: () => previewOf(),
      },
      { stage: false },
    );
    expect(cell(0, 2)).toHaveAttribute('data-editable', 'true');
    // Committing the overlay unchanged stages nothing.
    cell(0, 2).setAttribute('data-next', raw);
    await user.click(cell(0, 2));
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
    const next = raw.replace('"x": 1.10', '"x": 2.50');
    cell(0, 2).setAttribute('data-next', next);
    await user.click(cell(0, 2));
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview')).toMatchObject({ set: { edits: [{ set: [['doc', { kind: 'text', text: next }]] }] } });
    expect(next).toContain('12345678901234567890');
  });

  it('long text and JSON open the value editor instead of the in-place editor', async () => {
    vi.mocked(openValueEditor).mockClear();
    const long = 'a'.repeat(10_001);
    await editable(
      { table_data: () => rows3({ columns: [col('id'), col('we`ird"col'), col('doc', 'text')], rows: [1, 2, 3].map((i) => [int(i), int(0), { t: 'Text', v: long }]) }) },
      { stage: false },
    );
    expect(cell(0, 2)).toHaveAttribute('data-editable', 'false');
    fireEvent.doubleClick(cell(0, 2));
    expect(openValueEditor).toHaveBeenCalledWith(expect.objectContaining({ row: 0, col: 2, column: 'doc' }));
  });

  it('value_editor_stages_a_cut_cell_with_the_token_of_its_full_value', async () => {
    const { user } = await editable(
      {
        data_cell_fetch: () => ({ token: 'tok-9', payload: { kind: 'text', text: '{"full":true}' } }),
        data_preview: () => previewOf(),
      },
      { stage: false, pageOver: { truncated: [[0, 2]], truncated_cells: 1 } },
    );
    fireEvent.doubleClick(cell(0, 2));
    const box = await screen.findByRole('textbox', { name: 'Value' });
    await waitFor(() => expect(box).toHaveValue('{"full":true}'));
    expect(lastCall('data_cell_fetch')).toEqual({ id: ids.prod, obj: objectRef, key: [['id', int(1)]], column: 'doc' });
    await user.type(box, '!');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    // The editor closes and the change is staged against the row as loaded, with the full original's token.
    await waitFor(() => expect(screen.queryByRole('textbox', { name: 'Value' })).not.toBeInTheDocument());
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview')).toMatchObject({
      set: {
        edits: [
          {
            op: 'update',
            row: { key: [['id', int(1)]], loaded: [['doc', 'tok-9']] },
            set: [['doc', { kind: 'text', text: '{"full":true}!' }]],
          },
        ],
      },
    });
  });
  it('value_editor_closes_when_the_page_changes_and_never_stages_onto_another_row', async () => {
    const { user, tab } = await editable(
      { table_data: (a) => ((a.offset as number) === 0 ? page(3, 1, true, { row_versions: ['5', '6', '7'] }) : page(3, 4, false, { row_versions: ['8', '9', '10'] })) },
      { stage: false },
    );
    act(() => openValueEditor({ tabId: tab.id, row: 0, col: 1, column: 'we`ird"col', rowId: JSON.stringify([['id', int(1)]]) }));
    await screen.findByRole('textbox', { name: 'Value' });
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await screen.findByText('4–6');
    await waitFor(() => expect(screen.queryByRole('textbox', { name: 'Value' })).not.toBeInTheDocument());
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
  });

  it('value_editor_resaves_a_cut_cell_that_already_has_a_change', async () => {
    const { user } = await editable(
      {
        data_cell_fetch: () => ({ token: 'tok-1', payload: { kind: 'text', text: 'full' } }),
        data_preview: () => previewOf(),
      },
      { stage: false, pageOver: { truncated: [[0, 2]], truncated_cells: 1 } },
    );
    fireEvent.doubleClick(cell(0, 2));
    let box = await screen.findByRole('textbox', { name: 'Value' });
    await waitFor(() => expect(box).toHaveValue('full'));
    await user.type(box, '!');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('textbox', { name: 'Value' })).not.toBeInTheDocument());
    // Again: the staged text is shown and can be changed once more.
    fireEvent.doubleClick(cell(0, 2));
    box = await screen.findByRole('textbox', { name: 'Value' });
    await waitFor(() => expect(box).toHaveValue('full!'));
    await user.type(box, '?');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(screen.queryByRole('textbox', { name: 'Value' })).not.toBeInTheDocument());
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview')).toMatchObject({
      set: { edits: [{ row: { loaded: [['doc', 'tok-1']] }, set: [['doc', { kind: 'text', text: 'full!?' }]] }] },
    });
  });
  it('apply_shows_preview_then_reloads_on_commit', async () => {
    const { user } = await editable({
      data_preview: () => previewOf(),
      data_apply: () => reportOf(),
    });
    expect(cell(0, 1)).toHaveAttribute('data-status', 'edited');
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview')).toEqual({
      id: ids.prod,
      set: {
        object: objectRef,
        overwrite: false,
        edits: [
          {
            op: 'update',
            row: { key: [['id', int(1)]], original: [['we`ird"col', int(0)], ['doc', { t: 'Json', v: '{}' }]], xmin: '5' },
            set: [['we`ird"col', { kind: 'text', text: 'edited' }]],
          },
        ],
      },
    });
    expect(within(dialog).getByText(/UPDATE `orders` SET/)).toBeInTheDocument();
    expect(within(dialog).getByText('allow')).toBeInTheDocument();
    expect(callsOf('data_apply')).toEqual([]);
    await user.click(within(dialog).getByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    expect(callsOf('data_apply')).toEqual([{ planId: 'p1', confirmed: true }]);
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
    expect(cell(0, 1)).toHaveAttribute('data-status', '');
  });

  it('a blocked plan shows its reason and offers no Confirm', async () => {
    const { user } = await editable({ data_preview: () => previewOf({ verdict: 'block', reason: 'Blocked: read-only connection.' }) });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review changes' });
    expect(within(dialog).getByText('Blocked: read-only connection.')).toBeInTheDocument();
    expect(within(dialog).queryByRole('button', { name: 'Confirm' })).not.toBeInTheDocument();
  });

  it('warns about a table that cannot roll back and about unchecked columns', async () => {
    const { user } = await editable({
      data_editable: () => editableInfo({ atomic: 'no' }),
      data_preview: () =>
        previewOf({ atomic: 'no', protections: [{ level: 'columns', unchecked: ['price (double)', 'payload (truncated)'] }] }),
    });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review changes' });
    expect(within(dialog).getByText('Changes cannot be undone: statements that ran before an error stay applied.')).toBeInTheDocument();
    expect(within(dialog).getByText('Unchecked for conflicts: price (double), payload (truncated)')).toBeInTheDocument();
  });

  it('plan_stale_repreviews', async () => {
    let applies = 0;
    let previews = 0;
    const { user } = await editable({
      data_preview: () => previewOf({ plan_id: `p${++previews}` }),
      data_apply: () => {
        if (applies++ === 0) throw appError('plan_stale', 'The plan is stale.');
        return reportOf();
      },
    });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    expect(await screen.findByText('The table or connection changed; review the statements again.')).toBeInTheDocument();
    expect(callsOf('data_preview')).toHaveLength(2);
    // Nothing ran: the user reviews the new plan and confirms again.
    await user.click(screen.getByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(callsOf('data_apply')).toEqual([{ planId: 'p1', confirmed: true }, { planId: 'p2', confirmed: true }]));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });

  it('conflict_keeps_stage_and_marks_row', async () => {
    let applies = 0;
    const { user } = await editable({
      data_preview: (a) => previewOf({ plan_id: (a.set as { overwrite: boolean }).overwrite ? 'over' : 'p1' }),
      data_apply: () =>
        applies++ === 0
          ? reportOf({ outcome: { outcome: 'rolled_back' }, statements: [{ index: 0, affected: 0, error: null, conflict: true }] })
          : reportOf(),
    });
    // A second row is staged too: Overwrite must leave it alone.
    await user.click(cell(1, 1));
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(cell(0, 1)).toHaveAttribute('data-status', 'conflict'));
    expect(screen.getByRole('button', { name: 'Apply changes' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Reload row' })).toBeInTheDocument();
    expect(cell(1, 1)).toHaveAttribute('data-status', 'edited');

    // Overwrite acts on the conflicting row only, with overwrite: true, and needs its own confirmation.
    await user.click(screen.getByRole('button', { name: 'Overwrite' }));
    const dialog = await screen.findByRole('dialog', { name: 'Review changes' });
    const sent = lastCall('data_preview').set as { overwrite: boolean; edits: { row: { key: unknown } }[] };
    expect(sent.overwrite).toBe(true);
    expect(sent.edits).toHaveLength(1);
    expect(sent.edits[0]!.row.key).toEqual([['id', int(1)]]);
    expect(callsOf('data_apply')).toHaveLength(1);
    await user.click(within(dialog).getByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(callsOf('data_apply').at(-1)).toEqual({ planId: 'over', confirmed: true }));
    await waitFor(() => expect(cell(0, 1)).toHaveAttribute('data-status', ''));
    // The other row is still staged, and its next plan keeps the full row guard.
    expect(cell(1, 1)).toHaveAttribute('data-status', 'edited');
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview').set).toMatchObject({ overwrite: false });
    expect((lastCall('data_preview').set as { edits: unknown[] }).edits).toHaveLength(1);
  });

  it('Reload row drops that row\'s staged change and reloads', async () => {
    const { user } = await editable({
      data_preview: () => previewOf(),
      data_apply: () => reportOf({ outcome: { outcome: 'rolled_back' }, statements: [{ index: 0, affected: 0, error: null, conflict: true }] }),
    });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    await user.click(await screen.findByRole('button', { name: 'Reload row' }));
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    expect(screen.queryByRole('button', { name: 'Apply changes' })).not.toBeInTheDocument();
    expect(cell(0, 1)).toHaveAttribute('data-status', '');
  });

  it('unknown_outcome_banner', async () => {
    const { user } = await editable({
      data_preview: () => previewOf(),
      data_apply: () => reportOf({ outcome: { outcome: 'unknown', at: 1 } }),
    });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    expect(await screen.findByText('Outcome unknown: the change may have been applied. Reload to check.')).toBeInTheDocument();
    // The stage is kept.
    expect(screen.getByRole('button', { name: 'Apply changes' })).toBeInTheDocument();
    expect(cell(0, 1)).toHaveAttribute('data-status', 'edited');
    // Reloading would paint the staged values over the server's: it asks to discard them first.
    await user.click(screen.getByRole('button', { name: 'Discard staged changes and reload' }));
    expect(useDialogs.getState().question).toMatchObject({ kind: 'discardEdits', count: 1 });
    expect(callsOf('table_data')).toHaveLength(1);
  });

  it('Apply sends the rows as they were when staged, also after a reload changed or removed them', async () => {
    let version = 0;
    const { user } = await editable({
      table_data: () => {
        version++;
        // The reload brings other server values and versions, and row 2 is gone.
        return version === 1
          ? rows3()
          : page(1, 1, false, { rows: [[int(1), int(555), { t: 'Json', v: '{"x":1}' }]], row_versions: ['99'] });
      },
      data_preview: () => previewOf(),
    });
    await user.click(cell(1, 1));
    // An unguarded reload (Retry, Reconnect, Reload row of another row): the stage is untouched.
    await act(async () => {
      await useTableData.getState().load(makeTab());
    });
    await screen.findByText('1–1');
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    const edits = (lastCall('data_preview').set as { edits: { row: { key: unknown; xmin: string | null; original: unknown } }[] }).edits;
    expect(edits).toHaveLength(2);
    // Row 1: the original and xmin of the first load, not the reloaded ones.
    expect(edits[0]!.row).toMatchObject({ xmin: '5', original: [['we`ird"col', int(0)], ['doc', { t: 'Json', v: '{}' }]] });
    // Row 2 is not on the page any more and is still sent.
    expect(edits[1]!.row).toMatchObject({ key: [['id', int(2)]], xmin: '6' });
  });

  it('partially applied drops what ran: a re-Apply does not run an INSERT twice', async () => {
    const { user } = await editable(
      {
        data_preview: () => previewOf({ atomic: 'no' }),
        data_apply: () =>
          reportOf({
            outcome: { outcome: 'partially_applied', applied: 1 },
            statements: [
              { index: 0, affected: 1, error: null, conflict: false },
              { index: 1, affected: null, error: appError('server', 'boom'), conflict: false },
            ],
          }),
      },
      { stage: false },
    );
    // Staged by hand: this table is atomic for the store, the report is what matters here.
    fireEvent.keyDown(screen.getByTestId('grid'), { key: 'n', ctrlKey: true });
    await user.click(cell(0, 1));
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    // The outcome is certain: the server's error is shown, not "may have been applied".
    expect(await screen.findByText('1 change was applied before the error: boom')).toBeInTheDocument();
    expect(screen.queryByText(UNKNOWN_TEXT)).not.toBeInTheDocument();
    // The update (edit 0) ran; the INSERT (edit 1) did not: only the INSERT stays staged.
    expect(cell(0, 1)).toHaveAttribute('data-status', '');
    expect(screen.getByTestId('grid-rows')).toHaveTextContent('4');
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect((lastCall('data_preview').set as { edits: { op: string }[] }).edits.map((e) => e.op)).toEqual(['insert']);
  });

  it('a partial apply reloads the page, says how many changes ran and keeps the failed one staged', async () => {
    const { user } = await editable(
      {
        data_preview: () => previewOf({ atomic: 'no' }),
        data_apply: () =>
          reportOf({
            outcome: { outcome: 'partially_applied', applied: 1 },
            statements: [
              { index: 0, affected: 1, error: null, conflict: false },
              { index: 1, affected: null, error: appError('server', 'boom'), conflict: false },
            ],
          }),
      },
      { stage: false },
    );
    fireEvent.keyDown(screen.getByTestId('grid'), { key: 'n', ctrlKey: true });
    await user.click(cell(0, 1));
    const before = callsOf('table_data').length;
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    expect(await screen.findByText('1 change was applied before the error: boom')).toBeInTheDocument();
    await waitFor(() => expect(callsOf('table_data').length).toBe(before + 1));
    // The failed INSERT stays staged.
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect((lastCall('data_preview').set as { edits: { op: string }[] }).edits.map((e) => e.op)).toEqual(['insert']);
  });

  it('a partial apply that ran nothing does not reload', async () => {
    const { user } = await editable(
      {
        data_preview: () => previewOf({ atomic: 'no' }),
        data_apply: () =>
          reportOf({
            outcome: { outcome: 'partially_applied', applied: 0 },
            statements: [{ index: 0, affected: null, error: appError('server', 'boom'), conflict: false }],
          }),
      },
      { info: editableInfo({ atomic: 'no' }) },
    );
    const before = callsOf('table_data').length;
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    expect(await screen.findByText('boom')).toBeInTheDocument();
    expect(callsOf('table_data')).toHaveLength(before);
  });

  it('a conflict on a table without transactions marks the row; no unknown banner', async () => {
    const { user } = await editable(
      {
        data_preview: () => previewOf({ atomic: 'no' }),
        data_apply: () =>
          reportOf({ outcome: { outcome: 'partially_applied', applied: 0 }, statements: [{ index: 0, affected: 0, error: null, conflict: true }] }),
      },
      { info: editableInfo({ atomic: 'no' }) },
    );
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(cell(0, 1)).toHaveAttribute('data-status', 'conflict'));
    expect(screen.getByRole('button', { name: 'Reload row' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Overwrite' })).toBeInTheDocument();
    expect(screen.queryByText(UNKNOWN_TEXT)).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Apply changes' })).toBeInTheDocument();
  });

  it('a server error on a table without transactions is shown; no unknown banner', async () => {
    const { user } = await editable(
      {
        data_preview: () => previewOf({ atomic: 'no' }),
        data_apply: () =>
          reportOf({
            outcome: { outcome: 'partially_applied', applied: 0 },
            statements: [{ index: 0, affected: null, error: appError('server', "Duplicate entry '1' for key 'PRIMARY'"), conflict: false }],
          }),
      },
      { info: editableInfo({ atomic: 'no' }) },
    );
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    expect(await screen.findByText("Duplicate entry '1' for key 'PRIMARY'")).toBeInTheDocument();
    expect(screen.queryByText(UNKNOWN_TEXT)).not.toBeInTheDocument();
    // Nothing ran: the change stays staged.
    expect(cell(0, 1)).toHaveAttribute('data-status', 'edited');
  });

  it('an Overwrite that commits clears an earlier unknown outcome', async () => {
    const outcomes: ApplyReport[] = [
      reportOf({ outcome: { outcome: 'rolled_back' }, statements: [{ index: 0, affected: 0, error: null, conflict: true }] }),
      reportOf({ outcome: { outcome: 'unknown', at: 0 } }),
      reportOf(),
    ];
    const { user } = await editable({
      data_preview: (a) => previewOf({ plan_id: (a.set as { overwrite: boolean }).overwrite ? 'over' : 'p1' }),
      data_apply: () => outcomes.shift()!,
    });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    await screen.findByRole('button', { name: 'Overwrite' });
    await user.click(screen.getByRole('button', { name: 'Apply changes' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    expect(await screen.findByText(UNKNOWN_TEXT)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Overwrite' }));
    await user.click(await screen.findByRole('button', { name: 'Confirm' }));
    await waitFor(() => expect(callsOf('data_apply')).toHaveLength(3));
    await waitFor(() => expect(screen.queryByText(UNKNOWN_TEXT)).not.toBeInTheDocument());
  });

  it("a failed data_editable says so and stays read-only", async () => {
    setup({
      table_data: () => rows3(),
      data_editable: () => {
        throw appError('server', 'permission denied for table orders');
      },
    });
    expect(await screen.findByText("Couldn't check whether this table is editable: permission denied for table orders")).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add row' })).not.toBeInTheDocument();
  });

  it('a table that cannot roll back stages one change at a time', async () => {
    const { user } = await editable({}, { info: editableInfo({ atomic: 'no' }) });
    await user.click(cell(1, 1));
    expect(await screen.findByText(ONE_AT_A_TIME)).toBeInTheDocument();
    expect(cell(1, 1)).toHaveAttribute('data-status', '');
  });

  it('keys: Ctrl+N adds a row, Ctrl+Z undoes, Ctrl+0 sets NULL, Ctrl+Delete marks the row', async () => {
    const { user } = await editable({ data_preview: () => previewOf() }, { stage: false });
    const grid = screen.getByTestId('grid');
    fireEvent.keyDown(grid, { key: 'n', ctrlKey: true });
    expect(screen.getByTestId('grid-rows')).toHaveTextContent('4');
    expect(cell(3, 1)).toHaveAttribute('data-status', 'new');
    fireEvent.keyDown(grid, { key: 'z', ctrlKey: true });
    expect(screen.getByTestId('grid-rows')).toHaveTextContent('3');

    fireEvent.keyDown(grid, { key: '0', ctrlKey: true });
    expect(cell(0, 1)).toHaveAttribute('data-status', 'edited');
    fireEvent.keyDown(grid, { key: 'Delete', ctrlKey: true });
    expect(cell(0, 1)).toHaveAttribute('data-status', 'deleted');
    // A row marked for deletion takes no more edits.
    expect(cell(0, 1)).toHaveAttribute('data-editable', 'false');
    fireEvent.keyDown(grid, { key: 's', ctrlKey: true });
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview')).toMatchObject({ set: { edits: [{ op: 'delete' }] } });
    void user;
  });

  it('Ctrl+0 stages a NULL', async () => {
    await editable({ data_preview: () => previewOf() }, { stage: false });
    fireEvent.keyDown(screen.getByTestId('grid'), { key: '0', ctrlKey: true });
    fireEvent.keyDown(screen.getByTestId('grid'), { key: 's', ctrlKey: true });
    await screen.findByRole('dialog', { name: 'Review changes' });
    expect(lastCall('data_preview')).toMatchObject({ set: { edits: [{ op: 'update', set: [['we`ird"col', { kind: 'null' }]] }] } });
  });

  it('closing_tab_with_staged_edits_asks', async () => {
    const { tab } = await editable();
    const closing = closeTab(tab);
    const question = useDialogs.getState().question;
    expect(question).toMatchObject({ kind: 'discardEdits', count: 1 });
    if (question?.kind !== 'discardEdits') return;
    question.done(false);
    useDialogs.getState().answer();
    expect(await closing).toBe(false);
    expect(useTabs.getState().tabs.some((t) => t.id === tab.id)).toBe(true);
    expect(useDataEdits.getState().hasStaged(tab.id)).toBe(true);

    const again = closeTab(tab);
    const second = useDialogs.getState().question;
    if (second?.kind !== 'discardEdits') throw new Error('expected a question');
    second.done(true);
    expect(await again).toBe(true);
    expect(useTabs.getState().tabs.some((t) => t.id === tab.id)).toBe(false);
  });

  it('changing the page with staged edits asks first; the question reads "Discard N unsaved changes?"', async () => {
    const { user } = await editable({ table_data: () => rows3({ has_more: true }) });
    render(<DialogHost />);
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    expect(await screen.findByText('Discard 1 unsaved changes?')).toBeInTheDocument();
    expect(callsOf('table_data')).toHaveLength(1);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(callsOf('table_data')).toHaveLength(1);
    expect(useDataEdits.getState().hasStaged(makeTab().id)).toBe(true);
    await user.click(screen.getByRole('button', { name: 'Next page' }));
    await user.click(await screen.findByRole('button', { name: 'Discard' }));
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    expect(useDataEdits.getState().hasStaged(makeTab().id)).toBe(false);
  });

  it('a raw WHERE is sent as raw_where and its config error shows under the bar', async () => {
    const user = userEvent.setup();
    setup({
      table_data: (a) => {
        if ((a.filter as { raw_where?: string } | null)?.raw_where) throw appError('config', 'Syntax error near "=="', { field: 'raw_where' });
        return rows3();
      },
    });
    await screen.findByText('1–3');
    await user.click(screen.getByRole('button', { name: 'Filter' }));
    const bar = screen.getByRole('search', { name: 'Filter' });
    await user.click(within(bar).getByRole('button', { name: 'Raw WHERE' }));
    await user.type(within(bar).getByRole('textbox', { name: 'Raw WHERE' }), 'id ==  1{Enter}');
    await waitFor(() => expect(lastCallData().filter).toEqual({ mode: 'and', conditions: [], raw_where: 'id ==  1' }));
    expect(await within(bar).findByRole('alert')).toHaveTextContent('Syntax error near "=="');
    expect(screen.queryByRole('button', { name: 'Retry' })).not.toBeInTheDocument();
    expect(screen.getByText('Filtered (raw)')).toBeInTheDocument();
  });
});

const lastCallData = () => callsOf('table_data').at(-1)!;