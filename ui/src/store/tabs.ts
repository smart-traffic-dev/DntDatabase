import { create } from 'zustand';
import type { ObjectRef } from '../bindings';

export interface ObjectTabRef {
  id: string;
  kind: 'object';
  connectionId: string;
  objectRef: ObjectRef;
  title: string;
}

/** A query tab; its document lives in `useQueryDocs` under the same id. */
export interface QueryTabRef {
  id: string;
  kind: 'query';
  connectionId: string | null;
  title: string;
}

export type Tab = ObjectTabRef | QueryTabRef;

interface TabsState {
  tabs: Tab[];
  activeId?: string;
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

export const useTabs = create<TabsState>()((set) => ({
  tabs: [],
  activeId: undefined,
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
      if (s.activeId !== id) return { tabs };
      // Closing the active tab activates its right neighbour, else the left one.
      return { tabs, activeId: (tabs[index] ?? tabs[index - 1])?.id };
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
  reset: () => set({ tabs: [], activeId: undefined }),
}));
