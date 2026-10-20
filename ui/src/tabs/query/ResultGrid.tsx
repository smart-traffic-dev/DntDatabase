import '@glideapps/glide-data-grid/dist/index.css';
import {
  CompactSelection,
  DataEditor,
  GridCellKind,
  type DataEditorRef,
  type DrawHeaderCallback,
  type EditableGridCell,
  type GridCell,
  type GridColumn,
  type GridSelection,
  type Item,
  type Rectangle,
  type Theme,
} from '@glideapps/glide-data-grid';
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { ObjectRef, Value } from '../../bindings';
import { ActionMenu, type MenuEntry } from '../../components/ActionMenu';
import { codeFontSizes, monoFontFamily } from '../../monaco/fonts';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import { FindBar, GoToBar } from './FindBar';
import {
  FIT_SAMPLE_ROWS,
  MAX_COPY_CELLS,
  MIN_COL_WIDTH,
  bodyCharPx,
  canvasMeasurer,
  cellView,
  charsForWidth,
  copyText,
  editText,
  fitCellText,
  fitColumnWidth,
  type GridFont,
  type Measurer,
} from './format';
import { activateGrid, registerGrid, type CopyMode, type GridCommands } from './gridCommands';
import {
  TABLE_PLACEHOLDER,
  qualifiedTable,
  singleSourceTable,
  toCsv,
  toInsertStatements,
  toTsv,
  type Engine,
} from './gridCopy';
import { findMatches, firstMatchFrom, parseGoToRow, stepIndex } from './gridFind';
import { blockValues, cellValue, currentCol, currentRow, selectionBlock, selectionSize, type SelectionLike } from './gridSelection';
import { selectionStats } from './selectionStats';
import { GridStats } from './GridStats';
import { RecordView } from './RecordView';
import './gridTools.css';

const ROW_HEIGHT = 28;
const HEADER_HEIGHT = 44;
/** Statistics are not computed for larger selections. */
const MAX_STATS_CELLS = 50_000;
/** Two row-number clicks this close (ms) on one row are a double click. */
const DOUBLE_CLICK_MS = 500;

type Resized = { cols: ResultSet['columns']; w: Record<number, number> };
const NO_OVERRIDES: Record<number, number> = {};

/** The widths the user set for this result (a new result has none). */
export function overridesFor(r: Resized, cols: ResultSet['columns']): Record<number, number> {
  return r.cols === cols ? r.w : NO_OVERRIDES;
}

/** `prev` with column `index` of the result `cols` set to `size` (clamped). */
export function withResize(prev: Resized, cols: ResultSet['columns'], index: number, size: number): Resized {
  const w = Math.max(MIN_COL_WIDTH, Math.min(size, 2000));
  return { cols, w: { ...overridesFor(prev, cols), [index]: w } };
}

/** At most `max` loaded rows: the first half in order, the rest evenly spread over the other rows. */
export function sampleRows(result: Pick<ResultSet, 'chunks' | 'rowCount'>, max = FIT_SAMPLE_ROWS): Value[][] {
  const at = (r: number) => result.chunks[Math.floor(r / CHUNK)]?.[r % CHUNK];
  const rows: Value[][] = [];
  const total = result.rowCount;
  const head = Math.min(total, Math.ceil(max / 2));
  for (let r = 0; r < head; r++) {
    const row = at(r);
    if (row) rows.push(row);
  }
  const rest = Math.min(total - head, max - head);
  for (let i = 0; i < rest; i++) {
    const row = at(head + Math.floor((i * (total - head)) / rest));
    if (row) rows.push(row);
  }
  return rows;
}

/** Initial column widths: header plus sampled loaded rows, measured with the grid's fonts. */
export function initialWidths(result: ResultSet, font?: GridFont, m: Measurer = canvasMeasurer): number[] {
  const rows = sampleRows(result);
  return result.columns.map((c, i) => {
    const samples: string[] = [];
    for (const row of rows) {
      const v = row[i];
      if (v) samples.push(cellView(v).text);
    }
    return fitColumnWidth(c.name, c.db_type, samples, font, m);
  });
}

