import { create } from 'zustand';
import { api, toAppError } from '../api';
import { commands, type AppError, type Favorite, type FavoriteInput } from '../bindings';

interface FavoritesState {
  /** In the backend's order: root first, then by folder path, inside a folder by position. */
  favorites: Favorite[];
  loaded: boolean;
  /** Why the last load failed (shown in the panel with Retry). */
  error: AppError | null;
  /** Never throws. */
  load(): Promise<void>;
  /** The changes reject with the backend error (a dialog shows it), e.g. SQL holding a secret. */
  create(input: FavoriteInput): Promise<Favorite>;
  update(id: string, input: FavoriteInput): Promise<Favorite>;
  rename(id: string, name: string): Promise<Favorite>;
  move(id: string, folder: string | null): Promise<Favorite>;
  remove(id: string): Promise<void>;
  reset(): void;
}

/** The backend order (see `favorites_list`), so a change shows where a reload would put it. */
export function sortFavorites(list: Favorite[]): Favorite[] {
  const cmp = (a: string, b: string) => a.localeCompare(b, undefined, { sensitivity: 'base' });
  return [...list].sort((a, b) => {
    if ((a.folder === null) !== (b.folder === null)) return a.folder === null ? -1 : 1;
    const folder = cmp(a.folder ?? '', b.folder ?? '');
    if (folder !== 0) return folder;
    if (a.sort !== b.sort) return a.sort - b.sort;
    return cmp(a.name, b.name) || a.id.localeCompare(b.id);
  });
}

/** Every folder path in use, ancestors included (`Reports/Daily` gives `Reports` too), sorted. */
export function folderPaths(list: Favorite[]): string[] {
  const paths = new Set<string>();
  for (const f of list) {
    if (!f.folder) continue;
    const parts = f.folder.split('/');
    for (let i = 1; i <= parts.length; i++) paths.add(parts.slice(0, i).join('/'));
  }
  return [...paths].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: 'base' }));
}

export const useFavorites = create<FavoritesState>()((set, get) => {
  const put = (fav: Favorite) => {
    set({ favorites: sortFavorites([...get().favorites.filter((f) => f.id !== fav.id), fav]) });
    return fav;
  };
  return {
    favorites: [],
    loaded: false,
    error: null,
    load: async () => {
      try {
        // Straight through the bindings: the panel shows a failed load itself.
        const result = await commands.favoritesList();
        if (result.status === 'error') throw result.error;
        set({ favorites: Array.isArray(result.data) ? result.data : [], loaded: true, error: null });
      } catch (e) {
        set({ error: toAppError(e) });
      }
    },
    create: async (input) => put(await api.favoritesCreate(input)),
    update: async (id, input) => put(await api.favoritesUpdate(id, input)),
    rename: async (id, name) => put(await api.favoritesRename(id, name)),
    move: async (id, folder) => put(await api.favoritesMove(id, folder)),
    remove: async (id) => {
      await api.favoritesDelete(id);
      set({ favorites: get().favorites.filter((f) => f.id !== id) });
    },
    reset: () => set({ favorites: [], loaded: false, error: null }),
  };
});
