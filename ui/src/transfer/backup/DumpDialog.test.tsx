import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DumpObjects, DumpRequest, ToolDumpRequest, ToolsList } from '../../bindings';
import { rerun } from '../../jobs/rerun';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useLayout } from '../../store/layout';
import { appError, callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { DumpDialog, type DumpParams } from './DumpDialog';
import './rerunBackup';

vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));

const mysqlObjects: DumpObjects = {
  objects: [
    { schema: null, name: 'customers', kind: 'Table', data: true },
    { schema: null, name: 'orders', kind: 'Table', data: true },
    { schema: null, name: 'v_totals', kind: 'View', data: false },
  ],
  skipped: [{ schema: null, name: 'p_refresh', kind: 'Procedure', reason: 'Routines are not part of the built-in dump.' }],
};

const tools = (over: Partial<ToolsList> = {}): ToolsList => ({
  tools: [{ tool: 'mysql_dump', path: 'C:\\mysql\\bin\\mysqldump.exe', version: 'mysqldump  Ver 8.4.3 for Win64', mariadb: false }],
  missing: ['mariadb_dump', 'pg_dump', 'pg_restore', 'psql'],
  searched: ['C:\\tools', 'C:\\mysql\\bin'],
  ...over,
});

function backend(over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    tools_list: () => tools(),
    dump_objects: () => mysqlObjects,
    list_schemas: () => [{ name: 'public' }, { name: 'sales' }],
    dump_pick_path: (args) => {
      const ext = args.format === 'custom' ? 'dump' : args.format === 'tar' ? 'tar' : args.compress ? 'sql.gz' : 'sql';
      return `C:\\dumps\\${args.baseName as string}.${ext}`;
    },
    dump_start: () => 'job-1',
    tool_dump_start: () => 'job-2',
    job_list: () => [],
    ...over,
  });
}

const user = () => userEvent.setup();

function open(props: { database?: string; schema?: string | null; table?: string; params?: DumpParams } = {}) {
  const onClose = vi.fn();
  render(<DumpDialog connectionId={ids.prod} database={props.database ?? 'shop'} schema={props.schema} table={props.table} params={props.params} onClose={onClose} />);
  return onClose;
}

const startButton = () => screen.getByRole('button', { name: 'Start dump' });

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});

