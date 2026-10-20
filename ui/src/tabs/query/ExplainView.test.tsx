import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import type { PlanNode, PlanTree } from '../../bindings';
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
