import { create } from 'zustand';
import { api, isCancelled, notOpenError, toAppError } from '../api';
import type { AppError, DataFilter, FilterMode, FilterOp, SortSpec, TableDataPage } from '../bindings';
import { formatCount } from '../lib/plural';
import type { Loadable } from './objectData';
import { promptHandles, reopenConnection } from './reopen';
import type { ObjectTabRef as Tab } from './tabs';
import { useTabs } from './tabs';

/** One condition row of the filter bar, as typed (values are parsed on Apply). */
export interface FilterRow {
  id: number;
  column: string;
  op: FilterOp;
  /** The value; `IN` takes a comma-separated list here. */
  value: string;
  /** The upper bound of `BETWEEN`. */
  value2: string;
}

/** The filter bar of a tab as edited (applied only by Apply). */
export interface FilterDraft {
  mode: FilterMode;
  rows: FilterRow[];
  /** Raw WHERE mode: the single SQL input replaces the condition rows. */
  rawMode?: boolean;
  /** The typed WHERE condition (sent as raw_where). */
  rawText?: string;
}

/** The data view of one object tab: its paging, filter, sort and the page on screen. */
export interface TableDataState {
  connectionId: string;
  pageSize: number;
  offset: number;
  /** Offsets of the pages Next left, so Prev returns exactly (pages can stop early). */
  back: number[];
  /** Sort keys in order (empty: the primary key). */
  sort: SortSpec[];
  /** The filter the pages are read with (null: all rows). */
  filter: DataFilter | null;
  /** What the filter bar shows. */
  draft: FilterDraft;
  filterBarOpen: boolean;
  /** Column names of the last page read (the filter bar offers them while a page loads). */
  columns: string[];
  page?: Loadable<TableDataPage>;
  /** Exact row count (`table_row_count`), fetched on demand; cleared by any page load. */
  count?: Loadable<number>;
}

interface TableDataStore {
  /** Per tab id; lives as long as the tab is open. */
  byTab: Record<string, TableDataState>;
  /** Creates the tab's state with `pageSize` and loads the first page, unless it exists. */
  init(tab: Tab, pageSize: number): void;
  /**
   * (Re)loads the current page. Forgets the exact count (it may be stale) unless
   * `keepCount` (paging does not change it).
   */
  load(tab: Tab, opts?: { keepCount?: boolean }): Promise<void>;
  setPageSize(tab: Tab, pageSize: number): void;
  next(tab: Tab): void;
  prev(tab: Tab): void;
  /**
   * Header click: that column becomes the only key, cycling none -> ascending -> descending -> none.
   * With `additive` (Shift+click) the column is added as a further key, or cycled in place.
   */
  toggleSort(tab: Tab, column: string, additive?: boolean): void;
  clearSort(tab: Tab): void;
  /** Reads pages with `filter` from the first page (null: all rows). */
  applyFilter(tab: Tab, filter: DataFilter | null): void;
  /** Empties the filter bar and drops the filter. */
  clearFilter(tab: Tab): void;
  setDraft(tab: Tab, draft: FilterDraft): void;
  /** Shows / hides the filter bar; `open` forces it. */
  toggleFilterBar(tab: Tab, open?: boolean): void;
  count(tab: Tab): Promise<void>;
  /** Stops the page load / the count running for the tab (the backend cancels the statement). */
  cancelPage(tab: Tab): void;
  cancelCount(tab: Tab): void;
  /** Opens the tab's connection again, then reloads the page. */
  reconnect(tab: Tab): Promise<void>;
  reset(): void;
}

/** The backend request keys of a tab: one page and one count at a time, each. */
export const pageKey = (tabId: string) => `${tabId}:page`;
export const countKey = (tabId: string) => `${tabId}:count`;

type Dir = SortSpec['dir'];
const cycle = (dir: Dir | undefined): Dir | null => (dir === undefined ? 'asc' : dir === 'asc' ? 'desc' : null);

/**
 * The sort after a header click on `column`: a plain click makes it the only key (cycling it
 * when it already is); Shift+click (`additive`) adds it as a further key, or cycles it in place.
 */
export function nextSortKeys(current: SortSpec[], column: string, additive = false): SortSpec[] {
  const at = current.findIndex((k) => k.column === column);
  if (!additive) {
    const sole = current.length === 1 && at === 0;
    const dir = sole ? cycle(current[0]!.dir) : 'asc';
    return dir ? [{ column, dir }] : [];
  }
  const dir = cycle(current[at]?.dir);
  if (at < 0) return [...current, { column, dir: 'asc' }];
  return dir ? current.map((k, i) => (i === at ? { column, dir } : k)) : current.filter((_, i) => i !== at);
}

let rowIds = 0;
/** A new condition row on `column` (default `=`). */
export function newFilterRow(column: string, op: FilterOp = 'eq', value = ''): FilterRow {
  return { id: ++rowIds, column, op, value, value2: '' };
}