describe('DumpDialog', () => {
  it('lists objects with data toggles and the skipped list; Start needs a path; starts the built-in dump', async () => {
    backend();
    const u = user();
    const onClose = open();
    const list = await screen.findByTestId('dump-objects');
    expect(within(list).getByLabelText('customers')).toBeChecked();
    expect(within(list).getByLabelText('Data of orders')).toBeChecked();
    expect(within(list).getByLabelText('v_totals')).toBeChecked();
    // Views have no data toggle.
    expect(within(list).queryByLabelText('Data of v_totals')).toBeNull();
    expect(callsOf('dump_objects')).toEqual([{ connectionId: ids.prod, database: 'shop', schemas: [] }]);
    const skipped = screen.getByTestId('dump-skipped');
    expect(skipped).toHaveTextContent('p_refresh');
    expect(skipped).toHaveTextContent('Routines are not part of the built-in dump.');

    // No output file yet: Start is disabled.
    expect(startButton()).toBeDisabled();

    await u.click(within(list).getByLabelText('Data of orders'));
    await u.click(within(list).getByLabelText('v_totals'));
    await u.click(screen.getByLabelText('Drop statements'));
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    expect(callsOf('dump_pick_path')).toEqual([{ baseName: 'shop', compress: false, format: null }]);
    expect(await screen.findByText('C:\\dumps\\shop.sql')).toBeInTheDocument();
    await waitFor(() => expect(startButton()).toBeEnabled());

    await u.click(startButton());
    expect(callsOf('dump_start')).toEqual([
      {
        request: {
          connection_id: ids.prod,
          database: 'shop',
          schemas: [],
          objects: [
            { schema: null, name: 'customers', kind: 'Table', data: true },
            { schema: null, name: 'orders', kind: 'Table', data: false },
          ],
          include_drop: true,
          compress: false,
          path: 'C:\\dumps\\shop.sql',
        },
      },
    ]);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(useLayout.getState().rightVisible).toBe(true);
    expect(useLayout.getState().rightActivity).toBe('jobs');
  });

  it('changing Compress drops the picked file (its extension no longer fits)', async () => {
    backend();
    const u = user();
    open();
    await screen.findByTestId('dump-objects');
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    await u.click(screen.getByLabelText('Compress (.sql.gz)'));
    expect(screen.queryByText('C:\\dumps\\shop.sql')).toBeNull();
    expect(startButton()).toBeDisabled();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    expect(callsOf('dump_pick_path').at(-1)).toEqual({ baseName: 'shop', compress: true, format: null });
  });

  it('method switch shows the tool version, or "not found" with the searched folders; starts a tool dump', async () => {
    backend();
    const u = user();
    const onClose = open();
    await screen.findByTestId('dump-objects');
    const methods = screen.getByRole('radiogroup', { name: 'Dump method' });

    await u.click(within(methods).getByRole('radio', { name: 'mariadb-dump' }));
    expect(await screen.findByText('mariadb-dump was not found.')).toBeInTheDocument();
    const status = screen.getByTestId('dump-tool-status');
    expect(status).toHaveTextContent('C:\\tools');
    expect(status).toHaveTextContent('C:\\mysql\\bin');
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    expect(startButton()).toBeDisabled();

    await u.click(within(methods).getByRole('radio', { name: 'mysqldump' }));
    expect(screen.getByTestId('dump-tool-status')).toHaveTextContent('mysqldump Ver 8.4.3 for Win64');
    // The tool's own options replace the built-in ones.
    expect(screen.queryByLabelText('Drop statements')).toBeNull();
    await u.click(screen.getByLabelText('Routines'));
    // Only some tables: the tool gets their names.
    await u.click(within(screen.getByTestId('dump-objects')).getByLabelText('customers'));
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(startButton());
    expect(callsOf('tool_dump_start')).toEqual([
      {
        request: {
          connection_id: ids.prod,
          database: 'shop',
          tool: 'mysql_dump',
          mysql: { tables: ['orders', 'v_totals'], structure: true, data: true, routines: true, events: false, triggers: true, single_transaction: true },
          pg: null,
          compress: false,
          path: 'C:\\dumps\\shop.sql',
        },
      },
    ]);
    expect(callsOf('dump_start')).toEqual([]);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it('Dump table… pre-selects only that table and names the file after it', async () => {
    backend();
    const u = user();
    open({ table: 'orders' });
    const list = await screen.findByTestId('dump-objects');
    expect(within(list).getByLabelText('orders')).toBeChecked();
    expect(within(list).getByLabelText('customers')).not.toBeChecked();
    expect(within(list).getByLabelText('v_totals')).not.toBeChecked();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    expect(callsOf('dump_pick_path')).toEqual([{ baseName: 'orders', compress: false, format: null }]);
  });

  it('nothing selected keeps Start disabled', async () => {
    backend();
    const u = user();
    open();
    await screen.findByTestId('dump-objects');
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(screen.getByRole('button', { name: 'Select none' }));
    expect(startButton()).toBeDisabled();
    expect(screen.getByText('Choose at least one object.')).toBeInTheDocument();
  });

  it('PostgreSQL: schemas chosen from the list feed dump_objects; pg_dump options', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    backend({
      tools_list: () => tools({ tools: [{ tool: 'pg_dump', path: 'C:\\pg\\bin\\pg_dump.exe', version: 'pg_dump (PostgreSQL) 17.2', mariadb: false }] }),
      dump_objects: (args) => ({
        objects: (args.schemas as string[]).map((s) => ({ schema: s, name: `t_${s}`, kind: 'Table', data: true })),
        skipped: [],
      }),
    });
    const u = user();
    open({ database: 'shop', schema: 'sales' });
    const list = await screen.findByTestId('dump-objects');
    expect(callsOf('dump_objects')[0]).toEqual({ connectionId: ids.prod, database: 'shop', schemas: ['sales'] });
    expect(within(list).getByLabelText('sales.t_sales')).toBeChecked();

    await u.click(await screen.findByLabelText('Schema public'));
    await waitFor(() => expect(callsOf('dump_objects').at(-1)).toEqual({ connectionId: ids.prod, database: 'shop', schemas: ['public', 'sales'] }));
    await within(screen.getByTestId('dump-objects')).findByLabelText('public.t_public');

    const methods = screen.getByRole('radiogroup', { name: 'Dump method' });
    expect(within(methods).queryByRole('radio', { name: 'mysqldump' })).toBeNull();
    await u.click(within(methods).getByRole('radio', { name: 'pg_dump' }));
    expect(screen.getByTestId('dump-tool-status')).toHaveTextContent('pg_dump (PostgreSQL) 17.2');
    await u.selectOptions(screen.getByLabelText('Format'), 'custom');
    // A custom archive is compressed already.
    expect(screen.getByLabelText('Compress (.sql.gz)')).toBeDisabled();
    await u.click(screen.getByLabelText('No owner'));
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    // The save dialog names an archive after its format.
    expect(callsOf('dump_pick_path').at(-1)).toEqual({ baseName: 'shop', compress: false, format: 'custom' });
    await screen.findByText('C:\\dumps\\shop.dump');
    // Another format needs another file name: Browse again.
    await u.selectOptions(screen.getByLabelText('Format'), 'tar');
    expect(screen.queryByText('C:\\dumps\\shop.dump')).toBeNull();
    expect(startButton()).toBeDisabled();
    await u.selectOptions(screen.getByLabelText('Format'), 'custom');
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(startButton());
    expect(callsOf('tool_dump_start')[0]?.request).toEqual({
      connection_id: ids.prod,
      database: 'shop',
      tool: 'pg_dump',
      mysql: null,
      pg: { schemas: ['public', 'sales'], tables: [], structure: true, data: true, format: 'custom', no_owner: true, no_privileges: false },
      compress: false,
      path: 'C:\\dumps\\shop.dump',
    });
  });

  it('PostgreSQL: a schema change disables Start until the new listing answers; a stale answer is dropped', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    const pending: Array<(v: DumpObjects) => void> = [];
    const listingOf = (schemas: string[]): DumpObjects => ({
      objects: schemas.map((s) => ({ schema: s, name: `t_${s}`, kind: 'Table' as const, data: true })),
      skipped: [],
    });
    backend({
      dump_objects: (args) =>
        (args.schemas as string[]).length === 1 && (args.schemas as string[])[0] === 'sales'
          ? listingOf(['sales'])
          : new Promise<DumpObjects>((resolve) => pending.push(resolve)),
    });
    const u = user();
    open({ database: 'shop', schema: 'sales' });
    await screen.findByTestId('dump-objects');
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await waitFor(() => expect(startButton()).toBeEnabled());

    // public + sales: in flight, Start waits for it.
    await u.click(await screen.findByLabelText('Schema public'));
    await waitFor(() => expect(pending).toHaveLength(1));
    expect(startButton()).toBeDisabled();
    expect(screen.queryByTestId('dump-objects')).toBeNull();
    // Only public now: the public + sales answer that arrives late is ignored.
    await u.click(screen.getByLabelText('Schema sales'));
    await waitFor(() => expect(pending).toHaveLength(2));
    await act(async () => pending[0]!(listingOf(['public', 'sales'])));
    expect(screen.queryByTestId('dump-objects')).toBeNull();
    expect(startButton()).toBeDisabled();
    await act(async () => pending[1]!(listingOf(['public'])));
    const list = await screen.findByTestId('dump-objects');
    expect(within(list).getByLabelText('public.t_public')).toBeInTheDocument();
    expect(within(list).queryByLabelText('sales.t_sales')).toBeNull();
    await waitFor(() => expect(startButton()).toBeEnabled());
  });

  it('pg_dump cannot name a type: types are not listed and a selection without tables cannot start', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    backend({
      tools_list: () => tools({ tools: [{ tool: 'pg_dump', path: 'C:\\pg\\bin\\pg_dump.exe', version: 'pg_dump (PostgreSQL) 17.2', mariadb: false }] }),
      dump_objects: () => ({
        objects: [
          { schema: 'sales', name: 'orders', kind: 'Table', data: true },
          { schema: 'sales', name: 'mood', kind: 'Type', data: false },
        ],
        skipped: [],
      }),
    });
    const u = user();
    open({ database: 'shop', schema: 'sales' });
    const list = await screen.findByTestId('dump-objects');
    // The built-in dump lists the type.
    expect(within(list).getByLabelText('sales.mood')).toBeChecked();
    await u.click(screen.getByRole('radio', { name: 'pg_dump' }));
    expect(within(screen.getByTestId('dump-objects')).queryByLabelText('sales.mood')).toBeNull();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await waitFor(() => expect(startButton()).toBeEnabled());
    // Only the (hidden) type left: pg_dump would get "every table", so Start is refused.
    await u.click(within(screen.getByTestId('dump-objects')).getByLabelText('sales.orders'));
    expect(startButton()).toBeDisabled();
    expect(screen.getByText('Choose at least one table, view or sequence for pg_dump.')).toBeInTheDocument();
  });

  it('the method control is one Tab stop moved with the arrow keys', async () => {
    backend();
    const u = user();
    open();
    await screen.findByTestId('dump-objects');
    const radios = within(screen.getByRole('radiogroup', { name: 'Dump method' })).getAllByRole('radio');
    expect(radios.map((r) => r.tabIndex)).toEqual([0, -1, -1]);
    radios[0]!.focus();
    await u.keyboard('{ArrowRight}');
    expect(radios[1]).toHaveAttribute('aria-checked', 'true');
    expect(radios[1]).toHaveFocus();
    expect(radios.map((r) => r.tabIndex)).toEqual([-1, 0, -1]);
    await u.keyboard('{End}');
    expect(radios[2]).toHaveAttribute('aria-checked', 'true');
    await u.keyboard('{ArrowRight}');
    expect(radios[0]).toHaveAttribute('aria-checked', 'true');
    expect(radios[0]).toHaveFocus();
  });

  it('Dump table… on a table the built-in dump skips says why', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    backend({
      dump_objects: () => ({
        objects: [{ schema: 'sales', name: 'orders', kind: 'Table', data: true }],
        skipped: [{ schema: 'sales', name: 'events', kind: 'Table', reason: 'partitioned tables need an external dump tool' }],
      }),
    });
    open({ database: 'shop', schema: 'sales', table: 'events' });
    expect(await screen.findByText('events is not in the built-in dump: partitioned tables need an external dump tool')).toBeInTheDocument();
  });

  it('a file picked before a restart must be chosen again', async () => {
    backend({
      dump_start: () => {
        throw appError('config', 'Choose the file in the file dialog.', { field: 'path' });
      },
    });
    const request: DumpRequest = {
      connection_id: ids.prod,
      database: 'shop',
      schemas: [],
      objects: [{ schema: null, name: 'orders', kind: 'Table', data: true }],
      include_drop: false,
      compress: false,
      path: 'C:\\dumps\\shop.sql',
    };
    const u = user();
    open({ params: { kind: 'dump', request } });
    await screen.findByTestId('dump-objects');
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(startButton());
    expect(await screen.findByRole('alert')).toHaveTextContent('Choose the file again (files picked before a restart must be chosen again).');
  });

  it('a PostgreSQL database node selects every schema', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    backend({ dump_objects: () => ({ objects: [], skipped: [] }) });
    open({ database: 'shop' });
    await waitFor(() => expect(callsOf('dump_objects')[0]).toEqual({ connectionId: ids.prod, database: 'shop', schemas: ['public', 'sales'] }));
    expect(callsOf('list_schemas')[0]).toEqual({ id: ids.prod, db: 'shop', showSystem: false });
  });
});

