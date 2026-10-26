import { openExportDialog } from '../dialogs/resultExport';
import { useObjectData } from '../store/objectData';
import { useTabs, type ObjectTabRef } from '../store/tabs';
import { refreshData, turnPage } from '../tabs/DataView';
import { resourceOf, subTabsOf } from '../tabs/objectSections';
import { openDesignTab } from './openObject';

/** The Connections tree keeps F4 / F5 for the selected node. */
const TREE = '[role="tree"][aria-label="Connections"]';

const SUB_KEYS = ['1', '2', '3', '4'];

/** The focused group's active tab when it is a table / view / routine tab. */
function activeObjectTab(): ObjectTabRef | undefined {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  return tab?.kind === 'object' ? tab : undefined;
}

function consume(e: KeyboardEvent) {
  e.preventDefault();
  e.stopPropagation();
}

/**
 * Shortcuts of the active table / view tab:
 * `F5` refresh the visible section, `Alt+1…4` Data / Info / DDL / Structure,
 * `Alt+PgUp` / `Alt+PgDn` previous / next page of the Data section, `F4` design the table,
 * `Ctrl+Alt+E` export what the tab shows (an object tab's rows or a query tab's result).
 * Ignored while a modal dialog is open, in text fields and in the Connections tree (which has its own F4 / F5).
 * Returns true when it took the key.
 */
export function onObjectShortcut(e: KeyboardEvent): boolean {
  // Cheap test first: plain typing never reaches the modal lookup below.
  if (!(e.key === 'F4' || e.key === 'F5' || e.altKey)) return false;
  if (e.repeat || e.isComposing || document.querySelector('[aria-modal="true"]')) return false;
  const ctrl = e.ctrlKey || e.metaKey;
  const target = e.target instanceof Element ? e.target : null;
  if (target?.closest(`input, textarea, select, [contenteditable="true"], ${TREE}`) && !target.closest('.sql-editor')) return false;

  // Ctrl+Alt+E: export from an object or query tab. Windows reports AltGr as Ctrl+Alt: AltGr+E
  // types a character (ę, €) and is never ours.
  const altGr = e.getModifierState?.('AltGraph') ?? false;
  if (ctrl && e.altKey && !e.shiftKey && !altGr && (e.code === 'KeyE' || e.key.toLowerCase() === 'e')) {
    consume(e);
    openExportDialog();
    return true;
  }
  if (ctrl || e.shiftKey) return false;

  const tab = activeObjectTab();
  if (!tab) return false;
  const subs = subTabsOf(tab.objectRef.kind);
  const stored = useObjectData.getState().byTab[tab.id]?.sub;
  const sub = stored && subs.includes(stored) ? stored : subs[0]!;

  if (!e.altKey) {
    if (e.key === 'F5') {
      consume(e);
      if (sub === 'data') refreshData(tab);
      else void useObjectData.getState().refresh(tab, resourceOf(sub));
      return true;
    }
    if (e.key === 'F4' && tab.objectRef.kind === 'Table') {
      consume(e);
      const { database, schema, name } = tab.objectRef;
      void openDesignTab(tab.connectionId, { database, schema }, name);
      return true;
    }
    return false;
  }

  const n = SUB_KEYS.indexOf(e.key);
  if (n >= 0) {
    const next = subs[n];
    if (!next) return false;
    consume(e);
    useObjectData.getState().setSub(tab.id, next);
    return true;
  }
  if ((e.key === 'PageUp' || e.key === 'PageDown') && sub === 'data') {
    consume(e);
    turnPage(tab, e.key === 'PageUp' ? 'prev' : 'next');
    return true;
  }
  return false;
}
