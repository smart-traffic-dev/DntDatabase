import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { backendError } from '../test/backend';
import { TxCloseDialog } from './TxCloseDialog';

describe('TxCloseDialog', () => {
  it('offers Commit, Rollback and Cancel', async () => {
    const onSettle = vi.fn(async () => {});
    const onDone = vi.fn();
    render(<TxCloseDialog title="Query 1" onSettle={onSettle} onDone={onDone} />);
    expect(screen.getByRole('alertdialog')).toHaveTextContent('This tab has an open transaction.');
    expect(screen.getByRole('alertdialog')).toHaveTextContent('Query 1');
    await userEvent.setup().click(screen.getByRole('button', { name: 'Commit' }));
    expect(onSettle).toHaveBeenCalledWith('Commit');
    expect(onDone).toHaveBeenCalledWith(true);
  });

  it('names the connection and Production, and defaults to Cancel with a non-destructive Rollback', async () => {
    const onDone = vi.fn();
    render(<TxCloseDialog title="Query 1" connectionName="shop-prod" production onSettle={vi.fn()} onDone={onDone} />);
    expect(screen.getByRole('alertdialog')).toHaveTextContent('open transaction on shop-prod (PRODUCTION)');
    expect(screen.getByTestId('env-chip')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Cancel' })).toHaveFocus();
    expect(screen.getByRole('button', { name: 'Rollback' })).not.toHaveClass('danger');
    await userEvent.setup().keyboard('{Enter}');
    expect(onDone).toHaveBeenCalledWith(false);
  });

  it('Rollback settles with Rollback', async () => {
    const onSettle = vi.fn(async () => {});
    const onDone = vi.fn();
    render(<TxCloseDialog title="Query 1" onSettle={onSettle} onDone={onDone} />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Rollback' }));
    expect(onSettle).toHaveBeenCalledWith('Rollback');
    expect(onDone).toHaveBeenCalledWith(true);
  });

  it('Cancel and Escape settle nothing', async () => {
    const onSettle = vi.fn(async () => {});
    const onDone = vi.fn();
    render(<TxCloseDialog title="Query 1" onSettle={onSettle} onDone={onDone} />);
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    await user.keyboard('{Escape}');
    expect(onSettle).not.toHaveBeenCalled();
    expect(onDone.mock.calls).toEqual([[false], [false]]);
  });

  it('a failure stays open and shows the error', async () => {
    const onSettle = vi.fn(async () => {
      throw backendError('connection_lost');
    });
    const onDone = vi.fn();
    render(<TxCloseDialog title="Query 1" onSettle={onSettle} onDone={onDone} />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Commit' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(backendError('connection_lost').message);
    expect(onDone).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Commit' })).toBeEnabled();
  });
});
