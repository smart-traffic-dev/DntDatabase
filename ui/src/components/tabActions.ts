import { confirmDiscard, confirmDiscardMany, useDataEdits } from '../store/dataEdits';
import { confirmDiscardDdl, ddlDiscardCount, useDdlEdits } from '../store/ddlEdits';
import { useDialogs } from '../store/dialogs';
import { useQueryDocs } from '../store/queryTabs';
import { useQueryRun } from '../store/queryRun';
import { confirmDiscardDesign, confirmDiscardDesigns, designDiscardCount, useTableDesigns } from '../store/tableDesigns';
import { groupTabs, useTabs, type Tab } from '../store/tabs';

/** A query tab closes through its guards (open transaction, unsaved file), then drops its row. */
export async function closeTab(tab: Tab): Promise<boolean> {
  if (tab.kind === 'query') return useQueryDocs.getState().closeTab(tab.id);
  // An unapplied table design is thrown away with the tab: ask first.
  if (tab.kind === 'design') {
    if (!(await confirmDiscardDesign(tab.id))) return false;
    useTabs.getState().close(tab.id);
    return true;
  }
  // Unapplied DDL text is thrown away with the tab: ask first.
  if (tab.kind === 'ddl') {
    if (!(await confirmDiscardDdl([tab.id]))) return false;
    useTabs.getState().close(tab.id);
    return true;
  }
  // Staged data edits are thrown away with the tab: ask first.
  if (useDataEdits.getState().hasStaged(tab.id) && !(await confirmDiscard(tab.id))) return false;
  useTabs.getState().close(tab.id);
  return true;
}

/** One `discardEdits` question for unapplied designs and DDL text together (either alone asks its own way). */
function confirmDiscardDrafts(designs: string[], ddl: string[]): Promise<boolean> {
  const designCount = designDiscardCount(designs);
  const ddlCount = ddlDiscardCount(ddl);
  if (ddlCount === 0) return confirmDiscardDesigns(designs);
  if (designCount === 0) return confirmDiscardDdl(ddl);
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'discardEdits',
      count: designCount + ddlCount,
      done: (discard) => {
        useDialogs.getState().answer();
        resolve(discard);
      },
    });
  });
}

/**
 * Asks before the object tabs of a connection that is going away (deleted, replaced by an import)
 * lose their staged data edits; false when the user keeps them.
 */
export async function confirmDiscardConnection(connectionId: string): Promise<boolean> {
  const tabs = useTabs.getState().tabs;
  const ids = tabs.filter((t) => t.kind === 'object' && t.connectionId === connectionId).map((t) => t.id);
  const designs = tabs.filter((t) => t.kind === 'design' && t.connectionId === connectionId).map((t) => t.id);
  const ddl = tabs.filter((t) => t.kind === 'ddl' && t.connectionId === connectionId).map((t) => t.id);
  // Designs and DDL text first: they discard nothing, so keeping them cancels before any staged edit is dropped.
  if (!(await confirmDiscardDrafts(designs, ddl))) return false;
  return confirmDiscardMany(ids);
}

/**
 * An import that overwrites `connectionIds` while their tabs hold staged data edits: the only
 * choices are "Discard and import" (true; the edits are discarded) and "Cancel import" (false).
 */
export function confirmDiscardForImport(connectionIds: string[]): Promise<boolean> {
  const tabs = useTabs.getState().tabs;
  const ids = tabs.filter((t) => t.kind === 'object' && connectionIds.includes(t.connectionId)).map((t) => t.id);
  const designs = tabs.filter((t) => t.kind === 'design' && connectionIds.includes(t.connectionId)).map((t) => t.id);
  const ddl = tabs.filter((t) => t.kind === 'ddl' && connectionIds.includes(t.connectionId)).map((t) => t.id);
  const count = ids.reduce((n, id) => n + useDataEdits.getState().changeCount(id), 0) + designDiscardCount(designs) + ddlDiscardCount(ddl);
  if (count === 0) return Promise.resolve(true);
  return new Promise((resolve) => {
    useDialogs.getState().ask({
      kind: 'discardForImport',
      count,
      done: (discard) => {
        useDialogs.getState().answer();
        if (discard) for (const id of ids) useDataEdits.getState().discard(id);
        resolve(discard);
      },
    });
  });
}

