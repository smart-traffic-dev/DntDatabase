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

/**
 * The text a cell is edited as in place: the value itself, as stored (JSON as written, so a
 * 20-digit number or `1.10` stays exact). Null when the grid's text is not the value — binary
 * data, arrays, text or JSON longer than `MAX_CELL_CHARS` — such cells go to the value editor.
 */
export function editText(v: Value): string | null {
  switch (v.t) {
    case 'Bytes':
    case 'Array':
      return null;
    case 'Text':
    case 'Json':
      return v.v.length > MAX_CELL_CHARS ? null : v.v;
    default:
      return cellView(v).text;
  }
}

export const MIN_COL_WIDTH = 60;
export const MAX_COL_WIDTH = 400;
/** Rows sampled when fitting a column to its content. */
export const FIT_SAMPLE_ROWS = 200;
/** Glide's cell padding (8 px each side) plus a little slack. */
export const CELL_PAD = 20;
/** Fallback advance of the monospace body (13px) and header (12px bold) fonts, without a canvas. */
export const BODY_CHAR_PX = 7.9;
export const HEADER_CHAR_PX = 7.5;
const TYPE_CHAR_PX = 6.8;

/** The fonts the grid draws with (CSS `font` shorthand, family included). */
export interface GridFont {
  body: string;
  header: string;
  marker: string;
}

/** Text width in px for a CSS font. */
export interface Measurer {
  width(text: string, font: string): number;
}

let canvasCtx: CanvasRenderingContext2D | null | undefined;

/** Measures with a canvas (the real font); estimates from the character count where there is none. */
export const canvasMeasurer: Measurer = {
  width(text, font) {
    if (canvasCtx === undefined) {
      try {
        canvasCtx = typeof document === 'undefined' ? null : document.createElement('canvas').getContext('2d');
      } catch {
        canvasCtx = null;
      }
    }
    if (canvasCtx) {
      canvasCtx.font = font;
      const w = canvasCtx.measureText(text).width;
      if (Number.isFinite(w) && (w > 0 || text.length === 0)) return w;
    }
    const size = /(\d+(?:\.\d+)?)px/.exec(font);
    return text.length * (size ? Number(size[1]) * 0.6 : BODY_CHAR_PX);
  },
};

/** The advance of one body character of `font` (the grid font is monospace). */
export function bodyCharPx(font: string, m: Measurer = canvasMeasurer): number {
  const w = m.width('0', font);
  return Number.isFinite(w) && w > 0 ? w : BODY_CHAR_PX;
}

const DEFAULT_FONT: GridFont = {
  body: '13px monospace',
  header: '600 12px monospace',
  marker: '11px monospace',
};

/** Longest value text measured per column (more than `MAX_COL_WIDTH` can show anyway). */
const MEASURE_CHARS = 120;

/**
 * Initial width of a column: wide enough for its header (name, type) and sampled values (measured
 * with the grid's own fonts), plus the cell padding, capped at `MAX_COL_WIDTH`.
 */
export function fitColumnWidth(
  name: string,
  dbType: string,
  sampleTexts: string[],
  font: GridFont = DEFAULT_FONT,
  m: Measurer = canvasMeasurer,
): number {
  let px = Math.max(m.width(name, font.header), m.width(dbType, font.marker));
  if (!Number.isFinite(px)) px = Math.max(name.length * HEADER_CHAR_PX, dbType.length * TYPE_CHAR_PX);
  // Only the longest texts can set the width (equal-advance font); others are skipped for speed.
  let longest = 0;
  for (const t of sampleTexts) longest = Math.max(longest, t.length);
  for (const t of sampleTexts) {
    if (t.length * 2 < longest) continue;
    px = Math.max(px, m.width(t.length > MEASURE_CHARS ? t.slice(0, MEASURE_CHARS) : t, font.body));
  }
  return Math.round(Math.min(MAX_COL_WIDTH, Math.max(MIN_COL_WIDTH, Math.ceil(px) + CELL_PAD)));
}

/** How many body characters fit in a column of `width` px (`charPx`: the advance of one). */
export const charsForWidth = (width: number, charPx: number = BODY_CHAR_PX) =>
  Math.max(1, Math.floor((width - CELL_PAD) / charPx + 1e-6));

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
