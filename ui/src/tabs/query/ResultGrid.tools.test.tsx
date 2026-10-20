/* eslint-disable @typescript-eslint/no-explicit-any -- the Glide mock passes its props through untyped */
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ResultSet } from '../../store/queryRun';

// The tools are exercised through the props the grid hands to Glide (the canvas itself cannot run in jsdom).
const glide = vi.hoisted(() => ({ props: null as null | Record<string, any>, scrolled: [] as [number, number][] }));

vi.mock('@glideapps/glide-data-grid', async () => {
  const React = await import('react');
  const set = (items: number[] = []) => ({ length: items.length, toArray: () => items, first: () => items[0] });
  return {
    GridCellKind: { Text: 'text' },
    CompactSelection: { empty: () => set() },
    DataEditor: React.forwardRef((props: Record<string, any>, ref: any) => {
      glide.props = props;
      React.useImperativeHandle(ref, () => ({
        scrollTo: (c: number, r: number) => glide.scrolled.push([c, r]),
        focus: () => {},
      }));
      return React.createElement('div', { 'data-testid': 'glide-mock', tabIndex: 0 });
    }),
  };
});

import { ResultGrid } from './ResultGrid';
import { copySelection, openFindInResults, openRecordView } from './gridCommands';

const col = (name: string, db_type = 'text') => ({ name, db_type, logical: 'Text' as const, nullable: true, origin_table: null, origin_column: null });
const result: ResultSet = {
  id: 1,
  index: 0,
  columns: [col('id', 'int4'), col('name'), col('payload', 'json')],
  chunks: [
    [
      [{ t: 'Int', v: 10 }, { t: 'Text', v: 'alpha' }, { t: 'Json', v: '{"a":[1,2],"b":{"c":"x"}}' }],
      [{ t: 'Int', v: 20 }, { t: 'Text', v: 'Beta alpha' }, { t: 'Null' }],
      [{ t: 'Int', v: 30 }, { t: 'Text', v: "O'Neil" }, { t: 'Json', v: '[]' }],
    ],
  ],
  rowCount: 3,
  truncated: false,
  affected: null,
  elapsedMs: 1,
  done: true,
};

const set = (items: number[] = []) => ({ length: items.length, toArray: () => items, first: () => items[0] });
const range = (x: number, y: number, width: number, height: number) => ({
  current: { cell: [x, y], range: { x, y, width, height }, rangeStack: [] },
  rows: set(),
  columns: set(),
});
const select = (sel: ReturnType<typeof range>) => act(() => glide.props!.onGridSelectionChange(sel));
const key = (init: KeyboardEventInit) => fireEvent.keyDown(screen.getByTestId('glide-mock'), init);

let written: string[];

beforeEach(() => {
  written = [];
  glide.scrolled = [];
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true,
    value: { writeText: vi.fn(async (t: string) => void written.push(t)) },
  });
});
afterEach(() => vi.restoreAllMocks());

