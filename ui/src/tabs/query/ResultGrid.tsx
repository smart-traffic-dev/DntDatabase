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
import { useCallback, useEffect, useMemo, useState } from 'react';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import { MAX_COPY_CELLS, cellView, copyText, truncateText } from './format';

const ROW_HEIGHT = 28;
const HEADER_HEIGHT = 44;
const COLUMN_WIDTH = 150;
/** Longest text handed to the canvas; the full value is still copied. */
const DISPLAY_CHARS = 300;

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
      fontFamily: cssVar('--font-mono', 'ui-monospace, monospace'),
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

/** Result of one statement in a Glide grid; cells are read straight from the chunks. */
export function ResultGrid({ result }: { result: ResultSet }) {
  const themeVersion = useThemeVersion();
  // `themeVersion` is the dependency: the theme is read from CSS variables again when it changes.
  const { theme, nullStyle } = useMemo(() => (themeVersion >= 0 ? buildTheme() : buildTheme()), [themeVersion]);
  const [notice, setNotice] = useState<string | null>(null);
  const columns = useMemo<GridColumn[]>(
    () => result.columns.map((c, i) => ({ id: String(i), title: c.name, width: COLUMN_WIDTH })),
    [result.columns],
  );
  const { chunks } = result;

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      const value = chunks[Math.floor(row / CHUNK)]?.[row % CHUNK]?.[col];
      if (!value) return { kind: GridCellKind.Text, data: '', displayData: '', allowOverlay: false };
      const view = cellView(value);
      return {
        kind: GridCellKind.Text,
        data: view.text,
        displayData: truncateText(view.text, DISPLAY_CHARS).replace(/\s+/g, ' '),
        allowOverlay: false,
        readonly: true,
        contentAlign: view.kind === 'number' ? 'right' : 'left',
        themeOverride: view.kind === 'null' ? nullStyle : undefined,
      };
    },
    [chunks, nullStyle],
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
    <div className="qr-grid" data-testid="result-grid">
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
        width="100%"
        height="100%"
      />
    </div>
  );
}
