import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ColumnDetail, ObjectKind, ObjectRef, Scope, TableDataPage, TableDetail } from '../bindings';
import type { ServerInfo } from '../bindings';
import { useConnections } from '../store/connections';
import { useLayout } from '../store/layout';
import { config } from '../test/fixtures/data';
import { sqlCaps } from '../test/fixtures/caps';
import { useObjectInfo } from '../store/objectInfo';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId, type TreeNode } from '../store/treeModel';
import { callsOf, fail, ids, mockBackend, resetStores } from '../test/backend';
import { useQueryDocs } from '../store/queryTabs';
import { filterColumns, keyMarks, ObjectInfoPanel, PREVIEW_ROWS, referencedTable, splitType } from './ObjectInfoPanel';
import { RightSidebar } from './RightSidebar';

const insertAtCursor = vi.hoisted(() => vi.fn());
vi.mock('../store/runQuery', async (orig) => ({ ...(await orig<typeof import('../store/runQuery')>()), insertAtCursor }));

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
  useObjectInfo.getState().reset();
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
    expect(screen.getByRole('status')).toHaveTextContent('Select a database, table or view');
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

  it('shows an object picked in an editor over the tree selection, until the tree selects again', async () => {
    backend();
    const user = userEvent.setup();
    const orders = ref('Table', 'orders');
    const customers = ref('Table', 'customers');
    selectObject(orders, customers);
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'DDL' }));
    expect(await screen.findByTestId('object-info-ddl')).toHaveTextContent('`orders`');

    act(() => useObjectInfo.getState().show(ids.prod, customers));
    expect(await screen.findByTestId('object-info-ddl')).toHaveTextContent('`customers`');
    expect(useLayout.getState()).toMatchObject({ rightVisible: true, rightActivity: 'info' });

    act(() => useTree.getState().select(nodeId(orders) + '-other'));
    act(() => useTree.getState().select(nodeId(orders)));
    expect(await screen.findByTestId('object-info-ddl')).toHaveTextContent('`orders`');
  });

  it('says which connection, database and server the object is on; no collapse button of its own', () => {
    backend();
    useConnections.setState({
      summaries: new Map([[ids.prod, config(ids.prod, 'Shop prod', { env_tag: 'production', read_only: true })]]),
      info: new Map([[ids.prod, { kind: 'mysql', variant: 'MySql8', version: [8, 0, 36], version_string: '8.0.36', capabilities: sqlCaps('db') } satisfies ServerInfo]]),
    });
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    const context = screen.getByTestId('object-info-context');
    expect(context).toHaveTextContent('PROD');
    expect(context).toHaveTextContent('Shop prod › shop');
    expect(context).toHaveTextContent('MySQL 8.0.36');
    expect(within(context).getByLabelText('Read-only')).toBeInTheDocument();
    expect(context).toHaveClass('oi-prod');
    expect(screen.queryByRole('button', { name: 'Collapse right sidebar' })).toBeNull();
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

describe('pin, columns tools, foreign keys', () => {
  beforeEach(() => insertAtCursor.mockClear());

  it('pinned, a selection in the tree does not change the panel; unpinned it does again', async () => {
    backend();
    const user = userEvent.setup();
    const orders = ref('Table', 'orders');
    const customers = ref('Table', 'customers');
    selectObject(orders, customers);
    render(<ObjectInfoPanel />);
    const pin = screen.getByRole('button', { name: 'Pin' });
    expect(pin).toHaveAttribute('aria-pressed', 'false');
    await user.click(pin);
    expect(pin).toHaveAttribute('aria-pressed', 'true');

    act(() => useTree.getState().select(nodeId(customers)));
    expect(screen.getByText('orders', { selector: '.oi-object' })).toBeInTheDocument();

    await user.click(pin);
    act(() => useTree.getState().select(nodeId(orders)));
    act(() => useTree.getState().select(nodeId(customers)));
    expect(screen.getByText('customers', { selector: '.oi-object' })).toBeInTheDocument();
  });

  it('filters the columns by name, type or comment', async () => {
    backend();
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'Columns' }));
    const columns = await screen.findByRole('table', { name: 'Columns' });
    await user.type(screen.getByRole('searchbox', { name: 'Filter columns' }), 'VARCHAR');
    expect(within(columns).getAllByRole('row').slice(1).map((r) => r.querySelector('td')!.textContent)).toEqual(['status']);
    expect(screen.getByText('1 of 3 columns')).toBeInTheDocument();
    await user.clear(screen.getByRole('searchbox', { name: 'Filter columns' }));
    await user.type(screen.getByRole('searchbox', { name: 'Filter columns' }), 'nothing');
    expect(within(columns).getByText('No column matches “nothing”')).toBeInTheDocument();
    await user.keyboard('{Escape}');
    expect(within(columns).getAllByRole('row')).toHaveLength(4);
  });

  it('double-click inserts a column name, Insert all names, Copy names copies them', async () => {
    backend();
    const user = userEvent.setup();
    const writeText = vi.fn(async () => undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    selectObject(ref('Table', 'orders'));
    const tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'Columns' }));
    const columns = await screen.findByRole('table', { name: 'Columns' });

    await user.dblClick(within(columns).getByText('customer_id'));
    expect(insertAtCursor).toHaveBeenLastCalledWith(tab, 'customer_id');
    await user.click(screen.getByRole('button', { name: 'Insert' }));
    expect(insertAtCursor).toHaveBeenLastCalledWith(tab, 'id, status, customer_id');
    await user.click(screen.getByRole('button', { name: 'Copy names' }));
    expect(writeText).toHaveBeenCalledWith('id, status, customer_id');
    expect(await screen.findByText('Copied 3 column names')).toBeInTheDocument();
  });

  it('without a query tab Insert says to open one', async () => {
    backend();
    const user = userEvent.setup();
    selectObject(ref('Table', 'orders'));
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'Columns' }));
    await screen.findByRole('table', { name: 'Columns' });
    await user.click(screen.getByRole('button', { name: 'Insert' }));
    expect(insertAtCursor).not.toHaveBeenCalled();
    expect(screen.getByText('Open a query tab to insert')).toBeInTheDocument();
  });

  it('a foreign key shows its table here; Back returns, and a new tree selection drops the way back', async () => {
    backend();
    const user = userEvent.setup();
    const orders = ref('Table', 'orders');
    const customers = ref('Table', 'customers');
    selectObject(orders, customers);
    render(<ObjectInfoPanel />);
    await user.click(screen.getByRole('tab', { name: 'Columns' }));
    expect(screen.queryByRole('button', { name: 'Back' })).toBeNull();

    await user.click(await screen.findByRole('button', { name: 'customer_id → customers(id)' }));
    expect(screen.getByText('customers', { selector: '.oi-object' })).toBeInTheDocument();
    await waitFor(() => expect(callsOf('table_detail').at(-1)).toMatchObject({ obj: { database: 'shop', schema: null, name: 'customers', kind: 'Table' } }));
    // Still on Columns.
    expect(screen.getByRole('tab', { name: 'Columns' })).toHaveAttribute('aria-selected', 'true');

    await user.click(screen.getByRole('button', { name: 'Back' }));
    expect(screen.getByText('orders', { selector: '.oi-object' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Back' })).toBeNull();

    await user.click(await screen.findByRole('button', { name: 'customer_id → customers(id)' }));
    act(() => useTree.getState().select(nodeId(customers)));
    expect(screen.queryByRole('button', { name: 'Back' })).toBeNull();
  });

  it('a PostgreSQL foreign key resolves in a schema of the same database', () => {
    const fk = detail(ref('Table', 'orders')).foreign_keys[0]!;
    const pgOrders: ObjectRef = { database: 'app', schema: 'sales', name: 'orders', kind: 'Table' };
    expect(referencedTable({ ...fk, ref_schema: 'crm' }, pgOrders, true)).toEqual({ database: 'app', schema: 'crm', name: 'customers', kind: 'Table' });
    expect(referencedTable({ ...fk, ref_schema: null }, pgOrders, true)).toMatchObject({ schema: 'sales' });
    expect(referencedTable({ ...fk, ref_schema: 'other_db' }, ref('Table', 'orders'), false)).toEqual({ database: 'other_db', schema: null, name: 'customers', kind: 'Table' });
  });

  describe('a database or schema', () => {
    const server = (variant: ServerInfo['variant']): ServerInfo => ({
      kind: variant === 'Postgres' ? 'postgres' : 'mysql',
      variant,
      version: [8, 0, 36],
      version_string: '8.0.36',
      capabilities: sqlCaps(variant === 'Postgres' ? 'db_schema' : 'db'),
    });
    const selectNamespace = (database: string, schema: string | null = null) => {
      const id = `${connNodeId(ids.prod)}/db:${database}${schema ? `/schema:${schema}` : ''}`;
      act(() =>
        useTree.setState({ nodes: new Map([[id, { id, kind: schema ? 'schema' : 'database', label: schema ?? database, scope: { database, schema } }]]), selectedId: id }),
      );
    };
    const objects = (a: Record<string, unknown>) =>
      a.kind === 'Table'
        ? [
            { name: 'orders', kind: 'Table', comment: 'Customer orders', row_estimate: 1200 },
            { name: 'customers', kind: 'Table', comment: null, row_estimate: 300 },
          ]
        : a.kind === 'View'
          ? [{ name: 'order_totals', kind: 'View', comment: null, row_estimate: null }]
          : [];

    it('shows charset, collation, size and comment from namespace_detail', async () => {
      backend({
        list_objects: objects,
        namespace_detail: () => ({ charset: 'utf8mb4', collation: 'utf8mb4_0900_ai_ci', owner: null, size_bytes: 81920, comment: 'Shop data' }),
      });
      useConnections.setState({ info: new Map([[ids.prod, server('MySql8')]]) });
      selectNamespace('shop');
      render(<ObjectInfoPanel />);
      await screen.findByRole('table', { name: 'Tables and views' });
      const value = (label: string) => screen.getByText(label, { selector: 'dt' }).nextSibling;
      expect(value('Charset')).toHaveTextContent('utf8mb4');
      expect(value('Collation')).toHaveTextContent('utf8mb4_0900_ai_ci');
      expect(value('Size')).toHaveTextContent('80.0 KB');
      expect(value('Comment')).toHaveTextContent('Shop data');
      expect(screen.queryByText('Owner', { selector: 'dt' })).toBeNull();
      expect(callsOf('namespace_detail')[0]).toMatchObject({ id: ids.prod, scope: { database: 'shop', schema: null } });
    });

    it('without namespace_detail (it failed) the rest still shows', async () => {
      backend({ list_objects: objects, namespace_detail: () => { throw fail('internal', { message: 'denied' }); } });
      useConnections.setState({ info: new Map([[ids.prod, server('MySql8')]]) });
      selectNamespace('shop');
      render(<ObjectInfoPanel />);
      await screen.findByRole('table', { name: 'Tables and views' });
      expect(screen.queryByText('Charset', { selector: 'dt' })).toBeNull();
      expect(screen.getByText('Tables', { selector: 'dt' })).toBeInTheDocument();
    });

    it('shows counts, the row total and its tables; a click shows a table and Back returns', async () => {
      backend({ list_objects: objects });
      useConnections.setState({ info: new Map([[ids.prod, server('MySql8')]]) });
      const user = userEvent.setup();
      selectNamespace('shop');
      render(<ObjectInfoPanel />);

      const list = await screen.findByRole('table', { name: 'Tables and views' });
      expect(screen.getByText('Database', { selector: 'dd' })).toBeInTheDocument();
      expect(screen.getByText('Tables', { selector: 'dt' }).nextSibling).toHaveTextContent(/^2$/);
      expect(screen.getByText('Views', { selector: 'dt' }).nextSibling).toHaveTextContent(/^1$/);
      expect(screen.getByText('Rows (estimate)', { selector: 'dt' }).nextSibling).toHaveTextContent('≈ 1,500');
      expect(within(list).getByText('orders').closest('tr')).toHaveTextContent('ordersTable≈ 1,200Customer orders');
      expect(screen.getByTestId('object-info-context')).toHaveTextContent(`${ids.prod} › shop`);

      await user.type(screen.getByRole('searchbox', { name: 'Filter tables and views' }), 'cust');
      expect(within(list).getAllByRole('row')).toHaveLength(3);
      expect(screen.getByText('2 of 3 tables and views')).toBeInTheDocument();

      await user.click(within(list).getByRole('button', { name: 'customers' }));
      expect(screen.getByText('customers', { selector: '.oi-object' })).toBeInTheDocument();
      await user.click(screen.getByRole('button', { name: 'Back' }));
      expect(screen.getByText('shop', { selector: '.oi-object' })).toBeInTheDocument();
    });

    it('New query opens a query tab on the database', async () => {
      backend({ list_objects: objects });
      useConnections.setState({ info: new Map([[ids.prod, server('MySql8')]]) });
      selectNamespace('shop');
      render(<ObjectInfoPanel />);
      await userEvent.setup().click(screen.getByRole('button', { name: 'New query' }));
      const tab = useTabs.getState().tabs.find((t) => t.kind === 'query')!;
      expect(useQueryDocs.getState().docs.get(tab.id)).toMatchObject({ connectionId: ids.prod, database: 'shop' });
    });

    it('a PostgreSQL database lists its schemas; a schema shows its objects', async () => {
      backend({
        list_schemas: () => [{ name: 'public' }, { name: 'audit' }],
        list_objects: objects,
        namespace_detail: (a) =>
          (a.scope as Scope).schema
            ? { charset: null, collation: null, owner: 'auditor', size_bytes: null, comment: null }
            : { charset: 'UTF8', collation: 'en_US.utf8', owner: 'postgres', size_bytes: 8388608, comment: null },
      });
      useConnections.setState({ info: new Map([[ids.prod, server('Postgres')]]) });
      const user = userEvent.setup();
      selectNamespace('app');
      render(<ObjectInfoPanel />);
      const schemas = await screen.findByRole('table', { name: 'Schemas' });
      expect(screen.getByText('Schemas', { selector: 'dt' }).nextSibling).toHaveTextContent('2');
      expect(screen.getByText('Encoding', { selector: 'dt' }).nextSibling).toHaveTextContent('UTF8');
      expect(screen.getByText('Owner', { selector: 'dt' }).nextSibling).toHaveTextContent('postgres');
      expect(screen.getByText('Size', { selector: 'dt' }).nextSibling).toHaveTextContent('8.0 MB');
      await user.click(within(schemas).getByRole('button', { name: 'audit' }));
      expect(screen.getByText('audit', { selector: '.oi-object' })).toBeInTheDocument();
      expect(screen.getByText('Schema', { selector: 'dd' })).toBeInTheDocument();
      await screen.findByRole('table', { name: 'Tables and views' });
      expect(callsOf('list_objects').at(-1)).toMatchObject({ scope: { database: 'app', schema: 'audit' } });
      expect(screen.getByText('Owner', { selector: 'dt' }).nextSibling).toHaveTextContent('auditor');
    });

    it('a closed connection offers Reconnect', async () => {
      backend({ list_objects: objects });
      selectNamespace('shop');
      render(<ObjectInfoPanel />);
      expect(await screen.findByRole('button', { name: 'Reconnect' })).toBeInTheDocument();
    });
  });

  it('filterColumns matches any case and ignores surrounding spaces', () => {
    const cols = detail(ref('Table', 'orders')).columns;
    expect(filterColumns(cols, '  KEY ').map((c) => c.name)).toEqual(['id']);
    expect(filterColumns(cols, '')).toBe(cols);
  });
});
