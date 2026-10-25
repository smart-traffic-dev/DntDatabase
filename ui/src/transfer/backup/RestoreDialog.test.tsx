import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { RestorePreview, RestoreRequest } from '../../bindings';
import { rerun } from '../../jobs/rerun';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import { useLayout } from '../../store/layout';
import { appError, callsOf, config, fail, ids, mockBackend, resetStores, serverInfo } from '../../test/backend';
import { RestoreDialog } from './RestoreDialog';
import './rerunBackup';

vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));

const stats = { statements: 42, reads: 0, writes: 30, ddl: 10, transaction: 0, other: 2, drops: 3, copy_blocks: 0, client_commands: 0, app_dump: true };

const preview = (over: Partial<RestorePreview> = {}): RestorePreview => ({
  plan_id: 'plan-7',
  path: 'app_runner',
  file: { name: 'shop.sql', size: 2048, sha256: '0123456789abcdef0123' },
  compressed: false,
  stats,
  toc: [],
  toc_truncated: false,
  command: null,
  tool: null,
  transactional: false,
  warnings: ['MySQL restores are not transactional (DDL commits at once): a failure leaves a partial restore.'],
  confirm: { kind: 'none' },
  expires_in_seconds: 600,
  ...over,
});

function backend(over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    list_databases: () => [{ name: 'shop' }, { name: 'shop_copy' }],
    restore_pick_file: () => 'C:\\dumps\\shop.sql',
    restore_preview: () => preview(),
    restore_start: () => 'job-5',
    job_list: () => [],
    ...over,
  });
}

const user = () => userEvent.setup();

function open(props: { database?: string; params?: RestoreRequest } = { database: 'shop' }) {
  const onClose = vi.fn();
  render(<RestoreDialog connectionId={ids.prod} database={props.database} params={props.params} onClose={onClose} />);
  return onClose;
}

const analyze = () => screen.getByRole('button', { name: 'Analyze' });
const startButton = () => screen.getByRole('button', { name: 'Start restore' });

async function pickAndAnalyze(u: ReturnType<typeof user>) {
  await u.click(screen.getByRole('button', { name: 'Browse…' }));
  await screen.findByText('C:\\dumps\\shop.sql');
  await waitFor(() => expect(analyze()).toBeEnabled());
  await u.click(analyze());
  return screen.findByTestId('restore-preview');
}

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});

