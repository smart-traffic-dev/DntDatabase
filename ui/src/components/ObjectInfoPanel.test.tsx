import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { ColumnDetail, ObjectKind, ObjectRef, TableDataPage, TableDetail } from '../bindings';
import { useLayout } from '../store/layout';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId, type TreeNode } from '../store/treeModel';
import { callsOf, fail, ids, mockBackend, resetStores } from '../test/backend';
import { keyMarks, ObjectInfoPanel, PREVIEW_ROWS, splitType } from './ObjectInfoPanel';
import { RightSidebar } from './RightSidebar';

const ref = (kind: ObjectKind, name: string): ObjectRef => ({ database: 'shop', schema: null, name, kind });
const nodeId = (o: ObjectRef) => `${connNodeId(ids.prod)}/db:shop/folder:${o.kind}/obj:${o.kind}:${o.name}`;

const col = (name: string, db_type: string, over: Partial<ColumnDetail> = {}): ColumnDetail => ({
  name,
  ordinal: 1,
  db_type,
  nullable: false,
  default: null,
  auto_increment: false,
  identity: null,
  generated: null,
  invisible: false,
  collation: null,
  comment: null,
  ...over,
});

function detail(object: ObjectRef, over: Partial<TableDetail> = {}): TableDetail {
  return {
    object,
    columns: [
      col('id', 'bigint', { auto_increment: true, comment: 'surrogate key' }),
      col('status', 'varchar(20)', { nullable: true, default: "'new'" }),
      col('customer_id', 'int unsigned'),
    ],
    primary_key: { name: 'PRIMARY', unique: true, primary: true, partial: false, columns: ['id'], expression: null, method: 'BTREE', definition: null },
    indexes: [
      { name: 'PRIMARY', unique: true, primary: true, partial: false, columns: ['id'], expression: null, method: 'BTREE', definition: null },
      { name: 'idx_status', unique: false, primary: false, partial: false, columns: ['status'], expression: null, method: 'BTREE', definition: null },
    ],
    foreign_keys: [
      { name: 'fk_customer', columns: ['customer_id'], ref_schema: null, ref_table: 'customers', ref_columns: ['id'], on_update: 'CASCADE', on_delete: 'RESTRICT', definition: null },
    ],
    checks: [],
    triggers: [],
    options: { engine: 'InnoDB', collation: 'utf8mb4_0900_ai_ci', tablespace: null, partitioned_by: null, system_versioned: false },
    row_estimate: 1200,
    size_bytes: 16384,
    comment: 'Customer orders',
    ...over,
  };
}

const page = (rows: number, has_more = false): TableDataPage =>
  ({
    columns: [
      { name: 'id', db_type: 'bigint', logical: 'Integer', nullable: false, origin_table: null, origin_column: null },
      { name: 'status', db_type: 'varchar(20)', logical: 'Text', nullable: true, origin_table: null, origin_column: null },
    ],
    rows: Array.from({ length: rows }, (_, i) => [
      { t: 'Int', v: i + 1 },
      { t: 'Text', v: 'new' },
    ]),
    has_more,
    truncated_cells: 0,
    size_capped: false,
    key_order: [],
    elapsed_ms: 1,
  }) as unknown as TableDataPage;

function backend(handlers: Record<string, (a: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    table_detail: (a) => detail(a.obj as ObjectRef),
    object_ddl: (a) => `CREATE TABLE \`${(a.obj as ObjectRef).name}\` (\n  \`id\` bigint NOT NULL\n)`,
    table_data: () => page(3),
    table_data_cancel: () => true,
    refresh_node: () => null,
    tabs_save: () => null,
    ...handlers,
  });
}

/** Puts the objects in the tree store and selects the first. */
function selectObject(...objects: ObjectRef[]) {
  const nodes = new Map<string, TreeNode>(objects.map((o) => [nodeId(o), { id: nodeId(o), kind: 'object', label: o.name, objectRef: o }]));
  act(() => useTree.setState({ nodes, selectedId: nodeId(objects[0]!) }));
}

beforeEach(() => {
  resetStores();
  localStorage.clear();
  useLayout.setState({ rightVisible: false, rightActivity: 'history' });
});

describe('splitType / keyMarks', () => {
  it('splits the length out of a type, keeps enum lists and modifiers', () => {
    expect(splitType('varchar(255)')).toEqual(['varchar', '255']);
    expect(splitType('decimal(10,2) unsigned')).toEqual(['decimal unsigned', '10,2']);
    expect(splitType('int unsigned')).toEqual(['int unsigned', '']);
    expect(splitType("enum('a','b')")).toEqual(["enum('a','b')", '']);
    expect(splitType('timestamp(3) with time zone')).toEqual(['timestamp with time zone', '3']);
  });

  it('marks primary, foreign, unique and indexed columns', () => {
    const d = detail(ref('Table', 'orders'), {
      indexes: [{ name: 'uq', unique: true, primary: false, partial: false, columns: ['status'], expression: null, method: null, definition: null }],
    });
    expect(d.columns.map((c) => keyMarks(c, d))).toEqual([['PK'], ['UQ'], ['FK']]);
  });
});