export const tooLargeMessage = (cells: number) =>
  `Selection too large to copy (${cells} cells); narrow the selection`;

function cssVar(name: string, fallback: string): string {
  try {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;
  } catch {
    return fallback;
  }
}

/** Bumps when the app theme (the `data-theme` attribute or the OS scheme) changes. */
function useThemeVersion(): number {
  const [version, setVersion] = useState(0);
  useEffect(() => {
    const bump = () => setVersion((v) => v + 1);
    const observer = new MutationObserver(bump);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme', 'class', 'style'] });
    const media = typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)') : null;
    media?.addEventListener?.('change', bump);
    return () => {
      observer.disconnect();
      media?.removeEventListener?.('change', bump);
    };
  }, []);
  return version;
}

function buildTheme(): {
  theme: Partial<Theme>;
  nullStyle: Partial<Theme>;
  statusStyle: Record<'edited' | 'new' | 'deleted' | 'conflict', Partial<Theme>>;
  font: GridFont;
  charPx: number;
} {
  const muted = cssVar('--muted', '#5a6370');
  const size = codeFontSizes();
  const family = monoFontFamily();
  // The canvas fonts, as Glide draws them (`<baseFontStyle> <fontFamily>`).
  const font: GridFont = {
    body: `${size.body}px ${family}`,
    header: `600 ${size.header}px ${family}`,
    marker: `${size.marker}px ${family}`,
  };
  return {
    theme: {
      accentColor: cssVar('--accent', '#1f5fd1'),
      accentLight: cssVar('--sel', '#e3ebfa'),
      bgCell: cssVar('--panel', '#ffffff'),
      bgHeader: cssVar('--sidebar', '#fafafb'),
      bgHeaderHasFocus: cssVar('--sel', '#e3ebfa'),
      bgHeaderHovered: cssVar('--sel', '#e3ebfa'),
      textDark: cssVar('--text', '#1b1f24'),
      textMedium: muted,
      textHeader: cssVar('--text', '#1b1f24'),
      borderColor: cssVar('--border', '#dcdfe4'),
      // The same code font as the editor and every CSS mono surface (`--font-mono`).
      fontFamily: family,
      textLight: muted,
      markerFontStyle: `${size.marker}px`,
      baseFontStyle: `${size.body}px`,
      headerFontStyle: `600 ${size.header}px`,
    },
    nullStyle: { textDark: muted, baseFontStyle: `italic ${size.body}px` },
    // Staged changes: the same tokens as the `dnt-*` classes of dataEdit.css.
    statusStyle: {
      edited: { bgCell: cssVar('--warning-bg', '#fff4d6'), textDark: cssVar('--warning-fg', '#7a4b00') },
      new: { bgCell: cssVar('--sel', '#e3ebfa') },
      deleted: { bgCell: cssVar('--danger-fill', '#fdecea'), textDark: cssVar('--danger-fg', '#b42318') },
      conflict: { bgCell: cssVar('--danger-fill', '#fdecea'), textDark: cssVar('--danger-fg', '#b42318') },
    } satisfies Record<'edited' | 'new' | 'deleted' | 'conflict', Partial<Theme>>,
    font,
    charPx: bodyCharPx(font.body),
  };
}

/**
 * The cells of a copied selection, with their copy text (computed here, only when copying).
 * A selection over `MAX_COPY_CELLS` is refused (empty) and reported through `onTooLarge`.
 */
export function selectionCells(
  result: ResultSet,
  rect: Rectangle,
  onTooLarge: (cells: number) => void,
): GridCell[][] {
  const count = rect.width * rect.height;
  if (count > MAX_COPY_CELLS) {
    onTooLarge(count);
    return [];
  }
  const rows: GridCell[][] = [];
  for (let r = rect.y; r < rect.y + rect.height; r++) {
    const row: GridCell[] = [];
    for (let c = rect.x; c < rect.x + rect.width; c++) {
      const value = result.chunks[Math.floor(r / CHUNK)]?.[r % CHUNK]?.[c];
      const text = value ? copyText(value) : '';
      row.push({ kind: GridCellKind.Text, data: text, displayData: text, allowOverlay: false, copyData: text });
    }
    rows.push(row);
  }
  return rows;
}