describe('RestoreDialog', () => {
  it('analyze shows the path chip, statistics and warnings; Start restore starts the plan', async () => {
    backend();
    const u = user();
    const onClose = open();
    expect(analyze()).toBeDisabled();
    expect(screen.getByLabelText('Database')).toHaveValue('shop');
    const result = await pickAndAnalyze(u);
    expect(callsOf('restore_preview')).toEqual([
      { request: { connection_id: ids.prod, database: 'shop', path: 'C:\\dumps\\shop.sql', single_transaction: true, clean: false, no_owner: false } },
    ]);
    expect(within(result).getByTestId('restore-path')).toHaveTextContent('Built-in runner');
    expect(result).toHaveTextContent('shop.sql');
    expect(result).toHaveTextContent('0123456789ab');
    const statTable = within(result).getByRole('table', { name: 'Statements' });
    expect(statTable).toHaveTextContent('42');
    expect(statTable).toHaveTextContent('Drops');
    expect(result).toHaveTextContent('a failure leaves a partial restore');
    expect(result).toHaveTextContent('Not in one transaction');

    await u.click(startButton());
    expect(callsOf('restore_start')).toEqual([{ planId: 'plan-7', confirmed: true, name: null }]);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(useLayout.getState().rightActivity).toBe('jobs');
  });

  it('pg_restore shows the TOC and the masked command line', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    const toc = Array.from({ length: 1500 }, (_, i) => `${i + 1}; 1259 16390 TABLE public t${i + 1} postgres`);
    backend({
      restore_preview: () =>
        preview({
          path: 'pg_restore',
          stats: null,
          toc,
          toc_truncated: true,
          command: 'pg_restore --no-password --exit-on-error --single-transaction --dbname="host=db.local user=root" C:\\dumps\\shop.dump',
          tool: { tool: 'pg_restore', path: 'C:\\pg\\bin\\pg_restore.exe', version: 'pg_restore (PostgreSQL) 17.2', mariadb: false },
          transactional: true,
          warnings: [],
        }),
    });
    const u = user();
    open();
    await u.click(screen.getByLabelText('Clean (drop objects first)'));
    const result = await pickAndAnalyze(u);
    expect(callsOf('restore_preview')[0]?.request).toMatchObject({ clean: true, single_transaction: true, no_owner: false });
    expect(within(result).getByTestId('restore-path')).toHaveTextContent('pg_restore');
    expect(within(result).getByTestId('restore-command')).toHaveTextContent('pg_restore --no-password');
    const list = within(result).getByRole('list', { name: 'Archive contents' });
    // Virtualised: only a window of the 1,500 entries is in the DOM.
    expect(within(list).getAllByRole('listitem').length).toBeLessThan(200);
    expect(list).toHaveTextContent('TABLE public t1 postgres');
    expect(result).toHaveTextContent('Showing the first 1500 entries');
    expect(result).toHaveTextContent('One transaction');
  });

  it('Production requires typing the database name exactly (case-sensitive)', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
    backend({ restore_preview: () => preview({ confirm: { kind: 'type_name', name: 'shop' } }) });
    const u = user();
    open();
    await pickAndAnalyze(u);
    expect(startButton()).toBeDisabled();
    const box = screen.getByLabelText(/Type shop to confirm/);
    await u.type(box, 'Shop');
    expect(startButton()).toBeDisabled();
    await u.clear(box);
    await u.type(box, 'shop');
    expect(startButton()).toBeEnabled();
    await u.click(startButton());
    expect(callsOf('restore_start')).toEqual([{ planId: 'plan-7', confirmed: true, name: 'shop' }]);
  });

  it('Staging requires the checkbox', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-stg', { env_tag: 'staging' })]);
    backend({ restore_preview: () => preview({ confirm: { kind: 'click' } }) });
    const u = user();
    open();
    await pickAndAnalyze(u);
    expect(startButton()).toBeDisabled();
    await u.click(screen.getByLabelText(/I understand/));
    expect(startButton()).toBeEnabled();
  });

  it('PlanStale on start asks to analyze again', async () => {
    backend({
      restore_start: () => {
        throw fail('plan_stale');
      },
    });
    const u = user();
    const onClose = open();
    await pickAndAnalyze(u);
    await u.click(startButton());
    // The backend's reason follows the instruction.
    expect(await screen.findByRole('alert')).toHaveTextContent("The file or the connection changed; analyze again. (the table's structure changed)");
    expect(onClose).not.toHaveBeenCalled();
    // The plan is used up: no preview, Start disabled until analyzed again.
    expect(screen.queryByTestId('restore-preview')).toBeNull();
    expect(startButton()).toBeDisabled();
    await u.click(analyze());
    await screen.findByTestId('restore-preview');
    expect(callsOf('restore_preview')).toHaveLength(2);
    expect(startButton()).toBeEnabled();
  });

  it('Unsupported (psql missing) shows the message and no Start button', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    const message =
      'This file needs psql (it uses COPY … FROM stdin or psql commands) and psql was not found. Install the PostgreSQL client tools or set the External tools folder.';
    backend({
      restore_preview: () => {
        throw fail('internal', { kind: 'unsupported', message: 'This feature is not supported by the server.', detail: `${message} (server version PostgreSQL 17.2)` });
      },
    });
    const u = user();
    open();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    await u.click(analyze());
    expect(await screen.findByText(message)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Start restore' })).toBeNull();
    expect(screen.queryByTestId('restore-preview')).toBeNull();
  });

  it('a refused file shows the backend message', async () => {
    backend({
      restore_preview: () => {
        throw appError('config', 'The file creates or drops a database (CREATE DATABASE); edit it first.', { field: 'file' });
      },
    });
    const u = user();
    open();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    await u.click(analyze());
    expect(await screen.findByRole('alert')).toHaveTextContent('The file creates or drops a database');
    expect(startButton()).toBeDisabled();
  });

  it('changing the target database drops the analysis', async () => {
    backend();
    const u = user();
    open();
    await pickAndAnalyze(u);
    await u.selectOptions(screen.getByLabelText('Database'), 'shop_copy');
    expect(screen.queryByTestId('restore-preview')).toBeNull();
    expect(startButton()).toBeDisabled();
  });

  it('a late analysis for inputs changed meanwhile is dropped; inputs are locked while it runs', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-stg', { env_tag: 'staging' })]);
    const pending: Array<(v: RestorePreview) => void> = [];
    backend({ restore_preview: () => new Promise<RestorePreview>((resolve) => pending.push(resolve)) });
    const u = user();
    open();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    await u.click(analyze());
    await waitFor(() => expect(pending).toHaveLength(1));
    const select = screen.getByLabelText('Database');
    expect(select).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Browse…' })).toBeDisabled();
    // A change that still gets through (the select is disabled for the user) drops the analysis in flight.
    fireEvent.change(select, { target: { value: 'shop_copy' } });
    await act(async () => pending[0]!(preview({ confirm: { kind: 'click' } })));
    expect(screen.queryByTestId('restore-preview')).toBeNull();
    expect(startButton()).toBeDisabled();

    // Analyzed for shop_copy: the confirmation names the analyzed database.
    await waitFor(() => expect(analyze()).toBeEnabled());
    await u.click(analyze());
    await waitFor(() => expect(pending).toHaveLength(2));
    expect(callsOf('restore_preview')[1]?.request).toMatchObject({ database: 'shop_copy' });
    await act(async () => pending[1]!(preview({ plan_id: 'plan-8', confirm: { kind: 'click' } })));
    await screen.findByTestId('restore-preview');
    await u.click(screen.getByLabelText('I understand that the restore changes shop_copy'));
    await u.click(startButton());
    expect(callsOf('restore_start')).toEqual([{ planId: 'plan-8', confirmed: true, name: null }]);
  });

  it('a database list failure is shown; the given database stays', async () => {
    backend({
      list_databases: () => {
        throw fail('not_open');
      },
    });
    open();
    expect(await screen.findByText(/^The databases could not be listed: /)).toBeInTheDocument();
    expect(screen.getByLabelText('Database')).toHaveValue('shop');
  });

  it('a file picked before a restart must be chosen again', async () => {
    backend({
      restore_preview: () => {
        throw appError('config', 'Choose the file in the file dialog.', { field: 'path' });
      },
    });
    const u = user();
    open({ database: 'shop', params: { connection_id: ids.prod, database: 'shop', path: 'C:\\dumps\\shop.sql', single_transaction: true, clean: false, no_owner: false } });
    await u.click(analyze());
    expect(await screen.findByRole('alert')).toHaveTextContent('Choose the file again (files picked before a restart must be chosen again).');
  });

  it('read-only connections cannot analyze', async () => {
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    backend();
    const u = user();
    open();
    await u.click(screen.getByRole('button', { name: 'Browse…' }));
    await screen.findByText('C:\\dumps\\shop.sql');
    expect(screen.getByText('This connection is read-only: restoring is blocked.')).toBeInTheDocument();
    expect(analyze()).toBeDisabled();
  });
});

