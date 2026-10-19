import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { connNodeId, groupNodeId } from '../store/treeModel';
import {
  Failure,
  appError,
  calls,
  callsOf,
  ids,
  mockBackend,
  openResult,
  resetStores,
  sampleTree,
  serverInfo,
} from '../test/backend';
import { ConnectionTree } from './ConnectionTree';

const prodNode = connNodeId(ids.prod);
const dbNode = `${prodNode}/db:shop`;
const tablesNode = `${dbNode}/folder:Table`;

const row = (name: string) => screen.getByRole('treeitem', { name: new RegExp(name) });
const rowLabels = () => screen.getAllByTestId('tree-row').map((r) => r.querySelector('.label')?.textContent ?? '');

/** Expands nodes one after the other, waiting for each one's children to load. */
async function expandPath(...path: string[]) {
  for (const id of path) {
    act(() => useTree.getState().expand(id));
    await waitFor(() => expect(useTree.getState().childIds.has(id)).toBe(true));
  }
}

function backend(extra: Parameters<typeof mockBackend>[0] = {}) {
  return mockBackend({
    list_tree: () => sampleTree(),
    open_connection: () => openResult(serverInfo('MySql8')),
    list_databases: () => [{ name: 'shop' }, { name: 'blog' }],
    list_objects: () => [
      { name: 'customers', kind: 'Table', comment: null, row_estimate: null },
      { name: 'orders', kind: 'Table', comment: null, row_estimate: null },
      { name: 'products', kind: 'Table', comment: null, row_estimate: null },
    ],
    refresh_node: () => null,
    ...extra,
  });
}

async function renderTree() {
  await useTree.getState().reloadStored();
  useTree.getState().expand(groupNodeId(ids.groupA));
  useTree.getState().expand(groupNodeId(ids.groupB));
  return render(<ConnectionTree />);
}

