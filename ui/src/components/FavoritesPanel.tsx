import { useEffect, useId, useMemo, useState, type KeyboardEvent, type MouseEvent } from 'react';
import type { Favorite } from '../bindings';
import { ConfirmDialog, NameDialog } from '../dialogs/SimpleDialogs';
import { MoveFavoriteDialog } from '../dialogs/AddFavoriteDialog';
import { useConnections } from '../store/connections';
import { useFavorites } from '../store/favorites';
import { useSnippets } from '../store/snippets';
import { ActionMenu, type MenuEntry } from './ActionMenu';
import { openFavorite, openSnippetsManager } from './favoritesActions';
import { ChevronIcon, FolderIcon } from './icons';
import './favorites.css';

const StarIcon = () => (
  <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
    <path
      d="M8 1.8l1.9 3.9 4.2.6-3 3 .7 4.2L8 11.5l-3.8 2 .7-4.2-3-3 4.2-.6z"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.2"
      strokeLinejoin="round"
    />
  </svg>
);

export type FavoriteRow =
  | { kind: 'folder'; key: string; path: string; name: string; depth: number; expanded: boolean; count: number }
  | { kind: 'favorite'; key: string; fav: Favorite; depth: number };

interface FolderNode {
  path: string;
  name: string;
  folders: Map<string, FolderNode>;
  items: Favorite[];
  count: number;
}

const byName = (a: string, b: string) => a.localeCompare(b, undefined, { sensitivity: 'base' });

/**
 * The visible rows: folders (alphabetical) before the favorites of each level (in their
 * stored order). A filter keeps favorites whose name or folder path contains it (any case)
 * and opens every folder on the way.
 */
export function favoriteRows(favorites: Favorite[], collapsed: ReadonlySet<string>, filter: string): FavoriteRow[] {
  const q = filter.trim().toLowerCase();
  const root: FolderNode = { path: '', name: '', folders: new Map(), items: [], count: 0 };
  for (const fav of favorites) {
    if (q && !fav.name.toLowerCase().includes(q) && !(fav.folder ?? '').toLowerCase().includes(q)) continue;
    let node = root;
    node.count++;
    for (const part of fav.folder ? fav.folder.split('/') : []) {
      const path = node.path ? `${node.path}/${part}` : part;
      let next = node.folders.get(part);
      if (!next) {
        next = { path, name: part, folders: new Map(), items: [], count: 0 };
        node.folders.set(part, next);
      }
      node = next;
      node.count++;
    }
    node.items.push(fav);
  }
  const rows: FavoriteRow[] = [];
  const walk = (node: FolderNode, depth: number) => {
    for (const child of [...node.folders.values()].sort((a, b) => byName(a.name, b.name))) {
      const expanded = !!q || !collapsed.has(child.path);
      rows.push({ kind: 'folder', key: `d:${child.path}`, path: child.path, name: child.name, depth, expanded, count: child.count });
      if (expanded) walk(child, depth + 1);
    }
    for (const fav of node.items) rows.push({ kind: 'favorite', key: `f:${fav.id}`, fav, depth });
  };
  walk(root, 0);
  return rows;
}

const PANEL_KEY = 'dnt.favorites.open';

function readOpen(): boolean {
  try {
    return localStorage.getItem(PANEL_KEY) !== '0';
  } catch {
    return true;
  }
}

function writeOpen(open: boolean) {
  try {
    localStorage.setItem(PANEL_KEY, open ? '1' : '0');
  } catch {
    // Storage unavailable: the panel just opens expanded next time.
  }
}

type Pending = { kind: 'rename' | 'move' | 'delete'; fav: Favorite };

const copySql = (fav: Favorite) => void navigator.clipboard?.writeText(fav.sql).catch(() => undefined);

