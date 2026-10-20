// Favorites and snippets actions shared by the sidebar panel, the query toolbar, the
// window shortcuts and (at merge) the menu bar: `addActiveQueryToFavorites`,
// `openSnippetsManager` and `listFavoritesForMenu` are its menu hooks.
import type { Favorite } from '../bindings';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useFavorites } from '../store/favorites';
import { useQueryDocs } from '../store/queryTabs';
import { targetOf } from '../store/runQuery';
import { useTabs } from '../store/tabs';

/**
 * Opens a favorite in a new query tab, bound to its connection and database when that
 * connection still exists, unbound otherwise. Returns the tab id.
 */
export function openFavorite(fav: Favorite): string {
  const known = !!fav.connection_id && useConnections.getState().summaries.has(fav.connection_id);
  const docs = useQueryDocs.getState();
  const id = docs.newQuery(known ? fav.connection_id : null, known ? fav.database : null);
  docs.update(id, { sql: fav.sql, cursor: 0, title: fav.name });
  return id;
}

/**
 * Asks to save query tab `tabId` as a favorite: its selection if there is one, else the whole
 * text. Returns false (with a notice) when there is nothing to save.
 */
export function addQueryToFavorites(tabId: string): boolean {
  const doc = useQueryDocs.getState().docs.get(tabId);
  const dialogs = useDialogs.getState();
  if (!doc) {
    dialogs.setNotice('Open a query tab to add it to favorites.');
    return false;
  }
  const target = targetOf(tabId, 'selection');
  const selected =
    target && typeof target === 'object' && 'Selection' in target ? doc.sql.slice(target.Selection[0], target.Selection[1]) : '';
  const fromSelection = selected.trim() !== '';
  const sql = fromSelection ? selected : doc.sql;
  if (!sql.trim()) {
    dialogs.setNotice('The query is empty: there is nothing to add to favorites.');
    return false;
  }
  dialogs.requestDialog({
    kind: 'addFavorite',
    sql,
    name: doc.title,
    connectionId: doc.connectionId,
    database: doc.database,
    fromSelection,
  });
  return true;
}

/** Menu hook (and Ctrl+Shift+D): "Add to favorites" for the active query tab. */
export function addActiveQueryToFavorites(): boolean {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  if (tab?.kind !== 'query') {
    useDialogs.getState().setNotice('Open a query tab to add it to favorites.');
    return false;
  }
  return addQueryToFavorites(tab.id);
}

/** Menu hook: opens the snippets manager. */
export function openSnippetsManager(): void {
  useDialogs.getState().requestDialog({ kind: 'snippets' });
}

export interface FavoriteMenuItem {
  id: string;
  /** `Folder/Name`, or the name at the root. */
  label: string;
  open(): void;
}

/** How many favorites the menu lists. */
export const MENU_FAVORITES = 20;

/** Menu hook: the first 20 favorites (panel order) for a Navicat-like Favorites menu. */
export function listFavoritesForMenu(): FavoriteMenuItem[] {
  return useFavorites
    .getState()
    .favorites.slice(0, MENU_FAVORITES)
    .map((f) => ({
      id: f.id,
      label: f.folder ? `${f.folder}/${f.name}` : f.name,
      open: () => void openFavorite(f),
    }));
}