/** Writes `text` to the clipboard; false when the system refused. */
export async function writeClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const area = document.createElement('textarea');
      area.value = text;
      area.style.position = 'fixed';
      area.style.opacity = '0';
      document.body.appendChild(area);
      area.select();
      const ok = document.execCommand('copy');
      area.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

/** What a copy produced: the text and what to tell the user, or why it was refused. */
export type CopyOutcome = { ok: true; text: string; message: string } | { ok: false; message: string };

const countLabel = (n: number, unit: string) => `${n.toLocaleString('en-US')} ${unit}${n === 1 ? '' : 's'}`;

/** Formats the selection for `mode` (pure; the clipboard is written by the caller). */
export function copyOutcome(
  mode: CopyMode,
  sel: GridSelection | undefined,
  result: ResultSet,
  opts: { engine: Engine; tableRef?: ObjectRef | null; cellsCapped: boolean; firstRowNumber: number; cellColumn?: number | null },
): CopyOutcome {
  const colCount = result.columns.length;
  if (mode === 'column') {
    const col = opts.cellColumn ?? currentCol(sel);
    const name = col === null || col === undefined ? undefined : result.columns[col]?.name;
    return name === undefined ? { ok: false, message: 'Select a column first' } : { ok: true, text: name, message: `Copied column name ${name}` };
  }
  const picked = selectionBlock(sel, result.rowCount, colCount, MAX_COPY_CELLS);
  if (!picked.ok) return { ok: false, message: tooLargeMessage(picked.cells) };
  const { block } = picked;
  if (block.rows.length === 0 || block.cols.length === 0) return { ok: false, message: 'Select cells first' };
  if (mode === 'insert') {
    // A row is inserted whole: every column of every selected row.
    const cols = Array.from({ length: colCount }, (_, i) => i);
    const table = opts.tableRef ?? singleSourceTable(result.columns);
    const made = toInsertStatements({
      table: table ? qualifiedTable(table, opts.engine) : TABLE_PLACEHOLDER,
      engine: opts.engine,
      columns: result.columns.map((c) => c.name),
      rows: blockValues(result, { rows: block.rows, cols }),
      capped: opts.cellsCapped,
      rowNumbers: block.rows.map((r) => r + opts.firstRowNumber),
    });
    return made.ok ? { ok: true, text: made.text, message: `Copied ${countLabel(block.rows.length, 'INSERT')}` } : { ok: false, message: made.message };
  }
  const values = blockValues(result, block);
  const names = block.cols.map((c) => result.columns[c]?.name ?? '');
  const cells = block.rows.length * block.cols.length;
  const what = cells === 1 ? '1 cell' : countLabel(block.rows.length, 'row');
  if (mode === 'csv') return { ok: true, text: toCsv(values, names), message: `Copied ${what} as CSV` };
  return {
    ok: true,
    text: toTsv(values, mode === 'tsv-headers' ? names : null),
    message: `Copied ${what}${mode === 'tsv-headers' ? ' with headers' : ''}`,
  };
}

interface Notice {
  text: string;
  warn: boolean;
}

/** In-place editing of the data view (absent: the grid is read-only). */
export interface GridEditing {
  /** Whether the cell takes typed edits (else it is read-only). */
  isEditable(row: number, col: number): boolean;
  /** How the cell differs from the server (painted), or null. */
  status(row: number, col: number): 'edited' | 'new' | 'deleted' | 'conflict' | null;
  /** A typed value was committed in a cell. */
  onEdit(row: number, col: number, text: string): void;
  /** Enter / double click on a cell (a read-only cell can open a viewer). */
  onActivate?(row: number, col: number): void;
  /** A key pressed on the grid, with the selected cell; true when it was handled. */
  onKey?(e: { key: string; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean; altKey: boolean }, cell: { row: number; col: number } | null): boolean;
}

