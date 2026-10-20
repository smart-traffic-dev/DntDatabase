// Display rules of the result grid (spec §6.2): how a `Value` is shown, copied, and which
// statement is "current" for the editor highlight.
import type { Value } from '../../bindings';

export type CellView = { text: string; kind: 'null' | 'number' | 'text' | 'bytes' | 'json' | 'bool' };

/** Above this many rows the grid warns that the result is held in memory. */
export const MEMORY_WARNING_ROWS = 100_000;

const hex = (bytes: number[]) => bytes.map((b) => b.toString(16).padStart(2, '0')).join('');

function oneLineJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw));
  } catch {
    return raw.replace(/\s+/g, ' ').trim();
  }
}

export function cellView(v: Value): CellView {
  switch (v.t) {
    case 'Null':
      return { text: 'NULL', kind: 'null' };
    case 'Int':
    case 'UInt':
    case 'Float':
      return { text: String(v.v), kind: 'number' };
    case 'Decimal':
      return { text: v.v, kind: 'number' };
    case 'Bool':
      return { text: v.v ? 'true' : 'false', kind: 'bool' };
    case 'Bytes': {
      const more = v.v.len > v.v.preview.length ? '…' : '';
      return { text: `0x${hex(v.v.preview)}${more} (${v.v.len} bytes)`, kind: 'bytes' };
    }
    case 'Json':
      return { text: oneLineJson(v.v), kind: 'json' };
    case 'Array':
      return { text: `{${v.v.map((x) => cellView(x).text).join(',')}}`, kind: 'text' };
    case 'Other':
      return { text: v.v.text, kind: 'text' };
    case 'Text':
    case 'Uuid':
    case 'Date':
    case 'Time':
    case 'DateTime':
    case 'TimestampTz':
      return { text: v.v, kind: 'text' };
  }
}

/** The text of a cell as copied: tabs and line breaks would break the TSV, so they become spaces. */
export const copyText = (v: Value): string => cellView(v).text.replace(/\r\n|[\t\r\n]/g, (m) => (m === '\r\n' ? '  ' : ' '));

/** Rows as tab-separated text, columns `cols` only. `NULL` is copied as `NULL`. */
export function toTsv(rows: Value[][], cols: number[]): string {
  return rows.map((row) => cols.map((c) => (row[c] ? copyText(row[c]) : '')).join('\t')).join('\n');
}

/**
 * The statement a cursor at `offset` belongs to: the one whose range contains it (both ends
 * inclusive), else the nearest one ending before it, else the first. Same rule as the
 * backend's `Target::Cursor`.
 */
export function currentStatement(ranges: [number, number][], offset: number): [number, number] | null {
  const inside = ranges.find(([a, b]) => a <= offset && offset <= b);
  if (inside) return inside;
  for (let i = ranges.length - 1; i >= 0; i--) if (ranges[i]![1] <= offset) return ranges[i]!;
  return ranges[0] ?? null;
}
