import '@glideapps/glide-data-grid/dist/index.css';
import {
  DataEditor,
  GridCellKind,
  type DrawHeaderCallback,
  type GridCell,
  type GridColumn,
  type Item,
  type Rectangle,
  type Theme,
} from '@glideapps/glide-data-grid';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import {
  FIT_SAMPLE_ROWS,
  MAX_COPY_CELLS,
  MIN_COL_WIDTH,
  cellView,
  charsForWidth,
  copyText,
  fitCellText,
  fitColumnWidth,
} from './format';

const ROW_HEIGHT = 28;
const HEADER_HEIGHT = 44;
/** Cascadia Mono, not Code: the grid draws on a canvas and Code's ligatures would render >= as a single glyph. */
const GRID_FONT = "'Cascadia Mono', Consolas, 'SF Mono', Menlo, 'DejaVu Sans Mono', monospace";

type Resized = { cols: ResultSet['columns']; w: Record<number, number> };
const NO_OVERRIDES: Record<number, number> = {};

/** Initial column widths: header plus the first rows of the first chunk (no per-paint work). */
/** The widths the user set for this result (a new result has none). */
export function overridesFor(r: Resized, cols: ResultSet['columns']): Record<number, number> {
  return r.cols === cols ? r.w : NO_OVERRIDES;
}

/** `prev` with column `index` of the result `cols` set to `size` (clamped). */
export function withResize(prev: Resized, cols: ResultSet['columns'], index: number, size: number): Resized {
  const w = Math.max(MIN_COL_WIDTH, Math.min(size, 2000));
  return { cols, w: { ...overridesFor(prev, cols), [index]: w } };
}

export function initialWidths(result: ResultSet): number[] {
  const rows = result.chunks[0] ?? [];
  const n = Math.min(rows.length, FIT_SAMPLE_ROWS);
  return result.columns.map((c, i) => {
    const samples: string[] = [];
    for (let r = 0; r < n; r++) {
      const v = rows[r]?.[i];
      if (v) samples.push(cellView(v).text);
    }
    return fitColumnWidth(c.name, c.db_type, samples);
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

function buildTheme(): { theme: Partial<Theme>; nullStyle: Partial<Theme> } {
  const muted = cssVar('--muted', '#5a6370');
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
      fontFamily: GRID_FONT,
      textLight: muted,
      markerFontStyle: '11px',
      baseFontStyle: '13px',
      headerFontStyle: '600 12px',
    },
    nullStyle: { textDark: muted, baseFontStyle: 'italic 13px' },
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

/**
 * Result of one statement in a Glide grid; cells are read straight from the chunks.
 * `onHeaderClicked` (optional) receives the clicked column's index (the data view sorts by it).
 */
export function ResultGrid({ result, onHeaderClicked }: { result: ResultSet; onHeaderClicked?: (column: number) => void }) {
  const themeVersion = useThemeVersion();
  // `themeVersion` is the dependency: the theme is read from CSS variables again when it changes.
  const { theme, nullStyle } = useMemo(() => (themeVersion >= 0 ? buildTheme() : buildTheme()), [themeVersion]);
  const [notice, setNotice] = useState<string | null>(null);
  // Re-fitted once when the first rows arrive (columns can come before rows), never per rows event.
  const hasRows = (result.chunks[0]?.length ?? 0) > 0;
  const fitted = useMemo(() => initialWidths(result), [result.columns, hasRows]); // eslint-disable-line react-hooks/exhaustive-deps
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

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      const value = chunks[Math.floor(row / CHUNK)]?.[row % CHUNK]?.[col];
      if (!value) return { kind: GridCellKind.Text, data: '', displayData: '', allowOverlay: false };
      const view = cellView(value);
      const max = charsForWidth(widths[col] ?? MIN_COL_WIDTH);
      const clipped = view.text.length > max;
      return {
        kind: GridCellKind.Text,
        data: view.text,
        displayData: fitCellText(view, max).replace(/\s+/g, ' '),
        allowOverlay: true, // read-only overlay shows the full value
        readonly: true,
        // A clipped number keeps its leading digits (left side) and gets an ellipsis.
        contentAlign: view.kind === 'number' && !clipped ? 'right' : 'left',
        themeOverride: view.kind === 'null' ? nullStyle : undefined,
      };
    },
    [chunks, nullStyle, widths],
  );

  const getCellsForSelection = useCallback(
    (rect: Rectangle) =>
      selectionCells(result, rect, (n) => {
        setNotice(tooLargeMessage(n));
      }),
    [result],
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
      ctx.font = `600 12px ${t.fontFamily}`;
      ctx.fillText(meta.name, rect.x + 8, rect.y + rect.height / 2 - 8);
      ctx.fillStyle = t.textMedium;
      ctx.font = `11px ${t.fontFamily}`;
      ctx.fillText(meta.db_type, rect.x + 8, rect.y + rect.height / 2 + 9);
      ctx.restore();
    },
    [result.columns],
  );

  return (
    <div className="qr-grid" data-testid="result-grid" ref={boxRef}>
      {notice && (
        <div className="qr-banner warning qr-notice" role="status" onClick={() => setNotice(null)}>
          {notice}
        </div>
      )}
      <DataEditor
        columns={columns}
        rows={result.rowCount}
        getCellContent={getCellContent}
        getCellsForSelection={getCellsForSelection}
        drawHeader={drawHeader}
        rowHeight={ROW_HEIGHT}
        headerHeight={HEADER_HEIGHT}
        rowMarkers="number"
        rangeSelect="rect"
        theme={theme}
        smoothScrollX
        smoothScrollY
        onColumnResize={onColumnResize}
        onHeaderClicked={onHeaderClicked ? (column) => onHeaderClicked(column) : undefined}
        minColumnWidth={MIN_COL_WIDTH}
        maxColumnWidth={2000}
        width={size?.w ?? '100%'}
        height={size?.h ?? '100%'}
      />
    </div>
  );
}
