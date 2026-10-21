import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import type { PlanView } from '../bindings';
import { callsOf } from '../test/backend';
import { RunBlockedDialog } from './RunBlockedDialog';

const plan: PlanView = {
  plan_id: 'p1',
  stmts: [
    { index: 0, range: [0, 8], kind: 'read', verdict: 'allow', preview: 'SELECT 1' },
    { index: 1, range: [9, 20], kind: 'write', verdict: 'block', preview: 'DELETE FROM t' },
  ],
  dialect: { engine: 'mysql' } as PlanView['dialect'],
  connection_name: 'reports',
  env_tag: null,
  read_only: true,
  tx_state: 'idle',
};

describe('RunBlockedDialog', () => {
  it('blocked_dialog_only_close_and_no_run', async () => {
    const onClose = vi.fn();
    render(<RunBlockedDialog plan={plan} onClose={onClose} />);
    const dialog = screen.getByTestId('run-blocked');
    expect(dialog).toHaveTextContent('Blocked on read-only connection');
    expect(dialog).toHaveTextContent('reports is read-only. These statements were not run:');
    expect(dialog).toHaveTextContent('DELETE FROM t');
    expect(dialog).not.toHaveTextContent('SELECT 1');
    // The footer offers Close only (the header × closes too).
    expect(within(dialog.querySelector('.modal-foot') as HTMLElement).getAllByRole('button')).toHaveLength(1);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Close' }));
    expect(onClose).toHaveBeenCalled();
    expect(callsOf('query_run')).toEqual([]);
  });
});