describe('copy shortcuts', () => {
  it('ctrl_c_copies_the_selection_as_tsv', async () => {
    render(<ResultGrid result={result} />);
    select(range(0, 0, 2, 2));
    key({ key: 'c', ctrlKey: true });
    await waitFor(() => expect(written).toEqual(['10\talpha\r\n20\tBeta alpha']));
    expect(screen.getByText('Copied 2 rows')).toBeInTheDocument();
  });

  it('ctrl_shift_c_adds_the_header_row', async () => {
    render(<ResultGrid result={result} />);
    select(range(1, 0, 1, 2));
    key({ key: 'C', ctrlKey: true, shiftKey: true });
    await waitFor(() => expect(written).toEqual(['name\r\nalpha\r\nBeta alpha']));
  });

  it('ctrl_alt_c_copies_whole_rows_as_insert_with_the_engine_quoting', async () => {
    render(<ResultGrid result={result} engine="postgres" />);
    select(range(1, 2, 1, 1));
    key({ key: 'c', ctrlKey: true, altKey: true });
    await waitFor(() => expect(written).toHaveLength(1));
    expect(written[0]).toBe(`INSERT INTO table_name ("id", "name", "payload") VALUES (30, 'O''Neil', '[]');`);
  });

  it('insert_uses_the_data_tab_table', async () => {
    const ref = { database: 'shop', schema: 'public', name: 'orders', kind: 'Table' } as never;
    render(<ResultGrid result={result} engine="mysql" tableRef={ref} />);
    select(range(0, 0, 1, 1));
    key({ key: 'c', ctrlKey: true, altKey: true });
    await waitFor(() => expect(written).toHaveLength(1));
    expect(written[0]).toMatch(/^INSERT INTO `shop`\.`orders` \(`id`, `name`, `payload`\) VALUES \(10, 'alpha', '\{"a":\[1,2\],"b":\{"c":"x"\}\}'\);$/);
  });

  it('refuses_an_insert_it_cannot_write_exactly', async () => {
    const bin: ResultSet = { ...result, chunks: [[[{ t: 'Int', v: 1 }, { t: 'Bytes', v: { len: 2, preview: [1, 2] } }, { t: 'Null' }]]], rowCount: 1 };
    render(<ResultGrid result={bin} />);
    select(range(0, 0, 1, 1));
    key({ key: 'c', ctrlKey: true, altKey: true });
    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent(/Cannot copy as INSERT: column name, row 1/));
    expect(written).toEqual([]);
  });

  it('does_not_take_ctrl_c_from_an_input', () => {
    render(<ResultGrid result={result} />);
    select(range(0, 0, 1, 1));
    key({ key: 'f', ctrlKey: true });
    fireEvent.keyDown(screen.getByLabelText('Find in results'), { key: 'c', ctrlKey: true });
    expect(written).toEqual([]);
  });

  it('shows_a_message_when_nothing_is_selected', async () => {
    render(<ResultGrid result={result} />);
    key({ key: 'c', ctrlKey: true });
    await waitFor(() => expect(screen.getByRole('status')).toHaveTextContent('Select cells first'));
  });

  it('menu_hooks_act_on_the_focused_grid', async () => {
    render(<ResultGrid result={result} />);
    fireEvent.focus(screen.getByTestId('glide-mock'));
    select(range(0, 0, 3, 1));
    act(() => copySelection('csv'));
    await waitFor(() => expect(written).toEqual(['id,name,payload\r\n10,alpha,"{""a"":[1,2],""b"":{""c"":""x""}}"']));
    act(() => openFindInResults());
    expect(screen.getByTestId('find-bar')).toBeInTheDocument();
    act(() => openRecordView());
    expect(screen.getByTestId('record-view')).toBeInTheDocument();
  });
});

describe('find in results', () => {
  it('ctrl_f_opens_a_bar_that_steps_through_matches_and_selects_them', () => {
    render(<ResultGrid result={result} />);
    key({ key: 'f', ctrlKey: true });
    const input = screen.getByLabelText('Find in results');
    expect(input).toHaveFocus();
    fireEvent.change(input, { target: { value: 'ALPHA' } });
    expect(screen.getByTestId('find-count')).toHaveTextContent('1 / 2');
    expect(glide.props!.gridSelection.current.cell).toEqual([1, 0]);
    expect(glide.scrolled.at(-1)).toEqual([1, 0]);

    fireEvent.keyDown(input, { key: 'Enter' });
    expect(screen.getByTestId('find-count')).toHaveTextContent('2 / 2');
    expect(glide.props!.gridSelection.current.cell).toEqual([1, 1]);
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(screen.getByTestId('find-count')).toHaveTextContent('1 / 2');
    fireEvent.keyDown(input, { key: 'Enter', shiftKey: true });
    expect(screen.getByTestId('find-count')).toHaveTextContent('2 / 2');
  });

  it('reports_no_matches_and_closes_with_escape', () => {
    render(<ResultGrid result={result} />);
    key({ key: 'f', ctrlKey: true });
    const input = screen.getByLabelText('Find in results');
    fireEvent.change(input, { target: { value: 'zzz' } });
    expect(screen.getByTestId('find-count')).toHaveTextContent('No matches');
    fireEvent.keyDown(input, { key: 'Enter' });
    fireEvent.keyDown(input, { key: 'Escape' });
    expect(screen.queryByTestId('find-bar')).toBeNull();
  });

  it('starts_from_the_current_cell', () => {
    render(<ResultGrid result={result} />);
    select(range(0, 1, 1, 1));
    key({ key: 'f', ctrlKey: true });
    fireEvent.change(screen.getByLabelText('Find in results'), { target: { value: 'alpha' } });
    expect(screen.getByTestId('find-count')).toHaveTextContent('2 / 2');
  });
});

