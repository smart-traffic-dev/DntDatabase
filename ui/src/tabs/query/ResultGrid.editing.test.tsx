/* eslint-disable @typescript-eslint/no-explicit-any -- the Glide mock passes its props through untyped */
import { act, fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { ResultSet } from '../../store/queryRun';

// The editing wiring is read from the props the grid hands to Glide (the canvas cannot run in jsdom).
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

import { ResultGrid, type GridEditing } from './ResultGrid';

const col = (name: string) => ({ name, db_type: 'text', logical: 'Text' as const, nullable: true, origin_table: null, origin_column: null });
const result: ResultSet = {
  id: 1,
  index: 0,
  columns: [col('id'), col('name'), col('payload')],
  chunks: [
    [
      [{ t: 'Int', v: 10 }, { t: 'Text', v: 'alpha' }, { t: 'Text', v: 'cut' }],
      [{ t: 'Int', v: 20 }, { t: 'Text', v: 'beta' }, { t: 'Text', v: 'x' }],
    ],
  ],
  rowCount: 2,
  truncated: false,
  affected: null,
  elapsedMs: 1,
  done: true,
};

const set = (items: number[] = []) => ({ length: items.length, toArray: () => items, first: () => items[0] });
const select = (x: number, y: number) =>
  act(() => glide.props!.onGridSelectionChange({ current: { cell: [x, y], range: { x, y, width: 1, height: 1 }, rangeStack: [] }, rows: set(), columns: set() }));

function editing(over: Partial<GridEditing> = {}): GridEditing {
  return {
    // `name` (column 1) edits, except row 1 (deleted); `id` is a key, `payload` is cut.
    isEditable: (row, col) => col === 1 && row !== 1,
    status: (row, col) => (row === 0 && col === 1 ? 'edited' : row === 1 ? 'deleted' : null),
    onEdit: vi.fn(),
    onActivate: vi.fn(),
    onKey: vi.fn(() => false),
    ...over,
  };
}

describe('ResultGrid editing', () => {
  it('maps cells to readonly and a status colour; the grid is marked for window shortcuts', () => {
    render(<ResultGrid result={result} editing={editing()} />);
    const cell = (col: number, row: number) => glide.props!.getCellContent([col, row]);
    expect(cell(1, 0)).toMatchObject({ readonly: false, allowOverlay: true });
    expect(cell(0, 0)).toMatchObject({ readonly: true }); // key column
    expect(cell(2, 0)).toMatchObject({ readonly: true }); // truncated cell
    expect(cell(1, 1)).toMatchObject({ readonly: true }); // deleted row
    expect(cell(1, 0).themeOverride).toHaveProperty('bgCell');
    expect(cell(1, 1).themeOverride).toHaveProperty('bgCell');
    expect(cell(0, 0).themeOverride).toBeUndefined();
    expect(screen.getByTestId('result-grid')).toHaveAttribute('data-editing-grid', 'true');
  });

  it('without editing every cell is read-only and the grid is not marked', () => {
    render(<ResultGrid result={result} />);
    expect(glide.props!.getCellContent([1, 0])).toMatchObject({ readonly: true });
    expect(glide.props!.onCellEdited).toBeUndefined();
    expect(screen.getByTestId('result-grid')).not.toHaveAttribute('data-editing-grid');
  });

  it('onCellEdited reports (row, column, text); Glide passes [column, row]', () => {
    const e = editing();
    render(<ResultGrid result={result} editing={e} />);
    glide.props!.onCellEdited([1, 0], { kind: 'text', data: 'new' });
    expect(e.onEdit).toHaveBeenCalledWith(0, 1, 'new');
    glide.props!.onCellEdited([1, 0], { kind: 'number', data: 3 });
    expect(e.onEdit).toHaveBeenCalledTimes(1);
    glide.props!.onCellActivated([2, 1]);
    expect(e.onActivate).toHaveBeenCalledWith(1, 2);
  });

  it('an editable cell is edited as its stored value: JSON as written, a 20-digit number exact', () => {
    const raw = '{"big": 12345678901234567890, "x": 1.10}';
    const json: ResultSet = { ...result, chunks: [[[{ t: 'Int', v: 10 }, { t: 'Json', v: raw }, { t: 'Text', v: 'cut' }]]], rowCount: 1 };
    render(<ResultGrid result={json} editing={editing({ isEditable: (_r, c) => c === 1 })} />);
    const cell = glide.props!.getCellContent([1, 0]);
    expect(cell).toMatchObject({ readonly: false, data: raw });
    // The grid itself still shows the compact one-line form.
    expect(cell.displayData).not.toContain('\n');
  });

  it('Delete and Backspace never stage an empty text: no onDelete is wired, NULL is Ctrl+0 / the menu', () => {
    const e = editing();
    render(<ResultGrid result={result} editing={e} />);
    expect(glide.props!.onDelete).toBeUndefined();
    select(1, 0);
    fireEvent.keyDown(screen.getByTestId('glide-mock'), { key: 'Delete' });
    fireEvent.keyDown(screen.getByTestId('glide-mock'), { key: 'Backspace' });
    expect(e.onEdit).not.toHaveBeenCalled();
  });

  it('keys go to the data view with the selected cell; a handled key is consumed', () => {
    const onKey = vi.fn(() => true);
    render(<ResultGrid result={result} editing={editing({ onKey })} />);
    select(1, 1);
    const consumed = !fireEvent.keyDown(screen.getByTestId('glide-mock'), { key: '0', ctrlKey: true });
    expect(onKey).toHaveBeenCalledWith(expect.objectContaining({ key: '0', ctrlKey: true }), { row: 1, col: 1 });
    expect(consumed).toBe(true);
  });
});