describe('ConnectionTree', () => {
  beforeEach(() => {
    resetStores();
  });

  it('renders 2 groups and 3 connections with their engine chips', async () => {
    backend();
    await renderTree();

    expect(screen.getByRole('tree', { name: 'Connections' })).toBeInTheDocument();
    for (const group of ['Production', 'Local']) expect(row(group)).toHaveAttribute('aria-level', '1');
    const chips = (name: string) => within(row(name)).getByText(/^(MY|PG|MA)$/).textContent;
    expect(chips('shop-prod')).toBe('MY');
    expect(chips('analytics')).toBe('PG');
    expect(chips('legacy-crm')).toBe('MY');
    expect(row('shop-prod')).toHaveAttribute('aria-level', '2');
    expect(within(row('shop-prod')).getByText('SSH')).toBeInTheDocument();
    expect(screen.getAllByTestId('tree-row')).toHaveLength(5);

    // The variant is only known once the server answered: a MariaDB shows MA.
    backend({ open_connection: () => openResult(serverInfo('MariaDb', [10, 6, 16])) });
    act(() => useTree.getState().expand(connNodeId(ids.legacy)));
    await waitFor(() => expect(within(row('legacy-crm')).getByText('MA')).toBeInTheDocument());
  });

  it('opens the connection then lists databases, showing Loading… meanwhile', async () => {
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => (release = r));
    backend({
      open_connection: async () => {
        await gate;
        return openResult(serverInfo('MySql8'));
      },
    });
    await renderTree();
    const user = userEvent.setup();

    await user.dblClick(row('shop-prod'));
    expect(await screen.findByText('Loading…')).toBeInTheDocument();
    expect(row('shop-prod')).toHaveAttribute('aria-expanded', 'true');

    release();
    expect(await screen.findByRole('treeitem', { name: /^shop$/ })).toBeInTheDocument();
    expect(screen.queryByText('Loading…')).not.toBeInTheDocument();
    expect(calls()).toEqual(['list_tree', 'open_connection', 'list_databases']);
    expect(callsOf('open_connection')[0]).toEqual({ id: ids.prod, prompt: null });
    expect(callsOf('list_databases')[0]).toEqual({ id: ids.prod, showSystem: false });
  });

  it('shows the error under the node with a Retry button', async () => {
    let attempts = 0;
    backend({
      list_databases: () => {
        if (attempts++ === 0) throw new Failure(appError('server', 'Access denied for user', { server_code: '1045' }));
        return [{ name: 'shop' }];
      },
    });
    await renderTree();
    const user = userEvent.setup();

    await user.dblClick(row('shop-prod'));
    expect(await screen.findByText('Access denied for user')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Retry' }));

    expect(await screen.findByRole('treeitem', { name: /^shop$/ })).toBeInTheDocument();
    expect(screen.queryByText('Access denied for user')).not.toBeInTheDocument();
    expect(callsOf('list_databases')).toHaveLength(2);
    // The connection stays open: Retry does not open it again.
    expect(callsOf('open_connection')).toHaveLength(1);
  });

  it('asks for the password when the connection needs one', async () => {
    backend({
      open_connection: () => {
        throw new Failure(appError('secret', 'password_required'));
      },
    });
    await renderTree();
    const user = userEvent.setup();

    await user.dblClick(row('shop-prod'));
    await waitFor(() => expect(useDialogs.getState().prompt).not.toBeNull());
    const prompt = useDialogs.getState().prompt;
    expect(prompt).toMatchObject({ kind: 'password', connectionId: ids.prod });
    expect(prompt?.error.message).toBe('password_required');
    expect(row('shop-prod')).toHaveAttribute('aria-expanded', 'false');
    expect(callsOf('list_databases')).toHaveLength(0);
  });

  it('asks to trust the SSH host key when it is unknown', async () => {
    backend({
      open_connection: () => {
        throw new Failure(
          appError('ssh_host_key_unknown', 'unknown host key', { host: 'bastion-01', port: 22, fingerprint: 'SHA256:abc', key_openssh: 'ssh-ed25519 AAAA' }),
        );
      },
    });
    await renderTree();
    await userEvent.setup().dblClick(row('shop-prod'));

    await waitFor(() => expect(useDialogs.getState().prompt).not.toBeNull());
    expect(useDialogs.getState().prompt).toMatchObject({
      kind: 'hostKey',
      connectionId: ids.prod,
      error: { fingerprint: 'SHA256:abc' },
    });
  });

  it('filters case-insensitively, keeping the parents of the matches', async () => {
    backend();
    await renderTree();
    await expandPath(prodNode, dbNode, tablesNode);
    expect(screen.getByRole('treeitem', { name: /customers/ })).toBeInTheDocument();

    act(() => useTree.getState().setFilter('ORD'));
    await waitFor(() => expect(rowLabels()).toHaveLength(5));
    expect(rowLabels()).toEqual(['Production', 'shop-prod', 'shop', 'Tables', 'orders']);
    expect(screen.queryByRole('treeitem', { name: /customers/ })).not.toBeInTheDocument();

    act(() => useTree.getState().setFilter(''));
    await waitFor(() => expect(screen.getByRole('treeitem', { name: /customers/ })).toBeInTheDocument());
  });

  it('F5 on the selected node refreshes its key, then reloads it', async () => {
    backend();
    await renderTree();
    await expandPath(prodNode, dbNode, tablesNode);
    const user = userEvent.setup();

    await user.click(row('Tables'));
    expect(useTree.getState().selectedId).toBe(tablesNode);
    await user.keyboard('{F5}');

    await waitFor(() => expect(callsOf('list_objects')).toHaveLength(2));
    expect(callsOf('refresh_node')).toEqual([{ id: ids.prod, key: ['db:shop', 'folder:Table'] }]);
    const sequence = calls();
    expect(sequence.indexOf('refresh_node')).toBeLessThan(sequence.lastIndexOf('list_objects'));
    expect(callsOf('list_objects')[1]).toEqual({
      id: ids.prod,
      scope: { database: 'shop', schema: null },
      kind: 'Table',
    });
    expect(screen.getByRole('treeitem', { name: /orders/ })).toBeInTheDocument();
  });

  it('moves the selection and expands/collapses from the keyboard', async () => {
    backend();
    await renderTree();
    const user = userEvent.setup();

    await user.click(row('Production'));
    await user.keyboard('{ArrowDown}');
    expect(useTree.getState().selectedId).toBe(prodNode);
    await user.keyboard('{ArrowDown}');
    expect(useTree.getState().selectedId).toBe(connNodeId(ids.analytics));
    await user.keyboard('{ArrowUp}');
    expect(useTree.getState().selectedId).toBe(prodNode);

    await user.keyboard('{ArrowRight}');
    await waitFor(() => expect(row('shop-prod')).toHaveAttribute('aria-expanded', 'true'));
    await screen.findByRole('treeitem', { name: /^shop$/ });
    await user.keyboard('{ArrowLeft}');
    expect(row('shop-prod')).toHaveAttribute('aria-expanded', 'false');
    // Left on a collapsed node selects its parent group.
    await user.keyboard('{ArrowLeft}');
    expect(useTree.getState().selectedId).toBe(groupNodeId(ids.groupA));

    await user.keyboard('{Enter}');
    expect(row('Production')).toHaveAttribute('aria-expanded', 'false');
    await user.keyboard('{Enter}');
    expect(row('Production')).toHaveAttribute('aria-expanded', 'true');
  });

  it('Enter on the Retry button retries instead of toggling the node', async () => {
    let attempts = 0;
    backend({
      list_databases: () => {
        if (attempts++ === 0) throw new Failure(appError('server', 'boom'));
        return [{ name: 'shop' }];
      },
    });
    await renderTree();
    const user = userEvent.setup();
    await user.dblClick(row('shop-prod'));
    const retry = await screen.findByRole('button', { name: 'Retry' });
    retry.focus();
    await user.keyboard('{Enter}');

    expect(await screen.findByRole('treeitem', { name: /^shop$/ })).toBeInTheDocument();
    expect(callsOf('list_databases')).toHaveLength(2);
    expect(row('shop-prod')).toHaveAttribute('aria-expanded', 'true');
  });

  it('opens the context menu of a connection with its actions', async () => {
    backend();
    await renderTree();
    const user = userEvent.setup();

    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod') });
    const menu = screen.getByRole('menu');
    expect(within(menu).getAllByRole('menuitem').map((i) => i.textContent)).toEqual([
      'Open',
      'Edit',
      'Duplicate',
      'Delete',
      'New group',
      'RefreshF5',
      'Copy name',
    ]);

    await user.click(within(menu).getByRole('menuitem', { name: 'Edit' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'editConnection', id: ids.prod });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });
});
