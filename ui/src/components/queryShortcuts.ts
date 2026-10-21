import { useLayout } from '../store/layout';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { openDevtools } from './appMenus';
import { addActiveQueryToFavorites } from './favoritesActions';
import { onGroupShortcut } from './groupShortcuts';
import { openNewQuery } from './newQuery';
import { showCommandPalette, showQuickOpen } from '../store/palette';
import { closeTab, cycleTab, goToTab, reopenClosedTab } from './tabActions';
import { focusConnectionTree, openNewConnection, openSettings, openShortcuts, toggleResults, zoomIn, zoomOut, zoomReset } from './windowActions';

/** Stops the key for everyone else too (Monaco binds F1, Ctrl+E and others itself). */
function consume(e: KeyboardEvent) {
  e.preventDefault();
  e.stopPropagation();
}

/**
 * Navigation and window shortcuts: Ctrl+Shift+P command palette, Ctrl+P quick open,
 * Ctrl+1..8 / Ctrl+9 tab N / last tab, Ctrl+J results panel, Ctrl+E focus the tree,
 * Ctrl+Shift+N new connection, Ctrl+, settings, Ctrl+= / Ctrl+- / Ctrl+0 code font zoom.
 * Returns true when the key was one of them (already consumed).
 */
function windowShortcut(e: KeyboardEvent, key: string): boolean {
  const plain = !e.shiftKey;
  const run = (action: () => void) => {
    consume(e);
    action();
    return true;
  };
  if (key === 'p') return run(e.shiftKey ? showCommandPalette : showQuickOpen);
  if (plain && /^[1-9]$/.test(key)) return run(() => goToTab(Number(key), key === '9'));
  if (plain && key === 'j') return run(toggleResults);
  if (plain && key === 'e') return run(focusConnectionTree);
  if (e.shiftKey && key === 'n') return run(openNewConnection);
  if (plain && key === ',') return run(openSettings);
  // Ctrl+Shift+= is Ctrl++ on most layouts.
  if (key === '=' || key === '+' || e.code === 'NumpadAdd') return run(zoomIn);
  if (key === '-' || key === '_' || e.code === 'NumpadSubtract') return run(zoomOut);
  if (plain && (key === '0' || e.code === 'Numpad0')) {
    // In an editable data grid Ctrl+0 means "set the cell to NULL": the grid gets it.
    if (e.target instanceof Element && e.target.closest('[data-editing-grid]')) return false;
    return run(zoomReset);
  }
  return false;
}

/**
 * Window-level query shortcuts (spec §7.1): `Ctrl+S` save (`Ctrl+Shift+S` save as),
 * `Ctrl+O` open a .sql file, `Ctrl+T` new query, `Ctrl+Shift+D` add to favorites,
 * `Ctrl+W` close tab, `Ctrl+B` show / hide the sidebar, `Ctrl+Shift+I` developer tools, `Ctrl+Tab` / `Ctrl+Shift+Tab` / `Ctrl+PgDn` / `Ctrl+PgUp` next / previous tab.
 * Ignored while a modal dialog is open.
 */
export function onQueryShortcut(e: KeyboardEvent) {
  if (!document.querySelector('[aria-modal="true"]') && onGroupShortcut(e)) return;
  if (e.altKey || e.repeat) return;
  const ctrl = e.ctrlKey || e.metaKey;
  if (!ctrl && !(e.key === 'F1' && !e.shiftKey)) return;
  if (document.querySelector('[aria-modal="true"]')) return;
  const key = e.key.toLowerCase();
  // F1 is the app's keyboard shortcuts everywhere, Monaco's own F1 palette included
  // (that one is the palette command "Editor: Command palette").
  if (!ctrl) {
    consume(e);
    openShortcuts();
    return;
  }
  // Fields outside the editor (the sidebar filter, toolbar selects, the palette's own input)
  // keep their own keys; only the palette keys reach through, they mean the same everywhere.
  const target = e.target instanceof Element ? e.target : null;
  const inField = !!target?.closest('input, textarea, select, [contenteditable="true"]') && !target.closest('.sql-editor');
  if (inField && key !== 'p') return;
  const docs = useQueryDocs.getState();
  if (windowShortcut(e, key)) return;
  // Intentional: Ctrl+PageUp/PageDown switch tabs (as in VS Code) and override Monaco's own
  // Ctrl+PageUp/PageDown editor scroll, because this listener runs in the capture phase.
  if (key === 'tab' || key === 'pagedown' || key === 'pageup') {
    e.preventDefault();
    cycleTab(key === 'pageup' || (key === 'tab' && e.shiftKey) ? -1 : 1);
  } else if (key === 'w' && !e.shiftKey) {
    e.preventDefault();
    const { tabs, activeId } = useTabs.getState();
    const tab = tabs.find((t) => t.id === activeId);
    if (tab) void closeTab(tab);
  } else if (key === 's') {
    e.preventDefault();
    const { tabs, activeId } = useTabs.getState();
    const tab = tabs.find((t) => t.id === activeId);
    if (tab?.kind === 'query') void docs.saveFile(tab.id, e.shiftKey);
  } else if (key === 'o' && !e.shiftKey) {
    e.preventDefault();
    void docs.openFile();
  } else if (key === 't' && !e.shiftKey) {
    e.preventDefault();
    openNewQuery();
  } else if (key === 't' && e.shiftKey) {
    e.preventDefault();
    reopenClosedTab();
  } else if (key === 'd' && e.shiftKey) {
    // Ctrl+Shift+D: add the active query (its selection, else all of it) to favorites.
    e.preventDefault();
    addActiveQueryToFavorites();
  } else if (key === 'b' && !e.shiftKey) {
    e.preventDefault();
    useLayout.getState().toggleSidebar();
  } else if (key === 'i' && e.shiftKey) {
    e.preventDefault();
    void openDevtools();
  }
}
