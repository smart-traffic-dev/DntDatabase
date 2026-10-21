import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, PlanView } from '../bindings';
import { RunConfirmDialog } from './RunConfirmDialog';

const stmt = (index: number, preview: string, extra: Partial<PlanStmtView> = {}): PlanStmtView => ({
  index,
  range: [0, 1],
  kind: 'write',
  verdict: 'confirm',
  preview,
  ...extra,
});

const planOf = (stmts: PlanStmtView[], env: PlanView['env_tag'] = 'production'): PlanView => ({
  plan_id: 'p1',
  stmts,
  dialect: { engine: 'mysql' } as PlanView['dialect'],
  connection_name: 'shop-prod',
  env_tag: env,
  read_only: false,
  tx_state: 'idle',
});

describe('RunConfirmDialog typed names, estimates and implicit commits', () => {
  it('typed_name_gates_run_button', async () => {
    const onDone = vi.fn();
    const plan = planOf([
      stmt(0, 'TRUNCATE orders', { confirm_name: 'orders' }),
      stmt(1, 'DROP DATABASE shop', { kind: 'ddl', confirm_name: 'shop', drops: [{ kind: 'database', name: 'shop' }] }),
      stmt(2, 'DELETE FROM orders', { confirm_name: 'orders' }),
    ]);
    render(<RunConfirmDialog plan={plan} onDone={onDone} />);
    const run = screen.getByTestId('run-confirm-run');
    // One box per distinct name, in statement order.
    const boxes = screen.getAllByRole('textbox');
    expect(boxes).toHaveLength(2);
    expect(screen.getByLabelText('Type orders to confirm')).toBe(boxes[0]);
    expect(screen.getByLabelText('Type shop to confirm')).toBe(boxes[1]);
    expect(run).toBeDisabled();
    const user = userEvent.setup();
    await user.type(boxes[0]!, 'orders');
    expect(run).toBeDisabled();
    await user.type(boxes[1]!, 'SHOP');
    expect(run).toBeDisabled();
    await user.clear(boxes[1]!);
    await user.type(boxes[1]!, 'shop');
    expect(run).toBeEnabled();
    await user.click(run);
    expect(onDone).toHaveBeenCalledWith(true, ['orders', 'shop']);
  });

  it('Enter in a name box neither runs nor silently cancels: the dialog stays', async () => {
    const onDone = vi.fn();
    render(<RunConfirmDialog plan={planOf([stmt(0, 'TRUNCATE orders', { confirm_name: 'orders' })])} onDone={onDone} />);
    await userEvent.setup().type(screen.getByLabelText('Type orders to confirm'), 'orders{Enter}');
    expect(onDone).not.toHaveBeenCalled();
    expect(screen.getByRole('alertdialog')).toBeInTheDocument();
  });

  it('a_plan_without_names_has_no_boxes_and_runs_at_once', async () => {
    const onDone = vi.fn();
    render(<RunConfirmDialog plan={planOf([stmt(0, 'UPDATE t SET a = 1')])} onDone={onDone} />);
    expect(screen.queryByRole('textbox')).toBeNull();
    await userEvent.setup().click(screen.getByTestId('run-confirm-run'));
    expect(onDone).toHaveBeenCalledWith(true);
  });

  it('estimate_is_shown', () => {
    const plan = planOf([
      stmt(0, 'DELETE FROM orders', {
        unbounded: [{ op: 'delete', reason: 'no_where', table: 'orders' }],
        estimate: { kind: 'rows', n: 12345 },
      }),
      stmt(1, 'UPDATE users SET a = 1', {
        unbounded: [{ op: 'update', reason: 'no_where', table: 'users' }],
        estimate: { kind: 'unavailable' },
      }),
    ]);
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    expect(screen.getAllByText('≈ 12,345 rows')).toHaveLength(1);
    expect(screen.getAllByText('Row count unavailable')).toHaveLength(1);
  });

  it('estimate_is_shown_off_production_too', () => {
    const plan = planOf(
      [
        stmt(0, 'DELETE FROM orders', {
          unbounded: [{ op: 'delete', reason: 'no_where', table: 'orders' }],
          estimate: { kind: 'rows', n: 1 },
        }),
      ],
      'staging',
    );
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    expect(screen.getAllByText('≈ 1 row')).toHaveLength(1);
  });

  it('implicit_commit_warning', () => {
    const plan = planOf([stmt(0, 'ALTER TABLE t ADD c INT', { kind: 'ddl', implicit_commit: true })], 'staging');
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    expect(screen.getByText('This statement commits the open transaction.')).toBeInTheDocument();
  });

  it('an_allowed_implicit_commit_statement_shows_the_warning', () => {
    const plan = planOf([stmt(0, 'ALTER TABLE t ADD c INT', { kind: 'ddl', verdict: 'allow', implicit_commit: true })], 'local');
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    expect(screen.getByText('This statement commits the open transaction.')).toBeInTheDocument();
  });

  it('explains_a_drop', () => {
    const plan = planOf(
      [stmt(0, 'DROP DATABASE shop', { kind: 'ddl', drops: [{ kind: 'database', name: 'shop' }] })],
      'local',
    );
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    expect(screen.getByTestId('run-confirm-drops')).toHaveTextContent('Drops database shop');
  });
});
