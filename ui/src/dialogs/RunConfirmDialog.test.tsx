import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, PlanView, UnboundedWrite } from '../bindings';
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

  it('follows the production guard layout: badge, SQL block, danger solid primary on the right', () => {
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    const dialog = screen.getByTestId('run-confirm');
    // PRODUCTION badge leads the header.
    const head = dialog.querySelector('.modal-head')!;
    expect(head.firstElementChild).toBe(screen.getByTestId('env-chip'));
    // The statements read as one SQL block.
    const sql = dialog.querySelector('.run-confirm-sql');
    expect(sql).not.toBeNull();
    expect(sql).toContainElement(screen.getAllByTestId('run-confirm-row')[0]!);
    // Cancel, then the danger primary last.
    const foot = [...dialog.querySelectorAll('.modal-foot button')];
    expect(foot.map((b) => b.getAttribute('data-testid'))).toEqual(['run-confirm-cancel', 'run-confirm-run']);
    expect(foot[1]).toHaveClass('btn', 'danger');
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

  it('production_plan_without_unbounded_writes_has_no_warning', () => {
    render(<RunConfirmDialog plan={plan} onDone={vi.fn()} />);
    expect(screen.queryByTestId('run-confirm-unbounded')).toBeNull();
  });

  it('a single statement is singular in the title and the button', () => {
    render(<RunConfirmDialog plan={prodPlan([stmt(0, 'write', 'confirm', 'DELETE FROM t')])} onDone={vi.fn()} />);
    expect(screen.getByTestId('run-confirm-summary')).toHaveTextContent(
      '1 statement will run on shop-prod (PRODUCTION). 1 needs confirmation:',
    );
    expect(screen.getByTestId('run-confirm-run')).toHaveTextContent(/^Run 1 statement$/);
  });
});

const unbounded = (op: UnboundedWrite['op'], table: string, reason: UnboundedWrite['reason'] = 'no_where'): UnboundedWrite => ({
  op,
  reason: op === 'truncate' ? 'truncate' : reason,
  table,
});

const flagged = (index: number, preview: string, ...writes: UnboundedWrite[]): PlanStmtView => ({
  ...stmt(index, preview.startsWith('TRUNCATE') ? 'ddl' : 'write', 'confirm', preview),
  unbounded: writes,
});

const localPlan = (stmts: PlanStmtView[]): PlanView => ({
  ...prodPlan(stmts),
  connection_name: 'shop-dev',
  env_tag: 'local',
});

