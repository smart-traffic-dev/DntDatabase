import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useConnections } from '../store/connections';
import { useAppSettings } from '../store/appSettings';
import { useDialogs } from '../store/dialogs';
import { emptyRun, useQueryRun, type TabRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { useTxMode } from '../store/txMode';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { StatusBar } from './StatusBar';

const NOW = 1_800_000_000_000;

describe('StatusBar manual transactions', () => {
  let tab: string;
  const setRun = (patch: Partial<TabRun>) =>
    act(() => useQueryRun.setState({ runs: new Map([[tab, { ...emptyRun, ...patch }]]) }));
  const flush = () => act(async () => void (await vi.advanceTimersByTimeAsync(0)));

  beforeEach(() => {
    vi.useFakeTimers({ now: NOW });
    resetStores();
    useQueryRun.setState({ runs: new Map() });
    useTxMode.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('status_bar_shows_open_tx_age_and_warns', async () => {
    mockBackend({
      query_tx_info: () => ({ state: 'active', since_ms: NOW - 134_000, statements: 3 }),
      get_idle_tx_warn_minutes: () => 5,
    });
    setRun({ txState: 'active' });
    render(<StatusBar />);
    await flush();
    const tx = screen.getByTestId('status-tx');
    expect(tx).toHaveTextContent('Transaction open · 3 statements · 02:14');
    expect(tx).not.toHaveClass('warn');
    expect(useDialogs.getState().notice).toBeNull();

    // 5 minutes after BEGIN: warning colour and one toast.
    await act(async () => void (await vi.advanceTimersByTimeAsync(166_000)));
    expect(tx).toHaveTextContent('05:00');
    expect(tx).toHaveClass('warn');
    expect(useDialogs.getState().notice).toBe(
      'A transaction has been open for more than 5 minutes. Commit or roll it back.',
    );

    // The toast shows once per transaction.
    act(() => useDialogs.getState().setNotice(null));
    await act(async () => void (await vi.advanceTimersByTimeAsync(60_000)));
    expect(useDialogs.getState().notice).toBeNull();
    expect(tx).toHaveTextContent('06:00');
  });

  it('uses_the_configured_threshold', async () => {
    mockBackend({
      query_tx_info: () => ({ state: 'active', since_ms: NOW - 61_000, statements: 1 }),
      get_idle_tx_warn_minutes: () => 1,
    });
    useAppSettings.getState().reset();
    setRun({ txState: 'active' });
    render(<StatusBar />);
    await flush();
    expect(screen.getByTestId('status-tx')).toHaveTextContent('Transaction open · 1 statement · 01:01');
    expect(screen.getByTestId('status-tx')).toHaveClass('warn');
  });

  it('a_lost_transaction_asks_for_a_rollback_and_never_says_rolled_back_when_unknown', async () => {
    mockBackend({ query_tx: () => 'idle', query_tx_info: () => ({ state: 'idle', since_ms: null, statements: 0 }) });
    act(() => useTxMode.getState().patch(tab, { mode: 'Manual', lost: { outcomeUnknown: true } }));
    render(<StatusBar />);
    const lost = screen.getByTestId('status-tx-lost');
    expect(lost).toHaveTextContent('the last statement may have been applied');
    expect(lost.textContent).not.toMatch(/rolled back/i);
    fireEvent.click(screen.getByRole('button', { name: 'Rollback to continue' }));
    expect(callsOf('query_tx')).toEqual([{ tabId: tab, action: 'Rollback' }]);
    await flush();
    expect(screen.queryByTestId('status-tx-lost')).toBeNull();
  });
});
