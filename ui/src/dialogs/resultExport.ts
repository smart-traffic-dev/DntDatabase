// What the result export dialog exports, and the menu hooks that open it (`openExportDialog`)
// or toggle the data view's filter bar (`toggleFilterBar`).
import type { ColumnMeta, DataFilter, ExportFormat, ExportOptions, ObjectRef, SortSpec, Value } from '../bindings';
import { emptyRun, planAlive, useQueryRun, type ResultSet, type TabRun } from '../store/queryRun';
import { dialectOfConnection, useQueryDocs } from '../store/queryTabs';
import { useDialogs } from '../store/dialogs';
import { newFilterRow, useTableData, type TableDataState } from '../store/tableData';
import { useTabs, type ObjectTabRef } from '../store/tabs';

/** How "all rows" are read again on the backend (none: only the loaded rows can be exported). */
export type ExportAll =
  | { kind: 'table'; connectionId: string; obj: ObjectRef; sort: SortSpec[]; filter: DataFilter | null }
  | { kind: 'query'; tabId: string; connectionId: string; token: string };

export interface ExportRequest {
  /** Dialog title, e.g. "Export orders". */
  title: string;
  engine: 'mysql' | 'postgres';
  /** File name without extension. */
  baseName: string;
  /** Default table of the SQL INSERTs. */
  tableName: string;
  columns: ColumnMeta[];
  /** The loaded rows (read when the export starts). */
  rows: () => Value[][];
  loadedCount: number;
  /** A caveat of the loaded rows (cut values). */
  loadedNote: string | null;
  /** Binary values of the loaded rows held only as a preview (exported as NULL / empty). */
  partialBinaries: number;
  all: ExportAll | null;
  /** Why "all rows" is not offered. */
  allUnavailable: string | null;
}

export const FORMATS: { format: ExportFormat; label: string; ext: string; filter: string }[] = [
  { format: 'csv', label: 'CSV', ext: 'csv', filter: 'CSV' },
  { format: 'json', label: 'JSON (array of objects)', ext: 'json', filter: 'JSON' },
  { format: 'xlsx', label: 'Excel (.xlsx)', ext: 'xlsx', filter: 'Excel workbook' },
  { format: 'sql', label: 'SQL INSERT statements', ext: 'sql', filter: 'SQL' },
  { format: 'markdown', label: 'Markdown table', ext: 'md', filter: 'Markdown' },
];

export const defaultOptions = (tableName: string): ExportOptions => ({
  format: 'csv',
  delimiter: ',',
  header: true,
  quote_all: false,
  bom: false,
  table_name: tableName,
  formula_guard: false,
});

/** Whether `v` is a binary value the UI holds only a preview of (its first 64 bytes). */
const isPartial = (v: Value): boolean => v.t === 'Bytes' && v.v.len > v.v.preview.length;

/** The partly loaded binary values of `rows` (array items included). */
export function countPartialBinaries(rows: Value[][]): number {
  const count = (v: Value): number => (v.t === 'Array' ? v.v.reduce((n, item) => n + count(item), 0) : isPartial(v) ? 1 : 0);
  let n = 0;
  for (const row of rows) for (const v of row) n += count(v);
  return n;
}

