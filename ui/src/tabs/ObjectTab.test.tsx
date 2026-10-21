import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ObjectKind, ObjectRef, TableDetail } from '../bindings';
import { useDialogs } from '../store/dialogs';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs, type ObjectTabRef as Tab } from '../store/tabs';
import { backendError, callsOf, config, fail, ids, mockBackend, resetStores, sampleTree, serverInfo } from '../test/backend';
import { useObjectData } from '../store/objectData';
import { openObjectTab } from '../components/openObject';
import { ObjectActions } from './ObjectActions';
import { ObjectTab, formatBytes } from './ObjectTab';

const ref = (kind: ObjectKind, name = 'orders', schema: string | null = null): ObjectRef => ({
  database: 'shop',
  schema,
  name,
  kind,
});

function makeTab(kind: ObjectKind, name = 'orders', schema: string | null = null): Tab {
  const objectRef = ref(kind, name, schema);
  return { id: objectTabId(ids.prod, objectRef), kind: 'object', connectionId: ids.prod, objectRef, title: name };
}

function detail(over: Partial<TableDetail> = {}): TableDetail {
  return {
    object: ref('Table'),
    columns: [
      { name: 'id', ordinal: 1, db_type: 'bigint', nullable: false, default: null, auto_increment: true, identity: null, generated: null, invisible: false, collation: null, comment: 'surrogate key' },
      { name: 'status', ordinal: 2, db_type: 'varchar(20)', nullable: true, default: "'new'", auto_increment: false, identity: null, generated: null, invisible: false, collation: null, comment: null },
      { name: 'total', ordinal: 3, db_type: 'decimal(10,2)', nullable: false, default: 'NULL', auto_increment: false, identity: null, generated: 'STORED', invisible: true, collation: null, comment: null },
    ],
    primary_key: { name: 'PRIMARY', unique: true, primary: true, partial: false, columns: ['id'], expression: null, method: 'BTREE', definition: null },
    indexes: [
      { name: 'PRIMARY', unique: true, primary: true, partial: false, columns: ['id'], expression: null, method: 'BTREE', definition: null },
      { name: 'idx_status', unique: false, primary: false, partial: false, columns: ['status'], expression: null, method: 'BTREE', definition: null },
    ],
    foreign_keys: [
      { name: 'fk_customer', columns: ['customer_id'], ref_schema: 'crm', ref_table: 'customers', ref_columns: ['id'], on_update: 'CASCADE', on_delete: 'SET NULL', definition: null },
    ],
    checks: [{ name: 'chk_total', expression: '(total >= 0)', definition: null }],
    triggers: [{ name: 'trg_audit', timing: 'AFTER', event: 'INSERT' }],
    options: { engine: 'InnoDB', collation: 'utf8mb4_0900_ai_ci', tablespace: null, partitioned_by: 'HASH(id)', system_versioned: true },
    row_estimate: 48213,
    size_bytes: 1536,
    comment: 'Customer orders',
    ...over,
  };
}

const DDL = 'CREATE TABLE `orders` (\n  `id` bigint NOT NULL\n)';

/**
 * Renders the tab. Tables and views open on Data; these tests start them on Info unless
 * `onData` (the Data view has its own tests, in DataView.test.tsx).
 */
function setup(tab: Tab, handlers: Record<string, (a: Record<string, unknown>) => unknown> = {}, onData = false) {
  mockBackend({
    table_detail: () => detail(),
    object_ddl: () => DDL,
    refresh_node: () => null,
    get_table_page_size: () => 100,
    table_data: () => ({ columns: [], rows: [], has_more: false, truncated_cells: 0, size_capped: false, key_order: [], elapsed_ms: 1 }),
    ...handlers,
  });
  useTabs.getState().open(tab);
  if (!onData && tab.objectRef.kind !== 'Function') useObjectData.getState().setSub(tab.id, 'info');
  return render(withActions(tab));
}

/** The tab with its actions (in the app they sit at the right end of the group's tab strip). */
function withActions(tab: Tab) {
  return (
    <>
      <ObjectActions tab={tab} />
      <ObjectTab tab={tab} />
    </>
  );
}

