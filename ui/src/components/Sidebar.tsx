import { useRef, useState, type KeyboardEvent } from 'react';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { ROOT } from '../store/treeModel';
import { ActionMenu } from './ActionMenu';
import { ConnectionTree } from './ConnectionTree';
import { FavoritesPanel } from './FavoritesPanel';
import { MoreIcon, PlusIcon } from './icons';

type View = 'objects' | 'favorites';
const VIEW_KEY = 'dnt.sidebar.view';
const VIEWS: { id: View; label: string }[] = [
  { id: 'objects', label: 'Objects' },
  { id: 'favorites', label: 'Favorites' },
];

function readView(): View {
  try {
    return localStorage.getItem(VIEW_KEY) === 'favorites' ? 'favorites' : 'objects';
  } catch {
    return 'objects';
  }
}

export function Sidebar() {
  const filter = useTree((s) => s.filter);
  const setFilter = useTree((s) => s.setFilter);
  const showSystem = useTree((s) => s.showSystem);
  const error = useTree((s) => s.error);
  const requestDialog = useDialogs((s) => s.requestDialog);
  // Loaded and nothing stored: show the first-start call to action instead of a blank tree.
  const treeEmpty = useTree((s) => s.childIds.get(ROOT)?.length === 0);
  const storedCount = useConnections((c) => c.summaries.size);
  const noConnections = treeEmpty && storedCount === 0;
  const [view, setViewState] = useState<View>(readView);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const setView = (next: View) => {
    try {
      localStorage.setItem(VIEW_KEY, next);
    } catch {
      // Storage unavailable: the view resets to Objects next start.
    }
    setViewState(next);
  };

  // Roving focus between the two segments (WAI-ARIA tabs, automatic activation).
  const onTabKey = (e: KeyboardEvent, i: number) => {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
    e.preventDefault();
    const next = (i + (e.key === 'ArrowRight' ? 1 : -1) + VIEWS.length) % VIEWS.length;
    const target = VIEWS[next];
    if (!target) return;
    setView(target.id);
    tabRefs.current[next]?.focus();
  };

  return (
    <aside className="sidebar">
      <div className="sidebar-bar">
        <div className="segmented" role="tablist" aria-label="Sidebar view">
          {VIEWS.map((v, i) => (
            <button
              key={v.id}
              ref={(el) => {
                tabRefs.current[i] = el;
              }}
              type="button"
              role="tab"
              className={`segment${view === v.id ? ' on' : ''}`}
              aria-selected={view === v.id}
              tabIndex={view === v.id ? 0 : -1}
              onClick={() => setView(v.id)}
              onKeyDown={(e) => onTabKey(e, i)}
            >
              {v.label}
            </button>
          ))}
        </div>
        <button
          type="button"
          className="icon-btn"
          data-testid="new-connection"
          aria-label="New connection"
          title="New connection (Ctrl+Shift+N)"
          onClick={() => requestDialog({ kind: 'newConnection' })}
        >
          <PlusIcon />
        </button>
        <button
          type="button"
          className="icon-btn"
          aria-label="Sidebar options"
          title="Sidebar options"
          aria-haspopup="menu"
          aria-expanded={menu !== null}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            setMenu({ x: r.left, y: r.bottom });
          }}
        >
          <MoreIcon />
        </button>
      </div>
      {menu && (
        <ActionMenu
          label="Sidebar options"
          x={menu.x}
          y={menu.y}
          items={[
            { label: 'Show system schemas', checked: showSystem, run: () => useTree.getState().setShowSystem(!showSystem) },
            { separator: true },
            { label: 'Refresh all', run: () => void useTree.getState().reloadStored() },
            { label: 'Collapse all', run: () => useTree.setState({ expanded: new Set() }) },
          ]}
          onClose={() => setMenu(null)}
        />
      )}
      {/* Always mounted: it loads favorites (the Favorites menu) and snippets (completion) at startup. */}
      <FavoritesPanel hidden={view !== 'favorites'} />
      {view === 'objects' && (
        <>
          <div className="sidebar-head">
            <input
              type="search"
              aria-label="Filter objects"
              placeholder="Filter objects…"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
          </div>
          {error && (
            <div className="banner error" role="alert">
              <span>{error.message}</span>
              <button type="button" className="btn small" onClick={() => void useTree.getState().reloadStored()}>
                Retry
              </button>
            </div>
          )}
          {noConnections ? (
            <div className="sidebar-empty" data-testid="sidebar-empty">
              <strong>No connections yet</strong>
              <span>Add a database connection to browse its tables.</span>
              <button type="button" className="btn primary" onClick={() => requestDialog({ kind: 'newConnection' })}>
                New connection
              </button>
              <button type="button" className="btn" onClick={() => requestDialog({ kind: 'import' })}>
                Import…
              </button>
            </div>
          ) : (
            <ConnectionTree />
          )}
        </>
      )}
    </aside>
  );
}
