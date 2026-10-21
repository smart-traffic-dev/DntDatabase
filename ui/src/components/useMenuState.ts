import { useConnections } from '../store/connections';
import { resultsOf, useLayout } from '../store/layout';
import { canRun, canStop, emptyRun, useQueryRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { useSelection, useStarting } from '../store/runQuery';
import { useTabs } from '../store/tabs';
import { getActiveEditor } from '../tabs/query/activeEditor';
import { useTheme } from '../theme/ThemeProvider';
import type { MenuState } from './appMenus';

/**
 * Subscribes to exactly what the menu model needs (never the document text: no re-render per
 * keystroke). Shared by the menu bar and the command palette, which lists the same actions.
 */
export function useMenuState(): MenuState {
  const { theme, setTheme } = useTheme();
  const sidebarVisible = useLayout((s) => s.sidebarVisible);
  const rightSidebarVisible = useLayout((s) => s.rightVisible);
  const focusedGroup = useTabs((s) => s.focusedGroupId);
  const resultsVisible = useLayout((s) => resultsOf(s, focusedGroup).visible);
  const hasTab = useTabs((s) => s.tabs.length > 0);
  const canReopen = useTabs((s) => s.closed.length > 0);
  const queryTabId = useTabs((s) => {
    const tab = s.tabs.find((t) => t.id === s.activeId);
    return tab?.kind === 'query' ? tab.id : null;
  });
  const bound = useQueryDocs((s) => (queryTabId ? (s.docs.get(queryTabId)?.connectionId ?? null) : null));
  const stored = useConnections((s) => (bound ? s.summaries.has(bound) : false));
  const idle = useQueryRun((s) => canRun(queryTabId ? (s.runs.get(queryTabId) ?? emptyRun) : emptyRun));
  const stoppable = useQueryRun((s) => canStop(queryTabId ? (s.runs.get(queryTabId) ?? emptyRun) : emptyRun));
  const starting = useStarting((s) => (queryTabId ? s.ids.has(queryTabId) : false));
  const hasSelection = useSelection((s) => (queryTabId ? s.ids.has(queryTabId) : false));
  const layout = useTabs((s) => s.layout);
  const groupCount = useTabs((s) => s.groups.length);
  return {
    theme,
    setTheme,
    sidebarVisible,
    rightSidebarVisible,
    resultsVisible,
    hasTab,
    canReopen,
    queryTabId,
    // Read at render time: opening a menu re-renders, so the answer is current for that menu.
    hasEditor: queryTabId !== null && getActiveEditor() !== null,
    runnable: queryTabId !== null && !!bound && stored && idle && !starting,
    stoppable,
    hasSelection,
    layout,
    groupCount,
  };
}