/** "Open in query" of the strip's `⋯` (Data). */
async function openInQuery(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole('button', { name: 'More data actions' }));
  await user.click(screen.getByRole('menuitem', { name: 'Open in query' }));
}

beforeEach(() => {
  resetStores();
});

describe('ObjectTab', () => {
  it('table_shows_three_subtabs_and_columns', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'));
    const tabs = screen.getAllByRole('tab').map((t) => t.textContent);
    expect(tabs).toEqual(['Data', 'Info', 'DDL', 'Structure']);
    expect(screen.getByRole('tab', { name: 'Info' })).toHaveAttribute('aria-selected', 'true');

    expect(await screen.findByText('Customer orders')).toBeInTheDocument();
    expect(screen.getByText('≈ 48,213')).toBeInTheDocument();
    expect(screen.getByText('InnoDB')).toBeInTheDocument();
    expect(screen.getByText('HASH(id)')).toBeInTheDocument();
    // DDL is lazy: not fetched until its sub-tab opens.
    expect(callsOf('object_ddl')).toHaveLength(0);

    await user.click(screen.getByRole('tab', { name: 'Structure' }));
    const columns = within(await screen.findByRole('region', { name: 'Columns' }));
    const rows = columns.getAllByRole('row');
    expect(rows).toHaveLength(4);
    const cells = (row: HTMLElement) => within(row).getAllByRole('cell').map((c) => c.textContent);
    expect(cells(rows[1]!)).toEqual(['id', 'bigint', 'NO', '—', 'auto_increment', 'surrogate key']);
    expect(cells(rows[2]!)).toEqual(['status', 'varchar(20)', 'YES', "'new'", '', '']);
    expect(cells(rows[3]!)).toEqual(['total', 'decimal(10,2)', 'NO', '—', 'generated (STORED), invisible', '']);

    const indexRows = within(screen.getByRole('region', { name: 'Indexes' })).getAllByRole('row');
    expect(indexRows).toHaveLength(3); // header + PK + idx_status (PRIMARY not duplicated)
    expect(within(indexRows[1]!).getByText('PK')).toBeInTheDocument();
    expect(cells(indexRows[2]!)).toEqual(['idx_status', 'status', 'NO', 'BTREE']);
    const fkRows = within(screen.getByRole('region', { name: 'Foreign keys' })).getAllByRole('row');
    expect(cells(fkRows[1]!)).toEqual(['fk_customer', 'customer_id → crm.customers(id)', 'CASCADE', 'SET NULL']);
    expect(screen.getByText('(total >= 0)')).toBeInTheDocument();
    expect(screen.getByText('trg_audit')).toBeInTheDocument();
    // Info and Structure share one detail fetch.
    expect(callsOf('table_detail')).toHaveLength(1);
  });

  it('materialized_view_has_data_and_three_subtabs', () => {
    setup(makeTab('MaterializedView', 'mv_sales', 'public'));
    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['Data', 'Info', 'DDL', 'Structure']);
  });

  it.each(['Table', 'View', 'MaterializedView'] as const)('%s_opened_from_the_tree_shows_data_first', async (kind) => {
    mockBackend({
      table_detail: () => detail({ row_estimate: 12345 }),
      get_table_page_size: () => 250,
      table_data: () => ({
        columns: [{ name: 'id', db_type: 'int', logical: 'Integer', nullable: false, origin_table: null, origin_column: null }],
        rows: [[{ t: 'Int', v: 1 }]],
        has_more: false,
        truncated_cells: 0,
        size_capped: false,
        key_order: [],
        elapsed_ms: 3,
      }),
    });
    const objectRef = ref(kind, 'orders', kind === 'Table' ? null : 'public');
    // What the tree's double-click (and Enter / "Open") does.
    openObjectTab({ id: `conn:${ids.prod}/db:shop/obj`, kind: 'object', label: 'orders', objectRef });
    const tab = useTabs.getState().tabs.at(-1) as Tab;
    render(<ObjectTab tab={tab} />);
    expect(screen.getAllByRole('tab')[0]).toHaveTextContent('Data');
    expect(screen.getByRole('tab', { name: 'Data' })).toHaveAttribute('aria-selected', 'true');
    expect(await screen.findByText('1–1')).toBeInTheDocument();
    // The Settings default is the page size.
    expect(callsOf('table_data')).toEqual([{ id: ids.prod, obj: objectRef, limit: 250, offset: 0, sort: [], filter: null, requestKey: `${tab.id}:page` }]);
    // The detail's row estimate is shown next to the page.
    expect(await screen.findByTestId('data-total')).toHaveTextContent('~12,345');
  });

  it('view_has_info_ddl_and_structure', async () => {
    const user = userEvent.setup();
    // Views have no row estimate, size, engine or indexes.
    const view = detail({
      object: ref('View', 'v_open'),
      row_estimate: null,
      size_bytes: null,
      options: { engine: null, collation: null, tablespace: null, partitioned_by: null, system_versioned: false },
      primary_key: null,
      indexes: [],
      foreign_keys: [],
      checks: [],
      triggers: [],
      comment: null,
    });
    setup(makeTab('View', 'v_open'), { table_detail: () => view });
    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['Data', 'Info', 'DDL', 'Structure']);
    // Info of a view lists what a view has (columns, comment), not a column of dashes.
    const cols = await screen.findByText('Columns');
    expect(cols.closest('.ot-info-row')).toHaveTextContent(String(view.columns.length));
    for (const gone of ['Row estimate', 'Size', 'Engine', 'Tablespace', 'Partitioned by', 'System versioned']) {
      expect(screen.queryByText(gone)).toBeNull();
    }
    expect(callsOf('table_detail')[0]).toEqual({ id: ids.prod, obj: ref('View', 'v_open') });
    await user.click(screen.getByRole('tab', { name: 'Structure' }));
    expect(await screen.findByRole('region', { name: 'Columns' })).toBeInTheDocument();
    await user.click(screen.getByTestId('object-tab-ddl'));
    expect(await screen.findByTestId('monaco-mock')).toHaveValue(DDL);
    expect(callsOf('object_ddl')[0]).toEqual({ id: ids.prod, obj: ref('View', 'v_open') });
  });

  it('no header row: the sections lead the sub-tab row; the chip names the connection and path', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    const { container } = setup(makeTab('Table'));
    expect(screen.queryByRole('banner')).toBeNull();
    const tab = container.querySelector('.object-tab')!;
    expect(tab.firstElementChild).toHaveClass('ot-subbar');
    expect(within(tab as HTMLElement).getByRole('tablist', { name: 'Object sections' })).toBeInTheDocument();
    const chip = screen.getByTestId('object-context');
    expect(chip).toHaveTextContent('shop');
    expect(chip).toHaveAttribute('title', expect.stringContaining('shop-prod'));
    expect(chip).toHaveAttribute('title', expect.stringContaining('shop › orders'));
    expect(within(chip).queryByRole('img', { name: 'Read-only' })).toBeNull();
    // No duplicate actions: Query is "Open in query" on Data.
    expect(screen.queryByRole('button', { name: 'Query' })).toBeNull();
  });

  it('a schema joins the path; a read-only connection shows a lock with how to change it', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { read_only: true })]);
    setup(makeTab('Table', 'orders', 'sales'));
    const chip = screen.getByTestId('object-context');
    expect(chip).toHaveAttribute('title', expect.stringContaining('shop › sales › orders'));
    expect(within(chip).getByRole('img', { name: 'Read-only' })).toBeInTheDocument();
    expect(chip).toHaveAttribute('title', expect.stringContaining('This connection is read-only. Change it in Edit connection.'));
  });

  it('open_in_query_on_data_opens_a_select_in_a_new_tab', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'), {}, true);
    await openInQuery(user);
    const doc = [...useQueryDocs.getState().docs.values()].at(-1)!;
    expect(doc).toMatchObject({ connectionId: ids.prod, database: 'shop', sql: 'SELECT * FROM `shop`.`orders` LIMIT 100;' });
    expect(useTabs.getState().activeId).toBe(doc.id);
  });

  it.each([
    // The same quoting as the tree's "Select top 1000" (one helper: components/newQuery.ts).
    ['postgres schema-qualified', 'postgres', 'orders', 'sales', 'SELECT * FROM "sales"."orders" LIMIT 100;'],
    ['quote in a postgres name', 'postgres', 'we"ird', 'public', 'SELECT * FROM "public"."we""ird" LIMIT 100;'],
    ['backtick in a mysql name', 'mysql', 'we`ird', null, 'SELECT * FROM `shop`.`we``ird` LIMIT 100;'],
    ['vietnamese name', 'mysql', 'đơn_hàng', null, 'SELECT * FROM `shop`.`đơn_hàng` LIMIT 100;'],
  ])('open_in_query_quotes_%s', async (_label, kind, name, schema, sql) => {
    const user = userEvent.setup();
    useConnections.getState().setStored([], [config(ids.prod, 'c', { kind: kind as 'mysql' | 'postgres' })]);
    setup(makeTab('Table', name, schema), {}, true);
    await openInQuery(user);
    expect([...useQueryDocs.getState().docs.values()].at(-1)!.sql).toBe(sql);
  });
  it('functions_have_no_query_button_and_refresh_in_the_strip_actions', async () => {
    setup(makeTab('Function', 'fn'));
    await screen.findByTestId('monaco-mock');
    expect(screen.queryByRole('button', { name: 'Query' })).toBeNull();
    // The DDL bar keeps its own actions; Refresh is the strip's one icon button.
    expect(within(screen.getByRole('toolbar', { name: 'DDL' })).queryByRole('button', { name: 'Refresh' })).toBeNull();
    const actions = screen.getByRole('toolbar', { name: 'Object' });
    expect(within(actions).getAllByRole('button').map((b) => b.getAttribute('aria-label'))).toEqual(['Refresh']);
    expect(within(actions).getByRole('button', { name: 'Refresh' })).toHaveAttribute('title', 'Reload this section');
  });

  it.each(['Info', 'Structure'] as const)('%s_has_one_refresh_in_the_strip_actions', async (section) => {
    const user = userEvent.setup();
    setup(makeTab('Table'));
    await user.click(screen.getByRole('tab', { name: section }));
    const actions = screen.getByRole('toolbar', { name: 'Object' });
    expect(within(actions).getAllByRole('button').map((b) => b.getAttribute('aria-label'))).toEqual(['Refresh']);
    expect(screen.getAllByRole('button', { name: 'Refresh' })).toHaveLength(1);
  });

  it('data_refresh_reloads_the_rows_and_the_metadata', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'), {}, true);
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(1));
    await waitFor(() => expect(callsOf('table_detail')).toHaveLength(1));
    const bar = screen.getByRole('toolbar', { name: 'Object' });
    await user.click(within(bar).getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(callsOf('table_data')).toHaveLength(2));
    await waitFor(() => expect(callsOf('table_detail')).toHaveLength(2));
    expect(callsOf('refresh_node')).toHaveLength(1);
    expect(screen.getAllByRole('button', { name: 'Refresh' })).toHaveLength(1);
  });
  it('open_in_editor_puts_the_ddl_in_a_new_query_tab', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'));
    await user.click(screen.getByTestId('object-tab-ddl'));
    await screen.findByTestId('monaco-mock');
    await user.click(screen.getByRole('button', { name: 'Open in editor' }));
    const doc = [...useQueryDocs.getState().docs.values()].at(-1)!;
    expect(doc).toMatchObject({ connectionId: ids.prod, database: 'shop', sql: DDL });
  });
  it('copy_ddl_writes_clipboard', async () => {
    const user = userEvent.setup();
    const write = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    setup(makeTab('Table'));
    await user.click(screen.getByTestId('object-tab-ddl'));
    expect(screen.getByRole('tab', { name: 'DDL' })).toHaveAttribute('aria-selected', 'true');
    const pre = await screen.findByTestId('monaco-mock');
    expect(pre).toHaveValue(DDL);
    await user.click(screen.getByRole('button', { name: 'Copy' }));
    expect(write).toHaveBeenCalledWith(DDL);
    expect(await screen.findByRole('button', { name: 'Copied' })).toBeInTheDocument();
  });

  it('copy_failure_is_shown_and_cleared_on_next_success', async () => {
    const user = userEvent.setup();
    const write = vi.spyOn(navigator.clipboard, 'writeText').mockRejectedValueOnce(new Error('denied'));
    // A function opens on its only sub-tab, DDL.
    setup(makeTab('Function', 'f_add'));
    await screen.findByTestId('monaco-mock');
    await user.click(screen.getByRole('button', { name: 'Copy' }));
    expect(await screen.findByRole('alert')).toHaveTextContent("Couldn't copy — select the text and press Ctrl+C.");
    expect(screen.queryByRole('button', { name: 'Copied' })).not.toBeInTheDocument();
    write.mockResolvedValueOnce();
    await user.click(screen.getByRole('button', { name: 'Copy' }));
    expect(await screen.findByRole('button', { name: 'Copied' })).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('refresh_forgets_ddl_cache', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'));
    await screen.findByText('Customer orders');
    await user.click(screen.getByTestId('object-tab-ddl'));
    await screen.findByTestId('monaco-mock');
    expect(callsOf('object_ddl')).toHaveLength(1);
    await user.click(screen.getByRole('tab', { name: 'Info' }));
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(callsOf('table_detail')).toHaveLength(2));
    await user.click(screen.getByTestId('object-tab-ddl'));
    await screen.findByTestId('monaco-mock');
    expect(callsOf('object_ddl')).toHaveLength(2);
  });

  it('size_is_humanized', async () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(5_368_709_120)).toBe('5.0 GB');
    expect(formatBytes(1024 * 1024 - 1)).toBe('1.0 MB');
    setup(makeTab('Table'), { table_detail: () => detail({ size_bytes: 5_368_709_120 }) });
    expect(await screen.findByText('5.0 GB')).toBeInTheDocument();
  });

  it('null_row_estimate_and_size_show_dash', async () => {
    setup(makeTab('Table'), { table_detail: () => detail({ row_estimate: null, size_bytes: null }) });
    const row = (await screen.findByText('Row estimate')).parentElement!;
    expect(within(row).getByText('—')).toBeInTheDocument();
    expect(within(screen.getByText('Size').parentElement!).getByText('—')).toBeInTheDocument();
  });

  it('ddl_error_is_shown_inline', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'), {
      object_ddl: () => {
        throw fail('server_db_missing');
      },
    });
    expect(await screen.findByText('Customer orders')).toBeInTheDocument();
    await user.click(screen.getByTestId('object-tab-ddl'));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent("Unknown database 'shop'");
    expect(alert).toHaveTextContent('Database does not exist.');
    expect(within(alert).getByText('Details')).toBeInTheDocument();
    // The other sub-tabs keep working.
    await user.click(screen.getByRole('tab', { name: 'Info' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByText('Customer orders')).toBeInTheDocument();
  });

  it.each(['not_open', 'connection_lost'] as const)('%s_offers_reconnect_then_reloads', async (kind) => {
    const user = userEvent.setup();
    let attempts = 0;
    setup(makeTab('Table'), {
      table_detail: () => {
        if (attempts++ === 0) throw fail(kind);
        return detail();
      },
      open_connection: () => ({ info: serverInfo('MySql8'), timings: [] }),
      list_tree: () => sampleTree(),
    });
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(backendError(kind).message);
    expect(within(alert).queryByRole('button', { name: 'Retry' })).not.toBeInTheDocument();
    await user.click(within(alert).getByRole('button', { name: 'Reconnect' }));
    expect(await screen.findByText('Customer orders')).toBeInTheDocument();
    expect(callsOf('open_connection')).toEqual([{ id: ids.prod, prompt: null }]);
    expect(callsOf('table_detail')).toHaveLength(2);
  });

  it('reconnect_needing_a_password_opens_the_prompt', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'), {
      table_detail: () => {
        throw fail('not_open');
      },
      open_connection: () => {
        throw fail('password_required');
      },
    });
    await user.click(await screen.findByRole('button', { name: 'Reconnect' }));
    await waitFor(() => expect(useDialogs.getState().prompt).toMatchObject({ kind: 'password', connectionId: ids.prod }));
  });

  it('cancelled_is_not_shown_as_an_error', async () => {
    setup(makeTab('Table'), {
      table_detail: () => {
        throw fail('cancelled');
      },
    });
    await waitFor(() => expect(callsOf('table_detail')).toHaveLength(1));
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('arrow_keys_switch_subtabs', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'));
    await screen.findByText('Customer orders');
    screen.getByRole('tab', { name: 'Info' }).focus();
    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('tab', { name: 'DDL' })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tab', { name: 'DDL' })).toHaveFocus();
    await user.keyboard('{End}');
    expect(screen.getByRole('tab', { name: 'Structure' })).toHaveFocus();
    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('tab', { name: 'Data' })).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{ArrowLeft}');
    expect(screen.getByRole('tab', { name: 'Structure' })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tabpanel')).toHaveAccessibleName('Structure');
  });

  it('refresh_drops_backend_cache_with_object_key_and_refetches', async () => {
    const user = userEvent.setup();
    let size = 1536;
    setup(makeTab('Table', 'a/b', 'public'), { table_detail: () => detail({ size_bytes: size }) });
    expect(await screen.findByText('1.5 KB')).toBeInTheDocument();
    size = 2048;
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    expect(await screen.findByText('2.0 KB')).toBeInTheDocument();
    expect(callsOf('refresh_node')).toEqual([
      { id: ids.prod, key: ['db:shop', 'schema:public', 'folder:Table', 'obj:Table:a/b'] },
    ]);
    expect(callsOf('table_detail')).toHaveLength(2);
  });

  it('result_after_tab_close_is_ignored', async () => {
    let release: (d: TableDetail) => void = () => undefined;
    const pending = new Promise<TableDetail>((resolve) => (release = resolve));
    const tab = makeTab('Table');
    setup(tab, { table_detail: () => pending });
    await waitFor(() => expect(callsOf('table_detail')).toHaveLength(1));
    act(() => useTabs.getState().close(tab.id));
    await act(async () => release(detail()));
    const { useObjectData } = await import('../store/objectData');
    expect(useObjectData.getState().byTab[tab.id]).toBeUndefined();
  });

  it('refresh_shows_refreshing_and_is_disabled_until_the_section_loads', async () => {
    const user = userEvent.setup();
    let release: (d: TableDetail) => void = () => undefined;
    let calls = 0;
    setup(makeTab('Table'), {
      table_detail: () => (++calls === 1 ? detail() : new Promise<TableDetail>((resolve) => (release = resolve))),
    });
    await screen.findByText('Customer orders');
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    const busy = await screen.findByRole('button', { name: 'Refreshing…' });
    expect(busy).toBeDisabled();
    expect(busy).toHaveAttribute('aria-busy', 'true');
    await act(async () => release(detail()));
    expect(await screen.findByRole('button', { name: 'Refresh' })).toBeEnabled();
  });

  it('ddl_wrap_toggles_line_wrapping', async () => {
    const user = userEvent.setup();
    const tab = makeTab('Table');
    setup(tab);
    await user.click(screen.getByRole('tab', { name: 'DDL' }));
    const editor = await screen.findByTestId('monaco-mock');
    expect(editor).toHaveAttribute('data-word-wrap', 'off');
    const wrap = screen.getByRole('button', { name: 'Wrap' });
    expect(wrap).toHaveAttribute('aria-pressed', 'false');
    await user.click(wrap);
    expect(wrap).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-word-wrap', 'on');
  });

  it('cached_across_remount_for_tab_lifetime', async () => {
    const tab = makeTab('Table');
    const { unmount } = setup(tab);
    await screen.findByText('Customer orders');
    unmount();
    render(<ObjectTab tab={tab} />);
    expect(await screen.findByText('Customer orders')).toBeInTheDocument();
    expect(callsOf('table_detail')).toHaveLength(1);
  });
});
