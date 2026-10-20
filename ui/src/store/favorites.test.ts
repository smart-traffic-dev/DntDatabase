import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Favorite, Snippet } from '../bindings';
import { addActiveQueryToFavorites, addQueryToFavorites, listFavoritesForMenu, openFavorite, openSnippetsManager } from '../components/favoritesActions';
import { onQueryShortcut } from '../components/queryShortcuts';
import { appError, callsOf, config, ids, mockBackend, resetStores, Failure } from '../test/backend';
import { fav, snip } from '../test/favorites';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { folderPaths, sortFavorites, useFavorites } from './favorites';
import { useQueryDocs } from './queryTabs';
import { registerEditor, unregisterEditor } from './runQuery';
import { useSnippets } from './snippets';
import { setUserSnippets } from './snippetsBridge';
import { useTabs } from './tabs';

vi.mock('./snippetsBridge', () => ({ setUserSnippets: vi.fn() }));

beforeEach(() => {
  resetStores();
  useFavorites.getState().reset();
  useSnippets.getState().reset();
  vi.mocked(setUserSnippets).mockClear();
});

describe('favorites store', () => {
  it('loads, creates, renames, moves and deletes', async () => {
    const a = fav('a', 'Orders', { folder: 'Reports' });
    mockBackend({
      favorites_list: () => [a],
      favorites_create: ({ input }) => fav('b', (input as Favorite).name, { sort: 0 }),
      favorites_rename: ({ id, name }) => ({ ...a, id: id as string, name: name as string }),
      favorites_move: ({ id, folder }) => ({ ...a, id: id as string, folder: folder as string | null, name: 'Daily', sort: 1 }),
      favorites_delete: () => null,
    });
    await useFavorites.getState().load();
    expect(useFavorites.getState().favorites).toEqual([a]);
    expect(useFavorites.getState().loaded).toBe(true);

    await useFavorites.getState().create({ name: 'Users', folder: null, sql: 'SELECT 1', connection_id: null, database: null });
    // Root before folders, as the backend lists them.
    expect(useFavorites.getState().favorites.map((f) => f.name)).toEqual(['Users', 'Orders']);

    await useFavorites.getState().rename('a', 'Daily');
    expect(callsOf('favorites_rename')).toEqual([{ id: 'a', name: 'Daily' }]);
    await useFavorites.getState().move('a', null);
    expect(useFavorites.getState().favorites.map((f) => f.name)).toEqual(['Users', 'Daily']);

    await useFavorites.getState().remove('b');
    expect(useFavorites.getState().favorites.map((f) => f.id)).toEqual(['a']);
  });

  it('a failed load is kept as the panel error, never thrown or toasted', async () => {
    mockBackend({ favorites_list: () => { throw new Failure(appError('internal', 'disk full')); } });
    await useFavorites.getState().load();
    expect(useFavorites.getState().error?.message).toBe('disk full');
    expect(useDialogs.getState().internalError).toBeNull();
  });

  it('a refused save rejects with the backend message', async () => {
    mockBackend({
      favorites_create: () => {
        throw new Failure(appError('config', 'This SQL contains a password or another secret literal', { field: 'sql' }));
      },
    });
    await expect(
      useFavorites.getState().create({ name: 'x', folder: null, sql: "CREATE USER u IDENTIFIED BY 'x'", connection_id: null, database: null }),
    ).rejects.toMatchObject({ kind: 'config' });
    expect(useFavorites.getState().favorites).toEqual([]);
  });

  it('orders like the backend and lists every folder path', () => {
    const list = [
      fav('1', 'z', { folder: 'Reports/Daily', sort: 1 }),
      fav('2', 'y', { folder: 'Reports/Daily', sort: 0 }),
      fav('3', 'x', { folder: 'admin' }),
      fav('4', 'w'),
    ];
    expect(sortFavorites(list).map((f) => f.name)).toEqual(['w', 'x', 'y', 'z']);
    expect(folderPaths(list)).toEqual(['admin', 'Reports', 'Reports/Daily']);
  });
});

