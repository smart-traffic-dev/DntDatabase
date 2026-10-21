import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../../store/connections';
import { emptyRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { useTxMode } from '../../store/txMode';
import { callsOf, config, fail, ids, mockBackend, resetStores } from '../../test/backend';
import { QueryToolbar } from './QueryToolbar';

describe('QueryToolbar commit mode', () => {
  let id: string;
  beforeEach(() => {
    resetStores();
    useQueryRun.setState({ runs: new Map() });
    useTxMode.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  });
  const doc = () => useQueryDocs.getState().docs.get(id)!;
  /** Auto / Manual are two radio items of `⋯`. */
  const openMore = (user: ReturnType<typeof userEvent.setup>) => user.click(screen.getByTestId('query-more'));
  const item = (mode: 'auto' | 'manual') => screen.getByTestId(`query-tx-${mode}`);

  it('manual_toggle_calls_tx_mode', async () => {
    mockBackend({ query_tx_mode: () => 'idle' });
    render(<QueryToolbar doc={doc()} />);
    const user = userEvent.setup();
    await openMore(user);
    expect(item('auto')).toHaveAttribute('role', 'menuitemradio');
    expect(item('auto')).toHaveAttribute('aria-checked', 'true');
    expect(item('manual')).toHaveAttribute('title', expect.stringContaining('Procedures that commit internally need Auto mode.'));
    await user.click(item('manual'));
    expect(callsOf('query_tx_mode')).toEqual([{ tabId: id, mode: 'Manual' }]);
    await openMore(user);
    expect(item('manual')).toHaveAttribute('aria-checked', 'true');
    await user.click(item('auto'));
    expect(callsOf('query_tx_mode')[1]).toEqual({ tabId: id, mode: 'Auto' });
    await openMore(user);
    expect(item('auto')).toHaveAttribute('aria-checked', 'true');
  });

  it('toggle_disabled_with_open_manual_tx', async () => {
    mockBackend({});
    act(() => useTxMode.getState().patch(id, { mode: 'Manual' }));
    render(<QueryToolbar doc={doc()} />);
    const user = userEvent.setup();
    await openMore(user);
    expect(item('auto')).toBeEnabled();
    await user.keyboard('{Escape}');
    act(() => useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, txState: 'active' }]]) }));
    await openMore(user);
    expect(item('auto')).toBeDisabled();
    expect(item('manual')).toBeDisabled();
    expect(item('auto')).toHaveAttribute('title', 'Commit or roll back the open transaction first');
  });

  it('an open transaction shows an amber Tx chip with its age, Commit and Rollback', async () => {
    mockBackend({ query_tx: () => 'idle', query_tx_info: () => ({ state: 'idle', since_ms: null, statements: 0 }) });
    act(() => useTxMode.getState().patch(id, { mode: 'Manual' }));
    render(<QueryToolbar doc={doc()} />);
    expect(screen.queryByTestId('query-tx-chip')).toBeNull();
    expect(screen.queryByTestId('query-tx-commit')).toBeNull();
    act(() => {
      useTxMode.getState().patch(id, { info: { state: 'active', since_ms: Date.now() - 65_000, statements: 2 } });
      useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, txState: 'active' }]]) });
    });
    expect(screen.getByTestId('query-tx-chip')).toHaveTextContent('Tx 01:05');
    expect(screen.getByTestId('query-tx-chip')).toHaveClass('warning');
    // Icon buttons: named, with a tooltip.
    expect(screen.getByTestId('query-tx-rollback')).toHaveAccessibleName('Rollback');
    expect(screen.getByTestId('query-tx-rollback')).toHaveAttribute('title', 'Roll back the open transaction');
    expect(screen.getByTestId('query-tx-commit')).toHaveAccessibleName('Commit');
    await userEvent.setup().click(screen.getByTestId('query-tx-commit'));
    expect(callsOf('query_tx')).toEqual([{ tabId: id, action: 'Commit' }]);
  });

  it('an aborted transaction offers Rollback only, and a run disables it with a reason', () => {
    mockBackend({});
    act(() => {
      useTxMode.getState().patch(id, { mode: 'Manual' });
      useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, txState: 'aborted' }]]) });
    });
    render(<QueryToolbar doc={doc()} />);
    expect(screen.getByTestId('query-tx-chip')).toHaveTextContent('Tx aborted');
    expect(screen.queryByTestId('query-tx-commit')).toBeNull();
    expect(screen.getByTestId('query-tx-rollback')).toBeEnabled();
    act(() => useQueryRun.setState({ runs: new Map([[id, { ...emptyRun, txState: 'aborted', phase: { kind: 'stopping', since: 0, reason: 'user' } }]]) }));
    expect(screen.getByTestId('query-tx-rollback')).toBeDisabled();
    expect(screen.getByTestId('query-tx-rollback')).toHaveAttribute('title', 'A statement is running');
  });

  it('a_refused_switch_to_auto_keeps_manual_and_shows_the_error', async () => {
    mockBackend({
      query_tx_mode: () => {
        throw fail('transaction_open');
      },
    });
    act(() => useTxMode.getState().patch(id, { mode: 'Manual' }));
    render(<QueryToolbar doc={doc()} />);
    const user = userEvent.setup();
    await openMore(user);
    await user.click(item('auto'));
    await openMore(user);
    expect(item('manual')).toHaveAttribute('aria-checked', 'true');
    expect(useQueryRun.getState().get(id).messages.at(-1)?.error?.kind).toBe('transaction_open');
  });
});
