import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { api, needsReconnect } from '../api';
import type { AppError, CellInput, ColumnMeta, EditableInfo, FilterOp, SortSpec, TableDataPage, Value } from '../bindings';
import type { MenuEntry } from '../components/ActionMenu';
import { selectTopSql } from '../components/newQuery';
import { dataExportRequest, requestExport, toggleTabFilterBar } from '../dialogs/resultExport';
import { useAppSettings } from '../store/appSettings';
import { confirmDiscard, readOnlyCopy, rowIdOf, UNKNOWN_OUTCOME, useDataEdits, type RowId, type Staged } from '../store/dataEdits';
import { dialectOfConnection, useQueryDocs } from '../store/queryTabs';
import { CHUNK, type ResultSet } from '../store/queryRun';
import { useTabs, type ObjectTabRef as Tab } from '../store/tabs';
import { draftToFilter, newFilterRow, useTableData, type FilterRow } from '../store/tableData';
import { cellStatus, effectivePage } from './data/editModel';
import { EditPreviewDialog } from './data/EditPreviewDialog';
import { openValueEditor } from './data/valueEditor';
import { FilterBar, type FilterBarHandle } from './FilterBar';
import { cellView } from './query/format';
import { ResultGrid, type GridEditing } from './query/ResultGrid';
import './query/query.css';
import './data/dataEdit.css';
import './dataView.css';

const NO_STAGED: Staged = { updates: new Map(), inserts: [], deletes: new Set() };
const NO_ROW_IDS: (RowId | null)[] = [];
const NO_CONFLICTS: RowId[] = [];

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
 * WHERE from the filter bar), read-only. The page size starts at the Settings default; changing
 * it here affects this tab only. Header click sorts by that column; Shift+click adds a sort key.
 * F3 opens / focuses the filter bar.
 */