describe('RunConfirmDialog unbounded writes', () => {
  it('warns_on_a_non_production_connection_with_run_anyway_and_cancel_focused', async () => {
    const onDone = vi.fn();
    const p = localPlan([stmt(0, 'read', 'allow', 'SELECT 1'), flagged(1, 'UPDATE orders SET paid = 1', unbounded('update', 'orders'))]);
    render(<RunConfirmDialog plan={p} onDone={onDone} />);
    const dialog = screen.getByTestId('run-confirm');
    expect(dialog).toHaveAttribute('role', 'alertdialog');
    expect(screen.getByRole('heading')).toHaveTextContent('Confirm run');
    expect(screen.queryByTestId('env-chip')).toBeNull();
    expect(dialog.className).not.toContain('env-production');
    expect(screen.getByTestId('run-confirm-summary')).toHaveTextContent('2 statements will run on shop-dev.');
    const warning = screen.getByTestId('run-confirm-unbounded');
    expect(warning).toHaveTextContent('This UPDATE has no WHERE clause and will change every row in orders.');
    const rows = screen.getAllByTestId('run-confirm-unbounded-row');
    expect(rows).toHaveLength(1);
    expect(rows[0]).toHaveTextContent('#2');
    expect(rows[0]).toHaveTextContent('UPDATE orders SET paid = 1');
    expect(rows[0]!.querySelector('code')).toHaveTextContent(/^orders$/);
    // No separate Production list off Production.
    expect(screen.queryAllByTestId('run-confirm-row')).toHaveLength(0);

    const run = screen.getByTestId('run-confirm-run');
    expect(run).toHaveTextContent(/^Run anyway$/);
    expect(run).toHaveClass('danger');
    expect(screen.getByTestId('run-confirm-cancel')).toHaveFocus();
    await userEvent.setup().keyboard('{Enter}');
    expect(onDone).toHaveBeenCalledWith(false);
    expect(callsOf('query_run')).toEqual([]);
  });

  it('run_anyway_confirms', async () => {
    const onDone = vi.fn();
    render(<RunConfirmDialog plan={localPlan([flagged(0, 'DELETE FROM t', unbounded('delete', 't'))])} onDone={onDone} />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Run anyway' }));
    expect(onDone).toHaveBeenCalledWith(true);
  });

  it('lists_every_flagged_statement_with_its_own_sentence', () => {
    const p = localPlan([
      flagged(0, 'DELETE FROM `a`', unbounded('delete', '`a`')),
      stmt(1, 'write', 'allow', 'DELETE FROM b WHERE id = 1'),
      flagged(2, 'TRUNCATE c', unbounded('truncate', 'c')),
      flagged(3, 'UPDATE d SET x = 1 WHERE 1=1', unbounded('update', 'd', 'always_true')),
      flagged(4, 'SELECT 1; DELETE FROM e; UPDATE f SET y = 2', unbounded('delete', 'e'), unbounded('update', 'f')),
    ]);
    render(<RunConfirmDialog plan={p} onDone={vi.fn()} />);
    const warning = screen.getByTestId('run-confirm-unbounded');
    expect(warning).toHaveTextContent('4 statements change every row of a table:');
    const rows = screen.getAllByTestId('run-confirm-unbounded-row');
    expect(rows.map((r) => r.querySelector('.run-confirm-no')?.textContent)).toEqual(['#1', '#3', '#4', '#5']);
    expect(rows[0]).toHaveTextContent('This DELETE has no WHERE clause and will delete every row in `a`.');
    expect(rows[1]).toHaveTextContent('This TRUNCATE will delete every row in c.');
    expect(rows[2]).toHaveTextContent('This UPDATE has a WHERE clause that is always true and will change every row in d.');
    expect(rows[3]).toHaveTextContent('This DELETE has no WHERE clause and will delete every row in e.');
    expect(rows[3]).toHaveTextContent('This UPDATE has no WHERE clause and will change every row in f.');
  });

  it('production_shows_one_dialog_with_both_the_production_list_and_the_warning', () => {
    const p = prodPlan([
      stmt(0, 'write', 'confirm', 'INSERT INTO t VALUES (1)'),
      flagged(1, 'DELETE FROM orders', unbounded('delete', 'orders')),
    ]);
    render(<RunConfirmDialog plan={p} onDone={vi.fn()} />);
    expect(screen.getAllByRole('alertdialog')).toHaveLength(1);
    expect(screen.getByRole('heading')).toHaveTextContent('Confirm run on Production');
    expect(screen.getByTestId('env-chip')).toHaveTextContent('Production');
    expect(screen.getByTestId('run-confirm')).toHaveClass('env-production');
    expect(screen.getByTestId('run-confirm-summary')).toHaveTextContent(
      '2 statements will run on shop-prod (PRODUCTION). 2 need confirmation:',
    );
    // Every Production confirmation is still listed, the unbounded one included.
    expect(screen.getAllByTestId('run-confirm-row')).toHaveLength(2);
    expect(screen.getByTestId('run-confirm-unbounded')).toHaveTextContent(
      'This DELETE has no WHERE clause and will delete every row in orders.',
    );
    expect(screen.getByTestId('run-confirm-run')).toHaveTextContent(/^Run anyway$/);
    expect(screen.getByTestId('run-confirm-cancel')).toHaveFocus();
  });

  it('a flagged statement that does not need confirmation is not listed', () => {
    // Setting off: the backend sends the flag with an Allow verdict, and no dialog is
    // asked for; should one show for another statement, the flag is not repeated.
    const p = prodPlan([
      stmt(0, 'ddl', 'confirm', 'DROP TABLE x'),
      { ...flagged(1, 'DELETE FROM t', unbounded('delete', 't')), verdict: 'allow' },
    ]);
    render(<RunConfirmDialog plan={p} onDone={vi.fn()} />);
    expect(screen.queryByTestId('run-confirm-unbounded')).toBeNull();
    expect(screen.getByTestId('run-confirm-run')).toHaveTextContent(/^Run all 2$/);
  });

  it('a missing table name reads as the table', () => {
    render(<RunConfirmDialog plan={localPlan([flagged(0, 'TRUNCATE', unbounded('truncate', ''))])} onDone={vi.fn()} />);
    expect(screen.getByTestId('run-confirm-unbounded')).toHaveTextContent('This TRUNCATE will delete every row in the table.');
  });
});
