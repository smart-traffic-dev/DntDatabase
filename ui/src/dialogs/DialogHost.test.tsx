import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
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
    await user.click(screen.getByRole('button', { name: 'Delete' }));
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
