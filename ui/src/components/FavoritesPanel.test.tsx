import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Favorite } from '../bindings';
import { useConnections } from '../store/connections';
import { useFavorites } from '../store/favorites';
import { useQueryDocs } from '../store/queryTabs';
import { useSnippets } from '../store/snippets';
import { callsOf, config, ids, mockBackend, resetStores } from '../test/backend';
import { fav } from '../test/favorites';
import { FavoritesPanel, favoriteRows } from './FavoritesPanel';

const list: Favorite[] = [
  fav('r1', 'Users', { connection_id: ids.prod, database: 'shop', sql: 'SELECT * FROM users' }),
  fav('d1', 'Orders today', { folder: 'Reports/Daily' }),
  fav('d2', 'Revenue', { folder: 'Reports', sort: 1 }),
  fav('a1', 'Locks', { folder: 'Admin' }),
];

function backend(over: Record<string, (args: Record<string, unknown>) => unknown> = {}) {
  return mockBackend({
    favorites_list: () => list,
    snippets_list: () => [],
    ...over,
  });
}

const labels = () => screen.queryAllByRole('treeitem').map((r) => r.textContent);

beforeEach(() => {
  resetStores();
  useFavorites.getState().reset();
  useSnippets.getState().reset();
  try {
    localStorage.clear();
  } catch {
    // ignore
  }
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
});

describe('favoriteRows', () => {
  it('puts folders first, nests paths and filters by name or folder', () => {
    const rows = favoriteRows(list, new Set(), '');
    expect(rows.map((r) => (r.kind === 'folder' ? `[${r.path}]` : r.fav.name))).toEqual([
      '[Admin]',
      'Locks',
      '[Reports]',
      '[Reports/Daily]',
      'Orders today',
      'Revenue',
      'Users',
    ]);
    expect(favoriteRows(list, new Set(['Reports']), '').map((r) => r.key)).not.toContain('f:d1');
    // A filter opens collapsed folders and keeps only what matches.
    const filtered = favoriteRows(list, new Set(['Reports']), 'daily');
    expect(filtered.map((r) => r.key)).toEqual(['d:Reports', 'd:Reports/Daily', 'f:d1']);
  });
});

