import { create } from 'zustand';
import type { ObjectRef } from '../bindings';
import type { QueryDoc } from './queryTabs';

export interface ObjectTabRef {
  id: string;
  kind: 'object';
  /** A DDL outcome is unknown: hide cached data until the user verifies the object. */
  needsVerification?: boolean;
  connectionId: string;
  objectRef: ObjectRef;
  title: string;
  /** Pinned tabs sort first and are skipped by "Close others" / "Close tabs to the right". */
  pinned?: boolean;
}

/** A query tab; its document lives in `useQueryDocs` under the same id. */
export interface QueryTabRef {
  id: string;
  kind: 'query';
  connectionId: string | null;
  title: string;
  pinned?: boolean;
}

export type Tab = ObjectTabRef | QueryTabRef;

/** What "Reopen closed tab" brings back: an object tab by reference, a query tab with its document. */
export type ClosedTab = { kind: 'object'; tab: ObjectTabRef } | { kind: 'query'; doc: QueryDoc };

/** How many closed tabs are remembered. */
export const CLOSED_STACK_MAX = 10;

interface TabsState {
  tabs: Tab[];
  activeId?: string;
  /** Last closed tabs, most recent last (at most `CLOSED_STACK_MAX`). */
  closed: ClosedTab[];
  /** Remembers a closed tab (called by whoever closes a query tab; object tabs record themselves). */
  pushClosed(entry: ClosedTab): void;
  /** Takes the most recently closed tab off the stack. */
  popClosed(): ClosedTab | undefined;
  /** Pins or unpins a tab: pinned tabs stay at the left. */
  setPinned(id: string, pinned: boolean): void;
  open(tab: Tab): void;
  activate(id: string): void;
  /** Rebinds a query tab to another connection (or none). */
  bind(id: string, connectionId: string | null): void;
  rename(id: string, title: string): void;
  close(id: string): void;
  /** Closes every object tab of a connection (it was deleted or replaced); query tabs keep their binding. */
  closeConnection(connectionId: string): void;
  reset(): void;
}

/** One tab per object of a connection. */
export function objectTabId(connectionId: string, o: ObjectRef): string {
  return [connectionId, o.database, o.schema ?? '', o.kind, o.name].map(encodeURIComponent).join('/');
}

export const useTabs = create<TabsState>()((set, get) => ({
  tabs: [],
  activeId: undefined,
  closed: [],
  pushClosed: (entry) => set((s) => ({ closed: [...s.closed, entry].slice(-CLOSED_STACK_MAX) })),
  popClosed: () => {
    const entry = get().closed.at(-1);
    if (entry) set((s) => ({ closed: s.closed.slice(0, -1) }));
    return entry;
  },
  setPinned: (id, pinned) =>
    set((s) => {
      const tab = s.tabs.find((t) => t.id === id);
      if (!tab || !!tab.pinned === pinned) return s;
      const rest = s.tabs.filter((t) => t.id !== id);
      const changed = { ...tab, pinned } as Tab;
      // Pinned: after the last pinned tab. Unpinned: right after the pinned block.
      const firstUnpinned = rest.findIndex((t) => !t.pinned);
      const at = firstUnpinned < 0 ? rest.length : firstUnpinned;
      return { tabs: [...rest.slice(0, at), changed, ...rest.slice(at)] };
    }),
  open: (tab) =>
    set((s) => ({
      tabs: s.tabs.some((t) => t.id === tab.id) ? s.tabs : [...s.tabs, tab],
      activeId: tab.id,
    })),
  activate: (id) => set((s) => (s.tabs.some((t) => t.id === id) ? { activeId: id } : s)),
  bind: (id, connectionId) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id && t.kind === 'query' ? { ...t, connectionId } : t)) })),
  rename: (id, title) =>
    set((s) => (s.tabs.some((t) => t.id === id && t.title !== title) ? { tabs: s.tabs.map((t) => (t.id === id ? { ...t, title } : t)) } : s)),
  close: (id) =>
    set((s) => {
      const index = s.tabs.findIndex((t) => t.id === id);
      if (index < 0) return s;
      const tabs = s.tabs.filter((t) => t.id !== id);
      const gone = s.tabs[index];
      // Object tabs are remembered here (query tabs by `useQueryDocs.closeTab`, which has the document).
      const closed =
        gone?.kind === 'object' ? [...s.closed, { kind: 'object' as const, tab: { ...gone, pinned: false } }].slice(-CLOSED_STACK_MAX) : s.closed;
      if (s.activeId !== id) return { tabs, closed };
      // Closing the active tab activates its right neighbour, else the left one.
      return { tabs, closed, activeId: (tabs[index] ?? tabs[index - 1])?.id };
    }),
  closeConnection: (connectionId) =>
    set((s) => {
      const goes = (t: Tab) => t.kind === 'object' && t.connectionId === connectionId;
      const tabs = s.tabs.filter((t) => !goes(t));
      if (tabs.length === s.tabs.length) return s;
      if (tabs.some((t) => t.id === s.activeId)) return { tabs };
      // The active tab went away: activate the nearest remaining tab to its left, else the first.
      const at = s.tabs.findIndex((t) => t.id === s.activeId);
      const before = s.tabs.slice(0, Math.max(at, 0)).filter((t) => !goes(t));
      return { tabs, activeId: (before.at(-1) ?? tabs[0])?.id };
    }),
  reset: () => set({ tabs: [], activeId: undefined, closed: [] }),
}));
