// Find-in-results and go-to-row, over the rows the grid holds in memory.
import type { Value } from '../../bindings';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import { cellView } from './format';

export interface Match {
  row: number;
  col: number;
}

/** Matches beyond this are not collected (the count reads "N+"). */
export const MAX_MATCHES = 10_000;

/** The text that Find searches in a cell: what the grid shows (NULL included). */
export function searchText(v: Value): string {
  return v.t === 'Json' ? v.v : cellView(v).text;
}

/**
 * Cells of the loaded rows whose text contains `query` (case-insensitive), in row order. An empty
 * query matches nothing.
 */
export function findMatches(result: Pick<ResultSet, 'chunks' | 'rowCount' | 'columns'>, query: string): { matches: Match[]; capped: boolean } {
  const needle = query.toLowerCase();
  const matches: Match[] = [];
  if (!needle) return { matches, capped: false };
  const cols = result.columns.length;
  for (let r = 0; r < result.rowCount; r++) {
    const row = result.chunks[Math.floor(r / CHUNK)]?.[r % CHUNK];
    if (!row) continue;
    for (let c = 0; c < cols; c++) {
      const v = row[c];
      if (!v) continue;
      if (searchText(v).toLowerCase().includes(needle)) {
        if (matches.length >= MAX_MATCHES) return { matches, capped: true };
        matches.push({ row: r, col: c });
      }
    }
  }
  return { matches, capped: false };
}

/** The index in `size` items after (`dir` 1) or before (-1) `at`, wrapping; `at` null starts at an end. */
export function stepIndex(at: number | null, dir: 1 | -1, size: number): number {
  if (size === 0) return -1;
  if (at === null) return dir === 1 ? 0 : size - 1;
  return (at + dir + size) % size;
}

/**
 * The first match at or after the cell (`row`, `col`) in reading order, so typing refines from the
 * current position; 0 when there is none after it.
 */
export function firstMatchFrom(matches: Match[], row: number, col: number): number {
  const i = matches.findIndex((m) => m.row > row || (m.row === row && m.col >= col));
  return i < 0 ? 0 : i;
}

export type GoTo = { ok: true; row: number } | { ok: false; message: string };

/**
 * The 0-based row for a 1-based row number typed by the user. `first` is the number of the first
 * row (the Data tab shows rows from its page offset).
 */
export function parseGoToRow(input: string, rowCount: number, first = 1): GoTo {
  const text = input.trim().replaceAll(',', '');
  if (!/^\d+$/.test(text)) return { ok: false, message: 'Enter a row number' };
  const n = Number(text);
  const last = first + rowCount - 1;
  if (rowCount === 0 || n < first || n > last) {
    const range = `${first.toLocaleString('en-US')}–${last.toLocaleString('en-US')}`;
    return { ok: false, message: rowCount === 0 ? 'No rows' : `Out of range: rows ${range}` };
  }
  return { ok: true, row: n - first };
}
