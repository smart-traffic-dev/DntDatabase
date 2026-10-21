/* eslint-disable @typescript-eslint/no-explicit-any -- the Glide mock passes its props through untyped */
import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ResultSet } from '../../store/queryRun';

// Grid metrics and paint options, read from the props the grid hands to Glide (the canvas cannot run in jsdom).
const glide = vi.hoisted(() => ({ props: null as null | Record<string, any> }));

vi.mock('@glideapps/glide-data-grid', async () => {
  const React = await import('react');
  const set = (items: number[] = []) => ({ length: items.length, toArray: () => items, first: () => items[0] });
  return {
    GridCellKind: { Text: 'text' },
    CompactSelection: { empty: () => set() },
    DataEditor: React.forwardRef((props: Record<string, any>, ref: any) => {
      glide.props = props;
      React.useImperativeHandle(ref, () => ({ scrollTo: () => {}, focus: () => {} }));
      return React.createElement('div', { 'data-testid': 'glide-mock', tabIndex: 0 });
    }),
  };
});

import { GRID_HEADER_HEIGHT, ResultGrid, blankArea, buildTheme, scrollbarHeight } from './ResultGrid';

const col = (name: string, db_type = 'int') => ({ name, db_type, logical: 'Integer' as const, nullable: true, origin_table: null, origin_column: null });
const result = (rows: number): ResultSet => ({
  id: 1,
  index: 0,
  columns: [col('id'), col('name', 'varchar(20)')],
  chunks: [Array.from({ length: rows }, (_, i) => [{ t: 'Int' as const, v: i }, { t: 'Null' as const }])],
  rowCount: rows,
  truncated: false,
  affected: null,
  elapsedMs: 1,
  done: true,
});

const root = document.documentElement;
afterEach(() => {
  root.removeAttribute('style');
  root.removeAttribute('data-density');
});