export function DataView({ tab, estimate }: { tab: Tab; estimate: number | null }) {
  const state = useTableData((s) => s.byTab[tab.id]);
  const settingsDefault = useAppSettings((s) => s.tablePageSize);
  const [notice, setNotice] = useState<string | null>(null);
  const [filterError, setFilterError] = useState<string | null>(null);
  const [focusTick, setFocusTick] = useState(0);
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
    if (focusTick > 0) barRef.current?.focus();
  }, [focusTick]);

  // Whether the table may be edited, and how (the answer lives with the staged changes).
  useEffect(() => {
    let live = true;
    const set = (info: EditableInfo | null) => {
      if (live && useTabs.getState().tabs.some((t) => t.id === tab.id)) useDataEdits.getState().setInfo(tab.id, tab.objectRef, info);
    };
    api.dataEditable(tab.connectionId, tab.objectRef).then(set, () => set(null));
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
  const guarded = (action: () => void) => {
    if (!useDataEdits.getState().hasStaged(tab.id)) return action();
    void confirmDiscard(tab.id).then((go) => {
      if (go) action();
    });
  };

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
        if (id) report(edits.stage(tab.id, { kind: 'delete', row: id }));
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
  const setCell = (row: number, col: number, input: CellInput) => {
    if (!data || !info) return;
    const name = data.columns[col]?.name;
    if (!name || !info.editable_columns.includes(name)) return;
    const edits = useDataEdits.getState();
    const n = data.rows.length;
    if (row < n) {
      const id = rowIds[row];
      if (!id || truncatedCells.has(`${row},${col}`)) return;
      setEditNotice(null);
      const r = edits.stage(tab.id, { kind: 'update', row: id, column: name, input });
      if (!r.ok) setEditNotice(r.message);
    } else {
      const insert = (useDataEdits.getState().byTab[tab.id]?.staged ?? NO_STAGED).inserts[row - n];
      if (!insert) return;
      const r = edits.stage(tab.id, { kind: 'insertCell', insert: insert.id, column: name, input });
      setEditNotice(r.ok ? null : r.message);
    }
  };

  const truncatedCells = useMemo(() => new Set((data?.truncated ?? []).map(([r, c]) => `${r},${c}`)), [data]);

  const editing = useMemo<GridEditing | undefined>(() => {
    if (!editable || !data || !shown || !info) return undefined;
    const names = data.columns.map((c) => c.name);
    return {
      isEditable: (row, col) => {
        const name = names[col];
        if (!name || !info.editable_columns.includes(name) || truncatedCells.has(`${row},${col}`)) return false;
        const id = rowIds[row];
        return row >= data.rows.length || (!!id && !staged.deletes.has(id));
      },
      status: (row, col) => cellStatus(data, staged, conflicts, rowIds, row, names[col] ?? ''),
      onEdit: (row, col, text) => {
        const name = names[col];
        if (!name) return;
        if (row < data.rows.length && truncatedCells.has(`${row},${col}`)) {
          openValueEditor({ row, col, column: name, rowId: rowIds[row] ?? null });
          return;
        }
        const now = shown.rows[row]?.[col];
        if (now && cellView(now).text === text) return;
        setCell(row, col, { kind: 'text', text });
      },
      onActivate: (row, col) => {
        const name = names[col];
        if (name && row < data.rows.length && truncatedCells.has(`${row},${col}`)) {
          openValueEditor({ row, col, column: name, rowId: rowIds[row] ?? null });
        }
      },
      onKey: handleEditKey,
    };
    // `setCell` and `handleEditKey` read the latest values through the stores and these deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editable, data, shown, info, staged, conflicts, rowIds, truncatedCells]);

  if (!state) return <p className="dv-loading muted">Loading…</p>;

  const store = useTableData.getState();
  const engine = dialectOfConnection(tab.connectionId);
  const loading = page?.status === 'loading';
  const first = state.offset + 1;
  const last = state.offset + (data?.rows.length ?? 0);
  const rowsLabel = data ? (data.rows.length === 0 ? 'No rows' : `Rows ${fmt(first)}–${fmt(last)}`) : '';
  const filtered = state.filter !== null;

  const count = state.count;
  const total =
    count?.status === 'ok' ? `${fmt(count.data)} rows`
    : estimate !== null && !filtered ? `~${fmt(estimate)} rows`
    : null;

  const openInQuery = () => {
    const sql = selectTopSql(tab, engine, { limit: state.pageSize, offset: state.offset, orderBy: sort[0] ?? null });
    const docs = useQueryDocs.getState();
    const id = docs.newQuery(tab.connectionId, tab.objectRef.database);
    docs.update(id, { sql });
  };

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

  const openBar = () => {
    toggleTabFilterBar(tab, true);
    setFocusTick((n) => n + 1);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'F3') {
      e.preventDefault();
      openBar();
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
  const nUpdated = [...staged.updates.values()].reduce((n, cells) => n + cells.size, 0);

  const truncated = data !== null && data.truncated_cells > 0;
  const capped = data?.size_capped === true;

  return (
    <div className="data-view" onKeyDown={onKeyDown}>
      <div className="dv-toolbar" role="toolbar" aria-label="Data">
        {loading ? (
          <button type="button" className="qt-btn" onClick={() => store.cancelPage(tab)}>
            Cancel
          </button>
        ) : (
          <button type="button" className="qt-btn" onClick={() => guarded(() => void store.load(tab))}>
            Refresh
          </button>
        )}
        <span className="qt-sep" />
        <button type="button" className="qt-btn" aria-label="Previous page" onClick={() => guarded(() => store.prev(tab))} disabled={loading || state.offset === 0}>
          ‹ Prev
        </button>
        <button type="button" className="qt-btn" aria-label="Next page" onClick={() => guarded(() => store.next(tab))} disabled={loading || !data?.has_more}>
          Next ›
        </button>
        <label className="dv-size">
          <span>Page size</span>
          <select className="qt-select" value={state.pageSize} onChange={(e) => {
              const size = Number(e.target.value);
              guarded(() => store.setPageSize(tab, size));
            }}>
            {pageSizeOptions(settingsDefault, state.pageSize).map((n) => (
              <option key={n} value={n}>
                {fmt(n)}
              </option>
            ))}
          </select>
        </label>
        <span className="dv-status" aria-live="polite">
          {loading ? 'Loading…' : rowsLabel}
        </span>
        <span className="qt-sep" />
        <button
          type="button"
          className={`qt-btn${state.filterBarOpen ? ' dv-on' : ''}`}
          aria-pressed={state.filterBarOpen}
          title="Filter rows (F3)"
          onClick={() => {
            if (state.filterBarOpen) store.toggleFilterBar(tab, false);
            else openBar();
          }}
        >
          Filter
        </button>
        {filtered ? (
          <span className="dv-filtered">Filtered ({state.filter!.raw_where ? 'raw' : state.filter!.conditions.length})</span>
        ) : null}
        {sort.length > 0 ? (
          <span className="dv-sort">
            Sorted by {sortLabel(sort)}
            <button type="button" className="dv-link" onClick={() => guarded(() => store.clearSort(tab))}>
              Clear sort
            </button>
          </span>
        ) : data && data.key_order.length > 0 ? (
          <span className="dv-sort dv-key-order" title="Without a sort, pages follow the primary key so paging is stable">
            By primary key
          </span>
        ) : null}
        <span className="spacer" />
        {total ? <span className="dv-total">{total}</span> : null}
        {count?.status === 'loading' ? (
          <button type="button" className="qt-btn" onClick={() => store.cancelCount(tab)}>
            Cancel count
          </button>
        ) : (
          <button
            type="button"
            className="qt-btn"
            title={filtered ? 'Count the matching rows exactly (SELECT COUNT(*) … WHERE)' : 'Count the rows exactly (SELECT COUNT(*))'}
            onClick={() => void store.count(tab)}
          >
            Count
          </button>
        )}
        <button
          type="button"
          className="qt-btn"
          title="Export these rows or all rows to a file"
          disabled={!data}
          onClick={() => requestExport(dataExportRequest(tab, state))}
        >
          Export…
        </button>
        <button
          type="button"
          className="qt-btn"
          title={filtered ? "Open this page's SELECT in a new query tab (without the filter)" : "Open this page's SELECT in a new query tab"}
          onClick={openInQuery}
        >
          Open in query
        </button>
      </div>
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
      {info && !info.editable ? (
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
          <button type="button" className="qt-btn" onClick={() => void store.load(tab)}>
            Reload
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
              </span>
            );
          })}
          <button type="button" className="qt-btn" title="Apply the staged changes without the conflict check" onClick={() => void useDataEdits.getState().startApply(tab, true)}>
            Overwrite
          </button>
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
          {truncated ? 'Some values are truncated; open in query to see full values.' : null}
          {truncated && capped ? ' ' : null}
          {capped ? 'This page stopped early to stay small; Next continues after its last row.' : null}
        </p>
      ) : null}
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
          result.rowCount === 0 && state.offset === 0 && !editable ? (
            <p className="dv-empty muted">
              {filtered ? 'No rows match the filter.' : `This ${tab.objectRef.kind === 'Table' ? 'table' : 'view'} has no rows.`}
            </p>
          ) : (
            <ResultGrid
              result={result}
              onHeaderClicked={onHeaderClicked}
              headerMarks={marks}
              firstRowNumber={state.offset + 1}
              engine={engine}
              tableRef={tab.objectRef}
              cellsCapped={truncated}
              editing={editing}
              extraCellMenu={(ctx) => {
                const column = result.columns[ctx.col];
                const filters = column
                  ? dataCellMenu(tab)({ ...ctx, value: ctx.value as Value, column }).map((entry) =>
                      'run' in entry ? { ...entry, run: () => guarded(() => entry.run()) } : entry,
                    )
                  : [];
                if (!editing || !data) return filters;
                const editEntries: MenuEntry[] = [];
                if (editing.isEditable(ctx.row, ctx.col)) {
                  editEntries.push({ label: 'Set to NULL', hint: 'Ctrl+0', run: () => setCell(ctx.row, ctx.col, { kind: 'null' }) });
                }
                editEntries.push({
                  label: ctx.row < data.rows.length ? 'Delete row' : 'Remove new row',
                  hint: 'Ctrl+Delete',
                  run: () => handleEditKey({ key: 'Delete', ctrlKey: true, metaKey: false, shiftKey: false, altKey: false }, { row: ctx.row, col: ctx.col }),
                });
                return filters.length > 0 ? [...editEntries, { separator: true }, ...filters] : editEntries;
              }}
            />
          )
        ) : (
          <p className="dv-loading muted">Loading…</p>
        )}
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
