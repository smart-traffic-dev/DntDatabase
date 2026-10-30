import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, RunEvent, Value } from '../../bindings';
import { emptyRun, reduceRun, startRun, useQueryRun, type TabRun } from '../../store/queryRun';
import { appError, resetStores } from '../../test/backend';
import { ResultPanel, resultLabel } from './ResultPanel';
import { PLAN_GONE, rerunStatement } from './rerun';

vi.mock('./rerun', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./rerun')>()),
  rerunStatement: vi.fn(async () => {}),
}));

const stmt = (index: number, kind: PlanStmtView['kind']): PlanStmtView => ({
  index,
  range: [index * 10, index * 10 + 8],
  kind,
  verdict: 'allow',
  preview: `S${index}`,
});

const PLAN = {
  plan_id: 'p1',
  stmts: [stmt(0, 'read'), stmt(1, 'read'), stmt(2, 'write')],
  dialect: { engine: 'postgres' as const },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle' as const,
};

const col = { name: 'a', db_type: 'int4', logical: 'Integer' as const, nullable: true, origin_table: null, origin_column: null };
const rowsOf = (n: number): Value[][] => Array.from({ length: n }, (_, i) => [{ t: 'Int', v: i }]);

function play(events: RunEvent[], finish = true): TabRun {
  let run = startRun(emptyRun, PLAN, 0);
  events.forEach((ev, i) => (run = reduceRun(run, ev, i + 1)));
  if (finish && run.phase.kind !== 'idle') run = reduceRun(run, { t: 'done', stop: null, elapsed_ms: 1 }, 99);
  return run;
}

/** One finished result of statement `index`. */
const result = (id: number, index: number, rows: number, truncated = false, affected: number | null = null): RunEvent[] => [
  { t: 'statement_start', index, range: [index * 10, index * 10 + 8] },
  { t: 'result_start', result_id: id, index, columns: [col] },
  { t: 'rows', result_id: id, rows: rowsOf(rows) },
  { t: 'result_end', result_id: id, row_count: rows, affected, truncated, elapsed_ms: 5 },
];

const show = (run: TabRun) => {
  useQueryRun.setState({ runs: new Map([['t', run]]) });
  return render(<ResultPanel tabId="t" />);
};

