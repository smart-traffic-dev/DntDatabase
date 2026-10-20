// Display rules of the result grid (spec §6.2): how a `Value` is shown, copied, and which
// statement is "current" for the editor highlight.
import type { Value } from '../../bindings';

export type CellView = { text: string; kind: 'null' | 'number' | 'text' | 'bytes' | 'json' | 'bool' };

/** Above this many rows the grid warns that the result is held in memory. */
export const MEMORY_WARNING_ROWS = 100_000;

const hex = (bytes: number[]) => bytes.map((b) => b.toString(16).padStart(2, '0')).join('');

/** Longest text a cell shows; JSON beyond this is shown cut and unformatted (never parsed). */
export const MAX_CELL_CHARS = 10_000;
/** The most cells a copy may cover. */
export const MAX_COPY_CELLS = 100_000;

/** `text` cut to `max` UTF-16 units (ellipsis included) without splitting a surrogate pair. */
export function truncateText(text: string, max: number): string {
  if (text.length <= max) return text;
  let end = max;
  const c = text.charCodeAt(end - 1);
  if (c >= 0xd800 && c <= 0xdbff) end--;
  return `${text.slice(0, end)}…`;
}

const collapse = (raw: string) => raw.replace(/\s+/g, ' ').trim();

function oneLineJson(raw: string): string {
  if (raw.length > MAX_CELL_CHARS) return truncateText(collapse(raw.slice(0, MAX_CELL_CHARS + 1)), MAX_CELL_CHARS);
  try {
    return JSON.stringify(JSON.parse(raw));
  } catch {
    return collapse(raw);
  }
}

// Formatting JSON parses it: do it once per value (the grid asks on every paint).
const jsonViews = new WeakMap<object, CellView>();

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
    case 'Json': {
      let view = jsonViews.get(v);
      if (!view) jsonViews.set(v, (view = { text: oneLineJson(v.v), kind: 'json' }));
      return view;
    }
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

export const MIN_COL_WIDTH = 60;
export const MAX_COL_WIDTH = 400;
/** Rows sampled when fitting a column to its content. */
export const FIT_SAMPLE_ROWS = 100;
const CELL_PAD = 20;
/** Approximate advance of the monospace body (13px) and header (12px bold) fonts. */
export const BODY_CHAR_PX = 7.9;
export const HEADER_CHAR_PX = 7.5;

/** Initial width of a column: wide enough for its header (name, type) and sampled values, capped. */
export function fitColumnWidth(name: string, dbType: string, sampleTexts: string[]): number {
  let px = Math.max(name.length * HEADER_CHAR_PX, dbType.length * 6.8);
  for (const t of sampleTexts) px = Math.max(px, Math.min(t.length, 200) * BODY_CHAR_PX);
  return Math.round(Math.min(MAX_COL_WIDTH, Math.max(MIN_COL_WIDTH, px + CELL_PAD)));
}

/** How many body characters fit in a column of `width` px. */
export const charsForWidth = (width: number) => Math.max(1, Math.floor((width - CELL_PAD) / BODY_CHAR_PX));

/**
 * `view.text` cut to `max` characters with an ellipsis. Bytes keep their `(N bytes)` suffix
 * (the hex preview is what gets cut); everything else keeps its start (the meaningful part).
 */
export function fitCellText(view: CellView, max: number): string {
  const { text } = view;
  if (text.length <= max) return text;
  if (view.kind === 'bytes') {
    const i = text.lastIndexOf(' (');
    if (i > 0) {
      const suffix = text.slice(i);
      if (suffix.length < max) return `${text.slice(0, Math.max(1, max - suffix.length - 1))}…${suffix}`;
      return suffix.trim();
    }
  }
  return truncateText(text, Math.max(0, max - 1));
}

// Programming ligatures of the mono font (`>=` drawn as `≥`) mislead in SQL; the canvas has no
// font-feature control, so a zero-width non-joiner splits the pair.
const LIGATURE = /([<>=!\-+*/|&:.#~])(?=[<>=!\-+*/|&:.#~>])/g;
export const noLigatures = (text: string) => text.replace(LIGATURE, '$1‌');

/** The text of a cell as copied (computed only when copying): tabs and line breaks become spaces. */
export function copyText(v: Value): string {
  const text = v.t === 'Json' ? collapse(v.v) : cellView(v).text;
  return text.replace(/\r\n|[\t\r\n]/g, (m) => (m === '\r\n' ? '  ' : ' '));
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
