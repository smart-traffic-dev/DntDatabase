import { create } from 'zustand';
import { api, isCancelled, notOpenError, toAppError } from '../api';
import type { AppError, SortSpec, TableDataPage } from '../bindings';
import type { Loadable } from './objectData';
import { promptHandles, reopenConnection } from './reopen';
import type { ObjectTabRef as Tab } from './tabs';
import { useTabs } from './tabs';

/** The data view of one object tab: its paging, sort and the page on screen. */
export interface TableDataState {
  pageSize: number;
  offset: number;
  sort: SortSpec | null;
  page?: Loadable<TableDataPage>;
  /** Exact row count (`table_row_count`), fetched on demand. */
  count?: Loadable<number>;
}

interface TableDataStore {
  /** Per tab id; lives as long as the tab is open. */
  byTab: Record<string, TableDataState>;
  /** Creates the tab's state with `pageSize` and loads the first page, unless it exists. */
  init(tab: Tab, pageSize: number): void;
  /** (Re)loads the current page. */
  load(tab: Tab): Promise<void>;
  setPageSize(tab: Tab, pageSize: number): void;
  next(tab: Tab): void;
  prev(tab: Tab): void;
  /** Header click: none -> ascending -> descending -> none; another column starts ascending. */
  toggleSort(tab: Tab, column: string): void;
  clearSort(tab: Tab): void;
  count(tab: Tab): Promise<void>;
  /** Opens the tab's connection again, then reloads the page. */
  reconnect(tab: Tab): Promise<void>;
  reset(): void;
}

/** The next sort after a click on `column`. */
export function nextSort(current: SortSpec | null, column: string): SortSpec | null {
  if (!current || current.column !== column) return { column, dir: 'asc' };
  return current.dir === 'asc' ? { column, dir: 'desc' } : null;
}

let counter = 0;
const tokens = new Map<string, number>();
const isOpen = (tabId: string) => useTabs.getState().tabs.some((t) => t.id === tabId);

export const useTableData = create<TableDataStore>()((set, get) => {
  const patch = (tabId: string, change: Partial<TableDataState>) =>
    set((s) => {
      const current = s.byTab[tabId];
      if (!current) return s;
      return { byTab: { ...s.byTab, [tabId]: { ...current, ...change } } };
    });

  /** Moves to a new position (offset / size / sort) and loads it. */
  const go = (tab: Tab, change: Partial<TableDataState>) => {
    patch(tab.id, change);
    void get().load(tab);
  };

  return {
    byTab: {},
    init(tab, pageSize) {
      if (get().byTab[tab.id]) return;
      set((s) => ({ byTab: { ...s.byTab, [tab.id]: { pageSize, offset: 0, sort: null } } }));
      void get().load(tab);
    },
    async load(tab) {
      const state = get().byTab[tab.id];
      if (!state) return;
      const token = ++counter;
      tokens.set(tab.id, token);
      patch(tab.id, { page: { status: 'loading' } });
      const latest = () => tokens.get(tab.id) === token && isOpen(tab.id);
      try {
        const data = await api.tableData(tab.connectionId, tab.objectRef, state.pageSize, state.offset, state.sort);
        if (latest()) patch(tab.id, { page: { status: 'ok', data } });
      } catch (e) {
        if (!latest()) return;
        const error = toAppError(e);
        patch(tab.id, { page: isCancelled(error) ? undefined : { status: 'error', error } });
      }
    },
    setPageSize(tab, pageSize) {
      go(tab, { pageSize, offset: 0 });
    },
    next(tab) {
      const s = get().byTab[tab.id];
      if (s?.page?.status === 'ok' && s.page.data.has_more) go(tab, { offset: s.offset + s.pageSize });
    },
    prev(tab) {
      const s = get().byTab[tab.id];
      if (s && s.offset > 0) go(tab, { offset: Math.max(0, s.offset - s.pageSize) });
    },
    toggleSort(tab, column) {
      const s = get().byTab[tab.id];
      if (s) go(tab, { sort: nextSort(s.sort, column), offset: 0 });
    },
    clearSort(tab) {
      if (get().byTab[tab.id]?.sort) go(tab, { sort: null, offset: 0 });
    },
    async count(tab) {
      if (!get().byTab[tab.id]) return;
      patch(tab.id, { count: { status: 'loading' } });
      try {
        const { rows } = await api.tableRowCount(tab.connectionId, tab.objectRef);
        if (isOpen(tab.id)) patch(tab.id, { count: { status: 'ok', data: rows } });
      } catch (e) {
        if (!isOpen(tab.id)) return;
        const error: AppError = toAppError(e);
        patch(tab.id, { count: isCancelled(error) ? undefined : { status: 'error', error } });
      }
    },
    async reconnect(tab) {
      patch(tab.id, { page: { status: 'loading' } });
      const error = await reopenConnection(tab.connectionId);
      if (!isOpen(tab.id)) return;
      if (!error) {
        await get().load(tab);
      } else {
        // The password / host-key prompt finishes the open; the view keeps its Reconnect.
        patch(tab.id, { page: { status: 'error', error: promptHandles(error) ? notOpenError() : error } });
      }
    },
    reset() {
      tokens.clear();
      set({ byTab: {} });
    },
  };
});

// Forget the data (and ignore in-flight answers) of tabs that were closed.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  const { byTab } = useTableData.getState();
  const gone = Object.keys(byTab).filter((id) => !open.has(id));
  if (gone.length === 0) return;
  for (const id of gone) tokens.delete(id);
  useTableData.setState({ byTab: Object.fromEntries(Object.entries(byTab).filter(([id]) => open.has(id))) });
});