describe('ResultPanel', () => {
  beforeEach(() => {
    resetStores();
    vi.mocked(rerunStatement).mockClear();
  });

  it('result_tabs_named_after_their_table', () => {
    const r = (columns: { origin_table: { name: string } | null }[]) =>
      ({ columns: columns.map((c) => ({ ...col, ...c })) }) as unknown as Parameters<typeof resultLabel>[0];
    const orders = { origin_table: { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const } };
    expect(resultLabel(r([orders, orders]), 'SELECT a, b FROM x', 1)).toBe('orders');
    expect(resultLabel(r([{ origin_table: null }]), 'SELECT * FROM `shop`.`orders` o JOIN items i', 2)).toBe('orders');
    expect(resultLabel(r([{ origin_table: null }]), 'select * from public."Order Lines"', 3)).toBe('Order Lines');
    expect(resultLabel(r([{ origin_table: null }]), 'SELECT 1', 4)).toBe('Result 4');
  });

  it('result_tabs_named_result_n', async () => {
    show(play([...result(1, 0, 3), ...result(2, 1, 7)]));
    expect(screen.getByTestId('result-tab-1')).toHaveTextContent('Result 1');
    expect(screen.getByTestId('result-tab-2')).toHaveTextContent('Result 2');
    expect(screen.getByTestId('result-summary')).toHaveTextContent('3 rows');
    await userEvent.setup().click(screen.getByTestId('result-tab-2'));
    expect(screen.getByTestId('result-summary')).toHaveTextContent('7 rows');
    expect(screen.getByTestId('glide-mock')).toBeInTheDocument();
  });

  it('summary_shows_affected', () => {
    show(play(result(1, 0, 0, false, 4)));
    expect(screen.getByTestId('result-summary')).toHaveTextContent('4 rows affected');
  });

  it('truncated_banner_with_fetch_all_for_read', async () => {
    show(play(result(1, 0, 20, true)));
    expect(screen.getByText('Showing first 20 rows')).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Fetch all' }));
    expect(rerunStatement).toHaveBeenCalledWith('t', 0, { maxRows: 1_000_000 });
  });

  it('fetch_all_disabled_while_running_and_hidden_at_cap', () => {
    const running = play([...result(1, 0, 20, true), { t: 'statement_start', index: 1, range: [10, 18] }], false);
    const { unmount } = show(running);
    expect(screen.getByRole('button', { name: 'Fetch all' })).toBeDisabled();
    unmount();
    const capped = play(result(1, 0, 20, true));
    capped.results[0]!.rowCount = 1_000_000;
    show(capped);
    expect(screen.queryByRole('button', { name: 'Fetch all' })).toBeNull();
  });

  it('no_fetch_all_for_write', () => {
    show(play(result(1, 2, 20, true)));
    expect(screen.getByText('Showing first 20 rows')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Fetch all' })).toBeNull();
  });

  it('no_banner_when_not_truncated', () => {
    show(play(result(1, 0, 20)));
    expect(screen.queryByText(/Showing first/)).toBeNull();
    expect(screen.queryByRole('button', { name: 'Fetch all' })).toBeNull();
  });

  it('memory_warning_over_100k', () => {
    const big = play([
      { t: 'result_start', result_id: 1, index: 0, columns: [col] },
      { t: 'result_end', result_id: 1, row_count: 0, affected: null, truncated: false, elapsed_ms: 1 },
    ]);
    big.results[0]!.rowCount = 100_001;
    show(big);
    expect(screen.getByText('Large result: 100001 rows are kept in memory.')).toBeInTheDocument();
  });

  it('no_memory_warning_at_100k', () => {
    const run = play(result(1, 0, 1));
    run.results[0]!.rowCount = 100_000;
    show(run);
    expect(screen.queryByText(/Large result/)).toBeNull();
  });

  it('messages_panel_switch', async () => {
    show(play([...result(1, 0, 3), { t: 'done', stop: null, elapsed_ms: 9 }]));
    await userEvent.setup().click(screen.getByTestId('panel-messages'));
    expect(useQueryRun.getState().get('t').panel).toBe('messages');
    expect(screen.getAllByTestId('message-line').length).toBeGreaterThan(0);
    act(() => useQueryRun.getState().setPanel('t', 'result'));
    expect(screen.getByTestId('result-summary')).toBeInTheDocument();
  });

  it('has_no_history_tab_history_lives_in_the_right_sidebar', () => {
    show(play(result(1, 0, 3)));
    expect(screen.queryByTestId('panel-history')).toBeNull();
    expect(screen.queryByTestId('history-view')).toBeNull();
    expect(screen.getAllByRole('tab').map((t) => t.textContent)).toEqual(['Result 1', 'Messages']);
  });

  it('explain_event_opens_the_explain_panel', () => {
    const tree = { kind: 'table' as const, columns: ['id'], rows: [['1']], raw: 'r' };
    show(play([{ t: 'explain', index: 0, plan: tree }]));
    expect(useQueryRun.getState().get('t').panel).toBe('explain');
    expect(screen.getByTestId('explain-view')).toBeInTheDocument();
    expect(screen.getByTestId('panel-explain')).toHaveAttribute('aria-selected', 'true');
  });

  it('many_result_tabs_scroll_inside_a_strip_and_the_fixed_tabs_stay_outside', () => {
    const events = Array.from({ length: 41 }, (_, i) => result(i + 1, 0, 1)).flat();
    show(play(events));
    const strip = screen.getByTestId('result-tabs');
    expect(strip).toContainElement(screen.getByTestId('result-tab-41'));
    expect(strip).not.toContainElement(screen.getByTestId('panel-messages'));
    expect(screen.getByRole('tablist')).toContainElement(screen.getByTestId('panel-messages'));
  });

  it('messages_tab_is_badged_with_the_error_count', () => {
    const err = { t: 'statement_error', index: 1, error: appError('query', 'boom') } as RunEvent;
    const { unmount } = show(play([...result(1, 0, 1), err]));
    expect(screen.getByTestId('messages-badge')).toHaveTextContent('1');
    expect(screen.getByTestId('panel-messages')).toHaveAccessibleName('Messages (1 error)');
    unmount();
    show(play(result(1, 0, 1)));
    expect(screen.queryByTestId('messages-badge')).toBeNull();
  });

  it('a_run_that_stops_on_an_error_after_a_result_switches_to_messages', () => {
    const err = { t: 'statement_error', index: 1, error: appError('query', 'boom') } as RunEvent;
    const running = play([...result(1, 0, 1), err], false);
    show(running);
    expect(useQueryRun.getState().get('t').panel).toBe('result');
    act(() =>
      useQueryRun.setState({
        runs: new Map([['t', reduceRun(running, { t: 'done', stop: { kind: 'error', index: 1 }, elapsed_ms: 5 }, 50)]]),
      }),
    );
    expect(useQueryRun.getState().get('t').panel).toBe('messages');
    expect(screen.getByTestId('messages')).toBeInTheDocument();
  });

  it('a_finished_error_run_is_not_forced_back_to_messages_on_mount', () => {
    const err = { t: 'statement_error', index: 1, error: appError('query', 'boom') } as RunEvent;
    let run = play([...result(1, 0, 1), err], false);
    run = reduceRun(run, { t: 'done', stop: { kind: 'error', index: 1 }, elapsed_ms: 5 }, 50);
    show(run);
    expect(useQueryRun.getState().get('t').panel).toBe('result');
  });

  it('a_stop_by_the_user_is_stated_on_the_result', () => {
    const running = play(result(1, 0, 1), false);
    show(reduceRun(running, { t: 'done', stop: { kind: 'cancelled' }, elapsed_ms: 3093 }, 50));
    expect(screen.getByTestId('result-cancelled')).toHaveTextContent('Cancelled by user after 3.1 s');
  });

  it('fetch_all_is_disabled_with_a_tooltip_when_the_plan_is_gone', () => {
    show({ ...play(result(1, 0, 20, true)), planExpired: true });
    const btn = screen.getByRole('button', { name: 'Fetch all' });
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute('title', PLAN_GONE);
  });
});