describe('Run again (restore)', () => {
  const request: RestoreRequest = {
    connection_id: ids.prod,
    database: 'shop',
    path: 'C:\\dumps\\shop.sql',
    single_transaction: false,
    clean: false,
    no_owner: true,
  };

  it('reopens pre-filled and requires Analyze and confirmation again', async () => {
    expect(rerun({ kind: 'restore', request: request as never })).toBe(true);
    expect(useDialogs.getState().request).toEqual({ kind: 'restore', connectionId: ids.prod, database: 'shop', params: request, rerun: expect.any(Number) });
    expect(rerun({ kind: 'tool_restore', request: request as never })).toBe(true);
    expect(useDialogs.getState().request).toMatchObject({ kind: 'restore', params: request });

    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres', env_tag: 'staging' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    backend({ restore_preview: () => preview({ confirm: { kind: 'click' } }) });
    const u = user();
    open({ database: 'shop', params: request });
    expect(screen.getByText('C:\\dumps\\shop.sql')).toBeInTheDocument();
    expect(screen.getByLabelText('No owner')).toBeChecked();
    expect(screen.getByLabelText('Single transaction')).not.toBeChecked();
    // Nothing is analyzed or started by itself.
    expect(callsOf('restore_preview')).toEqual([]);
    expect(startButton()).toBeDisabled();
    await u.click(analyze());
    await screen.findByTestId('restore-preview');
    expect(callsOf('restore_preview')[0]?.request).toEqual(request);
    expect(startButton()).toBeDisabled();
    await u.click(screen.getByLabelText(/I understand/));
    expect(startButton()).toBeEnabled();
  });
});
