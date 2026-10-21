import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { Dialect, PlanNode, PlanTree, PlanView, ServerVariant } from '../../bindings';
import { useConnections } from '../../store/connections';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { config, ids, resetStores, serverInfo } from '../../test/backend';
import { ExplainView } from './ExplainView';

const explainQuery = vi.hoisted(() => vi.fn(async () => undefined));
vi.mock('../../store/runQuery', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../../store/runQuery')>()),
  explainQuery,
}));
vi.mock('../../store/completion', () => ({ invalidateCompletion: vi.fn(), forgetCompletionConnection: vi.fn() }));

const node = (label: string, est_cost: number | null, children: PlanNode[] = []): PlanNode => ({
  label,
  detail: null,
  est_cost,
  est_rows: 10,
  actual_rows: null,
  actual_ms: null,
  loops: null,
  children,
});

const TREE: PlanTree = {
  kind: 'tree',
  root: node('Hash Join', 95, [node('Seq Scan', 80, [node('Filter', 1)]), node('Index Scan', 10)]),
  raw: '[{"Plan": "raw-json"}]',
};

const planIn = (dialect: Dialect): PlanView => ({
  plan_id: 'p',
  stmts: [],
  dialect,
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle',
});

const show = (patch: Partial<typeof emptyRun>, tabId = 't') => {
  useQueryRun.setState({ runs: new Map([[tabId, { ...emptyRun, panel: 'explain', ...patch }]]) });
  return render(<ExplainView tabId={tabId} />);
};

/** A query tab on a stored connection whose server is `variant`. */
const tabOn = (variant: ServerVariant, version?: [number, number, number]) => {
  useConnections.getState().setStored([], [config(ids.prod, 'shop')]);
  useConnections.getState().setInfo(ids.prod, serverInfo(variant, version));
  return useQueryDocs.getState().newQuery(ids.prod, 'shop');
};

/** A MySQL-style analyzed leaf under a cheap root: the leaf is the hottest node. */
const LOOPED: PlanTree = {
  kind: 'tree',
  root: {
    label: 'Nested loop',
    detail: null,
    est_cost: 6100,
    est_rows: 3,
    actual_rows: 42,
    actual_ms: 419,
    loops: 1,
    children: [
      { label: 'Index lookup on r', detail: null, est_cost: 6000, est_rows: 3, actual_rows: 42, actual_ms: 0.03, loops: 4980, children: [] },
    ],
  },
  raw: '',
};

beforeEach(() => {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
  explainQuery.mockClear();
});

