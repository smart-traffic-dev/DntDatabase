import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ObjectRef } from '../bindings';
import { TabBar } from '../components/TabBar';
import { useConnections } from '../store/connections';
import { emptyRun, startRun, useQueryRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import { callsOf, config, ids, mockBackend, resetStores } from '../test/backend';
import { ObjectTab } from './ObjectTab';
import { QueryTab } from './query/QueryTab';

/** The cluster measures its group's strip: a ResizeObserver that reports `width` once observing. */
function stubWidth(width: number) {
  class FixedWidth {
    constructor(private readonly cb: ResizeObserverCallback) {}
    observe() {
      this.cb([{ contentRect: { width } } as ResizeObserverEntry], this as unknown as ResizeObserver);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal('ResizeObserver', FixedWidth);
}

const PLAN = {
  plan_id: 'p1',
  stmts: [{ index: 0, range: [0, 8] as [number, number], kind: 'read' as const, verdict: 'allow' as const, preview: 'S0' }],
  dialect: { engine: 'mysql' as const },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle' as const,
};

const row = () => document.querySelector<HTMLElement>('.tabstrip-toolbar')!;
const cluster = () => within(row()).getByRole('toolbar');

beforeEach(() => {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
  mockBackend({ tabs_save: () => null, list_databases: () => [] });
});
afterEach(() => vi.unstubAllGlobals());

describe('tab strip actions: query tab', () => {
  it('the query controls sit at the right end of the strip, not in a row of the tab', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<TabBar />);
    const bar = cluster();
    expect(bar).toHaveAttribute('aria-label', 'Query');
    for (const testId of ['query-context', 'query-run', 'query-run-menu', 'query-explain', 'query-more']) {
      expect(within(bar).getByTestId(testId)).toBeInTheDocument();
    }
    // Icon buttons: named, with the shortcut in the tooltip.
    expect(within(bar).getByTestId('query-run')).toHaveAttribute('aria-label', 'Run on PROD');
    expect(within(bar).getByTestId('query-run')).toHaveAttribute('title', 'Run the selection, or every statement (Ctrl+R)');
    expect(within(bar).getByTestId('query-explain')).toHaveAttribute('aria-label', 'Explain');
    expect(within(bar).getByTestId('query-explain')).toHaveAttribute('title', 'Explain (Ctrl+Shift+E)');
    // The chip: environment badge and database; the connection is in the tooltip.
    const chip = within(bar).getByTestId('query-context');
    expect(chip).toHaveTextContent('PRODshop');
    expect(chip).toHaveAttribute('title', expect.stringContaining('shop-prod'));
    // The tab itself has no toolbar row any more.
    const tab = useTabs.getState().tabs.find((t) => t.id === id)!;
    const view = render(<QueryTab tab={tab as never} />);
    expect(within(view.container).queryByRole('toolbar')).toBeNull();
  });

  it('⋯ holds Format, the Limit radio list, Auto / Manual commit and Open in new group', async () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    useQueryDocs.getState().newQuery(ids.prod, 'shop');
    act(() => useTabs.getState().activate(id));
    render(<TabBar />);
    const user = userEvent.setup();
    await user.click(screen.getByTestId('query-more'));
    const menu = screen.getByRole('menu', { name: 'More query actions' });
    const labels = [...menu.querySelectorAll('[role^="menuitem"]')].map((i) => i.querySelector('.menu-label')?.textContent);
    expect(labels).toEqual([
      'Format SQL',
      'Display limit 1,000',
      'Display limit 10,000',
      'Display limit 100,000',
      'Display limit 1,000,000',
      'Auto commit',
      'Manual commit',
      'Open in new group',
    ]);
    expect(within(menu).getByTestId('query-row-limit-10000')).toHaveAttribute('aria-checked', 'true');
    expect(within(menu).getByTestId('query-tx-auto')).toHaveAttribute('aria-checked', 'true');
    await user.click(within(menu).getByTestId('query-row-limit-1000'));
    expect(useQueryDocs.getState().docs.get(id)!.rowLimit).toBe(1000);
    await user.click(screen.getByTestId('query-more'));
    await user.click(screen.getByTestId('query-open-in-group'));
    expect(useTabs.getState().groups).toHaveLength(2);
    expect(useTabs.getState().groupOf[id]).toBe(useTabs.getState().groups[1]!.id);
  });

  it('a run puts Stop in the Run slot with the elapsed time beside it', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<TabBar />);
    const run = screen.getByTestId('query-run');
    act(() => useQueryRun.setState({ runs: new Map([[id, startRun(emptyRun, PLAN as never, Date.now() - 12_000)]]) }));
    const stop = screen.getByTestId('query-stop');
    expect(stop).toBe(run);
    expect(stop).toHaveAttribute('aria-label', 'Stop');
    expect(stop).toHaveAttribute('title', 'Stop (Esc)');
    expect(screen.getByTestId('query-elapsed')).toHaveTextContent('00:12');
  });

  it('under 360 px the cluster keeps the chip badge, Run / Stop and ⋯; Explain goes into ⋯', async () => {
    stubWidth(320);
    useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<TabBar />);
    const bar = cluster();
    expect(within(bar).getByTestId('query-context')).toHaveTextContent(/^PROD$/);
    expect(within(bar).getByTestId('query-run')).toBeInTheDocument();
    expect(within(bar).queryByTestId('query-explain')).toBeNull();
    await userEvent.setup().click(within(bar).getByTestId('query-more'));
    expect(screen.getByTestId('query-more-explain')).toBeInTheDocument();
  });
});

