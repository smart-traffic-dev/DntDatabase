import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { create } from 'zustand';
import { api, needsReconnect, toAppError } from '../api';
import type { AppError, CellInput, ColumnMeta, EditableInfo, FilterOp, SortSpec, TableDataPage, Value } from '../bindings';
import { ActionMenu, type MenuEntry } from '../components/ActionMenu';
import { CloseIcon, FilterIcon, MoreIcon, NextIcon, PrevIcon, RefreshIcon, StopIcon } from '../components/icons';
import { belowRight } from './stripWidth';
import { selectTopSql } from '../components/newQuery';
import { dataExportRequest, requestExport, toggleTabFilterBar } from '../dialogs/resultExport';
import { useAppSettings } from '../store/appSettings';
import { confirmDiscard, readOnlyCopy, rowIdOf, rowKeyPairs, rowRef, UNKNOWN_OUTCOME, useDataEdits, type RowId, type Staged } from '../store/dataEdits';
import { useObjectData } from '../store/objectData';
import { dialectOfConnection, useQueryDocs } from '../store/queryTabs';
import { CHUNK, type ResultSet } from '../store/queryRun';
import { useTabs, type ObjectTabRef as Tab } from '../store/tabs';
import { reportGridSelection } from '../store/gridSelection';
import { draftToFilter, newFilterRow, pageTotal, useTableData, type FilterRow } from '../store/tableData';
import { cellStatus, effectivePage } from './data/editModel';
import { EditPreviewDialog } from './data/EditPreviewDialog';
import { copyAs, copySql, type CopyFormat } from './data/copyAs';
import { isCutCell } from './data/cutCells';
import { closeValueEditor, openValueEditor, useValueEditors } from './data/openValueEditor';
import { ValueEditor } from './data/ValueEditor';
import { FilterBar, type FilterBarHandle } from './FilterBar';
import { editText } from './query/format';
import { GRID_HEADER_HEIGHT, ResultGrid, writeClipboard, type GridEditing } from './query/ResultGrid';
import './query/query.css';
import './data/dataEdit.css';
import './dataView.css';

const NO_STAGED: Staged = { updates: new Map(), inserts: [], deletes: new Map() };
const NO_ROW_IDS: (RowId | null)[] = [];
const NO_CONFLICTS: RowId[] = [];
/** Stands in for the table's edit info until `data_editable` answers: no key, so no rows to copy as SQL. */
const NO_INFO: EditableInfo = { editable: false, reason: null, key: [], hidden_key: [], editable_columns: [], atomic: 'unknown', protection: 'snapshot' };

/** Page sizes always offered; the Settings default and the current size are added when different. */
export const PAGE_SIZES = [100, 500, 1000];

const fmt = (n: number) => n.toLocaleString('en-US');
const ARROW = { asc: '↑', desc: '↓' } as const;

/** PostgreSQL types without an order (the backend refuses to sort by them too). */
const UNSORTABLE_PG = new Set(['json', 'xml', 'point', 'line', 'lseg', 'box', 'path', 'polygon', 'circle']);

/** Whether a column of `dbType` cannot be sorted on (MySQL sorts every type). */
export function unsortable(engine: 'mysql' | 'postgres', dbType: string): boolean {
  if (engine !== 'postgres') return false;
  let t = dbType.trim().toLowerCase();
  while (t.endsWith('[]')) t = t.slice(0, -2).trim();
  t = t.split('.').pop() ?? t;
  t = (t.split('(')[0] ?? t).trim();
  return UNSORTABLE_PG.has(t);
}

/** The page as a chunked `ResultSet` for `ResultGrid` (column names untouched: copy uses them). */
export function pageToResult(page: TableDataPage): ResultSet {
  const chunks = [];
  for (let i = 0; i < page.rows.length; i += CHUNK) chunks.push(page.rows.slice(i, i + CHUNK));
  return {
    id: 0,
    index: 0,
    columns: page.columns,
    chunks,
    rowCount: page.rows.length,
    truncated: false,
    affected: null,
    elapsedMs: page.elapsed_ms,
    done: true,
  };
}

/** The header marks (sort arrows, numbered when several keys) by column index. */
export function sortMarks(page: TableDataPage, sort: SortSpec[]): Record<number, string> {
  const marks: Record<number, string> = {};
  sort.forEach((key, i) => {
    const index = page.columns.findIndex((c) => c.name === key.column);
    if (index >= 0) marks[index] = sort.length > 1 ? `${ARROW[key.dir]}${i + 1}` : ARROW[key.dir];
  });
  return marks;
}

/** "a ↑, b ↓" */
export const sortLabel = (sort: SortSpec[]) => sort.map((k) => `${k.column} ${ARROW[k.dir]}`).join(', ');

/** The sizes of the page-size menu, ascending. */
export function pageSizeOptions(settingsDefault: number | null, current: number): number[] {
  const all = new Set([...PAGE_SIZES, current, ...(settingsDefault ? [settingsDefault] : [])]);
  return [...all].sort((a, b) => a - b);
}

