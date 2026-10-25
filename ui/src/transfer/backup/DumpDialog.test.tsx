import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DumpObjects, DumpRequest, ToolDumpRequest, ToolsList } from '../../bindings';
import { rerun } from '../../jobs/rerun';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useLayout } from '../../store/layout';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
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
    dump_pick_path: (args) => `C:\\dumps\\${args.baseName as string}.${args.compress ? 'sql.gz' : 'sql'}`,
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
    expect(callsOf('dump_pick_path')).toEqual([{ baseName: 'shop', compress: false }]);
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
    expect(callsOf('dump_pick_path').at(-1)).toEqual({ baseName: 'shop', compress: true });
  });

  it('method switch shows the tool version, or "not found" with the searched folders; starts a tool dump', async () => {
    backend();
    const u = user();
    const onClose = open();
    await screen.findByTestId('dump-objects');
    const methods = screen.getByRole('radiogroup', { name: 'Dump method' });

    await u.click(within(methods).getByRole('radio', { name: 'mariadb-dump' }));
    const status = screen.getByTestId('dump-tool-status');
    expect(status).toHaveTextContent('mariadb-dump was not found');
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
    expect(callsOf('dump_pick_path')).toEqual([{ baseName: 'orders', compress: false }]);
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
    await waitFor(() => expect(startButton()).toBeEnabled());
    await u.click(startButton());
    expect(callsOf('tool_dump_start')[0]?.request).toEqual({
      connection_id: ids.prod,
      database: 'shop',
      tool: 'pg_dump',
      mysql: null,
      pg: { schemas: ['public', 'sales'], tables: [], structure: true, data: true, format: 'custom', no_owner: true, no_privileges: false },
      compress: false,
      path: 'C:\\dumps\\shop.sql',
    });
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
    expect(useDialogs.getState().request).toEqual({ kind: 'dump', connectionId: ids.prod, database: 'shop', params: { kind: 'dump', request } });

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
