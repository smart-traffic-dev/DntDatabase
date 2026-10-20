import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { openNewQuery } from './newQuery';
import { closeTab, cycleTab } from './tabActions';

/**
 * Window-level query shortcuts (spec §7.1): `Ctrl+S` save (`Ctrl+Shift+S` save as),
 * `Ctrl+O` open a .sql file, `Ctrl+T` new query,
 * `Ctrl+W` close tab, `Ctrl+Tab` / `Ctrl+Shift+Tab` / `Ctrl+PgDn` / `Ctrl+PgUp` next / previous tab.
 * Ignored while a modal dialog is open.
 */
export function onQueryShortcut(e: KeyboardEvent) {
  if (!(e.ctrlKey || e.metaKey) || e.altKey || e.repeat) return;
  if (document.querySelector('[aria-modal="true"]')) return;
  // Fields outside the editor (the sidebar filter, toolbar selects) keep their own keys.
  const target = e.target instanceof Element ? e.target : null;
  if (target?.closest('input, textarea, select, [contenteditable="true"]') && !target.closest('.sql-editor')) return;
  const key = e.key.toLowerCase();
  const docs = useQueryDocs.getState();
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
  }
}
