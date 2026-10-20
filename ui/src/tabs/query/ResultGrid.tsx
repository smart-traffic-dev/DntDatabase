import '@glideapps/glide-data-grid/dist/index.css';
import { DataEditor, GridCellKind, type DrawHeaderCallback, type GridCell, type GridColumn, type Item, type Theme } from '@glideapps/glide-data-grid';
import { useCallback, useMemo } from 'react';
import { CHUNK, type ResultSet } from '../../store/queryRun';
import { cellView, copyText } from './format';

const ROW_HEIGHT = 28;
const HEADER_HEIGHT = 44;
const COLUMN_WIDTH = 150;
/** Longest text handed to the canvas; the full value is still copied. */
const DISPLAY_CHARS = 300;

function cssVar(name: string, fallback: string): string {
  try {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;
  } catch {
    return fallback;
  }
}

function gridTheme(): Partial<Theme> {
  return {
    accentColor: cssVar('--accent', '#1f5fd1'),
    accentLight: cssVar('--sel', '#e3ebfa'),
    bgCell: cssVar('--panel', '#ffffff'),
    bgHeader: cssVar('--sidebar', '#fafafb'),
    bgHeaderHasFocus: cssVar('--sel', '#e3ebfa'),
    bgHeaderHovered: cssVar('--sel', '#e3ebfa'),
    textDark: cssVar('--text', '#1b1f24'),
    textMedium: cssVar('--muted', '#5a6370'),
    textHeader: cssVar('--text', '#1b1f24'),
    borderColor: cssVar('--border', '#dcdfe4'),
    fontFamily: cssVar('--font-mono', 'ui-monospace, monospace'),
    baseFontStyle: '13px',
    headerFontStyle: '600 12px',
  };
}

/** Result of one statement in a Glide grid; cells are read straight from the chunks. */
export function ResultGrid({ result }: { result: ResultSet }) {
  const theme = useMemo(gridTheme, []);
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
      const text = view.text.length > DISPLAY_CHARS ? `${view.text.slice(0, DISPLAY_CHARS)}…` : view.text;
      return {
        kind: GridCellKind.Text,
        data: view.text,
        displayData: text.replace(/\s+/g, ' '),
        allowOverlay: false,
        readonly: true,
        copyData: copyText(value),
        contentAlign: view.kind === 'number' ? 'right' : 'left',
        themeOverride:
          view.kind === 'null'
            ? { textDark: cssVar('--muted', '#5a6370'), baseFontStyle: 'italic 13px' }
            : undefined,
      };
    },
    [chunks],
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
      <DataEditor
        columns={columns}
        rows={result.rowCount}
        getCellContent={getCellContent}
        getCellsForSelection
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