export const emptyDraft = (): FilterDraft => ({ mode: 'and', rows: [], rawMode: false, rawText: '' });

/** How many values each operator takes in the bar: 0, 1, 2 (BETWEEN) or a list (IN). */
export function opArity(op: FilterOp): 0 | 1 | 2 | 'list' {
  if (op === 'is_null' || op === 'is_not_null') return 0;
  if (op === 'between') return 2;
  if (op === 'in') return 'list';
  return 1;
}

/** `a, b ,c` -> `['a', 'b', 'c']` (empty items dropped). */
export function splitList(text: string): string[] {
  return text
    .split(',')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

/** The filter AST of a draft; rows without a column are skipped. An error names the first bad row. */
export function draftToFilter(draft: FilterDraft): { filter: DataFilter | null; error: string | null } {
  if (draft.rawMode) {
    const raw = (draft.rawText ?? '').trim();
    return { filter: raw ? { mode: 'and', conditions: [], raw_where: raw } : null, error: null };
  }
  const conditions = [];
  for (const row of draft.rows) {
    if (!row.column) continue;
    const arity = opArity(row.op);
    let values: string[];
    if (arity === 0) values = [];
    else if (arity === 1) values = [row.value];
    else if (arity === 2) values = [row.value, row.value2];
    else {
      values = splitList(row.value);
      if (values.length === 0) return { filter: null, error: `IN on ${row.column} needs at least one value.` };
    }
    conditions.push({ column: row.column, op: row.op, values });
  }
  return { filter: conditions.length ? { mode: draft.mode, conditions } : null, error: null };
}

/**
 * The total shown with the page (`1–100 / ~3,412`): the exact count once asked for, else the
 * table's estimate (not under a filter: it is of the whole table); null when neither is known.
 */
export function pageTotal(state: TableDataState, estimate: number | null): { text: string; tip: string } | null {
  const filtered = state.filter !== null;
  if (state.count?.status === 'ok') {
    const n = formatCount(state.count.data);
    return { text: n, tip: `${n} ${filtered ? 'matching rows' : 'rows'} (counted)` };
  }
  if (estimate === null || filtered) return null;
  const n = formatCount(estimate);
  return { text: `~${n}`, tip: `About ${n} rows (the table's estimate; Count rows in ⋯ counts exactly)` };
}

/** The status bar's row text: "Rows 101–200 of ~3,412", "No rows", null while nothing is loaded. */
export function pageRowsText(state: TableDataState, estimate: number | null): string | null {
  if (state.page?.status === 'loading') return 'Loading…';
  if (state.page?.status !== 'ok') return null;
  const n = state.page.data.rows.length;
  if (n === 0) return 'No rows';
  const total = pageTotal(state, estimate);
  const rows = `Rows ${formatCount(state.offset + 1)}–${formatCount(state.offset + n)}`;
  return total ? `${rows} of ${total.text}` : rows;
}

let counter = 0;
/** Latest request token per tab and kind (`page` / `count`). */
const tokens = new Map<string, number>();
const isOpen = (tabId: string) => useTabs.getState().tabs.some((t) => t.id === tabId);

function cancelRequest(connectionId: string, key: string) {
  // Best effort: the request may already have ended.
  api.tableDataCancel(connectionId, key).catch(() => undefined);
}

export const useTableData = create<TableDataStore>()((set, get) => {
  const patch = (tabId: string, change: Partial<TableDataState>) =>
    set((s) => {
      const current = s.byTab[tabId];
      if (!current) return s;
      return { byTab: { ...s.byTab, [tabId]: { ...current, ...change } } };
    });

  /** Moves to a new position (offset / size / sort / filter) and loads it. */
  const go = (tab: Tab, change: Partial<TableDataState>, keepCount: boolean) => {
    patch(tab.id, change);
    void get().load(tab, { keepCount });
  };

  return {
    byTab: {},
    init(tab, pageSize) {
      if (get().byTab[tab.id]) return;
      set((s) => ({
        byTab: {
          ...s.byTab,
          [tab.id]: {
            connectionId: tab.connectionId,
            pageSize,
            offset: 0,
            back: [],
            sort: [],
            filter: null,
            draft: emptyDraft(),
            filterBarOpen: false,
            columns: [],
          },
        },
      }));
      void get().load(tab);
    },
    async load(tab, opts = {}) {
      const state = get().byTab[tab.id];
      if (!state) return;
      const token = ++counter;
      const key = pageKey(tab.id);
      tokens.set(key, token);
      if (opts.keepCount) {
        patch(tab.id, { page: { status: 'loading' } });
      } else {
        // A count running for the old data is answered for nothing: drop it.
        if (state.count?.status === 'loading') cancelRequest(tab.connectionId, countKey(tab.id));
        tokens.delete(countKey(tab.id));
        patch(tab.id, { page: { status: 'loading' }, count: undefined });
      }
      const latest = () => tokens.get(key) === token && isOpen(tab.id);
      try {
        // The backend cancels the previous request of this key itself.
        const data = await api.tableData(tab.connectionId, tab.objectRef, state.pageSize, state.offset, state.sort, state.filter, key);
        if (latest()) patch(tab.id, { page: { status: 'ok', data }, columns: data.columns.map((c) => c.name) });
      } catch (e) {
        // A superseded request fails with `cancelled`: only the latest one is shown.
        if (latest()) patch(tab.id, { page: { status: 'error', error: toAppError(e) } });
      }
    },
    setPageSize(tab, pageSize) {
      go(tab, { pageSize, offset: 0, back: [] }, true);
    },
    next(tab) {
      const s = get().byTab[tab.id];
      // A page cut short by its byte budget holds fewer rows: continue right after them.
      if (s?.page?.status === 'ok' && s.page.data.has_more) {
        go(tab, { offset: s.offset + Math.max(1, s.page.data.rows.length), back: [...s.back, s.offset] }, true);
      }
    },
    prev(tab) {
      const s = get().byTab[tab.id];
      if (!s || s.offset === 0) return;
      const back = s.back.slice(0, -1);
      const offset = s.back.at(-1) ?? Math.max(0, s.offset - s.pageSize);
      go(tab, { offset, back }, true);
    },
    toggleSort(tab, column, additive = false) {
      // Working in a preview tab keeps it (editor v2 §4.1).
      useTabs.getState().keepTab(tab.id);
      const s = get().byTab[tab.id];
      // Sorting never changes the count.
      if (s) go(tab, { sort: nextSortKeys(s.sort, column, additive), offset: 0, back: [] }, true);
    },
    clearSort(tab) {
      if (get().byTab[tab.id]?.sort.length) go(tab, { sort: [], offset: 0, back: [] }, true);
    },
    applyFilter(tab, filter) {
      useTabs.getState().keepTab(tab.id);
      if (get().byTab[tab.id]) go(tab, { filter, offset: 0, back: [] }, false);
    },
    clearFilter(tab) {
      const s = get().byTab[tab.id];
      if (!s) return;
      patch(tab.id, { draft: { ...s.draft, rows: [], rawText: '' } });
      if (s.filter) go(tab, { filter: null, offset: 0, back: [] }, false);
    },
    setDraft(tab, draft) {
      patch(tab.id, { draft });
    },
    toggleFilterBar(tab, open) {
      const s = get().byTab[tab.id];
      if (s) patch(tab.id, { filterBarOpen: open ?? !s.filterBarOpen });
    },
    async count(tab) {
      const state = get().byTab[tab.id];
      if (!state) return;
      const token = ++counter;
      const key = countKey(tab.id);
      tokens.set(key, token);
      patch(tab.id, { count: { status: 'loading' } });
      const latest = () => tokens.get(key) === token && isOpen(tab.id);
      try {
        const { rows } = await api.tableRowCount(tab.connectionId, tab.objectRef, state.filter, key);
        if (latest()) patch(tab.id, { count: { status: 'ok', data: rows } });
      } catch (e) {
        if (!latest()) return;
        const error: AppError = toAppError(e);
        patch(tab.id, { count: isCancelled(error) ? undefined : { status: 'error', error } });
      }
    },
    cancelPage(tab) {
      if (get().byTab[tab.id]?.page?.status === 'loading') cancelRequest(tab.connectionId, pageKey(tab.id));
    },
    cancelCount(tab) {
      if (get().byTab[tab.id]?.count?.status !== 'loading') return;
      tokens.delete(countKey(tab.id));
      patch(tab.id, { count: undefined });
      cancelRequest(tab.connectionId, countKey(tab.id));
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

// Forget the data of closed tabs, ignore their in-flight answers and stop their statements.
useTabs.subscribe((state) => {
  const open = new Set(state.tabs.map((t) => t.id));
  const { byTab } = useTableData.getState();
  const gone = Object.keys(byTab).filter((id) => !open.has(id));
  if (gone.length === 0) return;
  for (const id of gone) {
    tokens.delete(pageKey(id));
    tokens.delete(countKey(id));
    const s = byTab[id]!;
    if (s.page?.status === 'loading') cancelRequest(s.connectionId, pageKey(id));
    if (s.count?.status === 'loading') cancelRequest(s.connectionId, countKey(id));
  }
  useTableData.setState({ byTab: Object.fromEntries(Object.entries(byTab).filter(([id]) => open.has(id))) });
});

/** Forget a DDL target's old page and cancel requests before loading its new structure. */
export function invalidateTableData(tabId: string): void {
  tokens.delete(pageKey(tabId));
  tokens.delete(countKey(tabId));
  const state = useTableData.getState().byTab[tabId];
  if (state?.page?.status === 'loading') cancelRequest(state.connectionId, pageKey(tabId));
  if (state?.count?.status === 'loading') cancelRequest(state.connectionId, countKey(tabId));
  useTableData.setState((s) => ({ byTab: Object.fromEntries(Object.entries(s.byTab).filter(([id]) => id !== tabId)) }));
}