describe('ObjectInfoPanel', () => {
  it('asks for a selection when no table or view is selected', () => {
    backend();
    render(<ObjectInfoPanel />);
    expect(screen.getByRole('status')).toHaveTextContent('Select a table or view');
  });

  it('shows a table: General, Columns with indexes and foreign keys, DDL, Preview', async () => {
    backend();
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);

    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['General', 'Columns', 'DDL', 'Preview']);
    expect(screen.getByRole('status')).toHaveTextContent('Loading…');
    expect(await screen.findByText('InnoDB')).toBeInTheDocument();
    expect(screen.getByText('Customer orders')).toBeInTheDocument();
    expect(screen.getByText('≈ 1,200')).toBeInTheDocument();

    await user.click(screen.getByRole('tab', { name: 'Columns' }));
    const columns = screen.getByRole('table', { name: 'Columns' });
    const status = within(columns).getByText('status').closest('tr')!;
    expect([...status.querySelectorAll('td')].map((td) => td.textContent)).toEqual(['status', 'varchar', '20', 'YES', 'IX', "'new'", '', '']);
    expect(within(columns).getByText('id').closest('tr')).toHaveTextContent('PK');
    expect(within(screen.getByRole('table', { name: 'Indexes' })).getByText('idx_status')).toBeInTheDocument();
    expect(within(screen.getByRole('table', { name: 'Foreign keys' })).getByText('customer_id → customers(id)')).toBeInTheDocument();
    // General and Columns share one fetch.
    expect(callsOf('table_detail')).toHaveLength(1);

    await user.click(screen.getByRole('tab', { name: 'DDL' }));
    expect(await screen.findByTestId('object-info-ddl')).toHaveTextContent('CREATE TABLE `orders`');

    await user.click(screen.getByRole('tab', { name: 'Preview' }));
    expect(await screen.findByTestId('object-info-preview')).toBeInTheDocument();
    expect(screen.getByText('3 rows · read-only')).toBeInTheDocument();
    expect(callsOf('table_data')[0]).toMatchObject({ id: ids.prod, obj: ref('Table', 'orders'), limit: PREVIEW_ROWS, offset: 0, sort: [], filter: null });
  });

  it('says when the preview is only the first rows', async () => {
    backend({ table_data: () => page(PREVIEW_ROWS, true) });
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'Preview' }));
    expect(await screen.findByText(`First ${PREVIEW_ROWS} rows · read-only`)).toBeInTheDocument();
  });

  it('switches to the next selected object and keeps the section', async () => {
    backend();
    const user = userEvent.setup();
    const orders = ref('Table', 'orders');
    const customers = ref('Table', 'customers');
    selectObject(orders, customers);
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'DDL' }));
    expect(await screen.findByTestId('object-info-ddl')).toHaveTextContent('`orders`');
    act(() => useTree.getState().select(nodeId(customers)));
    await waitFor(() => expect(screen.getByTestId('object-info-ddl')).toHaveTextContent('`customers`'));
    expect(screen.getByRole('tab', { name: 'DDL' })).toHaveAttribute('aria-selected', 'true');
  });

  it('a view has no indexes or foreign keys; a function shows General and DDL only', async () => {
    backend({ table_detail: (a) => detail(a.obj as ObjectRef, { primary_key: null, indexes: [], foreign_keys: [], row_estimate: null, size_bytes: null, options: { engine: null, collation: null, tablespace: null, partitioned_by: null, system_versioned: false } }) });
    const user = userEvent.setup();
    selectObject(ref('View', 'v_open'), ref('Function', 'f_total'));
    render(<ObjectInfoPanel />);
    expect(await screen.findByText('Columns')).toBeInTheDocument();
    expect(screen.queryByText('Rows (estimate)')).not.toBeInTheDocument();
    await user.click(screen.getByRole('tab', { name: 'Columns' }));
    expect(await screen.findByRole('table', { name: 'Columns' })).toBeInTheDocument();
    expect(screen.queryByRole('table', { name: 'Indexes' })).not.toBeInTheDocument();
    expect(screen.queryByText('Foreign keys')).not.toBeInTheDocument();
    // A view has no designer.
    expect(screen.queryByRole('button', { name: 'Design' })).not.toBeInTheDocument();

    act(() => useTree.getState().select(nodeId(ref('Function', 'f_total'))));
    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['General', 'DDL']);
    // The Columns section does not exist here: General is shown, from the tree alone.
    expect(screen.getByText('Function')).toBeInTheDocument();
    expect(callsOf('table_detail')).toHaveLength(1);
  });

  it('shows an error with Retry, and loads again', async () => {
    let n = 0;
    backend({
      table_detail: (a) => {
        if (n++ === 0) throw fail('internal', { message: 'Access denied for table orders' });
        return detail(a.obj as ObjectRef);
      },
    });
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    expect(await screen.findByRole('alert')).toHaveTextContent('Access denied for table orders');
    await user.click(screen.getByRole('button', { name: 'Retry' }));
    expect(await screen.findByText('InnoDB')).toBeInTheDocument();
  });

  it('Refresh drops the backend cache and reloads the section', async () => {
    backend();
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    await screen.findByText('InnoDB');
    await user.click(screen.getByRole('button', { name: 'Refresh' }));
    await waitFor(() => expect(callsOf('table_detail')).toHaveLength(2));
    expect(callsOf('refresh_node')).toHaveLength(1);
  });

  it('Open opens the data tab of the object', async () => {
    backend({ get_table_page_size: () => 100 });
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('button', { name: 'Open' }));
    expect(useTabs.getState().tabs).toEqual([expect.objectContaining({ kind: 'object', title: 'orders' })]);
    expect(useTabs.getState().tabs[0]?.preview).toBeFalsy();
  });

  it('is the Object info activity of the right sidebar', async () => {
    backend();
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<RightSidebar />);
    await user.click(screen.getByRole('button', { name: 'Object info' }));
    expect(screen.getByTestId('object-info-view')).toBeInTheDocument();
    expect(await screen.findByText('InnoDB')).toBeInTheDocument();
  });
});