describe('ExplainView', () => {
  it('marks_the_hottest_node', () => {
    show({ explain: TREE });
    const nodes = screen.getAllByTestId('explain-node');
    const hot = nodes.filter((n) => n.classList.contains('hot'));
    expect(hot).toHaveLength(1);
    expect(hot[0]).toHaveTextContent('Seq Scan');
  });

  it('selects_the_hottest_node_by_default', () => {
    show({ explain: TREE });
    expect(screen.getByTestId('explain-details')).toHaveTextContent('Seq Scan');
  });

  // F04: LD-62, a MySQL plan (mysql84 fixture costs, already per subtree from the backend)
  // under an uncosted nested loop.
  it('marks_the_costliest_table_of_a_mysql_plan_not_the_query_block', () => {
    const tree: PlanTree = {
      kind: 'tree',
      root: node('Query block #1', 1.55, [node('Nested loop', null, [node('index o', 1.2), node('eq_ref c', 0.35)])]),
      raw: '{}',
    };
    show({ explain: tree, plan: planIn({ engine: 'mysql' } as Dialect) });
    const nodes = screen.getAllByTestId('explain-node');
    const hot = nodes.filter((n) => n.classList.contains('hot'));
    expect(hot).toHaveLength(1);
    expect(hot[0]).toHaveTextContent('index o');
    expect(within(nodes[0]!).getByText('1.55')).toBeInTheDocument();
    expect(within(nodes[0]!).getByTitle('0 · 0% of self cost')).toBeInTheDocument();
    expect(within(nodes[3]!).getByTitle(/^0.35 ·/)).toBeInTheDocument();
  });

  // The backend already normalised MySQL costs: the UI must not correct them again.
  it('does_not_read_mysql_costs_as_running_totals', () => {
    const tree: PlanTree = {
      kind: 'tree',
      root: node('Query block #1', 5, [node('Nested loop', null, [node('ALL b', 2), node('ALL c', 4)])]),
      raw: '{}',
    };
    show({ explain: tree, plan: planIn({ engine: 'mysql' } as Dialect) });
    const hot = screen.getAllByTestId('explain-node').filter((n) => n.classList.contains('hot'));
    expect(hot).toHaveLength(1);
    expect(hot[0]).toHaveTextContent('ALL c');
  });

  it('reads_postgres_costs_per_subtree', () => {
    const tree: PlanTree = {
      kind: 'tree',
      root: node('Merge Join', 50, [node('Sort', 20), node('Index Scan', 40)]),
      raw: '[]',
    };
    show({ explain: tree, plan: planIn({ engine: 'postgres' }) });
    const hot = screen.getAllByTestId('explain-node').filter((n) => n.classList.contains('hot'));
    expect(hot).toHaveLength(1);
    expect(hot[0]).toHaveTextContent('Index Scan');
  });

  it('a_single_node_plan_marks_nothing', () => {
    show({ explain: { kind: 'tree', root: node('Seq Scan on orders', 35.5), raw: '' } });
    expect(screen.getAllByTestId('explain-node').filter((n) => n.classList.contains('hot'))).toHaveLength(0);
  });

  it('explain_tree_collapses', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    expect(screen.getByText('Filter')).toBeInTheDocument();
    const scan = screen.getAllByTestId('explain-node').find((n) => n.textContent?.includes('Seq Scan'))!;
    await user.click(within(scan).getByRole('button', { name: /collapse/i }));
    expect(screen.queryByText('Filter')).not.toBeInTheDocument();
    expect(screen.getByText('Index Scan')).toBeInTheDocument();
    await user.click(within(scan).getByRole('button', { name: /expand/i }));
    expect(screen.getByText('Filter')).toBeInTheDocument();
  });

  it('collapses_and_expands_the_whole_tree', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    await user.click(screen.getByRole('button', { name: 'Collapse all' }));
    expect(screen.queryByText('Filter')).not.toBeInTheDocument();
    expect(screen.queryByText('Index Scan')).not.toBeInTheDocument();
    expect(within(screen.getByRole('tree')).getByText('Hash Join')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Expand all' }));
    expect(screen.getByText('Filter')).toBeInTheDocument();
    expect(screen.getByText('Index Scan')).toBeInTheDocument();
  });

  it('switches_to_the_grid_and_sorts_by_cost', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    await user.click(screen.getByRole('button', { name: 'Grid' }));
    expect(screen.getByTestId('explain-grid')).toBeInTheDocument();
    expect(screen.queryByTestId('explain-node')).not.toBeInTheDocument();

    const rows = () => screen.getAllByTestId('explain-row');
    expect(rows()[0]).toHaveTextContent('Hash Join');

    const header = screen.getByRole('columnheader', { name: /Cost/ });
    await user.click(within(header).getByRole('button'));
    expect(header).toHaveAttribute('aria-sort', 'descending');
    expect(rows()[0]).toHaveTextContent('Hash Join');

    await user.click(within(header).getByRole('button'));
    expect(header).toHaveAttribute('aria-sort', 'ascending');
    expect(rows()[0]).toHaveTextContent('Filter');
  });

  it('grid_filter_shows_matches_only_and_hash_restores_plan_order', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    await user.click(screen.getByRole('button', { name: 'Grid' }));
    const rows = () => screen.getAllByTestId('explain-row').map((r) => r.textContent);

    const cost = screen.getByRole('columnheader', { name: /Cost/ });
    await user.click(within(cost).getByRole('button'));
    await user.click(within(cost).getByRole('button'));
    expect(rows()[0]).toContain('Filter');

    const plan = screen.getByRole('columnheader', { name: /#/ });
    await user.click(within(plan).getByRole('button'));
    expect(plan).toHaveAttribute('aria-sort', 'ascending');
    expect(cost).toHaveAttribute('aria-sort', 'none');
    expect(rows()[0]).toContain('Hash Join');

    await user.type(screen.getByRole('textbox', { name: 'Filter plan nodes' }), 'scan');
    expect(rows()).toHaveLength(2);
    expect(screen.getAllByText('Scan', { selector: 'mark' })).toHaveLength(2);
  });

  it('keeps_selection_and_collapse_across_views', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    const scan = screen.getAllByTestId('explain-node').find((n) => n.textContent?.includes('Seq Scan'))!;
    await user.click(within(scan).getByRole('button', { name: /collapse/i }));
    const index = screen.getAllByTestId('explain-node').find((n) => n.textContent?.includes('Index Scan'))!;
    index.focus();
    await user.keyboard('{Enter}');
    expect(screen.getByTestId('explain-details')).toHaveTextContent('Index Scan');

    await user.click(screen.getByRole('button', { name: 'Grid' }));
    expect(screen.getAllByTestId('explain-row').find((r) => r.classList.contains('sel'))).toHaveTextContent(
      'Index Scan',
    );
    await user.click(screen.getByRole('button', { name: 'Tree' }));
    expect(screen.queryByText('Filter')).not.toBeInTheDocument();
    expect(screen.getByTestId('explain-details')).toHaveTextContent('Index Scan');
  });

  it('labels_warning_pills_for_screen_readers', () => {
    const scan: PlanNode = { ...node('Seq Scan on big', 10), est_rows: 20_000 };
    show({ explain: { kind: 'tree', root: scan, raw: '{}' } });
    expect(screen.getAllByRole('img', { name: '1 warning' }).length).toBeGreaterThan(0);
  });

  it('filters_the_tree_and_keeps_ancestors', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    const input = screen.getByRole('textbox', { name: 'Filter plan nodes' });
    await user.type(input, 'Filter');
    const tree = screen.getByRole('tree');
    expect(within(tree).getByText('Filter')).toBeInTheDocument();
    expect(within(tree).getByText('Seq Scan')).toBeInTheDocument();
    expect(within(tree).getByText('Hash Join')).toBeInTheDocument();
    expect(within(tree).queryByText('Index Scan')).not.toBeInTheDocument();

    await user.clear(input);
    await user.type(input, 'no such node');
    expect(screen.getByText(/No nodes match/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Clear filter' }));
    expect(screen.getByText('Index Scan')).toBeInTheDocument();
  });

  it('shows_the_overview, warnings and the inspector', async () => {
    const scan: PlanNode = {
      label: 'Seq Scan on orders',
      detail: 'Filter: (status = paid)',
      est_cost: 120,
      est_rows: 50_000,
      actual_rows: 1_200,
      actual_ms: 240.5,
      loops: 1_500,
      children: [],
    };
    show({ explain: { kind: 'tree', root: scan, raw: '{}' } });

    expect(screen.getByTestId('explain-overview')).toHaveTextContent('Total cost');
    expect(screen.getByTestId('explain-overview')).toHaveTextContent('240.5 ms');
    expect(screen.getByTestId('explain-overview')).toHaveTextContent('⚠ 3');

    const details = screen.getByTestId('explain-details');
    expect(details).toHaveTextContent('Full table scan');
    expect(details).toHaveTextContent('Estimate skew 42×');
    expect(details).toHaveTextContent('High loop count');
    expect(details).toHaveTextContent('Filter: (status = paid)');
    expect(screen.getAllByTestId('explain-warning').length).toBeGreaterThan(0);
  });

  it('selects_a_node_from_the_cost_bar', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    await user.click(screen.getByRole('button', { name: /^Index Scan: self cost/ }));
    expect(screen.getByTestId('explain-details')).toHaveTextContent('Index Scan');
  });

  it('raw_toggle_shows_raw', async () => {
    const user = userEvent.setup();
    show({ explain: TREE });
    expect(screen.queryByTestId('explain-raw')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Raw' }));
    expect(screen.getByTestId('explain-raw')).toHaveTextContent('raw-json');
    expect(screen.queryByTestId('explain-node')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Raw' }));
    expect(screen.getAllByTestId('explain-node').length).toBeGreaterThan(0);
  });

  it('copies_the_raw_plan', async () => {
    const user = userEvent.setup();
    const writeText = vi.spyOn(navigator.clipboard, 'writeText');
    show({ explain: TREE });
    await user.click(screen.getByRole('button', { name: 'Copy plan' }));
    expect(writeText).toHaveBeenCalledWith('[{"Plan": "raw-json"}]');
  });

  it('table_plan_for_mysql57_defaults_to_the_grid', async () => {
    const user = userEvent.setup();
    show({
      explain: {
        kind: 'table',
        columns: ['id', 'select_type', 'table'],
        rows: [
          ['1', 'SIMPLE', 'users'],
          ['1', 'SIMPLE', 'orders'],
        ],
        raw: 'raw-table',
      },
    });
    expect(screen.getByRole('button', { name: 'Grid' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.queryByRole('button', { name: 'Tree' })).not.toBeInTheDocument();

    const table = screen.getByTestId('explain-table');
    expect(within(table).getAllByRole('columnheader').map((h) => h.textContent)).toEqual([
      '#',
      'id',
      'select_type',
      'table',
    ]);
    expect(within(table).getAllByRole('row')).toHaveLength(3);
    expect(within(table).getByText('orders')).toBeInTheDocument();
    expect(screen.queryByTestId('explain-node')).not.toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Raw' }));
    expect(screen.getByTestId('explain-raw')).toHaveTextContent('raw-table');
    await user.click(screen.getByRole('button', { name: 'Raw' }));
    expect(screen.getByTestId('explain-table')).toBeInTheDocument();
  });

  it('ddl_explain_shows_not_available', () => {
    show({ explain: null, explainError: 'Explain is not available for this statement.' });
    expect(screen.getByTestId('explain-view')).toHaveTextContent('Explain is not available for this statement.');
    expect(screen.queryByTestId('explain-node')).not.toBeInTheDocument();
  });

  it('shows_a_hint_before_the_first_explain', () => {
    show({ explain: null });
    expect(screen.getByTestId('explain-view')).toHaveTextContent('Ctrl+Shift+E');
  });

  it('only_the_hottest_self_cost_bar_is_amber', () => {
    show({ explain: TREE });
    const bars = screen.getAllByTestId('explain-node').map((n) => ({
      text: n.textContent ?? '',
      fill: (n.querySelector('.ex-selfbar') as HTMLElement).style.background,
    }));
    for (const b of bars) {
      expect(b.fill).toBe(b.text.includes('Seq Scan') ? 'var(--dnt-status-warning-solid)' : 'var(--dnt-accent)');
    }
    const segments = within(screen.getByRole('group', { name: 'Self cost distribution' })).getAllByRole('button');
    expect(segments[0]).toHaveAccessibleName(/^Seq Scan/);
    expect(segments[0]!.style.background).toBe('var(--dnt-status-warning-solid)');
    expect(segments[1]!.style.background).toContain('var(--dnt-accent)');
  });

  it('inspector_shows_time_per_loop_and_in_total_on_mysql', () => {
    const tab = tabOn('MySql8');
    show({ explain: LOOPED }, tab);
    const details = screen.getByTestId('explain-details');
    expect(details).toHaveTextContent('Time per loop0.03 ms');
    expect(details).toHaveTextContent('Time in total≈ 149.4 ms');
    expect(details).toHaveTextContent('35.7%');
    expect(details).toHaveTextContent('Rows in total209,160');
    expect(screen.getByTestId('explain-foot')).toHaveTextContent('149.4 ms');
  });

  it('inspector_shows_totals_only_on_mariadb', () => {
    const tab = tabOn('MariaDb', [11, 4, 2]);
    show({ explain: LOOPED }, tab);
    const details = screen.getByTestId('explain-details');
    expect(details).not.toHaveTextContent('Time per loop');
    expect(details).toHaveTextContent('Time in total0.03 ms');
    expect(details).not.toHaveTextContent('Rows in total');
  });

  it('table_plan_tints_known_slow_values', () => {
    show({
      explain: {
        kind: 'table',
        columns: ['id', 'type', 'Extra'],
        rows: [
          ['1', 'ALL', 'Using where; Using filesort'],
          ['1', 'ref', 'Using where'],
        ],
        raw: '',
      },
    });
    const table = screen.getByTestId('explain-table');
    const slow = within(table).getAllByTestId('explain-slow');
    expect(slow.map((s) => [s.textContent, s.getAttribute('title')])).toEqual([
      ['ALL', 'Full table scan'],
      ['Using filesort', 'Sorts without an index'],
    ]);
    expect(slow[1]!.closest('td')).toHaveTextContent('Using where; Using filesort');
  });

  it('empty_state_offers_explain_and_explain_analyze', async () => {
    const user = userEvent.setup();
    const tab = tabOn('MySql8');
    show({ explain: null }, tab);
    expect(screen.getByTestId('explain-view')).toHaveTextContent('No plan yet for the statement at the cursor.');
    await user.click(screen.getByRole('button', { name: /^Explain\b(?! Analyze)/ }));
    expect(explainQuery).toHaveBeenLastCalledWith(tab, false);
    await user.click(screen.getByRole('button', { name: 'Explain Analyze' }));
    expect(explainQuery).toHaveBeenLastCalledWith(tab, true);
  });

  it('analyze_buttons_are_disabled_where_analyze_is_not_supported', () => {
    const tab = tabOn('MySql8', [8, 0, 12]);
    show({ explain: null }, tab);
    const analyze = screen.getByRole('button', { name: 'Explain Analyze' });
    expect(analyze).toBeDisabled();
    expect(analyze).toHaveAttribute('title', 'Explain Analyze needs MySQL 8.0.18 or later');
    expect(screen.getByRole('button', { name: /^Explain\b(?! Analyze)/ })).toBeEnabled();
  });

  it('empty_state_buttons_are_disabled_without_a_connection', () => {
    show({ explain: null });
    expect(screen.getByRole('button', { name: 'Explain Analyze' })).toBeDisabled();
    expect(screen.getByRole('button', { name: /^Explain\b(?! Analyze)/ })).toBeDisabled();
  });

  it('not_analyzed_overview_runs_explain_analyze', async () => {
    const user = userEvent.setup();
    const tab = tabOn('Postgres', [16, 0, 0]);
    show({ explain: TREE }, tab);
    expect(screen.getByTestId('explain-overview')).toHaveTextContent('Not analyzed');
    await user.click(screen.getByRole('button', { name: 'Run Explain Analyze' }));
    expect(explainQuery).toHaveBeenLastCalledWith(tab, true);
  });

  it('an_analyzed_plan_has_no_run_explain_analyze_button', () => {
    const tab = tabOn('MySql8');
    show({ explain: LOOPED }, tab);
    expect(screen.queryByRole('button', { name: 'Run Explain Analyze' })).not.toBeInTheDocument();
  });
});