describe('go to row', () => {
  it('ctrl_g_jumps_to_a_one_based_row_respecting_the_page_offset', () => {
    render(<ResultGrid result={result} firstRowNumber={501} />);
    key({ key: 'g', ctrlKey: true });
    const input = screen.getByLabelText('Go to row');
    fireEvent.change(input, { target: { value: '503' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(glide.props!.gridSelection.current.cell).toEqual([0, 2]);
    expect(glide.scrolled.at(-1)).toEqual([0, 2]);
    expect(screen.queryByTestId('goto-bar')).toBeNull();
  });

  it('keeps_the_bar_open_with_a_message_for_a_bad_row', () => {
    render(<ResultGrid result={result} firstRowNumber={501} />);
    key({ key: 'g', ctrlKey: true });
    const input = screen.getByLabelText('Go to row');
    fireEvent.change(input, { target: { value: '3' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(screen.getByRole('alert')).toHaveTextContent('Out of range: rows 501–503');
    fireEvent.keyDown(input, { key: 'Escape' });
    expect(screen.queryByTestId('goto-bar')).toBeNull();
  });
});

describe('selection statistics', () => {
  it('shows_count_sum_avg_min_max_for_two_or_more_numbers', () => {
    render(<ResultGrid result={result} />);
    expect(screen.queryByTestId('grid-stats')).toBeNull();
    select(range(0, 0, 1, 3));
    expect(screen.getByTestId('grid-stats')).toHaveTextContent('Count 3 · Sum 60 · Avg 20 · Min 10 · Max 30');
    select(range(0, 0, 1, 1));
    expect(screen.queryByTestId('grid-stats')).toBeNull();
  });

  it('shows_only_the_count_for_text', () => {
    render(<ResultGrid result={result} />);
    select(range(1, 0, 1, 3));
    expect(screen.getByTestId('grid-stats')).toHaveTextContent(/^Count 3$/);
  });
});

describe('record view', () => {
  it('shift_space_opens_it_with_json_as_a_tree_and_ctrl_arrows_move_rows', () => {
    render(<ResultGrid result={result} firstRowNumber={501} />);
    select(range(0, 0, 1, 1));
    key({ key: ' ', shiftKey: true });
    const view = screen.getByTestId('record-view');
    expect(within(view).getByText('Row 501')).toBeInTheDocument();
    expect(within(view).getByText('alpha')).toBeInTheDocument();
    // JSON object: a tree with keys, not raw text.
    expect(within(view).getAllByRole('button', { name: /^[ab]/ }).length).toBeGreaterThan(0);

    key({ key: 'ArrowDown', ctrlKey: true });
    expect(within(screen.getByTestId('record-view')).getByText('Row 502')).toBeInTheDocument();
    expect(within(screen.getByTestId('record-view')).getByText('Beta alpha')).toBeInTheDocument();
    expect(within(screen.getByTestId('record-view')).getByText('NULL')).toBeInTheDocument();
    key({ key: 'ArrowUp', ctrlKey: true });
    expect(within(screen.getByTestId('record-view')).getByText('Row 501')).toBeInTheDocument();
  });

  it('prev_next_buttons_and_close', () => {
    render(<ResultGrid result={result} />);
    select(range(0, 0, 1, 1));
    key({ key: ' ', shiftKey: true });
    const view = screen.getByTestId('record-view');
    expect(within(view).getByRole('button', { name: 'Previous row' })).toBeDisabled();
    fireEvent.click(within(view).getByRole('button', { name: 'Next row' }));
    expect(within(screen.getByTestId('record-view')).getByText('Row 2')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Close record view' }));
    expect(screen.queryByTestId('record-view')).toBeNull();
  });

  it('copies_one_field', async () => {
    render(<ResultGrid result={result} />);
    select(range(0, 2, 1, 1));
    key({ key: ' ', shiftKey: true });
    fireEvent.click(screen.getByRole('button', { name: 'Copy name' }));
    await waitFor(() => expect(written).toEqual(["O'Neil"]));
  });

  it('opens_on_a_double_click_of_the_row_number', async () => {
    render(<ResultGrid result={result} />);
    act(() => glide.props!.onCellClicked([-1, 1]));
    expect(screen.queryByTestId('record-view')).toBeNull();
    act(() => glide.props!.onCellClicked([-1, 1]));
    await waitFor(() => expect(screen.getByTestId('record-view')).toBeInTheDocument());
    expect(within(screen.getByTestId('record-view')).getByText('Row 2')).toBeInTheDocument();
  });

  it('ignores_clicks_on_different_rows_or_cells', () => {
    render(<ResultGrid result={result} />);
    act(() => glide.props!.onCellClicked([-1, 1]));
    act(() => glide.props!.onCellClicked([-1, 2]));
    act(() => glide.props!.onCellClicked([0, 2]));
    expect(screen.queryByTestId('record-view')).toBeNull();
  });
});

describe('cell context menu', () => {
  const openMenu = (cell: [number, number]) => act(() => glide.props!.onCellContextMenu(cell, { preventDefault: vi.fn() }));

  it('lists_the_copy_actions_and_view_record', async () => {
    render(<ResultGrid result={result} />);
    select(range(1, 0, 1, 1));
    openMenu([1, 0]);
    const menu = screen.getByRole('menu', { name: 'Cell menu' });
    const labels = within(menu).getAllByRole('menuitem').map((b) => b.textContent);
    expect(labels).toEqual([
      'CopyCtrl+C',
      'Copy with headersCtrl+Shift+C',
      'Copy as INSERTCtrl+Alt+C',
      'Copy as CSV',
      'Copy column name',
      'View recordShift+Space',
    ]);
    fireEvent.click(within(menu).getByRole('menuitem', { name: /^Copy column name/ }));
    await waitFor(() => expect(written).toEqual(['name']));
  });

  it('copies_via_the_menu', async () => {
    render(<ResultGrid result={result} />);
    select(range(0, 1, 2, 1));
    openMenu([0, 1]);
    fireEvent.click(screen.getByRole('menuitem', { name: /^Copy as CSV/ }));
    await waitFor(() => expect(written).toEqual(['id,name\r\n20,Beta alpha']));
  });

  it('view_record_opens_the_panel', () => {
    render(<ResultGrid result={result} />);
    select(range(0, 1, 1, 1));
    openMenu([0, 1]);
    fireEvent.click(screen.getByRole('menuitem', { name: /^View record/ }));
    expect(screen.getByTestId('record-view')).toBeInTheDocument();
  });

  it('appends_the_extra_entries_with_the_cell_context', () => {
    const run = vi.fn();
    const extra = vi.fn((ctx: { row: number; col: number; value: unknown; column: string }) => [{ label: `Filter ${ctx.column}`, run }]);
    render(<ResultGrid result={result} extraCellMenu={extra} />);
    openMenu([1, 2]);
    expect(extra).toHaveBeenCalledWith({ row: 2, col: 1, value: { t: 'Text', v: "O'Neil" }, column: 'name' });
    fireEvent.click(screen.getByRole('menuitem', { name: 'Filter name' }));
    expect(run).toHaveBeenCalledOnce();
  });

  it('offers_no_column_actions_on_the_row_number', () => {
    render(<ResultGrid result={result} extraCellMenu={() => [{ label: 'Extra', run: vi.fn() }]} />);
    openMenu([-1, 0]);
    expect(screen.queryByRole('menuitem', { name: /Copy column name/ })).toBeNull();
    expect(screen.queryByRole('menuitem', { name: 'Extra' })).toBeNull();
    expect(screen.getByRole('menuitem', { name: /^View record/ })).toBeInTheDocument();
  });
});
