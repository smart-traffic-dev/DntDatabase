import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, PlanView } from '../bindings';
import { callsOf } from '../test/backend';
import { RunConfirmDialog } from './RunConfirmDialog';

const stmt = (index: number, kind: PlanStmtView['kind'], verdict: PlanStmtView['verdict'], preview: string): PlanStmtView => ({
  index,
  range: [0, 1],
  kind,
  verdict,
  preview,
});

const prodPlan = (stmts: PlanStmtView[]): PlanView => ({
  plan_id: 'p1',
  stmts,
  dialect: { engine: 'mysql' } as PlanView['dialect'],
  connection_name: 'shop-prod',
  env_tag: 'production',
  read_only: false,
  tx_state: 'idle',
});

const plan = prodPlan([
  stmt(0, 'read', 'allow', 'SELECT 1'),
  stmt(1, 'write', 'confirm', 'UPDATE t SET a = 1'),
  stmt(2, 'ddl', 'confirm', 'DROP TABLE t'),
  stmt(3, 'other', 'confirm', 'LOCK TABLES t WRITE'),
]);

describe('RunConfirmDialog', () => {
  it('lists_confirm_statements_with_labels', () => {
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    const dialog = screen.getByTestId('run-confirm');
    expect(screen.getByTestId('run-confirm-summary')).toHaveTextContent(
      '4 statements will run on shop-prod (PRODUCTION). 3 need confirmation:',
    );
    expect(screen.getByTestId('env-chip')).toHaveTextContent('Production');
    expect(dialog.querySelector('strong')).toHaveTextContent('shop-prod');
    const rows = dialog.querySelectorAll('[data-testid="run-confirm-row"]');
    expect(rows).toHaveLength(3);
    expect(rows[0]).toHaveTextContent('#2');
    expect(rows[0]).toHaveTextContent('WRITE');
    expect(rows[0]).toHaveTextContent('UPDATE t SET a = 1');
    expect(rows[1]).toHaveTextContent('#3');
    expect(rows[1]).toHaveTextContent('DDL');
    expect(rows[2]).toHaveTextContent('#4');
    expect(rows[2]).toHaveTextContent('OTHER');
    expect(dialog.className).toContain('env-production');
  });

  it('cancel_is_default_focus_and_runs_nothing', async () => {
    const onDone = vi.fn();
    render(<RunConfirmDialog plan={plan} onDone={onDone} />);
    expect(screen.getByTestId('run-confirm-cancel')).toHaveFocus();
    await userEvent.setup().keyboard('{Enter}');
    expect(onDone).toHaveBeenCalledWith(false);
    expect(callsOf('query_run')).toEqual([]);
  });

  it('escape_cancels', async () => {
    const onDone = vi.fn();
    render(<RunConfirmDialog plan={plan} onDone={onDone} />);
    await userEvent.setup().keyboard('{Escape}');
    expect(onDone).toHaveBeenCalledWith(false);
  });

  it('ddl_row_is_red', () => {
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    const rows = screen.getAllByTestId('run-confirm-row');
    expect(rows[1]).toHaveClass('danger');
    expect(rows[0]).not.toHaveClass('danger');
  });

  it('run_button_counts_statements', async () => {
    const onDone = vi.fn();
    render(<RunConfirmDialog plan={plan} onDone={onDone} />);
    const run = screen.getByTestId('run-confirm-run');
    expect(run).toHaveTextContent(/^Run all 4$/);
    await userEvent.setup().click(run);
    expect(onDone).toHaveBeenCalledWith(true);
  });

  it('a single statement is singular in the title and the button', () => {
    render(<RunConfirmDialog plan={prodPlan([stmt(0, 'write', 'confirm', 'DELETE FROM t')])} onDone={vi.fn()} />);
    expect(screen.getByTestId('run-confirm-summary')).toHaveTextContent(
      '1 statement will run on shop-prod (PRODUCTION). 1 needs confirmation:',
    );
    expect(screen.getByTestId('run-confirm-run')).toHaveTextContent(/^Run 1 statement$/);
  });
});
