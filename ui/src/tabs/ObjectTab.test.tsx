import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ObjectKind, ObjectRef, TableDetail } from '../bindings';
import { objectTabId, useTabs, type Tab } from '../store/tabs';
import { Failure, appError, callsOf, ids, mockBackend, resetStores } from '../test/backend';
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
      { name: 'id', ordinal: 1, db_type: 'bigint', nullable: false, default: null, auto_increment: true, identity: null, generated: null, invisible: false, comment: 'surrogate key' },
      { name: 'status', ordinal: 2, db_type: 'varchar(20)', nullable: true, default: "'new'", auto_increment: false, identity: null, generated: null, invisible: false, comment: null },
      { name: 'total', ordinal: 3, db_type: 'decimal(10,2)', nullable: false, default: 'NULL', auto_increment: false, identity: null, generated: 'STORED', invisible: true, comment: null },
    ],
    primary_key: { name: 'PRIMARY', unique: true, primary: true, columns: ['id'], expression: null, method: 'BTREE', definition: null },
    indexes: [
      { name: 'PRIMARY', unique: true, primary: true, columns: ['id'], expression: null, method: 'BTREE', definition: null },
      { name: 'idx_status', unique: false, primary: false, columns: ['status'], expression: null, method: 'BTREE', definition: null },
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

function setup(tab: Tab, handlers: Record<string, (a: Record<string, unknown>) => unknown> = {}) {
  mockBackend({
    table_detail: () => detail(),
    object_ddl: () => DDL,
    refresh_node: () => null,
    ...handlers,
  });
  useTabs.getState().open(tab);
  return render(<ObjectTab tab={tab} />);
}

beforeEach(() => {
  resetStores();
});

describe('ObjectTab', () => {
  it('table_shows_three_subtabs_and_columns', async () => {
    const user = userEvent.setup();
    setup(makeTab('Table'));
    const tabs = screen.getAllByRole('tab').map((t) => t.textContent);
    expect(tabs).toEqual(['Info', 'DDL', 'Structure']);
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

  it('materialized_view_has_three_subtabs', () => {
    setup(makeTab('MaterializedView', 'mv_sales', 'public'));
    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['Info', 'DDL', 'Structure']);
  });

  it('view_shows_only_ddl', async () => {
    setup(makeTab('View', 'v_open'));
    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['DDL']);
    expect(await screen.findByTestId('object-ddl-text')).toHaveTextContent('CREATE TABLE');
    expect(callsOf('table_detail')).toHaveLength(0);
    expect(callsOf('object_ddl')[0]).toEqual({ id: ids.prod, obj: ref('View', 'v_open') });
  });

  it('copy_ddl_writes_clipboard', async () => {
    const user = userEvent.setup();
    const write = vi.spyOn(navigator.clipboard, 'writeText').mockResolvedValue();
    setup(makeTab('Table'));
    await user.click(screen.getByTestId('object-tab-ddl'));
    expect(screen.getByRole('tab', { name: 'DDL' })).toHaveAttribute('aria-selected', 'true');
    const pre = await screen.findByTestId('object-ddl-text');
    expect(pre.textContent).toBe(DDL);
    await user.click(screen.getByRole('button', { name: 'Copy' }));
    expect(write).toHaveBeenCalledWith(DDL);
    expect(await screen.findByRole('button', { name: 'Copied' })).toBeInTheDocument();
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
        throw new Failure(appError('server', 'permission denied for orders', { hint: 'Ask your DBA', detail: 'SQLSTATE 42501' }));
      },
    });
    expect(await screen.findByText('Customer orders')).toBeInTheDocument();
    await user.click(screen.getByTestId('object-tab-ddl'));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('permission denied for orders');
    expect(alert).toHaveTextContent('Ask your DBA');
    expect(within(alert).getByText('Details')).toBeInTheDocument();
    // The other sub-tabs keep working.
    await user.click(screen.getByRole('tab', { name: 'Info' }));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByText('Customer orders')).toBeInTheDocument();
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
    expect(screen.getByRole('tab', { name: 'Info' })).toHaveAttribute('aria-selected', 'true');
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
