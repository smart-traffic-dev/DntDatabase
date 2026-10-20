import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { useQueryDocs } from '../store/queryTabs';
import { useTree } from '../store/tree';
import { connNodeId, groupNodeId } from '../store/treeModel';
import {
  appError,
  calls,
  callsOf,
  config,
  fail,
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
        if (attempts++ === 0) throw fail('timeout_execute');
        return [{ name: 'shop' }];
      },
    });
    await renderTree();
    const user = userEvent.setup();

    await user.dblClick(row('shop-prod'));
    expect(await screen.findByText('The operation timed out.')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Retry' }));

    expect(await screen.findByRole('treeitem', { name: /^shop$/ })).toBeInTheDocument();
    expect(screen.queryByText('The operation timed out.')).not.toBeInTheDocument();
    expect(callsOf('list_databases')).toHaveLength(2);
    // The connection stays open: Retry does not open it again.
    expect(callsOf('open_connection')).toHaveLength(1);
  });

  it('a lost connection offers Reconnect, which reopens it and reloads the node', async () => {
    let attempts = 0;
    backend({
      list_databases: () => {
        if (attempts++ === 0) throw fail('connection_lost');
        return [{ name: 'shop' }];
      },
    });
    await renderTree();
    const user = userEvent.setup();

    await user.dblClick(row('shop-prod'));
    expect(await screen.findByText('The connection to the server was lost.')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Retry' })).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Reconnect' }));

    expect(await screen.findByRole('treeitem', { name: /^shop$/ })).toBeInTheDocument();
    // Reconnect opens the connection again before listing.
    expect(callsOf('open_connection')).toHaveLength(2);
    expect(callsOf('list_databases')).toHaveLength(2);
  });

  it('a cancelled open is not shown as an error', async () => {
    backend({
      open_connection: () => {
        throw fail('cancelled');
      },
    });
    await renderTree();
    await userEvent.setup().dblClick(row('shop-prod'));
    await waitFor(() => expect(callsOf('open_connection')).toHaveLength(1));
    await waitFor(() => expect(screen.queryByText('Loading…')).not.toBeInTheDocument());
    expect(screen.queryByText('The operation was cancelled.')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Retry' })).not.toBeInTheDocument();
  });

  it('asks for the password when the connection needs one', async () => {
    backend({
      open_connection: () => {
        throw fail('password_required');
      },
    });
    await renderTree();
    const user = userEvent.setup();

    await user.dblClick(row('shop-prod'));
    await waitFor(() => expect(useDialogs.getState().prompt).not.toBeNull());
    const prompt = useDialogs.getState().prompt;
    expect(prompt).toMatchObject({ kind: 'password', connectionId: ids.prod });
    expect(prompt?.error.message).toBe('A password is required.');
    expect(row('shop-prod')).toHaveAttribute('aria-expanded', 'false');
    expect(callsOf('list_databases')).toHaveLength(0);
  });

  it('asks to trust the SSH host key when it is unknown', async () => {
    backend({
      open_connection: () => {
        throw fail('ssh_host_key_unknown');

      },
    });
    await renderTree();
    await userEvent.setup().dblClick(row('shop-prod'));

    await waitFor(() => expect(useDialogs.getState().prompt).not.toBeNull());
    expect(useDialogs.getState().prompt).toMatchObject({
      kind: 'hostKey',
      connectionId: ids.prod,
      error: { fingerprint: 'SHA256:abc123' },
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
        if (attempts++ === 0) throw fail('timeout_execute');
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
      'OpenEnter',
      'New query',
      'Edit',
      'Duplicate',
      'New group',
      'RefreshF5',
      'Copy name',
      'Delete',
    ]);
    // Groups of items are divided; Delete is last.
    expect(within(menu).getAllByRole('separator').length).toBeGreaterThanOrEqual(2);

    await user.click(within(menu).getByRole('menuitem', { name: 'Edit' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'editConnection', id: ids.prod });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('moves through the menu with the arrow keys, Home and End', async () => {
    backend();
    await renderTree();
    const user = userEvent.setup();

    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod') });
    const items = within(screen.getByRole('menu')).getAllByRole('menuitem');
    expect(items[0]).toHaveFocus();
    await user.keyboard('{ArrowDown}');
    expect(items[1]).toHaveFocus();
    await user.keyboard('{End}');
    expect(items[items.length - 1]).toHaveFocus();
    await user.keyboard('{ArrowDown}');
    expect(items[0]).toHaveFocus();
    await user.keyboard('{ArrowUp}');
    expect(items[items.length - 1]).toHaveFocus();
    await user.keyboard('{Home}');
    expect(items[0]).toHaveFocus();
  });

  it('clamps the menu into the viewport', async () => {
    backend();
    await renderTree();
    const user = userEvent.setup();
    Object.defineProperty(window, 'innerWidth', { value: 300, configurable: true });
    Object.defineProperty(window, 'innerHeight', { value: 200, configurable: true });
    const rect = vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({
      width: 180, height: 120, top: 0, left: 0, right: 180, bottom: 120, x: 0, y: 0, toJSON: () => ({}),
    });
    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod'), coords: { clientX: 290, clientY: 190 } });
    const menu = screen.getByRole('menu');
    await waitFor(() => expect(parseInt(menu.style.left, 10)).toBeLessThanOrEqual(300 - 180));
    expect(parseInt(menu.style.top, 10)).toBeLessThanOrEqual(200 - 120);
    rect.mockRestore();
  });

  it('a table menu offers a SELECT of the first 1000 rows in a new query tab', async () => {
    backend();
    await renderTree();
    await expandPath(prodNode, dbNode, tablesNode);
    const user = userEvent.setup();

    await user.pointer({ keys: '[MouseRight]', target: row('orders') });
    await user.click(within(screen.getByRole('menu')).getByRole('menuitem', { name: /Select top 1000/ }));
    const docs = [...useQueryDocs.getState().docs.values()];
    expect(docs.at(-1)?.sql).toBe('SELECT * FROM `shop`.`orders` LIMIT 1000;');
  });

  it('marks a read-only connection in the tree', async () => {
    backend({
      list_tree: () => {
        const t = sampleTree();
        t.connections[0] = config(ids.prod, 'shop-prod', { group_id: ids.groupA, read_only: true });
        return t;
      },
    });
    await renderTree();
    expect(within(row('shop-prod')).getByText('RO')).toHaveAttribute('title', 'Read-only connection');
    expect(within(row('analytics')).queryByText('RO')).not.toBeInTheDocument();
  });

  it('shows the full name as a tooltip on the label', async () => {
    backend();
    await renderTree();
    expect(within(row('shop-prod')).getByText('shop-prod')).toHaveAttribute('title', 'shop-prod');
  });

  it('says so when the filter matches nothing', async () => {
    backend();
    await renderTree();
    act(() => useTree.getState().setFilter('zzz'));
    expect(await screen.findByText("No objects match 'zzz' (loaded objects only)")).toBeInTheDocument();
    act(() => useTree.getState().setFilter(''));
    expect(screen.queryByText(/No objects match/)).not.toBeInTheDocument();
  });

  it('shows matches / total on a filtered folder', async () => {
    backend();
    await renderTree();
    await expandPath(prodNode, dbNode, tablesNode);
    expect(row('Tables')).toHaveTextContent('3');
    act(() => useTree.getState().setFilter('ord'));
    await waitFor(() => expect(row('Tables')).toHaveTextContent('1 / 3'));
  });

  it('returns the focus to the tree when the menu closes (Esc, item, outside click)', async () => {
    backend();
    await renderTree();
    const user = userEvent.setup();
    const tree = screen.getByRole('tree');

    await user.click(row('shop-prod'));
    expect(tree).toHaveFocus();
    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod') });
    expect(screen.getByRole('menu')).toContainElement(document.activeElement as HTMLElement);
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
    expect(tree).toHaveFocus();

    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod') });
    await user.click(within(screen.getByRole('menu')).getByRole('menuitem', { name: 'Copy name' }));
    expect(tree).toHaveFocus();

    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod') });
    tree.blur();
    await user.pointer({ keys: '[MouseLeft]', target: document.body });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });

  it('measures the menu at the origin so a right-edge click still fits', async () => {
    backend();
    await renderTree();
    const user = userEvent.setup();
    Object.defineProperty(window, 'innerWidth', { value: 400, configurable: true });
    Object.defineProperty(window, 'innerHeight', { value: 600, configurable: true });
    // A real browser squeezes a fixed box near the right edge: the full width shows only at left 0.
    const rect = vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
      const atOrigin = this.getAttribute('role') === 'menu' && this.style.left === '0px';
      const width = this.getAttribute('role') === 'menu' ? (atOrigin ? 180 : 60) : 0;
      return { width, height: 100, top: 0, left: 0, right: width, bottom: 100, x: 0, y: 0, toJSON: () => ({}) };
    });
    await user.pointer({ keys: '[MouseRight]', target: row('shop-prod'), coords: { clientX: 390, clientY: 10 } });
    const menu = screen.getByRole('menu');
    await waitFor(() => expect(parseInt(menu.style.left, 10)).toBe(400 - 180 - 4));
    rect.mockRestore();
  });

  it('shows the filter scope hint while filtering', async () => {
    backend();
    await renderTree();
    expect(screen.queryByText('Searching loaded objects')).not.toBeInTheDocument();
    act(() => useTree.getState().setFilter('shop'));
    expect(await screen.findByText('Searching loaded objects')).toBeInTheDocument();
  });

  it('shows the hint of an error under its message', async () => {
    backend({
      list_databases: () => {
        throw appError('connect_failed', 'Could not connect to the database server.', { hint: 'Check the host and port.' });
      },
    });
    await renderTree();
    const user = userEvent.setup();
    await user.dblClick(row('shop-prod'));
    expect(await screen.findByText('Could not connect to the database server.')).toBeInTheDocument();
    expect(screen.getByText('Check the host and port.')).toBeInTheDocument();
  });
});