/** Text cells the backend cut (64 KB) cannot be filtered on exactly. */
const CUT_CELL_CHARS = 64 * 1024;

/**
 * The text a grid value is filtered by ("Filter = this value"), sent as a bound parameter;
 * null when the value cannot be matched exactly (binary, arrays, a cut text cell).
 */
export function filterValueText(v: Value, engine: 'mysql' | 'postgres'): string | null {
  switch (v.t) {
    case 'Null':
    case 'Bytes':
    case 'Array':
      return null;
    case 'Bool':
      return engine === 'postgres' ? String(v.v) : v.v ? '1' : '0';
    case 'Int':
    case 'UInt':
    case 'Float':
      return String(v.v);
    case 'Other':
      return v.v.text;
    case 'Text':
    case 'Json':
      return v.v.length >= CUT_CELL_CHARS ? null : v.v;
    default:
      return v.v;
  }
}

const shortText = (s: string) => (s.length > 40 ? `${s.slice(0, 40)}…` : s);

/** Adds a condition to the tab's filter and applies it (opening the bar). */
export function addFilterCondition(tab: Tab, row: FilterRow) {
  const store = useTableData.getState();
  const state = store.byTab[tab.id];
  if (!state) return;
  const kept = state.draft.mode === 'or' || state.draft.rawMode ? [] : state.draft.rows.filter((r) => r.column);
  const draft = { mode: 'and' as const, rows: [...kept, row], rawMode: false, rawText: state.draft.rawText ?? '' };
  store.setDraft(tab, draft);
  store.toggleFilterBar(tab, true);
  store.applyFilter(tab, draftToFilter(draft).filter);
}

/** The context of a grid cell menu (`ResultGrid`'s `extraCellMenu`). */
export interface CellMenuContext {
  row: number;
  col: number;
  value: Value;
  column: ColumnMeta;
}

/**
 * The data view's extra grid cell menu: "Filter: col = value" / "<> value" (IS NULL / IS NOT NULL
 * for NULL). Hook-up: `<ResultGrid … extraCellMenu={dataCellMenu(tab)} />`.
 */
export function dataCellMenu(tab: Tab): (ctx: CellMenuContext) => MenuEntry[] {
  return ({ value, column }) => {
    const add = (op: FilterOp, v = '') => () => addFilterCondition(tab, newFilterRow(column.name, op, v));
    if (value.t === 'Null') {
      return [
        { label: `Filter: ${column.name} IS NULL`, run: add('is_null') },
        { label: `Filter: ${column.name} IS NOT NULL`, run: add('is_not_null') },
      ];
    }
    const text = filterValueText(value, dialectOfConnection(tab.connectionId));
    if (text === null) return [];
    return [
      { label: `Filter: ${column.name} = ${shortText(text)}`, run: add('eq', text) },
      { label: `Filter: ${column.name} <> ${shortText(text)}`, run: add('ne', text) },
    ];
  };
}

/** Ticks per tab that ask its data view to focus the filter bar (the strip's Filter button sits outside the view). */
const useFilterFocus = create<{ ticks: Record<string, number> }>(() => ({ ticks: {} }));

/** Opens the filter bar of `tab`'s data view and moves the focus into it (F3, the strip's Filter button). */
export function openFilterBar(tab: Tab) {
  toggleTabFilterBar(tab, true);
  useFilterFocus.setState((s) => ({ ticks: { ...s.ticks, [tab.id]: (s.ticks[tab.id] ?? 0) + 1 } }));
}

/** Runs `action` once the staged changes of the tab are kept or thrown away (the user is asked first). */
function guardedAction(tabId: string, action: () => void) {
  if (!useDataEdits.getState().hasStaged(tabId)) return action();
  void confirmDiscard(tabId).then((go) => {
    if (go) action();
  });
}

/** Reloads the rows and the table's metadata (the Refresh button, F5); staged changes are asked about first. */
export function refreshData(tab: Tab) {
  guardedAction(tab.id, () => {
    void useTableData.getState().load(tab);
    // The metadata too (columns, row estimate): the one Refresh of the Data section.
    void useObjectData.getState().refresh(tab, 'detail');
  });
}

/** The previous / next page (Alt+PgUp / Alt+PgDn); false when there is none to go to (first page, last page, still loading). */
export function turnPage(tab: Tab, direction: 'prev' | 'next'): boolean {
  const store = useTableData.getState();
  const state = store.byTab[tab.id];
  if (!state || state.page?.status === 'loading') return false;
  if (direction === 'prev' ? state.offset === 0 : !(state.page?.status === 'ok' && state.page.data.has_more)) return false;
  guardedAction(tab.id, () => store[direction](tab));
  return true;
}