describe('FavoritesPanel', () => {
  it('loads favorites and snippets at startup and shows folders', async () => {
    backend();
    render(<FavoritesPanel />);
    await waitFor(() => expect(screen.getAllByTestId('favorite-row')).toHaveLength(4));
    expect(callsOf('favorites_list')).toHaveLength(1);
    expect(callsOf('snippets_list')).toHaveLength(1);
    expect(labels()[0]).toContain('Admin');
    // A bound favorite shows its connection and database.
    expect(screen.getByText('shop-prod / shop')).toBeInTheDocument();
  });

  it('collapses a folder', async () => {
    backend();
    const user = userEvent.setup();
    render(<FavoritesPanel />);
    await waitFor(() => expect(screen.getAllByTestId('favorite-row')).toHaveLength(4));
    const folder = screen.getAllByTestId('favorite-folder').find((f) => f.textContent?.startsWith('Reports'));
    await user.dblClick(folder!);
    expect(screen.getAllByTestId('favorite-row')).toHaveLength(2);
  });

  it('is a full-height view: no collapse toggle, Manage snippets is an icon button', async () => {
    backend();
    render(<FavoritesPanel />);
    await waitFor(() => expect(screen.getAllByTestId('favorite-row')).toHaveLength(4));
    expect(screen.queryByRole('button', { name: /^Favorites/ })).toBeNull();
    const snippets = screen.getByRole('button', { name: 'Manage snippets' });
    expect(snippets.querySelector('svg')).not.toBeNull();
    expect(snippets).not.toHaveTextContent('{ }');
  });

  it('hidden keeps it mounted (favorites still load) but out of view', async () => {
    backend();
    render(<FavoritesPanel hidden />);
    await waitFor(() => expect(callsOf('favorites_list')).toHaveLength(1));
    expect(screen.getByTestId('favorites-panel')).not.toBeVisible();
  });

  it('filters with the filter box', async () => {
    backend();
    const user = userEvent.setup();
    render(<FavoritesPanel />);
    await waitFor(() => expect(screen.getAllByTestId('favorite-row')).toHaveLength(4));
    await user.type(screen.getByRole('searchbox', { name: 'Filter favorites' }), 'rev');
    expect(screen.getAllByTestId('favorite-row').map((r) => r.textContent)).toEqual(['Revenue']);
    await user.clear(screen.getByRole('searchbox', { name: 'Filter favorites' }));
    await user.type(screen.getByRole('searchbox', { name: 'Filter favorites' }), 'zzz');
    expect(screen.getByText("No favorites match 'zzz'")).toBeInTheDocument();
  });

  it('double-click and Enter open the favorite in a new bound query tab', async () => {
    backend();
    const user = userEvent.setup();
    render(<FavoritesPanel />);
    const row = await screen.findByText('Users');
    await user.dblClick(row);
    const docs = [...useQueryDocs.getState().docs.values()];
    expect(docs).toHaveLength(1);
    expect(docs[0]).toMatchObject({ connectionId: ids.prod, database: 'shop', sql: 'SELECT * FROM users', title: 'Users' });

    // Keyboard: select Locks (second row) and press Enter; it has no connection.
    const tree = screen.getByRole('tree');
    act(() => tree.focus());
    await user.keyboard('{Home}{ArrowDown}{Enter}');
    const opened = [...useQueryDocs.getState().docs.values()].find((d) => d.title === 'Locks');
    expect(opened).toMatchObject({ connectionId: null, database: null });
  });

  it('F2 renames, Delete confirms, and the context menu moves and copies', async () => {
    backend({
      favorites_rename: ({ id, name }) => ({ ...list[0]!, id: id as string, name: name as string }),
      favorites_delete: () => null,
      favorites_move: ({ id, folder }) => ({ ...list[0]!, id: id as string, name: 'All users', folder: folder as string | null }),
    });
    const user = userEvent.setup();
    // After setup: user-event installs its own clipboard.
    const writeText = vi.spyOn(navigator.clipboard, 'writeText');
    render(<FavoritesPanel />);
    await user.click(await screen.findByText('Users'));
    act(() => screen.getByRole('tree').focus());

    await user.keyboard('{F2}');
    const rename = screen.getByRole('dialog', { name: 'Rename favorite' });
    const input = within(rename).getByLabelText('Name');
    await user.clear(input);
    await user.type(input, 'All users{Enter}');
    await waitFor(() => expect(callsOf('favorites_rename')).toEqual([{ id: 'r1', name: 'All users' }]));
    expect(await screen.findByText('All users')).toBeInTheDocument();

    // Context menu: Copy SQL.
    await user.pointer({ keys: '[MouseRight]', target: screen.getByText('All users') });
    await user.click(screen.getByRole('menuitem', { name: 'Copy SQL' }));
    expect(writeText).toHaveBeenCalledWith('SELECT * FROM users');

    // Context menu: Move to folder.
    await user.pointer({ keys: '[MouseRight]', target: screen.getByText('All users') });
    await user.click(screen.getByRole('menuitem', { name: 'Move to folder…' }));
    const move = screen.getByRole('dialog', { name: /Move/ });
    await user.type(within(move).getByLabelText('Folder'), 'Archive');
    await user.click(within(move).getByRole('button', { name: 'Move' }));
    await waitFor(() => expect(callsOf('favorites_move')).toEqual([{ id: 'r1', folder: 'Archive' }]));
    expect(await screen.findByText('Archive')).toBeInTheDocument();

    // Delete asks first.
    await user.click(screen.getByText('All users'));
    act(() => screen.getByRole('tree').focus());
    await user.keyboard('{Delete}');
    const confirm = screen.getByRole('alertdialog', { name: 'Delete favorite' });
    await user.click(within(confirm).getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(callsOf('favorites_delete')).toEqual([{ id: 'r1' }]));
    expect(screen.queryByText('All users')).toBeNull();
  });

  it('shows a failed load with Retry', async () => {
    let fail = true;
    backend({
      favorites_list: () => {
        if (fail) throw new Error('boom');
        return list;
      },
    });
    const user = userEvent.setup();
    render(<FavoritesPanel />);
    expect(await screen.findByRole('button', { name: 'Retry' })).toBeInTheDocument();
    fail = false;
    await user.click(screen.getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(screen.getAllByTestId('favorite-row')).toHaveLength(4));
  });
});
