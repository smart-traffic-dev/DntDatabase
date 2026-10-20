import { confirmDiscard, useDataEdits } from '../store/dataEdits';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs, type Tab } from '../store/tabs';

/** A query tab closes through its guards (open transaction, unsaved file), then drops its row. */
export async function closeTab(tab: Tab): Promise<boolean> {
  if (tab.kind === 'query') return useQueryDocs.getState().closeTab(tab.id);
  // Staged data edits are thrown away with the tab: ask first.
  if (useDataEdits.getState().hasStaged(tab.id) && !(await confirmDiscard(tab.id))) return false;
  useTabs.getState().close(tab.id);
  return true;
}

/** Closes `tabs` one after another (each query tab may ask a question first); stops when the user cancels one. */
export async function closeTabs(tabs: Tab[]): Promise<void> {
  for (const tab of tabs) if (!(await closeTab(tab))) return;
}

/** Closes every unpinned tab but `keep`. */
export const closeOtherTabs = (keep: Tab): Promise<void> =>
  closeTabs(useTabs.getState().tabs.filter((t) => t.id !== keep.id && !t.pinned));

/** Closes the unpinned tabs to the right of `from`. */
export function closeTabsToRight(from: Tab): Promise<void> {
  const { tabs } = useTabs.getState();
  const at = tabs.findIndex((t) => t.id === from.id);
  return closeTabs(at < 0 ? [] : tabs.slice(at + 1).filter((t) => !t.pinned));
}

/** Brings back the most recently closed tab (a query tab with its text); false when there is none. */
export function reopenClosedTab(): boolean {
  const entry = useTabs.getState().popClosed();
  if (!entry) return false;
  if (entry.kind === 'object') useTabs.getState().open({ ...entry.tab, pinned: false });
  else useQueryDocs.getState().reopen(entry.doc);
  return true;
}

/** Ctrl+1 to Ctrl+8: the tab at `index` (1-based); Ctrl+9 (`last`): the last tab. */
export function goToTab(index: number, last = false): void {
  const { tabs, activate } = useTabs.getState();
  const tab = last ? tabs.at(-1) : tabs[index - 1];
  if (tab) activate(tab.id);
}

/** Pins the tab, or unpins it when it is pinned. */
export function togglePin(tab: Tab): void {
  useTabs.getState().setPinned(tab.id, !tab.pinned);
}

/** Activates the tab `step` positions away from the active one, wrapping around. */
export function cycleTab(step: number): void {
  const { tabs, activeId, activate } = useTabs.getState();
  if (tabs.length < 2) return;
  const at = tabs.findIndex((t) => t.id === activeId);
  const next = tabs[(((at < 0 ? 0 : at) + step) % tabs.length + tabs.length) % tabs.length];
  if (next) activate(next.id);
}