/** Opens this page's SELECT (sort, offset and size; not the filter) in a new query tab. */
function openPageInQuery(tab: Tab) {
  const state = useTableData.getState().byTab[tab.id];
  if (!state) return;
  const sql = selectTopSql(tab, dialectOfConnection(tab.connectionId), { limit: state.pageSize, offset: state.offset, orderBy: state.sort[0] ?? null });
  const docs = useQueryDocs.getState();
  const id = docs.newQuery(tab.connectionId, tab.objectRef.database);
  docs.update(id, { sql });
}

/**
 * The Data sub-tab's controls in the group's tab strip (editor v2 §2): Filter (F3), Refresh
 * (Cancel while a page loads) and `⋯` with Export, Open in query, the page size and Count rows.
 */
export function DataActions({ tab }: { tab: Tab }) {
  const state = useTableData((s) => s.byTab[tab.id]);
  const settingsDefault = useAppSettings((s) => s.tablePageSize);
  const [more, setMore] = useState<{ x: number; y: number } | null>(null);
  const store = useTableData.getState();
  const loading = state?.page?.status === 'loading';
  const hasData = state?.page?.status === 'ok';
  const guarded = (action: () => void) => guardedAction(tab.id, action);

  const refresh = () => refreshData(tab);

  const moreItems = (): MenuEntry[] => {
    if (!state) return [];
    return [
      {
        label: 'Export…',
        disabled: !hasData,
        title: 'Export these rows or all rows to a file',
        run: () => requestExport(dataExportRequest(tab, state)),
      },
      {
        label: 'Open in query',
        title: state.filter !== null ? "Open this page's SELECT in a new query tab (without the filter)" : "Open this page's SELECT in a new query tab",
        run: () => openPageInQuery(tab),
      },
      { separator: true },
      ...pageSizeOptions(settingsDefault, state.pageSize).map((n) => ({
        label: `${fmt(n)} rows per page`,
        checked: n === state.pageSize,
        run: () => {
          if (n !== state.pageSize) guarded(() => store.setPageSize(tab, n));
        },
      })),
      { separator: true },
      state.count?.status === 'loading'
        ? { label: 'Cancel count', run: () => store.cancelCount(tab) }
        : { label: 'Count rows', run: () => void store.count(tab) },
    ];
  };

  return (
    <>
      <button
        type="button"
        className="ta-btn"
        aria-label="Filter"
        aria-pressed={state?.filterBarOpen ?? false}
        title="Filter rows (F3)"
        disabled={!state}
        onClick={() => {
          if (!state) return;
          if (state.filterBarOpen) store.toggleFilterBar(tab, false);
          else openFilterBar(tab);
        }}
      >
        <FilterIcon />
      </button>
      {loading ? (
        <button type="button" className="ta-btn" aria-label="Cancel" title="Stop loading the page" onClick={() => store.cancelPage(tab)}>
          <StopIcon />
        </button>
      ) : (
        <button
          type="button"
          className="ta-btn"
          aria-label="Refresh"
          title="Refresh the rows and the table's metadata"
          disabled={!state}
          onClick={refresh}
        >
          <RefreshIcon />
        </button>
      )}
      <button
        type="button"
        className="ta-btn"
        aria-label="More data actions"
        title="Export, open in query, page size, count rows"
        aria-haspopup="menu"
        aria-expanded={more !== null}
        disabled={!state}
        onMouseDown={(e) => {
          if (more) e.stopPropagation();
        }}
        onClick={(e) => setMore(more ? null : belowRight(e.currentTarget))}
      >
        <MoreIcon />
      </button>
      {more ? <ActionMenu label="Data actions" x={more.x} y={more.y} items={moreItems()} checkRole="radio" onClose={() => setMore(null)} /> : null}
    </>
  );
}

/**
 * The Data sub-tab's pager, in the object tab's sub-tab row: the filter and sort state, the
 * rows shown and the total, Previous and Next.
 */
export function DataPager({ tab, estimate }: { tab: Tab; estimate: number | null }) {
  const state = useTableData((s) => s.byTab[tab.id]);
  if (!state) return null;
  const store = useTableData.getState();
  const guarded = (action: () => void) => guardedAction(tab.id, action);
  const page = state.page;
  const data = page?.status === 'ok' ? page.data : null;
  const loading = page?.status === 'loading';
  const first = state.offset + 1;
  const last = state.offset + (data?.rows.length ?? 0);
  const range = loading ? 'Loading…' : data ? (data.rows.length === 0 ? 'No rows' : `${fmt(first)}–${fmt(last)}`) : '';
  const total = pageTotal(state, estimate);
  const sort = state.sort;
  return (
    <div className="dv-pagerbar" role="group" aria-label="Pages">
      {state.filter !== null ? <span className="dv-filtered">Filtered ({state.filter.raw_where ? 'raw' : state.filter.conditions.length})</span> : null}
      {sort.length > 0 ? (
        <span className="dv-chip" title={`Sorted by ${sortLabel(sort)}`}>
          {sortLabel(sort)}
          <button type="button" className="dv-chip-x" aria-label="Clear sort" title="Clear sort" onClick={() => guarded(() => store.clearSort(tab))}>
            <CloseIcon />
          </button>
        </span>
      ) : data && data.key_order.length > 0 ? (
        <span className="dv-key-order" title="Without a sort, pages follow the primary key so paging is stable">
          By primary key
        </span>
      ) : null}
      <span className="dv-pager" data-testid="data-pager" aria-live="polite">
        <span>{range}</span>
        {total ? (
          <>
            <span aria-hidden="true"> / </span>
            <span data-testid="data-total" title={total.tip}>
              {total.text}
            </span>
          </>
        ) : null}
      </span>
      <button
        type="button"
        className="icon-btn"
        aria-label="Previous page"
        title="Previous page"
        onClick={() => guarded(() => store.prev(tab))}
        disabled={loading || state.offset === 0}
      >
        <PrevIcon />
      </button>
      <button
        type="button"
        className="icon-btn"
        aria-label="Next page"
        title="Next page"
        onClick={() => guarded(() => store.next(tab))}
        disabled={loading || !data?.has_more}
      >
        <NextIcon />
      </button>
    </div>
  );
}