/**
 * Result of one statement in a Glide grid; cells are read straight from the chunks.
 * Optional (the data view): `onHeaderClicked` gets the clicked column's index,
 * `headerMarks` draws a mark (a sort arrow) at the right of a column's header, and
 * `firstRowNumber` is the number of the first row marker.
 *
 * Tools (keyboard when the grid has the focus, or the right-click menu): copy (Ctrl+C TSV,
 * Ctrl+Shift+C with headers, Ctrl+Alt+C as INSERT), find (Ctrl+F), go to row (Ctrl+G),
 * record view (Shift+Space, double click on the row number), selection statistics.
 */
export function ResultGrid({
  result,
  onHeaderClicked,
  headerMarks,
  firstRowNumber,
  engine = 'mysql',
  tableRef,
  cellsCapped = false,
  extraCellMenu,
  editing,
  onOpenValue,
}: {
  /** Opens the value of a cell in the data view's editor (Shift+Enter, "Open value editor"). */
  onOpenValue?: (row: number, col: number) => void;
  /** The data view's editing hooks. */
  editing?: GridEditing;
  result: ResultSet;
  onHeaderClicked?: (column: number) => void;
  headerMarks?: Record<number, string>;
  firstRowNumber?: number;
  /** Quoting rules for INSERT statements. */
  engine?: Engine;
  /** The table the rows come from (the Data tab); else the result's single source table. */
  tableRef?: ObjectRef | null;
  /** The page reports cells cut at 64 KB: copying text of that size as INSERT is refused. */
  cellsCapped?: boolean;
  /** Entries appended to the cell context menu. */
  extraCellMenu?: (ctx: { row: number; col: number; value: unknown; column: string; selection?: SelectionLike }) => MenuEntry[];
}) {
  const themeVersion = useThemeVersion();
  // `themeVersion` is the dependency: the theme is read from CSS variables again when it changes.
  const { theme, nullStyle, statusStyle, font, charPx } = useMemo(() => (themeVersion >= 0 ? buildTheme() : buildTheme()), [themeVersion]);
  const [notice, setNotice] = useState<Notice | null>(null);
  const noticeTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const say = useCallback((text: string, warn: boolean) => {
    clearTimeout(noticeTimer.current);
    setNotice({ text, warn });
    if (!warn) noticeTimer.current = setTimeout(() => setNotice(null), 2500);
  }, []);
  useEffect(() => () => clearTimeout(noticeTimer.current), []);

  // Re-fitted once when the first rows arrive (columns can come before rows), never per rows event.
  const hasRows = (result.chunks[0]?.length ?? 0) > 0;
  const fitted = useMemo(() => initialWidths(result, font), [result.columns, hasRows, font]); // eslint-disable-line react-hooks/exhaustive-deps
  // User-resized widths belong to one result: a new result starts from the fitted widths.
  const [resized, setResized] = useState<Resized>({ cols: result.columns, w: {} });
  const overrides = overridesFor(resized, result.columns);
  const widths = useMemo(() => fitted.map((w, i) => overrides[i] ?? w), [fitted, overrides]);
  const columns = useMemo<GridColumn[]>(
    () => result.columns.map((c, i) => ({ id: String(i), title: c.name, width: widths[i] ?? MIN_COL_WIDTH })),
    [result.columns, widths],
  );
  const onColumnResize = useCallback(
    (_col: GridColumn, newSize: number, index: number) => {
      setResized((prev) => withResize(prev, result.columns, index, newSize));
    },
    [result.columns],
  );

  // The canvas only follows its container when given explicit pixels (F06).
  const boxRef = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState<{ w: number; h: number } | null>(null);
  useEffect(() => {
    const el = boxRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const measure = () => {
      const w = Math.floor(el.clientWidth);
      const h = Math.floor(el.clientHeight);
      if (w > 0 && h > 0) setSize((p) => (p && p.w === w && p.h === h ? p : { w, h }));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const { chunks } = result;
  const gridRef = useRef<DataEditorRef>(null);

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      const value = chunks[Math.floor(row / CHUNK)]?.[row % CHUNK]?.[col];
      if (!value) return { kind: GridCellKind.Text, data: '', displayData: '', allowOverlay: false };
      const view = cellView(value);
      const max = charsForWidth(widths[col] ?? MIN_COL_WIDTH, charPx);
      const clipped = view.text.length > max;
      const status = editing?.status(row, col) ?? null;
      const editable = editing?.isEditable(row, col) ?? false;
      return {
        kind: GridCellKind.Text,
        // The overlay edits the value as stored, never the display text (compacted JSON, cut text).
        data: editable ? (editText(value) ?? view.text) : view.text,
        displayData: fitCellText(view, max).replace(/\s+/g, ' '),
        allowOverlay: true, // a read-only overlay shows the full value
        readonly: !editable,
        // A clipped number keeps its leading digits (left side) and gets an ellipsis.
        contentAlign: view.kind === 'number' && !clipped ? 'right' : 'left',
        themeOverride: status ? { ...(view.kind === 'null' ? nullStyle : {}), ...statusStyle[status] } : view.kind === 'null' ? nullStyle : undefined,
      };
    },
    [chunks, nullStyle, statusStyle, widths, charPx, editing],
  );

  const onCellEdited = useCallback(
    ([col, row]: Item, value: EditableGridCell) => {
      if (value.kind === GridCellKind.Text) editing?.onEdit(row, col, value.data);
    },
    [editing],
  );
  const onCellActivated = useCallback(
    ([col, row]: Item) => {
      if (col >= 0) editing?.onActivate?.(row, col);
    },
    [editing],
  );

  const getCellsForSelection = useCallback(
    (rect: Rectangle) =>
      selectionCells(result, rect, (n) => {
        say(tooLargeMessage(n), true);
      }),
    [result, say],
  );

  const drawHeader = useCallback<DrawHeaderCallback>(
    ({ ctx, rect, theme: t, columnIndex }) => {
      const meta = result.columns[columnIndex];
      if (!meta) return;
      ctx.save();
      ctx.beginPath();
      ctx.rect(rect.x, rect.y, rect.width, rect.height);
      ctx.clip();
      ctx.textBaseline = 'middle';
      ctx.fillStyle = t.textHeader;
      ctx.font = `${t.headerFontStyle} ${t.fontFamily}`;
      ctx.fillText(meta.name, rect.x + 8, rect.y + rect.height / 2 - 8);
      ctx.fillStyle = t.textMedium;
      ctx.font = `${t.markerFontStyle} ${t.fontFamily}`;
      ctx.fillText(meta.db_type, rect.x + 8, rect.y + rect.height / 2 + 9);
      const mark = headerMarks?.[columnIndex];
      if (mark) {
        ctx.fillStyle = t.textHeader;
        ctx.font = `600 12px ${t.fontFamily}`;
        ctx.textAlign = 'right';
        ctx.fillText(mark, rect.x + rect.width - 8, rect.y + rect.height / 2 - 8);
      }
      ctx.restore();
    },
    [result.columns, headerMarks],
  );

  // ---- selection (controlled, so find / go to row can move it) ----
  const [sel, setSel] = useState<GridSelection | undefined>(undefined);
  const selRef = useRef(sel);
  selRef.current = sel;
  const selectCell = useCallback((col: number, row: number) => {
    setSel({
      columns: CompactSelection.empty(),
      rows: CompactSelection.empty(),
      current: { cell: [col, row], range: { x: col, y: row, width: 1, height: 1 }, rangeStack: [] },
    });
    gridRef.current?.scrollTo(col, row);
  }, []);

  const stats = useMemo(() => {
    const colCount = result.columns.length;
    const total = selectionSize(sel, result.rowCount, colCount);
    if (total < 2 || total > MAX_STATS_CELLS) return null;
    const picked = selectionBlock(sel, result.rowCount, colCount, MAX_STATS_CELLS);
    if (!picked.ok) return null;
    return selectionStats(blockValues(result, picked.block).flat(), total);
  }, [sel, result]);

  // ---- copy ----
  const copyWith = useCallback(
    async (mode: CopyMode, cellColumn?: number | null) => {
      const outcome = copyOutcome(mode, selRef.current, result, {
        engine,
        tableRef,
        cellsCapped,
        firstRowNumber: firstRowNumber ?? 1,
        cellColumn,
      });
      if (!outcome.ok) return say(outcome.message, true);
      if (await writeClipboard(outcome.text)) say(outcome.message, false);
      else say('Could not write to the clipboard', true);
    },
    [result, engine, tableRef, cellsCapped, firstRowNumber, say],
  );

  // ---- find ----
  const [find, setFind] = useState<{ query: string; at: number } | null>(null);
  const found = useMemo(() => (find ? findMatches(result, find.query) : null), [find, result]);
  const foundRef = useRef(found);
  foundRef.current = found;
  const showMatch = useCallback(
    (i: number) => {
      const m = foundRef.current?.matches[i];
      setFind((f) => (f ? { ...f, at: m ? i : -1 } : f));
      if (m) selectCell(m.col, m.row);
    },
    [selectCell],
  );
  const onFindQuery = useCallback((query: string) => setFind({ query, at: -1 }), []);
  // A new query jumps to the first match from the current cell (once per query, not per rows event).
  const query = find?.query;
  useEffect(() => {
    if (!query) return;
    const matches = foundRef.current?.matches ?? [];
    if (matches.length === 0) return setFind((f) => (f ? { ...f, at: -1 } : f));
    const cell = selRef.current?.current?.cell;
    showMatch(firstMatchFrom(matches, cell?.[1] ?? 0, cell?.[0] ?? 0));
  }, [query, showMatch]);
  const findRef = useRef(find);
  findRef.current = find;
  const stepMatch = useCallback(
    (dir: 1 | -1) => {
      const at = findRef.current?.at ?? -1;
      showMatch(stepIndex(at < 0 ? null : at, dir, foundRef.current?.matches.length ?? 0));
    },
    [showMatch],
  );
  const focusGrid = useCallback(() => gridRef.current?.focus(), []);
  const closeFind = useCallback(() => {
    setFind(null);
    focusGrid();
  }, [focusGrid]);

  // ---- go to row ----
  const [goTo, setGoTo] = useState(false);
  const first = firstRowNumber ?? 1;
  const onGo = useCallback(
    (input: string) => {
      const target = parseGoToRow(input, result.rowCount, first);
      if (!target.ok) return target.message;
      selectCell(Math.max(0, currentCol(selRef.current) ?? 0), target.row);
      return null;
    },
    [result.rowCount, first, selectCell],
  );
  const closeGoTo = useCallback(() => {
    setGoTo(false);
    focusGrid();
  }, [focusGrid]);

  // ---- record view ----
  const [record, setRecord] = useState(false);
  const recordRow = Math.min(Math.max(0, currentRow(sel) ?? 0), Math.max(0, result.rowCount - 1));
  const moveRecord = useCallback(
    (delta: -1 | 1) => {
      const row = currentRow(selRef.current) ?? 0;
      const next = Math.min(Math.max(0, row + delta), Math.max(0, result.rowCount - 1));
      selectCell(Math.max(0, currentCol(selRef.current) ?? 0), next);
    },
    [result.rowCount, selectCell],
  );
  const onFieldCopy = useCallback(
    async (text: string, what: string) => {
      if (await writeClipboard(text)) say(`Copied ${what}`, false);
      else say('Could not write to the clipboard', true);
    },
    [say],
  );

  // ---- menus entry points ----
  const commands = useRef<GridCommands | null>(null);
  const latest = {
    copy: copyWith,
    openFind: () => {
      setGoTo(false);
      setFind((f) => f ?? { query: '', at: -1 });
    },
    openRecord: () => setRecord(true),
  };
  const latestRef = useRef(latest);
  latestRef.current = latest;
  if (!commands.current) {
    commands.current = {
      copySelection: (mode) => void latestRef.current.copy(mode),
      openFindInResults: () => latestRef.current.openFind(),
      openRecordView: () => latestRef.current.openRecord(),
    };
  }
  useEffect(() => registerGrid(commands.current!), []);

  // ---- keyboard ----
  const onKeyDownCapture = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    const target = e.target as HTMLElement;
    if (target.closest('input, textarea, select, [contenteditable="true"]')) return;
    const key = e.key.toLowerCase();
    const mod = e.ctrlKey || e.metaKey;
    const handled = () => {
      e.preventDefault();
      e.stopPropagation();
    };
    if (record && mod && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
      handled();
      moveRecord(e.key === 'ArrowUp' ? -1 : 1);
      return;
    }
    if (record && e.key === 'Escape' && target.closest('.gt-record')) {
      handled();
      setRecord(false);
      focusGrid();
      return;
    }
    if (!target.closest('.gt-canvas')) return;
    if (onOpenValue && !mod && !e.altKey && e.shiftKey && e.key === 'Enter') {
      const row = currentRow(selRef.current);
      if (row !== null && row !== undefined) {
        handled();
        onOpenValue(row, currentCol(selRef.current) ?? 0);
        return;
      }
    }
    if (editing?.onKey) {
      const row = currentRow(selRef.current);
      const col = currentCol(selRef.current);
      if (editing.onKey(e, row === null || row === undefined ? null : { row, col: col ?? 0 })) {
        handled();
        return;
      }
    }
    if (mod && key === 'c') {
      handled();
      void copyWith(e.altKey ? 'insert' : e.shiftKey ? 'tsv-headers' : 'tsv');
    } else if (mod && !e.altKey && key === 'f') {
      handled();
      latestRef.current.openFind();
    } else if (mod && !e.altKey && key === 'g') {
      handled();
      setFind(null);
      setGoTo(true);
    } else if (!mod && !e.altKey && e.shiftKey && e.key === ' ') {
      handled();
      setRecord((r) => !r);
    }
  };

  // ---- mouse ----
  const lastMarkerClick = useRef<{ row: number; at: number } | null>(null);
  const onCellClicked = useCallback(
    (cell: Item) => {
      if (cell[0] >= 0) return;
      const last = lastMarkerClick.current;
      const now = Date.now();
      if (last && last.row === cell[1] && now - last.at < DOUBLE_CLICK_MS) {
        lastMarkerClick.current = null;
        // After Glide has applied the click's own row selection.
        setTimeout(() => {
          selectCell(0, cell[1]);
          setRecord(true);
        }, 0);
      } else {
        lastMarkerClick.current = { row: cell[1], at: now };
      }
    },
    [selectCell],
  );

  const pointer = useRef({ x: 0, y: 0 });
  const [menu, setMenu] = useState<{ x: number; y: number; cell: Item } | null>(null);
  const onCellContextMenu = useCallback((cell: Item, event: { preventDefault(): void }) => {
    event.preventDefault();
    setMenu({ x: pointer.current.x, y: pointer.current.y, cell });
  }, []);
  const closeMenu = useCallback(() => setMenu(null), []);
  const menuItems = useMemo<MenuEntry[]>(() => {
    if (!menu) return [];
    const [col, row] = menu.cell;
    const onColumn = col >= 0;
    const items: MenuEntry[] = [
      { label: 'Copy', hint: 'Ctrl+C', run: () => void copyWith('tsv') },
      { label: 'Copy with headers', hint: 'Ctrl+Shift+C', run: () => void copyWith('tsv-headers') },
      { label: 'Copy as INSERT', hint: 'Ctrl+Alt+C', run: () => void copyWith('insert') },
      { label: 'Copy as CSV', run: () => void copyWith('csv') },
    ];
    if (onColumn) items.push({ label: 'Copy column name', run: () => void copyWith('column', col) });
    items.push({ separator: true }, { label: 'View record', hint: 'Shift+Space', run: () => setRecord(true) });
    if (onOpenValue && onColumn && row >= 0 && row < result.rowCount) {
      items.push({ label: 'Open value editor', hint: 'Shift+Enter', run: () => onOpenValue(row, col) });
    }
    if (onColumn && row >= 0 && row < result.rowCount && extraCellMenu) {
      const value = cellValue(result, row, col);
      const extra = extraCellMenu({ row, col, value, column: result.columns[col]?.name ?? '', selection: selRef.current });
      if (extra.length > 0) items.push({ separator: true }, ...extra);
    }
    return items;
  }, [menu, copyWith, extraCellMenu, onOpenValue, result]);

  return (
    <div
      className="qr-grid"
      data-testid="result-grid"
      // Window shortcuts that clash with the editing keys (Ctrl+0) look for this.
      data-editing-grid={editing ? 'true' : undefined}
      onKeyDownCapture={onKeyDownCapture}
      onFocusCapture={() => commands.current && activateGrid(commands.current)}
      onContextMenuCapture={(e) => {
        pointer.current = { x: e.clientX, y: e.clientY };
      }}
    >
      {notice && (
        <div className={`qr-banner qr-notice${notice.warn ? ' warning' : ''}`} role="status" onClick={() => setNotice(null)}>
          {notice.text}
        </div>
      )}
      <div className="gt-main">
        <div className="gt-canvas" ref={boxRef}>
          {find && (
            <FindBar
              query={find.query}
              index={find.at}
              total={found?.matches.length ?? 0}
              capped={found?.capped ?? false}
              onQuery={onFindQuery}
              onStep={stepMatch}
              onClose={closeFind}
            />
          )}
          {goTo && <GoToBar rows={result.rowCount} first={first} onGo={onGo} onClose={closeGoTo} />}
          <DataEditor
            ref={gridRef}
            columns={columns}
            rows={result.rowCount}
            getCellContent={getCellContent}
            getCellsForSelection={getCellsForSelection}
            drawHeader={drawHeader}
            rowHeight={ROW_HEIGHT}
            headerHeight={HEADER_HEIGHT}
            rowMarkers={firstRowNumber === undefined ? 'number' : { kind: 'number', startIndex: firstRowNumber }}
            rangeSelect="rect"
            theme={theme}
            smoothScrollX
            smoothScrollY
            gridSelection={sel}
            onGridSelectionChange={setSel}
            // Ctrl+F and Ctrl+G belong to the grid tools above, not Glide's built-in search.
            keybindings={{ search: false }}
            onCellClicked={onCellClicked}
            // No `onDelete`: Delete / Backspace on a cell do nothing (they never stage an empty text);
            // NULL is explicit (Ctrl+0 or the cell menu).
            onCellEdited={editing ? onCellEdited : undefined}
            onCellActivated={editing ? onCellActivated : undefined}
            onCellContextMenu={onCellContextMenu}
            onColumnResize={onColumnResize}
            onHeaderClicked={onHeaderClicked ? (column) => onHeaderClicked(column) : undefined}
            minColumnWidth={MIN_COL_WIDTH}
            maxColumnWidth={2000}
            width={size?.w ?? '100%'}
            height={size?.h ?? '100%'}
          />
        </div>
        {record && (
          <RecordView
            result={result}
            row={recordRow}
            rowNumber={recordRow + first}
            onMove={moveRecord}
            onClose={() => {
              setRecord(false);
              focusGrid();
            }}
            onCopy={(text, what) => void onFieldCopy(text, what)}
          />
        )}
      </div>
      <GridStats stats={stats} />
      {menu && <ActionMenu label="Cell menu" x={menu.x} y={menu.y} items={menuItems} onClose={closeMenu} />}
    </div>
  );
}
