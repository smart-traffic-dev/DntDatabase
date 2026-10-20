import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { Dialect, PlanNode, PlanTree, PlanView } from '../../bindings';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { resetStores } from '../../test/backend';
import { ExplainView } from './ExplainView';

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

const show = (patch: Partial<typeof emptyRun>) => {
  useQueryRun.setState({ runs: new Map([['t', { ...emptyRun, panel: 'explain', ...patch }]]) });
  return render(<ExplainView tabId="t" />);
};

beforeEach(() => {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
});

describe('ExplainView', () => {
  it('marks_the_hottest_node', () => {
    show({ explain: TREE });
    const nodes = screen.getAllByTestId('explain-node');
    const hot = nodes.filter((n) => n.classList.contains('hot'));
    expect(hot).toHaveLength(1);
    expect(hot[0]).toHaveTextContent('Seq Scan');
  });

  // F04: LD-62, a MySQL plan with cumulative `prefix_cost`s under an uncosted nested loop.
  it('marks_the_costliest_table_of_a_mysql_plan_not_the_query_block', () => {
    const tree: PlanTree = {
      kind: 'tree',
      root: node('Query block #1', 0.9, [node('Nested loop', null, [node('index o', 0.55), node('eq_ref c', 0.9)])]),
      raw: '{}',
    };
    show({ explain: tree, plan: planIn({ engine: 'mysql' } as Dialect) });
    const nodes = screen.getAllByTestId('explain-node');
    const hot = nodes.filter((n) => n.classList.contains('hot'));
    expect(hot).toHaveLength(1);
    expect(hot[0]).toHaveTextContent('index o');
    expect(nodes[0]).toHaveTextContent('cost 0.90 · self 0');
    expect(nodes[3]).toHaveTextContent('cost 0.90 · self 0.35');
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

  it('table_plan_for_mysql57', () => {
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
    const table = screen.getByTestId('explain-table');
    expect(within(table).getAllByRole('columnheader').map((h) => h.textContent)).toEqual([
      'id',
      'select_type',
      'table',
    ]);
    expect(within(table).getAllByRole('row')).toHaveLength(3);
    expect(within(table).getByText('orders')).toBeInTheDocument();
    expect(screen.queryByTestId('explain-node')).not.toBeInTheDocument();
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
});
