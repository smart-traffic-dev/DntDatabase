import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, RunEvent, Value } from '../../bindings';
import { emptyRun, reduceRun, startRun, useQueryRun, type TabRun } from '../../store/queryRun';
import { mockBackend, resetStores } from '../../test/backend';
import { ResultPanel } from './ResultPanel';
import { rerunStatement } from './rerun';

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

  it('history_tab_shows_the_history_view', async () => {
    mockBackend({ history_list: () => [] });
    show(play(result(1, 0, 3)));
    expect(screen.queryByTestId('explain-view')).toBeNull();
    await userEvent.setup().click(screen.getByTestId('panel-history'));
    expect(useQueryRun.getState().get('t').panel).toBe('history');
    expect(await screen.findByTestId('history-view')).toBeInTheDocument();
    expect(screen.queryByTestId('result-summary')).toBeNull();
  });

  it('explain_event_opens_the_explain_panel', () => {
    const tree = { kind: 'table' as const, columns: ['id'], rows: [['1']], raw: 'r' };
    show(play([{ t: 'explain', index: 0, plan: tree }]));
    expect(useQueryRun.getState().get('t').panel).toBe('explain');
    expect(screen.getByTestId('explain-view')).toBeInTheDocument();
    expect(screen.getByTestId('panel-explain')).toHaveAttribute('aria-selected', 'true');
  });
});
