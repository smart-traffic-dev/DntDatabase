import { useQueryDocs } from '../store/queryTabs';
import { useTabs, type Tab } from '../store/tabs';

/** A query tab closes through its guards (open transaction, unsaved file), then drops its row. */
export async function closeTab(tab: Tab): Promise<void> {
  if (tab.kind === 'query') await useQueryDocs.getState().closeTab(tab.id);
  else useTabs.getState().close(tab.id);
}

/** Closes `tabs` one after another (each query tab may ask a question first). */
export async function closeTabs(tabs: Tab[]): Promise<void> {
  for (const tab of tabs) await closeTab(tab);
}

/** Activates the tab `step` positions away from the active one, wrapping around. */
export function cycleTab(step: number): void {
  const { tabs, activeId, activate } = useTabs.getState();
  if (tabs.length < 2) return;
  const at = tabs.findIndex((t) => t.id === activeId);
  const next = tabs[(((at < 0 ? 0 : at) + step) % tabs.length + tabs.length) % tabs.length];
  if (next) activate(next.id);
}
