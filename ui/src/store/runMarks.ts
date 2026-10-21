import { create } from 'zustand';
import { useQueryRun } from './queryRun';
import { useTabs } from './tabs';

export type RunMark = 'ok' | 'error';

/**
 * Marks on query tabs whose run ended while they were out of sight (editor v2 §4.4): green when
 * it finished, red when a statement failed. A mark goes once the tab is shown.
 */
export const useRunMarks = create<{ marks: Map<string, RunMark> }>()(() => ({ marks: new Map() }));

/** A tab is in sight when it is the active tab of a group that is on screen. */
function visible(tabId: string): boolean {
  const s = useTabs.getState();
  const group = s.groupOf[tabId];
  if (!group || s.groups.find((g) => g.id === group)?.activeId !== tabId) return false;
  return !s.maximizedGroupId || s.maximizedGroupId === group;
}

function setMark(tabId: string, mark: RunMark | null) {
  const marks = useRunMarks.getState().marks;
  if ((marks.get(tabId) ?? null) === mark) return;
  const next = new Map(marks);
  if (mark) next.set(tabId, mark);
  else next.delete(tabId);
  useRunMarks.setState({ marks: next });
}

/** Watches the runs and the tabs; returns the unsubscribe. Installed once by the app. */
export function installRunMarks(): () => void {
  const offRuns = useQueryRun.subscribe((s, prev) => {
    for (const [id, run] of s.runs) {
      const before = prev.runs.get(id);
      const ended = before && before.phase.kind !== 'idle' && run.phase.kind === 'idle';
      if (!ended || visible(id)) continue;
      setMark(id, (run.lastRun?.errors ?? 0) > 0 ? 'error' : 'ok');
    }
  });
  const offTabs = useTabs.subscribe(() => {
    for (const id of useRunMarks.getState().marks.keys()) {
      const open = useTabs.getState().tabs.some((t) => t.id === id);
      if (!open || visible(id)) setMark(id, null);
    }
  });
  return () => {
    offRuns();
    offTabs();
    useRunMarks.setState({ marks: new Map() });
  };
}
