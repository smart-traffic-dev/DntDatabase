import { useDialogs } from '../store/dialogs';
import { FONT_SCALE_DEFAULT, useLayout } from '../store/layout';
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

export const toggleResults = () => useLayout.getState().toggleResults();
export const zoomIn = () => useLayout.getState().zoomFont(1);
export const zoomOut = () => useLayout.getState().zoomFont(-1);
export const zoomReset = () => useLayout.getState().setFontScale(FONT_SCALE_DEFAULT);

export const openNewConnection = () => useDialogs.getState().requestDialog({ kind: 'newConnection' });
export const openSettings = () => useDialogs.getState().requestDialog({ kind: 'settings' });
export const openShortcuts = () => useDialogs.getState().requestDialog({ kind: 'shortcuts' });
