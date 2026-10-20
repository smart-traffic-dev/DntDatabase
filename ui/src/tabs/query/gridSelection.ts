// What the user has selected in the grid, as plain row / column lists (pure, no Glide runtime).
import type { Value } from '../../bindings';
import { CHUNK, type ResultSet } from '../../store/queryRun';

/** The parts of Glide's `CompactSelection` used here. */
export interface IndexSet {
  readonly length: number;
  toArray(): number[];
  first(): number | undefined;
}

/** The parts of Glide's `GridSelection` used here. */
export interface SelectionLike {
  readonly current?: {
    readonly cell: readonly [number, number];
    readonly range: { readonly x: number; readonly y: number; readonly width: number; readonly height: number };
  };
  readonly columns: IndexSet;
  readonly rows: IndexSet;
}

/** A rectangle of cells given by its (sorted) rows and columns. */
export interface Block {
  rows: number[];
  cols: number[];
}

export type BlockResult = { ok: true; block: Block } | { ok: false; cells: number };

const range = (from: number, count: number) => Array.from({ length: count }, (_, i) => from + i);

/** Number of cells `sel` covers. */
export function selectionSize(sel: SelectionLike | undefined, rowCount: number, colCount: number): number {
  if (!sel) return 0;
  if (sel.rows.length > 0) return sel.rows.length * colCount;
  if (sel.columns.length > 0) return sel.columns.length * rowCount;
  return sel.current ? sel.current.range.width * sel.current.range.height : 0;
}

/**
 * The rows and columns `sel` covers: selected rows (all columns), else selected columns (all rows),
 * else the range. Over `maxCells` nothing is built and the size is reported.
 */
export function selectionBlock(sel: SelectionLike | undefined, rowCount: number, colCount: number, maxCells: number): BlockResult {
  const cells = selectionSize(sel, rowCount, colCount);
  if (cells > maxCells) return { ok: false, cells };
  if (!sel || cells === 0) return { ok: true, block: { rows: [], cols: [] } };
  const allCols = () => range(0, colCount);
  if (sel.rows.length > 0) return { ok: true, block: { rows: sel.rows.toArray(), cols: allCols() } };
  if (sel.columns.length > 0) return { ok: true, block: { rows: range(0, rowCount), cols: sel.columns.toArray() } };
  const r = sel.current!.range;
  return {
    ok: true,
    block: { rows: range(r.y, Math.min(r.height, Math.max(0, rowCount - r.y))), cols: range(r.x, Math.min(r.width, Math.max(0, colCount - r.x))) },
  };
}

/** The row the user is on: the first selected row, else the active cell's row. */
export function currentRow(sel: SelectionLike | undefined): number | null {
  if (!sel) return null;
  return sel.rows.first() ?? sel.current?.cell[1] ?? null;
}

/** The column the user is on: the active cell's, else the first selected column. */
export function currentCol(sel: SelectionLike | undefined): number | null {
  if (!sel) return null;
  return sel.current?.cell[0] ?? sel.columns.first() ?? null;
}

const NULL: Value = { t: 'Null' };

/** The value of a loaded cell (NULL when the row is not loaded). */
export const cellValue = (result: Pick<ResultSet, 'chunks'>, row: number, col: number): Value =>
  result.chunks[Math.floor(row / CHUNK)]?.[row % CHUNK]?.[col] ?? NULL;

/** The values of `cols` for every row of `rows`. */
export const blockValues = (result: Pick<ResultSet, 'chunks'>, block: Block): Value[][] =>
  block.rows.map((r) => block.cols.map((c) => cellValue(result, r, c)));