describe('grid metrics', () => {
  it('row height follows --dnt-row-h-grid (24 compact, 30 comfortable); the header is 36px', async () => {
    render(<ResultGrid result={result(3)} />);
    expect(glide.props!.rowHeight).toBe(24);
    expect(glide.props!.headerHeight).toBe(36);
    expect(GRID_HEADER_HEIGHT).toBe(36);
    await act(async () => {
      root.style.setProperty('--dnt-row-h-grid', '30px');
      root.setAttribute('data-density', 'comfortable');
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(glide.props!.rowHeight).toBe(30);
  });

  it('paints no row lines below the last row', () => {
    render(<ResultGrid result={result(3)} />);
    const over = glide.props!.getRowThemeOverride as (row: number) => Record<string, string> | undefined;
    // A line is drawn on a row's top edge: row 3's is the bottom of the last row and stays.
    expect(over(0)).toBeUndefined();
    expect(over(3)).toBeUndefined();
    expect(over(4)).toEqual({ horizontalBorderColor: 'transparent' });
    expect(over(40)).toEqual({ horizontalBorderColor: 'transparent' });
  });

  it('the grid fills its box whatever the row count, so the horizontal scrollbar sits at the bottom', () => {
    // jsdom (test setup) reports 280 x 600 for every element.
    render(<ResultGrid result={result(3)} />);
    expect(glide.props!.height).toBe(600);
    // The empty space under the rows is covered (no column lines there), above the scrollbar.
    const cover = screen.getByTestId('grid-blank');
    expect(cover).toHaveStyle({ top: `${36 + 3 * 24 + 1}px` });
    expect(cover).toHaveAttribute('aria-hidden', 'true');
  });

  it('blankArea: from under the last row to the bottom, leaving the scrollbar; none when rows fill the box', () => {
    expect(blankArea({ boxHeight: 600, boxWidth: 800, rows: 3, rowHeight: 24, columnsWidth: 300 })).toEqual({ top: 109, height: 600 - 109 });
    // Wider than the box: the horizontal scrollbar at the bottom stays uncovered.
    expect(blankArea({ boxHeight: 600, boxWidth: 800, rows: 3, rowHeight: 24, columnsWidth: 790 })).toEqual({ top: 109, height: 600 - 109 - 17 });
    expect(blankArea({ boxHeight: 600, boxWidth: 800, rows: 500, rowHeight: 24, columnsWidth: 300 })).toBeNull();
    // A measured scrollbar (thin, platform) wins over the estimate.
    expect(blankArea({ boxHeight: 600, boxWidth: 800, rows: 3, rowHeight: 24, columnsWidth: 790, scrollbar: 8 })).toEqual({ top: 109, height: 600 - 109 - 8 });
    expect(scrollbarHeight({ offsetHeight: 100, clientHeight: 92 } as HTMLElement)).toBe(8);
    expect(scrollbarHeight(null)).toBeUndefined();
  });

  it('selection is the muted fill until the grid has the focus', () => {
    root.style.setProperty('--dnt-bg-selected', '#dce7fd');
    root.style.setProperty('--dnt-bg-selected-muted', '#ebebed');
    render(<ResultGrid result={result(3)} />);
    expect(glide.props!.theme.accentLight).toBe('#ebebed');
    expect(glide.props!.theme.bgHeaderHasFocus).toBe('#ebebed');
    fireEvent.focus(screen.getByTestId('glide-mock'));
    expect(glide.props!.theme.accentLight).toBe('#dce7fd');
    expect(glide.props!.theme.bgHeaderHasFocus).toBe('#dce7fd');
    fireEvent.blur(screen.getByTestId('glide-mock'));
    expect(glide.props!.theme.accentLight).toBe('#ebebed');
  });

  it('NULL is italic in fg-subtle', () => {
    root.style.setProperty('--dnt-fg-subtle', '#707076');
    root.style.setProperty('--dnt-fg-muted', '#5a5a60');
    const { nullStyle } = buildTheme();
    expect(nullStyle.textDark).toBe('#707076');
    expect(nullStyle.baseFontStyle).toMatch(/^italic /);
  });

  it('numbers are right-aligned in the code (fixed-width, so tabular) font', () => {
    render(<ResultGrid result={result(1)} />);
    const cell = glide.props!.getCellContent([0, 0]);
    expect(cell.contentAlign).toBe('right');
    expect(glide.props!.theme.fontFamily).toBe(buildTheme().theme.fontFamily);
  });

  it('reports a summary of the selection (for the status bar)', () => {
    const onSelectionSummary = vi.fn();
    render(<ResultGrid result={result(3)} onSelectionSummary={onSelectionSummary} />);
    expect(onSelectionSummary).toHaveBeenLastCalledWith(null);
    const empty = { length: 0, toArray: () => [], first: () => undefined };
    act(() =>
      glide.props!.onGridSelectionChange({ current: { cell: [0, 0], range: { x: 0, y: 0, width: 2, height: 3 }, rangeStack: [] }, rows: empty, columns: empty }),
    );
    expect(onSelectionSummary).toHaveBeenLastCalledWith('3 rows × 2 columns selected');
  });

  it('the header draws the name on line 1 and the type on line 2 in the caption font', () => {
    render(<ResultGrid result={result(1)} />);
    const texts: { text: string; y: number; font: string }[] = [];
    const ctx: Record<string, any> = {
      save() {},
      restore() {},
      beginPath() {},
      rect() {},
      clip() {},
      fillText(text: string, _x: number, y: number) {
        texts.push({ text, y, font: ctx.font });
      },
    };
    const t = buildTheme().theme;
    const rect = { x: 0, y: 0, width: 120, height: 36 };
    glide.props!.drawHeader({ ctx, rect, theme: t, columnIndex: 1 });
    const [name, type] = texts;
    expect(name).toMatchObject({ text: 'name' });
    expect(type).toMatchObject({ text: 'varchar(20)', font: `${t.markerFontStyle} ${t.fontFamily}` });
    expect(name!.y).toBeLessThan(type!.y);
    expect(name!.y).toBeGreaterThan(rect.y + 4);
    expect(type!.y).toBeLessThan(rect.y + rect.height - 4);
  });
});