/** A file name from `name`: characters Windows refuses become `_`. */
export function safeFileName(name: string): string {
  const cleaned = name.replace(/[<>:"/\\|?*]/g, '_').trim();
  // Control characters too.
  return [...cleaned].map((c) => (c.charCodeAt(0) < 32 ? '_' : c)).join('') || 'export';
}

/** Rows of a chunked result, flattened. */
export function resultRows(result: ResultSet): Value[][] {
  return result.chunks.flat();
}

const plural = (n: number, one: string) => `${n.toLocaleString('en-US')} ${one}${n === 1 ? '' : 's'}`;

/** The export of the data view of `tab` (its current page as the loaded rows; all rows read again). */
export function dataExportRequest(tab: ObjectTabRef, state: TableDataState): ExportRequest | null {
  const data = state.page?.status === 'ok' ? state.page.data : null;
  if (!data) return null;
  return {
    title: `Export ${tab.objectRef.name}`,
    engine: dialectOfConnection(tab.connectionId),
    baseName: safeFileName(tab.objectRef.name),
    tableName: tab.objectRef.name,
    columns: data.columns,
    rows: () => data.rows,
    loadedCount: data.rows.length,
    loadedNote: data.truncated_cells > 0 ? `${plural(data.truncated_cells, 'value')} of this page are cut; "All rows" reads them whole.` : null,
    partialBinaries: countPartialBinaries(data.rows),
    all: { kind: 'table', connectionId: tab.connectionId, obj: tab.objectRef, sort: state.sort, filter: state.filter },
    allUnavailable: null,
  };
}

/** The reason a query result can only export its loaded rows; null when it can be read again. */
export function queryAllUnavailable(run: TabRun, result: ResultSet, alive: boolean): string | null {
  const stmt = run.plan?.stmts.find((s) => s.index === result.index);
  if (run.mode === 'explain') return 'An Explain result exports the rows shown.';
  if (stmt?.kind !== 'read') return 'Only the result of a read statement can be read again.';
  if (stmt.verdict !== 'allow') return 'This statement needs confirmation, so it is not read again for an export.';
  if (!alive) return 'This result is no longer linked to its statement; run it again to export all rows.';
  return null;
}

/** The export of result `at` of the query tab `tabId`. */
export function queryExportRequest(tabId: string, at: number): ExportRequest | null {
  const run = useQueryRun.getState().runs.get(tabId) ?? emptyRun;
  const result = run.results[at];
  if (!result || result.columns.length === 0) return null;
  const doc = useQueryDocs.getState().docs.get(tabId);
  const connectionId = run.origin?.connectionId ?? doc?.connectionId ?? null;
  if (!connectionId) return null;
  const alive = planAlive(run, doc);
  const unavailable = result.token ? null : queryAllUnavailable(run, result, alive);
  return {
    title: `Export result ${at + 1}`,
    engine: dialectOfConnection(connectionId),
    baseName: safeFileName(`${doc?.title ?? 'query'}-result-${at + 1}`),
    tableName: result.columns[0]?.origin_table?.name ?? 'exported_rows',
    columns: result.columns,
    rows: () => resultRows(result),
    loadedCount: result.rowCount,
    loadedNote: result.truncated ? `Only the first ${plural(result.rowCount, 'row')} were fetched.` : null,
    partialBinaries: countPartialBinaries(resultRows(result)),
    all: unavailable || !result.token ? null : { kind: 'query', tabId, connectionId, token: result.token },
    allUnavailable: unavailable,
  };
}

/** The result tab each query tab shows (set by `ResultPanel`), for `openExportDialog`. */
export const shownResult = new Map<string, number>();

export function requestExport(request: ExportRequest | null) {
  if (request) useDialogs.getState().requestDialog({ kind: 'exportResult', request });
}

/**
 * Menu hook: opens the export dialog for what the active tab shows (the open result of a
 * query tab, the data view of an object tab). Does nothing when there is nothing to export.
 */
export function openExportDialog(): void {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  if (!tab || tab.kind === 'design') return;
  if (tab.kind === 'object') {
    const state = useTableData.getState().byTab[tab.id];
    if (state) requestExport(dataExportRequest(tab, state));
    return;
  }
  requestExport(queryExportRequest(tab.id, shownResult.get(tab.id) ?? 0));
}

/** Opens / closes the filter bar of `tab`'s data view; an opened empty bar gets a first row. */
export function toggleTabFilterBar(tab: ObjectTabRef, open?: boolean): void {
  const store = useTableData.getState();
  const state = store.byTab[tab.id];
  if (!state) return;
  const opening = open ?? !state.filterBarOpen;
  if (opening && state.draft.rows.length === 0) {
    const first = state.columns[0];
    if (first !== undefined) store.setDraft(tab, { ...state.draft, rows: [newFilterRow(first)] });
  }
  store.toggleFilterBar(tab, opening);
}

/** Menu hook: toggles the filter bar of the active object tab's data view. */
export function toggleFilterBar(): void {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  if (tab?.kind === 'object') toggleTabFilterBar(tab);
}