function DataError({ error, onRetry, onReconnect }: { error: AppError; onRetry(): void; onReconnect(): void }) {
  const reconnect = needsReconnect(error);
  return (
    <div role="alert" className="dv-error">
      <strong>{error.message}</strong>
      {error.hint ? <p>{error.hint}</p> : null}
      {error.detail ? (
        <details>
          <summary>Details</summary>
          <pre>{error.detail}</pre>
        </details>
      ) : null}
      <button type="button" className="qt-btn" onClick={reconnect ? onReconnect : onRetry}>
        {reconnect ? 'Reconnect' : 'Retry'}
      </button>
    </div>
  );
}

/**
 * The Data sub-tab of a table / view: one page of rows (server-side LIMIT / OFFSET / ORDER BY /
 * WHERE from the filter bar). The page size starts at the Settings default; changing it here
 * affects this tab only. Header click sorts by that column; Shift+click adds a sort key. F3
 * opens / focuses the filter bar. Its controls are `DataActions` (in the tab strip) and
 * `DataPager` (in the object tab's sub-tab row).
 */
export function DataView({ tab }: { tab: Tab }) {
  const state = useTableData((s) => s.byTab[tab.id]);
  const [notice, setNotice] = useState<string | null>(null);
  const [editableError, setEditableError] = useState<string | null>(null);
  const [filterError, setFilterError] = useState<string | null>(null);
  // F3 and the strip's Filter button ask for the focus through this tick (only changes count, not the value at mount).
  const focusTick = useFilterFocus((s) => s.ticks[tab.id] ?? 0);
  const seenTick = useRef(focusTick);
  const barRef = useRef<FilterBarHandle>(null);
  // Glide's header click carries no modifier keys through `ResultGrid`: remember Shift here.
  const shift = useRef(false);

  useEffect(() => {
    if (useTableData.getState().byTab[tab.id]) return;
    let live = true;
    void useAppSettings
      .getState()
      .loadTablePageSize()
      .then((size) => {
        if (live) useTableData.getState().init(tab, size);
      });
    return () => {
      live = false;
    };
  }, [tab]);

  useEffect(() => {
    if (focusTick === seenTick.current) return;
    seenTick.current = focusTick;
    barRef.current?.focus();
  }, [focusTick]);

  // The grid's selection summary goes to the status bar while this view is shown.
  const onSelectionSummary = useCallback((summary: string | null) => reportGridSelection(tab.id, summary), [tab.id]);
  useEffect(() => () => reportGridSelection(tab.id, null), [tab.id]);

  // Whether the table may be edited, and how (the answer lives with the staged changes).
  useEffect(() => {
    let live = true;
    const set = (info: EditableInfo | null) => {
      if (live && useTabs.getState().tabs.some((t) => t.id === tab.id)) useDataEdits.getState().setInfo(tab.id, tab.objectRef, info);
    };
    setEditableError(null);
    api.dataEditable(tab.connectionId, tab.objectRef).then(set, (e) => {
      // Read-only meanwhile; the reason is shown.
      if (live) setEditableError(toAppError(e).message);
      set(null);
    });
    return () => {
      live = false;
    };
  }, [tab.id, tab.connectionId, tab.objectRef]);

  const page = state?.page;
  const data = page?.status === 'ok' ? page.data : null;
  const edit = useDataEdits((s) => s.byTab[tab.id]);
  const info = edit?.info ?? null;
  const staged = edit?.staged ?? NO_STAGED;
  const conflicts = edit?.conflicts ?? NO_CONFLICTS;
  const editable = info?.editable === true && data !== null;
  const rowIds = useMemo(() => (data && info ? data.rows.map((_, i) => rowIdOf(data, info, i)) : NO_ROW_IDS), [data, info]);
  const shown = useMemo(() => (data && editable ? effectivePage(data, staged, rowIds) : data), [data, editable, staged, rowIds]);
  const sort = useMemo(() => state?.sort ?? [], [state?.sort]);
  const result = useMemo(() => (shown ? pageToResult(shown) : null), [shown]);
  const marks = useMemo(() => (data ? sortMarks(data, sort) : {}), [data, sort]);
  const [editNotice, setEditNotice] = useState<string | null>(null);

  /** Runs `action` once staged changes are kept or thrown away (the user is asked first). */
  const guarded = (action: () => void) => guardedAction(tab.id, action);

  const handleEditKey = (
    e: { key: string; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean; altKey: boolean },
    cell: { row: number; col: number } | null,
  ): boolean => {
    const dataNow = data;
    if (!editable || !dataNow || !(e.ctrlKey || e.metaKey) || e.altKey) return false;
    const edits = useDataEdits.getState();
    const report = (r: { ok: true } | { ok: false; message: string }) => setEditNotice(r.ok ? null : r.message);
    const key = e.key.toLowerCase();
    if (key === 'n' && !e.shiftKey) {
      report(edits.stage(tab.id, { kind: 'insert' }));
      return true;
    }
    if (key === 'z' && !e.shiftKey) {
      edits.undo(tab.id);
      setEditNotice(null);
      return true;
    }
    if (key === 's' && !e.shiftKey) {
      if (edits.hasStaged(tab.id)) void edits.startApply(tab);
      return true;
    }
    if (!cell) return false;
    const n = dataNow.rows.length;
    if (e.key === 'Delete') {
      if (cell.row < n) {
        const id = rowIds[cell.row];
        const ref = info ? rowRef(dataNow, info, cell.row) : null;
        if (id && ref) report(edits.stage(tab.id, { kind: 'delete', row: id, ref }));
      } else {
        const insert = staged.inserts[cell.row - n];
        if (insert) report(edits.stage(tab.id, { kind: 'dropInsert', insert: insert.id }));
      }
      return true;
    }
    if (e.key === '0') {
      const name = dataNow.columns[cell.col]?.name;
      if (name) setCell(cell.row, cell.col, { kind: 'null' });
      return true;
    }
    return false;
  };

  /** Stages `input` for a grid cell (an existing row's update or a new row's value). */
  const setCell = (row: number, col: number, input: CellInput, fetched: string | null | undefined = undefined): boolean => {
    if (!data || !info) return false;
    const name = data.columns[col]?.name;
    if (!name || !info.editable_columns.includes(name)) {
      setEditNotice('This column cannot be edited.');
      return false;
    }
    const edits = useDataEdits.getState();
    const n = data.rows.length;
    if (row < n) {
      const id = rowIds[row];
      // The row as loaded now is what the change is guarded with, whatever the page shows later.
      const base = rowRef(data, info, row);
      // A cut cell is changed from the value editor only (`fetched` is then set: null or the token of its full value).
      if (!id || !base) {
        setEditNotice('This row has no complete key, so it cannot be edited.');
        return false;
      }
      if (fetched === undefined && isCutCell(data, row, col)) {
        setEditNotice('This value is cut short; open it in the value editor to change it.');
        return false;
      }
      if (fetched === undefined && input.kind === 'text' && !inline(row, col)) {
        setEditNotice('Change this value in the value editor.');
        return false;
      }
      // The token of a cut original fetched in full lets the backend check that column too.
      const ref = fetched ? { ...base, loaded: [[name, fetched] as [string, string]] } : base;
      setEditNotice(null);
      const r = edits.stage(tab.id, { kind: 'update', row: id, ref, column: name, input });
      if (!r.ok) setEditNotice(r.message);
      return r.ok;
    }
    const insert = (useDataEdits.getState().byTab[tab.id]?.staged ?? NO_STAGED).inserts[row - n];
    if (!insert) {
      setEditNotice('That new row is gone.');
      return false;
    }
    const r = edits.stage(tab.id, { kind: 'insertCell', insert: insert.id, column: name, input });
    setEditNotice(r.ok ? null : r.message);
    return r.ok;
  };

  /** Whether a cell of the loaded page is not its whole value (the one rule: `isCutCell`). */
  const cutAt = (row: number, col: number) => !!data && row < data.rows.length && isCutCell(data, row, col);

  /**
   * Whether a cell is edited in place: its text in the grid is its value. Binary columns, cut
   * cells, arrays and long text / JSON are changed in the value editor (which fetches cut values first).
   */
  function inline(row: number, col: number): boolean {
    if (!data || data.columns[col]?.logical === 'Binary' || cutAt(row, col)) return false;
    const now = shown?.rows[row]?.[col];
    return now === undefined || editText(now) !== null;
  }

  /** Whether a cell may be changed at all (an editable column of a row not marked for deletion). */
  const writable = (row: number, col: number) => {
    const name = data?.columns[col]?.name;
    if (!data || !info || !name || !info.editable_columns.includes(name)) return false;
    const id = rowIds[row];
    return row >= data.rows.length || (!!id && !staged.deletes.has(id));
  };

  const editing = useMemo<GridEditing | undefined>(() => {
    if (!editable || !data || !shown || !info) return undefined;
    const names = data.columns.map((c) => c.name);
    const open = (row: number, col: number, column: string) =>
      openValueEditor({ tabId: tab.id, row, col, column, rowId: row < data.rows.length ? (rowIds[row] ?? null) : null });
    return {
      isEditable: (row, col) => writable(row, col) && inline(row, col),
      status: (row, col) => cellStatus(data, staged, conflicts, rowIds, row, names[col] ?? ''),
      onEdit: (row, col, text) => {
        const name = names[col];
        if (!name) return;
        // Never stage the grid's text of a value it does not show whole.
        if (!inline(row, col)) {
          open(row, col, name);
          return;
        }
        const now = shown.rows[row]?.[col];
        if (now && editText(now) === text) return;
        setCell(row, col, { kind: 'text', text });
      },
      onActivate: (row, col) => {
        const name = names[col];
        if (name && !inline(row, col)) open(row, col, name);
      },
      onKey: handleEditKey,
    };
    // `setCell`, `inline`, `writable` and `handleEditKey` read the latest values through the stores and these deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editable, data, shown, info, staged, conflicts, rowIds]);

  /** Opens the value editor on any cell (Shift+Enter, the cell menu). */
  const openCell = (row: number, col: number) => {
    const name = data?.columns[col]?.name;
    if (name) openValueEditor({ tabId: tab.id, row, col, column: name, rowId: rowIds[row] ?? null });
  };

  const editorCell = useValueEditors((s) => s.byTab[tab.id]);
  // An editor left over from another page or row is closed, so it can never stage onto a different row.
  useEffect(() => {
    if (!editorCell || !data) return;
    const stale = editorCell.row < data.rows.length ? rowIds[editorCell.row] !== editorCell.rowId : editorCell.rowId !== null;
    if (stale) closeValueEditor(tab.id);
  }, [editorCell, data, rowIds, tab.id]);
  // Stale after a reload that left the page smaller or another table's page.
  const valuePanel = (() => {
    if (!editorCell || !data || !shown) return null;
    const column = data.columns[editorCell.col];
    if (!column || column.name !== editorCell.column || editorCell.row >= shown.rows.length) return null;
    const { row, col } = editorCell;
    const isNew = row >= data.rows.length;
    const id = rowIds[row] ?? null;
    // The page changed under the editor (paging, reload, filter): the cell is another row now.
    if (!isNew && editorCell.rowId !== id) return null;
    // What the cell holds now: a change staged for it (when it is text, NULL or a value), else the page's value.
    const change = isNew ? staged.inserts[row - data.rows.length]?.values.get(column.name) : id ? staged.updates.get(id)?.cells.get(column.name) : undefined;
    const showsChange = change !== undefined && (change.kind === 'text' || change.kind === 'null' || change.kind === 'value');
    const cut = !isNew && !showsChange && isCutCell(data, row, col);
    const value = showsChange ? (shown.rows[row]?.[col] ?? { t: 'Null' as const }) : (data.rows[row]?.[col] ?? shown.rows[row]?.[col] ?? { t: 'Null' as const });
    const canEdit = editable && !!info?.editable_columns.includes(column.name) && (isNew || (!!id && !staged.deletes.has(id)));
    return (
      <ValueEditor
        key={`${editorCell.rowId ?? 'new'},${row},${column.name},${String(showsChange)}`}
        connectionId={tab.connectionId}
        object={tab.objectRef}
        column={column}
        value={value}
        truncated={cut}
        fetchKey={!isNew && info ? rowKeyPairs(data, info, row) : null}
        editable={canEdit}
        onStage={(input, token) => {
          if (setCell(row, col, input, cut ? token : null)) closeValueEditor(tab.id);
        }}
        onClose={() => closeValueEditor(tab.id)}
      />
    );
  })();

  /** Copies the selection as `format` (text) or as SQL (built by the backend); says what happened. */
  const copySelection = async (what: CopyFormat | 'insert' | 'update', selection: Parameters<typeof copyAs>[1], cell: { row: number; col: number }) => {
    if (!data) return;
    try {
      const text =
        what === 'insert' || what === 'update'
          ? await copySql(what, selection, data, { connectionId: tab.connectionId, object: tab.objectRef, info: info ?? NO_INFO, cell })
          : copyAs(what, selection, data, { cell });
      setNotice((await writeClipboard(text)) ? null : 'Could not write to the clipboard');
    } catch (e) {
      setNotice(e instanceof Error ? e.message : toAppError(e).message);
    }
  };

  if (!state) return <p className="dv-loading muted">Loading…</p>;

  const store = useTableData.getState();
  const engine = dialectOfConnection(tab.connectionId);
  const filtered = state.filter !== null;
  const count = state.count;

  const onHeaderClicked = (index: number) => {
    const column = data?.columns[index];
    if (!column) return;
    if (unsortable(engine, column.db_type)) {
      setNotice(`Can't sort by ${column.name}: values of type ${column.db_type} have no order.`);
      return;
    }
    setNotice(null);
    const additive = shift.current;
    guarded(() => store.toggleSort(tab, column.name, additive));
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'F3') {
      e.preventDefault();
      openFilterBar(tab);
      return;
    }
    // Edit keys outside the grid (the grid handles its own); typing in a box keeps its own Ctrl+Z etc.
    if ((e.target as HTMLElement).closest('input, textarea, select, [contenteditable="true"]')) return;
    if (handleEditKey(e, null)) e.preventDefault();
  };

  const apply = () => {
    const { filter, error } = draftToFilter(state.draft);
    setFilterError(error);
    if (!error) guarded(() => store.applyFilter(tab, filter));
  };

  const addRow = () => {
    const r = useDataEdits.getState().stage(tab.id, { kind: 'insert' });
    setEditNotice(r.ok ? null : r.message);
  };

  // A raw WHERE the server refused (`config`) is shown under the filter bar, not as a page failure.
  const rawError =
    page?.status === 'error' && page.error.kind === 'config' && state.filter?.raw_where
      ? [page.error.message, page.error.hint].filter(Boolean).join(' ')
      : null;
  const changes = edit ? useDataEdits.getState().changeCount(tab.id) : 0;
  const nUpdated = [...staged.updates.values()].reduce((n, u) => n + u.cells.size, 0);

  const truncated = data !== null && data.truncated_cells > 0;
  // Every cut cell is a binary preview (the value editor shows all bytes; a query would not).
  const onlyBinaryCut =
    truncated &&
    data !== null &&
    data.truncated.length > 0 &&
    data.rows.every((row, r) => row.every((v, c) => !isCutCell(data, r, c) || v.t === 'Bytes'));
  const capped = data?.size_capped === true;

  return (
    <div className="data-view" onKeyDown={onKeyDown}>
      {state.filterBarOpen || rawError ? (
        <FilterBar
          handle={barRef}
          columns={state.columns}
          draft={state.draft}
          active={filtered}
          error={filterError ?? rawError}
          onChange={(draft) => store.setDraft(tab, draft)}
          onApply={apply}
          onClear={() => {
            setFilterError(null);
            guarded(() => store.clearFilter(tab));
          }}
        />
      ) : null}
      {editable ? (
        <div className="de-bar" role="toolbar" aria-label="Edit">
          <button type="button" className="qt-btn" title="Add a row (Ctrl+N)" onClick={addRow}>
            Add row
          </button>
          {changes > 0 ? (
            <>
              <button type="button" className="qt-btn" title="Undo (Ctrl+Z)" onClick={() => useDataEdits.getState().undo(tab.id)}>
                Undo
              </button>
              <button type="button" className="qt-btn" title="Review and apply (Ctrl+S)" onClick={() => void useDataEdits.getState().startApply(tab)}>
                Apply changes
              </button>
              <button type="button" className="qt-btn" onClick={() => void confirmDiscard(tab.id)}>
                Discard changes
              </button>
              <span className="spacer" />
              {nUpdated > 0 ? <span className="de-chip dnt-cell-edited">{nUpdated} edited</span> : null}
              {staged.inserts.length > 0 ? <span className="de-chip dnt-row-new">{staged.inserts.length} new</span> : null}
              {staged.deletes.size > 0 ? <span className="de-chip dnt-row-deleted">{staged.deletes.size} deleted</span> : null}
            </>
          ) : null}
        </div>
      ) : null}
      {editableError ? (
        <p role="status" className="dv-note dv-note-error">
          Couldn't check whether this table is editable: {editableError}
        </p>
      ) : null}
      {/* A read-only connection is the context chip's lock (in the tab strip), not a note here. */}
      {info && !info.editable && info.reason !== 'read_only_connection' ? (
        <p role="status" className="dv-note">
          {readOnlyCopy(info.reason)}
        </p>
      ) : null}
      {editNotice ? (
        <p role="status" className="dv-note">
          {editNotice}
        </p>
      ) : null}
      {edit?.unknown ? (
        <p role="status" className="de-banner de-error">
          <span>{UNKNOWN_OUTCOME}</span>
          {/* Reloading would paint the staged values over what the server holds: show the server's. */}
          <button
            type="button"
            className="qt-btn"
            onClick={() =>
              void confirmDiscard(tab.id).then((go) => {
                if (go) void store.load(tab);
              })
            }
          >
            Discard staged changes and reload
          </button>
        </p>
      ) : null}
      {edit?.error ? (
        <p role="status" className="de-banner de-error">
          {edit.error}
        </p>
      ) : null}
      {conflicts.length > 0 ? (
        <div role="status" className="de-banner">
          <span>
            {conflicts.length === 1 ? 'A row changed on the server since it was loaded.' : `${conflicts.length} rows changed on the server since they were loaded.`}
          </span>
          {conflicts.map((id) => {
            const at = rowIds.indexOf(id);
            return (
              <span key={id} className="de-conflict-row">
                {at >= 0 ? `Row ${fmt(state.offset + at + 1)}` : 'Row'}
                <button type="button" className="qt-btn" onClick={() => useDataEdits.getState().reloadRow(tab, id)}>
                  Reload row
                </button>
                <button
                  type="button"
                  className="qt-btn"
                  title="Apply this row's change without the conflict check; the other changes stay staged"
                  onClick={() => void useDataEdits.getState().startApply(tab, { overwrite: true, only: id })}
                >
                  Overwrite
                </button>
              </span>
            );
          })}
        </div>
      ) : null}
      {count?.status === 'error' ? (
        <p role="status" className="dv-note dv-note-error">
          Couldn't count the rows: {count.error.message}
        </p>
      ) : null}
      {notice ? (
        <p role="status" className="dv-note">
          {notice}
        </p>
      ) : null}
      {truncated || capped ? (
        <p role="status" className="dv-note">
          {truncated
            ? onlyBinaryCut
              ? 'Some binary values are shown as a preview; open them in the value editor (Shift+Enter) to see all bytes.'
              : 'Some values are truncated; open in query to see full values.'
            : null}
          {truncated && capped ? ' ' : null}
          {capped ? 'This page stopped early to stay small; Next continues after its last row.' : null}
        </p>
      ) : null}
      <div className="dv-main">
      <div
        className="dv-body"
        onPointerDownCapture={(e) => {
          shift.current = e.shiftKey;
        }}
        onMouseDownCapture={(e) => {
          shift.current = e.shiftKey;
        }}
      >
        {rawError ? (
          <p className="dv-empty muted">The WHERE condition was not accepted; fix it in the filter bar.</p>
        ) : page?.status === 'error' ? (
          <DataError error={page.error} onRetry={() => void store.load(tab)} onReconnect={() => void store.reconnect(tab)} />
        ) : result ? (
          <>
            {/* No rows: the header still shows the columns, the message sits under it. */}
            {result.rowCount === 0 && state.offset === 0 && !editable ? (
              <p className="dv-empty dv-empty-rows muted" style={{ top: GRID_HEADER_HEIGHT }}>
                {filtered ? 'No rows match the filter.' : `This ${tab.objectRef.kind === 'Table' ? 'table' : 'view'} has no rows.`}
              </p>
            ) : null}
            <ResultGrid
              result={result}
              onHeaderClicked={onHeaderClicked}
              headerMarks={marks}
              firstRowNumber={state.offset + 1}
              engine={engine}
              tableRef={tab.objectRef}
              cellsCapped={truncated}
              editing={editing}
              onOpenValue={openCell}
              onSelectionSummary={onSelectionSummary}
              extraCellMenu={(ctx) => {
                const column = result.columns[ctx.col];
                const filters = column
                  ? dataCellMenu(tab)({ ...ctx, value: ctx.value as Value, column }).map((entry) =>
                      'run' in entry ? { ...entry, run: () => guarded(() => entry.run()) } : entry,
                    )
                  : [];
                if (!data) return filters;
                const at = { row: ctx.row, col: ctx.col };
                const copies: MenuEntry[] = [
                  { label: 'Copy as JSON', run: () => void copySelection('json', ctx.selection, at) },
                  { label: 'Copy as Markdown', run: () => void copySelection('markdown', ctx.selection, at) },
                  { label: 'Copy as SQL INSERT', run: () => void copySelection('insert', ctx.selection, at) },
                  { label: 'Copy as SQL UPDATE', run: () => void copySelection('update', ctx.selection, at) },
                ];
                if (!editing) return [...copies, ...(filters.length > 0 ? [{ separator: true } as MenuEntry, ...filters] : [])];
                const editEntries: MenuEntry[] = [];
                if (writable(ctx.row, ctx.col) && !cutAt(ctx.row, ctx.col)) {
                  editEntries.push({ label: 'Set to NULL', hint: 'Ctrl+0', run: () => setCell(ctx.row, ctx.col, { kind: 'null' }) });
                }
                editEntries.push({
                  label: ctx.row < data.rows.length ? 'Delete row' : 'Remove new row',
                  hint: 'Ctrl+Delete',
                  run: () => handleEditKey({ key: 'Delete', ctrlKey: true, metaKey: false, shiftKey: false, altKey: false }, { row: ctx.row, col: ctx.col }),
                });
                const head = [...editEntries, { separator: true } as MenuEntry, ...copies];
                return filters.length > 0 ? [...head, { separator: true }, ...filters] : head;
              }}
            />
          </>
        ) : (
          <p className="dv-loading muted">Loading…</p>
        )}
      </div>
      {valuePanel}
      </div>
      {edit?.applying ? (
        <EditPreviewDialog
          pending={edit.applying}
          onConfirm={() => void useDataEdits.getState().confirmApply(tab)}
          onCancel={() => useDataEdits.getState().cancelApply(tab.id)}
        />
      ) : null}
    </div>
  );
}