describe('snippets store', () => {
  it('loads and pushes every change to the completion bridge', async () => {
    mockBackend({
      snippets_list: () => [snip('2', 'sel'), snip('1', 'Ins')],
      snippets_create: ({ input }) => ({ ...snip('3', (input as Snippet).prefix) }),
      snippets_update: ({ id, input }) => ({ ...snip(id as string, (input as Snippet).prefix), label: (input as Snippet).label }),
      snippets_delete: () => null,
    });
    await useSnippets.getState().load();
    expect(useSnippets.getState().snippets.map((s) => s.prefix)).toEqual(['Ins', 'sel']);
    expect(setUserSnippets).toHaveBeenLastCalledWith([
      { prefix: 'Ins', label: 'Label Ins', body: 'SELECT ${1:*} FROM ${2:t};', description: '' },
      { prefix: 'sel', label: 'Label sel', body: 'SELECT ${1:*} FROM ${2:t};', description: '' },
    ]);

    await useSnippets.getState().create({ prefix: 'del', label: 'Delete', body: 'DELETE FROM $1', description: '' });
    expect(vi.mocked(setUserSnippets).mock.lastCall?.[0].map((s) => s.prefix)).toEqual(['del', 'Ins', 'sel']);

    await useSnippets.getState().update('2', { prefix: 'sel', label: 'Select rows', body: 'SELECT 1', description: '' });
    expect(useSnippets.getState().snippets.find((s) => s.id === '2')?.label).toBe('Select rows');

    await useSnippets.getState().remove('1');
    expect(vi.mocked(setUserSnippets).mock.lastCall?.[0].map((s) => s.prefix)).toEqual(['del', 'sel']);
  });

  it('a failed load keeps the error and pushes nothing', async () => {
    mockBackend({ snippets_list: () => { throw new Failure(appError('internal', 'nope')); } });
    await useSnippets.getState().load();
    expect(useSnippets.getState().error?.message).toBe('nope');
    expect(setUserSnippets).not.toHaveBeenCalled();
  });
});

describe('favorites actions (menu hooks)', () => {
  it('opens a favorite bound to its connection, or unbound when the connection is gone', () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    const bound = openFavorite(fav('a', 'Orders', { connection_id: ids.prod, database: 'shop', sql: 'SELECT * FROM orders' }));
    const doc = useQueryDocs.getState().docs.get(bound);
    expect(doc).toMatchObject({ connectionId: ids.prod, database: 'shop', sql: 'SELECT * FROM orders', title: 'Orders' });
    expect(useTabs.getState().activeId).toBe(bound);

    const gone = openFavorite(fav('b', 'Old', { connection_id: ids.legacy, database: 'x' }));
    expect(useQueryDocs.getState().docs.get(gone)).toMatchObject({ connectionId: null, database: null });
  });

  it('adds the selection if any, else the whole query', () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    useQueryDocs.getState().update(id, { sql: 'SELECT 1;\nSELECT 2;' });
    expect(addQueryToFavorites(id)).toBe(true);
    expect(useDialogs.getState().request).toEqual({
      kind: 'addFavorite',
      sql: 'SELECT 1;\nSELECT 2;',
      name: 'Query 1',
      connectionId: ids.prod,
      database: 'shop',
      fromSelection: false,
    });

    const pos = (offset: number) => ({ lineNumber: 1, column: offset + 1 });
    const editor = {
      getModel: () => ({ getOffsetAt: (p: { column: number }) => p.column - 1 }),
      getSelection: () => ({ isEmpty: () => false, getStartPosition: () => pos(0), getEndPosition: () => pos(8) }),
      getPosition: () => pos(0),
    };
    registerEditor(id, editor);
    try {
      addQueryToFavorites(id);
      expect(useDialogs.getState().request).toMatchObject({ sql: 'SELECT 1', fromSelection: true });
    } finally {
      unregisterEditor(id);
    }
  });

  it('refuses an empty query or a non-query tab with a notice', () => {
    expect(addActiveQueryToFavorites()).toBe(false);
    expect(useDialogs.getState().notice).toMatch(/Open a query tab/);
    useQueryDocs.getState().newQuery(null, null);
    expect(addActiveQueryToFavorites()).toBe(false);
    expect(useDialogs.getState().notice).toMatch(/empty/);
    expect(useDialogs.getState().request).toBeNull();
  });

  it('Ctrl+Shift+D asks to add the active query', () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { sql: 'SELECT 42' });
    window.addEventListener('keydown', onQueryShortcut, true);
    try {
      document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'D', ctrlKey: true, shiftKey: true, bubbles: true, cancelable: true }));
    } finally {
      window.removeEventListener('keydown', onQueryShortcut, true);
    }
    expect(useDialogs.getState().request).toMatchObject({ kind: 'addFavorite', sql: 'SELECT 42' });
  });

  it('lists the first 20 favorites for the menu and opens the snippets manager', () => {
    useFavorites.setState({
      favorites: Array.from({ length: 25 }, (_, i) => fav(`f${i}`, `Fav ${i}`, { folder: i === 0 ? 'Reports' : null })),
    });
    const items = listFavoritesForMenu();
    expect(items).toHaveLength(20);
    expect(items[0]?.label).toBe('Reports/Fav 0');
    items[1]?.open();
    expect(useQueryDocs.getState().docs.size).toBe(1);

    openSnippetsManager();
    expect(useDialogs.getState().request).toEqual({ kind: 'snippets' });
  });
});
