// "Copy as" of the data view: the selected cells or rows as JSON or a Markdown table
// (pure), and as INSERT / UPDATE statements (built by the backend, which knows the column types).
import { api } from '../../api';
import type { EditableInfo, ObjectRef, TableDataPage, Value } from '../../bindings';
import { rowRef } from '../../store/dataEdits';
import { MAX_COPY_CELLS, cellView } from '../query/format';
import { plainText } from '../query/gridCopy';
import { isCutCell } from './cutCells';
import { blockValues, selectionBlock, type Block, type SelectionLike } from '../query/gridSelection';

export type CopyFormat = 'json' | 'markdown';

/** Why a row with a cut cell is not copied as SQL (the backend words it the same). */
export const TRUNCATED_COPY = 'Some cells are truncated; open them first or use Export.';

/** The cell to copy when nothing is selected. */
export interface CopyCell {
  cell?: { row: number; col: number };
}

/** The rows and columns to copy: the selection, else the active cell; new (unsaved) rows are left out. */
function blockOf(sel: SelectionLike | undefined, page: TableDataPage, meta: CopyCell): Block {
  const picked = selectionBlock(sel, page.rows.length, page.columns.length, MAX_COPY_CELLS);
  if (!picked.ok) throw new Error(`Selection too large to copy (${picked.cells} cells); narrow the selection`);
  const block = picked.block;
  if (block.rows.length === 0 && meta.cell) return { rows: [meta.cell.row], cols: [meta.cell.col] };
  return block;
}

const isSafe = (n: number) => Number.isSafeInteger(n);

/** `v` as a JSON value: integers beyond 2^53 stay exact as strings, JSON values are embedded. */
function jsonValue(v: Value): unknown {
  switch (v.t) {
    case 'Null':
      return null;
    case 'Bool':
      return v.v;
    case 'Int':
    case 'UInt': {
      const n = typeof v.v === 'string' ? Number(v.v) : v.v;
      return isSafe(n) && String(n) === String(v.v) ? n : String(v.v);
    }
    case 'Float':
      return Number.isFinite(v.v) ? v.v : String(v.v);
    case 'Json':
      // A number with 16 or more digits may not survive parsing: keep such text as it is.
      if (/\d{16,}/.test(v.v)) return v.v;
      try {
        return JSON.parse(v.v) as unknown;
      } catch {
        return v.v;
      }
    case 'Bytes':
      return `0x${v.v.preview.map((b) => b.toString(16).padStart(2, '0')).join('')}`;
    case 'Array':
      return v.v.map(jsonValue);
    default:
      return plainText(v);
  }
}

const markdownCell = (v: Value) =>
  (v.t === 'Null' ? '' : cellView(v).text).replaceAll('|', '\\|').replace(/\r\n|\r|\n/g, '<br>');

/** The selection as text in `format`, with the column names as header (JSON: an array of objects). */
export function copyAs(format: CopyFormat, sel: SelectionLike | undefined, page: TableDataPage, meta: CopyCell = {}): string {
  const block = blockOf(sel, page, meta);
  const rows = blockValues({ chunks: [page.rows] }, block);
  // Never copy a value the page cut short as if it were complete.
  if (block.rows.some((r) => block.cols.some((c) => isCutCell(page, r, c)))) throw new Error(TRUNCATED_COPY);
  const header = block.cols.map((c) => page.columns[c]?.name ?? '');
  switch (format) {
    case 'json':
      return JSON.stringify(rows.map((row) => Object.fromEntries(row.map((v, i) => [header[i], jsonValue(v)]))), null, 2);
    case 'markdown': {
      const line = (cells: string[]) => `| ${cells.join(' | ')} |`;
      return [line(header.map((h) => h.replaceAll('|', '\\|'))), line(header.map(() => '---')), ...rows.map((r) => line(r.map(markdownCell)))].join('\n');
    }
  }
}

/** What `copySql` needs to know about the table. */
export interface CopySqlMeta extends CopyCell {
  connectionId: string;
  object: ObjectRef;
  info: EditableInfo;
}

/**
 * The selected rows as `INSERT` / `UPDATE` statements, built by the backend (nothing is run). A row
 * with a cut cell is refused (never copied truncated); the table's key identifies the rows.
 */
export async function copySql(format: 'insert' | 'update', sel: SelectionLike | undefined, page: TableDataPage, meta: CopySqlMeta): Promise<string> {
  const rows = [...new Set(blockOf(sel, page, meta).rows)].filter((r) => r < page.rows.length).sort((a, b) => a - b);
  if (rows.length === 0) throw new Error('Select the rows to copy.');
  if (rows.some((r) => page.columns.some((_, c) => isCutCell(page, r, c)))) throw new Error(TRUNCATED_COPY);
  const refs = rows.map((r) => rowRef(page, meta.info, r));
  if (refs.some((r) => r === null)) throw new Error("Copying as SQL needs the table's key columns.");
  return api.dataCopySql(meta.connectionId, meta.object, refs as NonNullable<(typeof refs)[number]>[], format);
}
