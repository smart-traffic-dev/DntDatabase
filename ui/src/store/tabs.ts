import { create } from 'zustand';
import type { ObjectRef } from '../bindings';

export interface Tab {
  id: string;
  kind: 'object';
  connectionId: string;
  objectRef: ObjectRef;
  title: string;
}

interface TabsState {
  tabs: Tab[];
  activeId?: string;
  open(tab: Tab): void;
  activate(id: string): void;
  close(id: string): void;
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
  close: (id) =>
    set((s) => {
      const index = s.tabs.findIndex((t) => t.id === id);
      if (index < 0) return s;
      const tabs = s.tabs.filter((t) => t.id !== id);
      if (s.activeId !== id) return { tabs };
      // Closing the active tab activates its right neighbour, else the left one.
      return { tabs, activeId: (tabs[index] ?? tabs[index - 1])?.id };
    }),
  reset: () => set({ tabs: [], activeId: undefined }),
}));
