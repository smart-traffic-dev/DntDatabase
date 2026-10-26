import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { objectTabId, useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import {
  Failure,
  backendError,
  callsOf,
  fail,
  ids,
  mockBackend,
  openResult,
  resetStores,
  sampleTree,
  serverInfo,
} from '../test/backend';
import { DialogHost } from './DialogHost';

beforeEach(resetStores);

const seed = async () => {
  await useTree.getState().reloadStored();
};

describe('DialogHost', () => {
  it('the transaction close question names the connection and PRODUCTION', () => {
    useDialogs.getState().ask({
      kind: 'txClose',
      title: 'Query 1',
      connectionName: 'shop-prod',
      envTag: 'production',
      settle: async () => {},
      done: () => {},
    });
    render(<DialogHost />);
    const dlg = screen.getByRole('alertdialog');
    expect(dlg).toHaveTextContent('open transaction on shop-prod');
    expect(within(dlg).getByTestId('env-chip')).toBeInTheDocument();
  });

  it('a saved connection expands every ancestor group', async () => {
    const tree = sampleTree();
    tree.groups = [...tree.groups, { id: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc', parent_id: ids.groupB, name: 'Nested', sort_order: 0 }];
    const created = { ...tree.connections[2]!.config, group_id: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc' };
    tree.connections = [{ ...tree.connections[2]!, config: created }];
    mockBackend({ list_tree: () => tree, create_connection: () => created });
    await seed();
    useDialogs.getState().requestDialog({ kind: 'newConnection' });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.type(screen.getByTestId('conn-host'), 'h');
    await user.type(screen.getByTestId('conn-name'), 'legacy-crm');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(useTree.getState().selectedId).toBe(`conn:${ids.legacy}`));
    expect(useTree.getState().expanded.has('group:cccccccc-cccc-4ccc-8ccc-cccccccccccc')).toBe(true);
    expect(useTree.getState().expanded.has(`group:${ids.groupB}`)).toBe(true);
  });

  it('a new connection saved into a group expands the group and selects the row', async () => {
    const created = sampleTree().connections[2]!.config;
    mockBackend({ list_tree: () => sampleTree(), create_connection: () => created });
    await seed();
    useDialogs.getState().requestDialog({ kind: 'newConnection' });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.type(screen.getByTestId('conn-host'), 'h');
    await user.type(screen.getByTestId('conn-name'), 'legacy-crm');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(useTree.getState().selectedId).toBe(`conn:${ids.legacy}`));
    expect(useTree.getState().expanded.has(`group:${ids.groupB}`)).toBe(true);
  });

  it('password prompt opens the connection and expands it', async () => {
    const info = serverInfo('MySql8');
    mockBackend({
      list_tree: () => sampleTree(),
      open_connection: (a) => {
        if (a.prompt === null) throw fail('password_required');
        return openResult(info);
      },
      list_databases: () => [{ name: 'shop' }],
    });
    await seed();
    // Ask mode: the database password is always needed.
    const stored = useConnections.getState().summaries.get(ids.prod)!;
    useConnections.getState().summaries.set(ids.prod, { ...stored, config: { ...stored.config, secret_mode: 'ask' } });
    useDialogs.getState().showPrompt({
      kind: 'password',
      connectionId: ids.prod,
      error: backendError('password_required'),
    });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.type(screen.getByLabelText('Database password'), 'pw');
    await user.click(screen.getByRole('button', { name: 'Connect' }));
    await waitFor(() => expect(useConnections.getState().info.has(ids.prod)).toBe(true));
    expect(callsOf('open_connection')[0]).toMatchObject({ id: ids.prod, prompt: { db_password: 'pw' } });
    expect(useDialogs.getState().prompt).toBeNull();
    await waitFor(() => expect(useTree.getState().expanded.has(`conn:${ids.prod}`)).toBe(true));
  });

  it('host key retry hitting another unknown key leaves a usable dialog', async () => {
    const unknown = (fp: string) =>
      backendError('ssh_host_key_unknown', { fingerprint: fp, key_openssh: `ssh-ed25519 ${fp}` });

    mockBackend({ list_tree: () => sampleTree(), trust_host_key: () => null, open_connection: () => { throw new Failure(unknown('SHA256:second')); } });
    await seed();
    useDialogs.getState().showPrompt({ kind: 'hostKey', connectionId: ids.prod, error: unknown('SHA256:first') });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.click(screen.getByRole('button', { name: 'Trust and connect' }));
    expect(await screen.findByText('SHA256:second')).toBeInTheDocument();
    const trust = screen.getByRole('button', { name: 'Trust and connect' });
    expect(trust).toBeEnabled();
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeEnabled();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(useDialogs.getState().prompt).toBeNull();
    expect(callsOf('trust_host_key')).toHaveLength(1);
  });

  it('host key mismatch on tree open shows the warning without a trust button', async () => {
    mockBackend({
      list_tree: () => sampleTree(),
      open_connection: () => {
        throw fail('ssh_host_key_mismatch');
      },
    });
    await seed();
    useTree.getState().expand(`conn:${ids.prod}`);
    await waitFor(() => expect(useDialogs.getState().prompt).not.toBeNull());
    const user = userEvent.setup();
    render(<DialogHost />);
    const dialog = screen.getByRole('alertdialog');
    expect(dialog).toHaveTextContent('has CHANGED');
    expect(dialog).toHaveTextContent('Remove the old key from known_hosts manually to continue');
    expect(screen.queryByRole('button', { name: /trust/i })).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Close' }));
    expect(useDialogs.getState().prompt).toBeNull();
  });

  it('asks only for the key passphrase when the stored SSH key is encrypted', async () => {
    mockBackend({ list_tree: () => sampleTree(), open_connection: () => openResult(serverInfo('MySql8')), list_databases: () => [] });
    await seed();
    // shop-prod: Saved mode, SSH key auth; pretend a stale SSH secret flag is set.
    const stored = useConnections.getState().summaries.get(ids.prod)!;
    useConnections.getState().summaries.set(ids.prod, { ...stored, has_ssh_secret: true });
    useDialogs.getState().showPrompt({ kind: 'password', connectionId: ids.prod, error: backendError('key_passphrase_required') });
    const user = userEvent.setup();
    render(<DialogHost />);
    expect(screen.queryByLabelText('Database password')).not.toBeInTheDocument();
    await user.type(screen.getByLabelText('Key passphrase'), 'kp');
    await user.click(screen.getByRole('button', { name: 'Connect' }));
    await waitFor(() => expect(callsOf('open_connection')).toHaveLength(1));
    expect(callsOf('open_connection')[0]).toMatchObject({ id: ids.prod, prompt: { ssh_key_passphrase: 'kp' } });
  });

  it('cancelling the prompt leaves the connection collapsed', async () => {
    mockBackend({ list_tree: () => sampleTree() });
    await seed();
    useDialogs.getState().showPrompt({ kind: 'password', connectionId: ids.legacy, error: backendError('password_required') });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(useDialogs.getState().prompt).toBeNull();
    expect(useTree.getState().expanded.has(`conn:${ids.legacy}`)).toBe(false);
  });

  it('an internal error shows a toast that opens the log folder', async () => {
    mockBackend({
      list_tree: () => {
        throw fail('internal');
      },
      open_log_folder: () => null,
    });
    await seed();
    const user = userEvent.setup();
    render(<DialogHost />);
    const toast = await screen.findByRole('alert', { name: 'Internal error' });
    expect(toast).toHaveTextContent('An internal error occurred.');
    await user.click(within(toast).getByRole('button', { name: 'Open log folder' }));
    expect(callsOf('open_log_folder')).toHaveLength(1);
    await user.click(within(toast).getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByRole('alert', { name: 'Internal error' })).not.toBeInTheDocument();
  });

  it('a notice toast can be dismissed, and showing it again restarts its timer', () => {
    vi.useFakeTimers();
    try {
      render(<DialogHost />);
      act(() => useDialogs.getState().setNotice('Imported 3 connections'));
      expect(screen.getByRole('status')).toHaveTextContent('Imported 3 connections');
      act(() => vi.advanceTimersByTime(3000));
      // The same text again: a fresh 4 s, not the 1 s left of the first one.
      act(() => useDialogs.getState().setNotice('Imported 3 connections'));
      act(() => vi.advanceTimersByTime(3000));
      expect(screen.getByRole('status')).toHaveTextContent('Imported 3 connections');
      act(() => vi.advanceTimersByTime(1500));
      expect(screen.queryByRole('status')).toBeNull();

      act(() => useDialogs.getState().setNotice('Saved'));
      fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
      expect(screen.queryByRole('status')).toBeNull();
      expect(useDialogs.getState().notice).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('other errors do not raise the internal toast', async () => {
    mockBackend({
      list_tree: () => {
        throw fail('connection_lost');
      },
    });
    await seed();
    render(<DialogHost />);
    expect(screen.queryByRole('alert', { name: 'Internal error' })).not.toBeInTheDocument();
  });

  it('settings toggles debug logging', async () => {
    let level = 'Info';
    mockBackend({
      get_log_level: () => level,
      set_log_level: (a) => {
        level = a.level as string;
        return null;
      },
      get_table_page_size: () => 100,
      get_confirm_unbounded_writes: () => true,
      get_idle_tx_warn_minutes: () => 5,
      get_jobs_per_connection: () => 2,
      get_external_tools_dir: () => null,
    });
    useDialogs.getState().requestDialog({ kind: 'settings', tab: 'advanced' });
    const user = userEvent.setup();
    render(<DialogHost />);
    const box = await screen.findByRole('checkbox', { name: /Debug logging/ });
    await waitFor(() => expect(box).toBeEnabled());
    expect(box).not.toBeChecked();
    await user.click(box);
    await waitFor(() => expect(callsOf('set_log_level')).toEqual([{ level: 'Debug' }]));
    expect(box).toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Close' }));
    expect(useDialogs.getState().request).toBeNull();
  });

  it('settings shows and saves the table data page size', async () => {
    let size = 100;
    mockBackend({
      get_log_level: () => 'Info',
      get_table_page_size: () => size,
      set_table_page_size: (a) => {
        size = a.size as number;
        return null;
      },
      get_confirm_unbounded_writes: () => true,
      get_idle_tx_warn_minutes: () => 5,
      get_jobs_per_connection: () => 2,
      get_external_tools_dir: () => null,
    });
    useDialogs.getState().requestDialog({ kind: 'settings', tab: 'queryData' });
    const user = userEvent.setup();
    render(<DialogHost />);
    const input = await screen.findByRole('spinbutton', { name: 'Table data page size' });
    await waitFor(() => expect(input).toHaveValue(100));
    // Out of range: refused with a message, nothing saved.
    await user.clear(input);
    await user.type(input, '5{Enter}');
    expect(await screen.findByRole('alert')).toHaveTextContent('Enter a whole number from 10 to 10,000.');
    expect(callsOf('set_table_page_size')).toEqual([]);
    await user.clear(input);
    await user.type(input, '250{Enter}');
    await waitFor(() => expect(callsOf('set_table_page_size')).toEqual([{ size: 250 }]));
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    const { useAppSettings } = await import('../store/appSettings');
    expect(useAppSettings.getState().tablePageSize).toBe(250);
  });

  it('settings toggles the unbounded-write confirmation (on by default)', async () => {
    let on = true;
    mockBackend({
      get_log_level: () => 'Info',
      get_table_page_size: () => 100,
      get_confirm_unbounded_writes: () => on,
      get_idle_tx_warn_minutes: () => 5,
      get_jobs_per_connection: () => 2,
      get_external_tools_dir: () => null,
      set_confirm_unbounded_writes: (a) => {
        on = a.enabled as boolean;
        return null;
      },
    });
    useDialogs.getState().requestDialog({ kind: 'settings', tab: 'queryData' });
    const user = userEvent.setup();
    render(<DialogHost />);
    const box = await screen.findByRole('checkbox', { name: 'Confirm UPDATE/DELETE without WHERE' });
    await waitFor(() => expect(box).toBeEnabled());
    expect(box).toBeChecked();
    expect(box).toHaveAccessibleDescription(/Local and untagged connections\. Production and Staging connections always ask\./);
    await user.click(box);
    await waitFor(() => expect(callsOf('set_confirm_unbounded_writes')).toEqual([{ enabled: false }]));
    expect(box).not.toBeChecked();
    await user.click(box);
    await waitFor(() => expect(callsOf('set_confirm_unbounded_writes')).toHaveLength(2));
    expect(callsOf('set_confirm_unbounded_writes')[1]).toEqual({ enabled: true });
    expect(box).toBeChecked();
  });

  it('a failed unbounded-write save keeps the box as it was and shows the error', async () => {
    mockBackend({
      get_log_level: () => 'Info',
      get_table_page_size: () => 100,
      get_confirm_unbounded_writes: () => true,
      get_idle_tx_warn_minutes: () => 5,
      get_jobs_per_connection: () => 2,
      get_external_tools_dir: () => null,
      set_confirm_unbounded_writes: () => {
        throw fail('busy');
      },
    });
    useDialogs.getState().requestDialog({ kind: 'settings', tab: 'queryData' });
    const user = userEvent.setup();
    render(<DialogHost />);
    const box = await screen.findByRole('checkbox', { name: 'Confirm UPDATE/DELETE without WHERE' });
    await waitFor(() => expect(box).toBeEnabled());
    await user.click(box);
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    expect(box).toBeChecked();
  });

  it('deleting a connection closes its tabs', async () => {
    mockBackend({ list_tree: () => sampleTree(), delete_connection: () => null });
    const orders = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
    for (const connectionId of [ids.prod, ids.analytics]) {
      useTabs.getState().open({ id: objectTabId(connectionId, orders), kind: 'object', connectionId, objectRef: orders, title: 'orders' });
    }
    useDialogs.getState().requestDialog({ kind: 'deleteNode', node: { type: 'connection', id: ids.prod }, label: 'shop-prod' });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.click(screen.getByRole('button', { name: 'Delete connection' }));
    await waitFor(() => expect(useDialogs.getState().request).toBeNull());
    expect(useTabs.getState().tabs.map((t) => t.connectionId)).toEqual([ids.analytics]);
  });

  it('new group creates it and reloads the tree', async () => {
    mockBackend({
      list_tree: () => sampleTree(),
      create_group: (a) => ({ id: ids.groupA, parent_id: a.parentId, name: a.name, sort_order: 0 }),
    });
    useDialogs.getState().requestDialog({ kind: 'newGroup', parentId: null });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.type(screen.getByLabelText('Group name'), 'Team');
    await user.click(screen.getByRole('button', { name: 'Create' }));
    await waitFor(() => expect(useDialogs.getState().request).toBeNull());
    expect(callsOf('create_group')[0]).toEqual({ name: 'Team', parentId: null });
    expect(callsOf('list_tree')).toHaveLength(1);
  });

  it('delete confirmation deletes the connection', async () => {
    mockBackend({ list_tree: () => sampleTree(), delete_connection: () => null });
    useDialogs.getState().requestDialog({
      kind: 'deleteNode',
      node: { type: 'connection', id: ids.prod },
      label: 'shop-prod',
    });
    const user = userEvent.setup();
    render(<DialogHost />);
    expect(screen.getByRole('alertdialog')).toHaveTextContent('shop-prod');
    await user.click(screen.getByRole('button', { name: 'Delete connection' }));
    await waitFor(() => expect(useDialogs.getState().request).toBeNull());
    expect(callsOf('delete_connection')[0]).toEqual({ id: ids.prod });
  });

  it('editing an open connection closes its runtime in the tree', async () => {
    mockBackend({
      list_tree: () => sampleTree(),
      update_connection: (a) => a.cfg,
      close_connection: () => null,
    });
    await seed();
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    useDialogs.getState().requestDialog({ kind: 'editConnection', id: ids.prod });
    const user = userEvent.setup();
    render(<DialogHost />);
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(useDialogs.getState().request).toBeNull());
    await waitFor(() => expect(useConnections.getState().info.has(ids.prod)).toBe(false));
  });
});