describe('Run again (dump)', () => {
  const request: DumpRequest = {
    connection_id: ids.prod,
    database: 'shop',
    schemas: [],
    objects: [{ schema: null, name: 'orders', kind: 'Table', data: false }],
    include_drop: true,
    compress: false,
    path: 'C:\\dumps\\shop.sql',
  };

  it('reopens the dialog pre-filled; Start dump starts it again', async () => {
    expect(rerun({ kind: 'dump', request: request as never })).toBe(true);
    expect(useDialogs.getState().request).toEqual({ kind: 'dump', connectionId: ids.prod, database: 'shop', params: { kind: 'dump', request }, rerun: expect.any(Number) });

    backend();
    const u = user();
    open({ params: { kind: 'dump', request } });
    const list = await screen.findByTestId('dump-objects');
    expect(within(list).getByLabelText('orders')).toBeChecked();
    expect(within(list).getByLabelText('Data of orders')).not.toBeChecked();
    expect(within(list).getByLabelText('customers')).not.toBeChecked();
    expect(screen.getByLabelText('Drop statements')).toBeChecked();
    expect(screen.getByText('C:\\dumps\\shop.sql')).toBeInTheDocument();
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(startButton());
    expect(callsOf('dump_start')).toEqual([{ request }]);
  });

  it('a tool dump reopens on its tool with its options', async () => {
    const toolRequest: ToolDumpRequest = {
      connection_id: ids.prod,
      database: 'shop',
      tool: 'mysql_dump',
      mysql: { tables: [], structure: true, data: false, routines: true, events: true, triggers: false, single_transaction: true },
      pg: null,
      compress: true,
      path: 'C:\\dumps\\shop.sql.gz',
    };
    expect(rerun({ kind: 'tool_dump', request: toolRequest as never })).toBe(true);
    expect(useDialogs.getState().request).toMatchObject({ kind: 'dump', params: { kind: 'tool_dump', request: toolRequest } });

    backend();
    const u = user();
    open({ params: { kind: 'tool_dump', request: toolRequest } });
    await screen.findByTestId('dump-objects');
    expect(screen.getByRole('radio', { name: 'mysqldump' })).toHaveAttribute('aria-checked', 'true');
    expect(screen.getByLabelText('Data')).not.toBeChecked();
    expect(screen.getByLabelText('Events')).toBeChecked();
    expect(screen.getByLabelText('Compress (.sql.gz)')).toBeChecked();
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(startButton());
    expect(callsOf('tool_dump_start')).toEqual([{ request: toolRequest }]);
  });
});
