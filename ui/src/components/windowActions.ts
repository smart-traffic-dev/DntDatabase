import { useDialogs } from '../store/dialogs';
import { FONT_SCALE_DEFAULT, useLayout } from '../store/layout';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';

/** Window-level actions behind the shortcuts and menu items (Ctrl+E, Ctrl+J, zoom, dialogs). */

const TREE = '[role="tree"][aria-label="Connections"]';

/**
 * Ctrl+E: shows the sidebar if hidden, selects the tree's selected node (else its first one)
 * and moves the keyboard focus into the tree.
 */
export function focusConnectionTree(): void {
  const layout = useLayout.getState();
  if (!layout.sidebarVisible) layout.toggleSidebar();
  const tree = useTree.getState();
  if (!tree.selectedId || !tree.nodes.has(tree.selectedId)) {
    const first = tree.visibleRows().find((r) => r.type === 'node');
    if (first) tree.select(first.key);
  }
  const focus = () => {
    const el = document.querySelector<HTMLElement>(TREE);
    if (!el) return false;
    el.focus();
    return true;
  };
  // A sidebar that was hidden mounts the tree on the next render.
  if (!focus()) setTimeout(focus, 0);
}

/** Ctrl+J: shows / hides the results of the focused editor group. */
export const toggleResults = () => useLayout.getState().toggleGroupResults(useTabs.getState().focusedGroupId);
/** Ctrl+Shift+J: the focused group's results fill the group, or go back to the split. */
export const toggleMaximizeResults = () => useLayout.getState().toggleGroupMaximize(useTabs.getState().focusedGroupId);

/** Ctrl+Alt+B: shows / hides the right sidebar. */
export const toggleRightSidebar = () => useLayout.getState().toggleRight();

const HISTORY_SEARCH = '[data-testid="history-search"]';

/** Ctrl+Shift+H: opens the right sidebar's History and puts the keyboard in its search box. */
export function showHistory(): void {
  useLayout.getState().openRightActivity('history');
  const focus = () => {
    const el = document.querySelector<HTMLInputElement>(HISTORY_SEARCH);
    if (!el) return false;
    el.focus();
    el.select();
    return true;
  };
  // A collapsed panel mounts the search on the next render.
  if (!focus()) setTimeout(focus, 0);
}

/** The status bar's jobs counter: opens the right sidebar on the Jobs panel. */
export const showJobs = () => useLayout.getState().openRightActivity('jobs');

export const zoomIn = () => useLayout.getState().zoomFont(1);
export const zoomOut = () => useLayout.getState().zoomFont(-1);
export const zoomReset = () => useLayout.getState().setFontScale(FONT_SCALE_DEFAULT);

export const openNewConnection = () => useDialogs.getState().requestDialog({ kind: 'newConnection' });
export const openSettings = () => useDialogs.getState().requestDialog({ kind: 'settings' });
/** F1: the keyboard shortcuts live on a tab of the settings dialog. */
export const openShortcuts = () => useDialogs.getState().requestDialog({ kind: 'settings', tab: 'shortcuts' });