describe('tab strip actions: object tab', () => {
  const orders: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
  const tab: ObjectTabRef = { id: objectTabId(ids.prod, orders), kind: 'object', connectionId: ids.prod, objectRef: orders, title: 'orders' };
  const page = {
    columns: [{ name: 'id', db_type: 'int', logical: 'Integer', nullable: false, origin_table: null, origin_column: null }],
    rows: [[{ t: 'Int', v: 1 }]],
    has_more: false,
    truncated_cells: 0,
    truncated: [],
    size_capped: false,
    key_order: [],
    row_versions: null,
    hidden_key: [],
    hidden_key_values: [],
    elapsed_ms: 1,
  };

  beforeEach(() => {
    mockBackend({
      tabs_save: () => null,
      get_table_page_size: () => 100,
      table_data: () => page,
      table_detail: () => ({ row_estimate: 3412 }) as never,
      data_editable: () => ({ editable: false, reason: 'no_key', key: [], hidden_key: [], editable_columns: [], atomic: 'yes', protection: 'snapshot' }),
      refresh_node: () => null,
      table_data_cancel: () => true,
      table_row_count: () => ({ rows: 1, elapsed_ms: 1 }),
    });
    useTabs.getState().open(tab);
  });

  it('context chip, Filter, Refresh and ⋯ (Export, Open in query, page size, count rows)', async () => {
    render(
      <>
        <TabBar />
        <ObjectTab tab={tab} />
      </>,
    );
    expect(await screen.findByText('1–1')).toBeInTheDocument();
    const bar = cluster();
    expect(bar).toHaveAttribute('aria-label', 'Object');
    expect(within(bar).getByTestId('object-context')).toHaveTextContent('PRODshop');
    expect(within(bar).getByRole('button', { name: 'Filter' })).toHaveAttribute('title', 'Filter rows (F3)');
    expect(within(bar).getByRole('button', { name: 'Refresh' })).toHaveAttribute('title');
    const user = userEvent.setup();
    await user.click(within(bar).getByRole('button', { name: 'More data actions' }));
    const menu = screen.getByRole('menu', { name: 'Data actions' });
    const labels = [...menu.querySelectorAll('[role^="menuitem"]')].map((i) => i.querySelector('.menu-label')?.textContent);
    expect(labels).toEqual(['Export…', 'Open in query', '100 rows per page', '500 rows per page', '1,000 rows per page', 'Count rows']);
    await user.click(within(menu).getByRole('menuitem', { name: 'Count rows' }));
    await waitFor(() => expect(callsOf('table_row_count')).toHaveLength(1));
  });

  it('the sub-tabs and the pager share one row; the tab has no header row', async () => {
    render(<ObjectTab tab={tab} />);
    expect(await screen.findByText('1–1')).toBeInTheDocument();
    expect(screen.queryByRole('banner')).toBeNull();
    const row = screen.getByRole('tablist', { name: 'Object sections' }).closest('.ot-subbar')!;
    expect(row).toContainElement(screen.getByTestId('data-pager'));
    expect(row).toContainElement(screen.getByRole('button', { name: 'Next page' }));
  });
});
