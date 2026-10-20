import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { needsReconnect } from '../api';
import type { AppError, ColumnMeta, FilterOp, SortSpec, TableDataPage, Value } from '../bindings';
import type { MenuEntry } from '../components/ActionMenu';
import { selectTopSql } from '../components/newQuery';
import { dataExportRequest, requestExport, toggleTabFilterBar } from '../dialogs/resultExport';
import { useAppSettings } from '../store/appSettings';
import { dialectOfConnection, useQueryDocs } from '../store/queryTabs';
import { CHUNK, type ResultSet } from '../store/queryRun';
import type { ObjectTabRef as Tab } from '../store/tabs';
import { draftToFilter, newFilterRow, useTableData, type FilterRow } from '../store/tableData';
import { FilterBar, type FilterBarHandle } from './FilterBar';
import { ResultGrid } from './query/ResultGrid';
import './query/query.css';
import './dataView.css';

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
  const kept = state.draft.mode === 'or' ? [] : state.draft.rows.filter((r) => r.column);
  const draft = { mode: 'and' as const, rows: [...kept, row] };
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

  const page = state?.page;
  const data = page?.status === 'ok' ? page.data : null;
  const sort = useMemo(() => state?.sort ?? [], [state?.sort]);
  const result = useMemo(() => (data ? pageToResult(data) : null), [data]);
  const marks = useMemo(() => (data ? sortMarks(data, sort) : {}), [data, sort]);

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
    store.toggleSort(tab, column.name, shift.current);
  };

  const openBar = () => {
    toggleTabFilterBar(tab, true);
    setFocusTick((n) => n + 1);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'F3') {
      e.preventDefault();
      openBar();
    }
  };

  const apply = () => {
    const { filter, error } = draftToFilter(state.draft);
    setFilterError(error);
    if (!error) store.applyFilter(tab, filter);
  };

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
          <button type="button" className="qt-btn" onClick={() => void store.load(tab)}>
            Refresh
          </button>
        )}
        <span className="qt-sep" />
        <button type="button" className="qt-btn" aria-label="Previous page" onClick={() => store.prev(tab)} disabled={loading || state.offset === 0}>
          ‹ Prev
        </button>
        <button type="button" className="qt-btn" aria-label="Next page" onClick={() => store.next(tab)} disabled={loading || !data?.has_more}>
          Next ›
        </button>
        <label className="dv-size">
          <span>Page size</span>
          <select className="qt-select" value={state.pageSize} onChange={(e) => store.setPageSize(tab, Number(e.target.value))}>
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
        {filtered ? <span className="dv-filtered">Filtered ({state.filter!.conditions.length})</span> : null}
        {sort.length > 0 ? (
          <span className="dv-sort">
            Sorted by {sortLabel(sort)}
            <button type="button" className="dv-link" onClick={() => store.clearSort(tab)}>
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
      {state.filterBarOpen ? (
        <FilterBar
          handle={barRef}
          columns={state.columns}
          draft={state.draft}
          active={filtered}
          error={filterError}
          onChange={(draft) => store.setDraft(tab, draft)}
          onApply={apply}
          onClear={() => {
            setFilterError(null);
            store.clearFilter(tab);
          }}
        />
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
        {page?.status === 'error' ? (
          <DataError error={page.error} onRetry={() => void store.load(tab)} onReconnect={() => void store.reconnect(tab)} />
        ) : result ? (
          result.rowCount === 0 && state.offset === 0 ? (
            <p className="dv-empty muted">
              {filtered ? 'No rows match the filter.' : `This ${tab.objectRef.kind === 'Table' ? 'table' : 'view'} has no rows.`}
            </p>
          ) : (
            <ResultGrid result={result} onHeaderClicked={onHeaderClicked} headerMarks={marks} firstRowNumber={state.offset + 1} />
          )
        ) : (
          <p className="dv-loading muted">Loading…</p>
        )}
      </div>
    </div>
  );
}