/** "Favorites" under the connection tree: saved queries in folders (Navicat's saved queries). */
export function FavoritesPanel() {
  const favorites = useFavorites((s) => s.favorites);
  const error = useFavorites((s) => s.error);
  const summaries = useConnections((s) => s.summaries);
  const [open, setOpen] = useState(readOpen);
  const [filter, setFilter] = useState('');
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; row: FavoriteRow } | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const idPrefix = useId();
  const listId = `${idPrefix}-list`;

  useEffect(() => {
    // Startup: the favorites for this panel, and the user's snippets for the editor's completion.
    void useFavorites.getState().load();
    void useSnippets.getState().load();
  }, []);

  const rows = useMemo(() => favoriteRows(favorites, collapsed, filter), [favorites, collapsed, filter]);
  const selectedIndex = rows.findIndex((r) => r.key === selectedKey);
  const rowId = (i: number) => `${idPrefix}-row-${i}`;

  const toggleFolder = (path: string, expand?: boolean) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      const isOpen = !prev.has(path);
      if (expand ?? !isOpen) next.delete(path);
      else next.add(path);
      return next;
    });

  const toggleOpen = () => {
    setOpen((o) => {
      writeOpen(!o);
      return !o;
    });
  };

  const activate = (row: FavoriteRow) => {
    if (row.kind === 'favorite') openFavorite(row.fav);
    else toggleFolder(row.path);
  };

  const menuItems = (row: FavoriteRow): MenuEntry[] => {
    if (row.kind === 'folder') {
      return [{ label: row.expanded ? 'Collapse' : 'Expand', run: () => toggleFolder(row.path) }];
    }
    const fav = row.fav;
    return [
      { label: 'Open', hint: 'Enter', run: () => void openFavorite(fav) },
      { separator: true },
      { label: 'Rename', hint: 'F2', run: () => setPending({ kind: 'rename', fav }) },
      { label: 'Move to folder…', run: () => setPending({ kind: 'move', fav }) },
      { label: 'Copy SQL', run: () => copySql(fav) },
      { separator: true },
      { label: 'Delete', hint: 'Del', danger: true, run: () => setPending({ kind: 'delete', fav }) },
    ];
  };

  const showMenu = (row: FavoriteRow, e: MouseEvent) => {
    e.preventDefault();
    setSelectedKey(row.key);
    setMenu({ x: e.clientX, y: e.clientY, row });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (rows.length === 0) return;
    const at = selectedIndex;
    const row = at >= 0 ? rows[at] : undefined;
    const select = (i: number) => {
      e.preventDefault();
      const target = rows[Math.max(0, Math.min(rows.length - 1, i))];
      if (target) {
        setSelectedKey(target.key);
        document.getElementById(rowId(rows.indexOf(target)))?.scrollIntoView?.({ block: 'nearest' });
      }
    };
    switch (e.key) {
      case 'ArrowDown':
        return select(at + 1);
      case 'ArrowUp':
        return select(at < 0 ? rows.length - 1 : at - 1);
      case 'Home':
        return select(0);
      case 'End':
        return select(rows.length - 1);
      case 'ArrowRight':
        if (row?.kind === 'folder') {
          e.preventDefault();
          if (!row.expanded) toggleFolder(row.path, true);
          else select(at + 1);
        }
        return;
      case 'ArrowLeft': {
        if (!row) return;
        e.preventDefault();
        if (row.kind === 'folder' && row.expanded && !filter.trim()) return toggleFolder(row.path, false);
        // Up to the enclosing folder.
        for (let i = at - 1; i >= 0; i--) {
          const r = rows[i];
          if (r?.kind === 'folder' && r.depth < row.depth) return select(i);
        }
        return;
      }
      case 'Enter':
        if (row) {
          e.preventDefault();
          activate(row);
        }
        return;
      case 'F2':
        if (row?.kind === 'favorite') {
          e.preventDefault();
          setPending({ kind: 'rename', fav: row.fav });
        }
        return;
      case 'Delete':
        if (row?.kind === 'favorite') {
          e.preventDefault();
          setPending({ kind: 'delete', fav: row.fav });
        }
        return;
      case 'ContextMenu':
      case 'F10':
        if (row && (e.key === 'ContextMenu' || e.shiftKey)) {
          e.preventDefault();
          const box = document.getElementById(rowId(at))?.getBoundingClientRect();
          setMenu({ x: box ? box.left + 24 : 0, y: box ? box.bottom : 0, row });
        }
        return;
    }
  };

  const connectionLabel = (fav: Favorite) => {
    if (!fav.connection_id) return null;
    const name = summaries.get(fav.connection_id)?.config.name;
    if (!name) return null;
    return fav.database ? `${name} / ${fav.database}` : name;
  };

  return (
    <section className={`fav-panel${open ? ' open' : ''}`} aria-label="Favorites" data-testid="favorites-panel">
      <div className="fav-head">
        <button type="button" className="fav-toggle" aria-expanded={open} aria-controls={listId} onClick={toggleOpen}>
          <span className={`chev-icon${open ? ' open' : ''}`}>
            <ChevronIcon />
          </span>
          <span>Favorites</span>
          {favorites.length > 0 && <span className="meta">{favorites.length}</span>}
        </button>
        <button
          type="button"
          className="icon-btn fav-snippets"
          aria-label="Manage snippets"
          title="Snippets…"
          onClick={openSnippetsManager}
        >
          {'{ }'}
        </button>
      </div>
      {open && (
        <>
          <div className="sidebar-head fav-filter">
            <input
              type="search"
              aria-label="Filter favorites"
              placeholder="Filter favorites…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
          </div>
          {error && (
            <div className="banner error" role="status">
              <span>{error.message}</span>
              <button type="button" className="btn small" onClick={() => void useFavorites.getState().load()}>
                Retry
              </button>
            </div>
          )}
          <div
            id={listId}
            className="tree fav-tree"
            role="tree"
            aria-label="Favorites list"
            tabIndex={0}
            aria-activedescendant={selectedIndex >= 0 ? rowId(selectedIndex) : undefined}
            onKeyDown={onKeyDown}
          >
            {rows.map((row, i) => {
              const selected = row.key === selectedKey;
              const indent = { paddingLeft: 4 + row.depth * 14 };
              if (row.kind === 'folder') {
                return (
                  <div
                    key={row.key}
                    id={rowId(i)}
                    role="treeitem"
                    aria-level={row.depth + 1}
                    aria-expanded={row.expanded}
                    aria-selected={selected}
                    data-testid="favorite-folder"
                    className={`tree-row kind-group${selected ? ' selected' : ''}`}
                    style={indent}
                    onClick={() => setSelectedKey(row.key)}
                    onDoubleClick={() => toggleFolder(row.path)}
                    onContextMenu={(e) => showMenu(row, e)}
                  >
                    <span
                      className="chev"
                      onClick={(e) => {
                        e.stopPropagation();
                        setSelectedKey(row.key);
                        toggleFolder(row.path);
                      }}
                    >
                      <span className={`chev-icon${row.expanded ? ' open' : ''}`}>
                        <ChevronIcon />
                      </span>
                    </span>
                    <FolderIcon />
                    <span className="label" title={row.path}>
                      {row.name}
                    </span>
                    <span className="meta">{row.count}</span>
                  </div>
                );
              }
              const target = connectionLabel(row.fav);
              return (
                <div
                  key={row.key}
                  id={rowId(i)}
                  role="treeitem"
                  aria-level={row.depth + 1}
                  aria-selected={selected}
                  data-testid="favorite-row"
                  className={`tree-row kind-favorite${selected ? ' selected' : ''}`}
                  style={indent}
                  title={row.fav.sql.length > 300 ? `${row.fav.sql.slice(0, 300)}…` : row.fav.sql}
                  onClick={() => setSelectedKey(row.key)}
                  onDoubleClick={() => openFavorite(row.fav)}
                  onContextMenu={(e) => showMenu(row, e)}
                >
                  <span className="chev" />
                  <StarIcon />
                  <span className="label">{row.fav.name}</span>
                  {target && <span className="meta fav-target">{target}</span>}
                </div>
              );
            })}
            {rows.length === 0 && (
              <div className="tree-empty fav-empty" role="status">
                {filter.trim()
                  ? `No favorites match '${filter.trim()}'`
                  : 'No favorites yet. Use Add to favorites (Ctrl+Shift+D) in a query tab.'}
              </div>
            )}
          </div>
        </>
      )}
      {menu && (
        <ActionMenu
          label={`${menu.row.kind === 'folder' ? menu.row.name : menu.row.fav.name} actions`}
          x={menu.x}
          y={menu.y}
          items={menuItems(menu.row)}
          onClose={() => setMenu(null)}
        />
      )}
      {pending?.kind === 'rename' && (
        <NameDialog
          title="Rename favorite"
          label="Name"
          submitLabel="Rename"
          initial={pending.fav.name}
          onClose={() => setPending(null)}
          onSubmit={async (name) => {
            await useFavorites.getState().rename(pending.fav.id, name);
            setPending(null);
          }}
        />
      )}
      {pending?.kind === 'move' && <MoveFavoriteDialog favorite={pending.fav} onClose={() => setPending(null)} />}
      {pending?.kind === 'delete' && (
        <ConfirmDialog
          title="Delete favorite"
          message={`Delete the favorite "${pending.fav.name}"?`}
          confirmLabel="Delete"
          onClose={() => setPending(null)}
          onConfirm={async () => {
            await useFavorites.getState().remove(pending.fav.id);
            setPending(null);
          }}
        />
      )}
    </section>
  );
}