/** Closes `tabs` one after another (each query tab may ask a question first); stops when the user cancels one. */
export async function closeTabs(tabs: Tab[]): Promise<void> {
  for (const tab of tabs) if (!(await closeTab(tab))) return;
}

/** The tabs of `tab`'s editor group (bulk closes stay inside one group). */
const siblingsOf = (tab: Tab): Tab[] => {
  const s = useTabs.getState();
  const group = s.groupOf[tab.id];
  return group ? groupTabs(s, group) : s.tabs;
};

/** The tabs of the focused group. */
const focusedTabs = (): Tab[] => {
  const s = useTabs.getState();
  return groupTabs(s, s.focusedGroupId);
};

/** Closes every unpinned tab of its group but `keep`. */
export const closeOtherTabs = (keep: Tab): Promise<void> =>
  closeTabs(siblingsOf(keep).filter((t) => t.id !== keep.id && !t.pinned));

/** Closes every unpinned tab of a group, by default the focused one (pinned tabs are never closed in bulk). */
export const closeAllTabs = (groupId = useTabs.getState().focusedGroupId): Promise<void> =>
  closeTabs(groupTabs(useTabs.getState(), groupId).filter((t) => !t.pinned));

/** Closes a group: all its tabs, pinned ones too, each through its guards; the group goes with its last tab. */
export async function closeGroup(groupId = useTabs.getState().focusedGroupId): Promise<void> {
  await closeTabs(groupTabs(useTabs.getState(), groupId));
  // An empty group (from a layout preset) has no tab to take it away.
  useTabs.getState().dropGroup(groupId);
}

/** Whether a tab holds something a close would ask about: unsaved file text, staged edits, an open transaction. */
function unsaved(tab: Tab): boolean {
  if (tab.kind === 'object') return useDataEdits.getState().hasStaged(tab.id);
  if (tab.kind === 'design') return useTableDesigns.getState().isDirty(tab.id);
  if (tab.kind === 'ddl') return useDdlEdits.getState().isDirty(tab.id);
  const doc = useQueryDocs.getState().docs.get(tab.id);
  const tx = useQueryRun.getState().runs.get(tab.id)?.txState ?? 'idle';
  return (!!doc?.filePath && doc.dirty) || tx !== 'idle';
}

/** Closes the tabs of a group (default: the focused one) that have nothing unsaved (VS Code's Close Saved). */
export const closeSavedTabs = (groupId = useTabs.getState().focusedGroupId): Promise<void> =>
  closeTabs(groupTabs(useTabs.getState(), groupId).filter((t) => !t.pinned && !unsaved(t)));

/** Closes the unpinned tabs to the right of `from` in its group. */
export function closeTabsToRight(from: Tab): Promise<void> {
  const tabs = siblingsOf(from);
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

/** Ctrl+1 to Ctrl+8: the tab at `index` (1-based) of the focused group; Ctrl+9 (`last`): its last tab. */
export function goToTab(index: number, last = false): void {
  const tabs = focusedTabs();
  const tab = last ? tabs.at(-1) : tabs[index - 1];
  if (tab) useTabs.getState().activate(tab.id);
}

/** Pins the tab, or unpins it when it is pinned. */
export function togglePin(tab: Tab): void {
  useTabs.getState().setPinned(tab.id, !tab.pinned);
}

/** Activates the tab `step` positions away from the active one in the focused group, wrapping around. */
export function cycleTab(step: number): void {
  const { activeId, activate } = useTabs.getState();
  const tabs = focusedTabs();
  if (tabs.length < 2) return;
  const at = tabs.findIndex((t) => t.id === activeId);
  const next = tabs[(((at < 0 ? 0 : at) + step) % tabs.length + tabs.length) % tabs.length];
  if (next) activate(next.id);
}
