import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useConnections } from '../store/connections';
import { emptyRun, useQueryRun, type TabRun } from '../store/queryRun';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { StatusBar } from './StatusBar';

function select(summary: ReturnType<typeof config>, open = true) {
  act(() => {
    useConnections.getState().setStored([], [summary]);
    if (open) useConnections.getState().setInfo(summary.config.id, serverInfo('MySql8'));
    useTree.getState().select(connNodeId(summary.config.id));
  });
}

describe('StatusBar', () => {
  beforeEach(() => {
    resetStores();
  });

  it('shows the environment as the first segment instead of washing the bar', () => {
    const { container } = render(<StatusBar />);
    select(config(ids.prod, 'shop-prod', { env_tag: 'production' }));
    const bar = container.querySelector('footer');
    expect(bar).not.toHaveClass('env-production');
    const env = screen.getByTestId('status-env');
    expect(env).toHaveTextContent('PRODUCTION');
    expect(env).toHaveClass('env-production');
    expect(bar?.firstElementChild).toBe(env);
    expect(screen.getByText(/shop-prod/)).toBeInTheDocument();
  });

  it('names staging and local too; no segment without a tag', () => {
    const { unmount } = render(<StatusBar />);
    select(config(ids.prod, 'stg', { env_tag: 'staging' }));
    expect(screen.getByTestId('status-env')).toHaveTextContent('STAGING');
    unmount();
    render(<StatusBar />);
    select(config(ids.prod, 'dev', { env_tag: 'local' }));
    expect(screen.getByTestId('status-env')).toHaveTextContent('LOCAL');
    act(() => useConnections.getState().setStored([], [config(ids.prod, 'plain')]));
    expect(screen.queryByTestId('status-env')).toBeNull();
  });

  it('shows the database of the active query tab', () => {
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    act(() => void useQueryDocs.getState().newQuery(ids.prod, 'shop'));
    render(<StatusBar />);
    expect(screen.getByTestId('status-db')).toHaveTextContent('shop');
  });

  it('shows the connection, server version and SSH host', () => {
    render(<StatusBar />);
    select(
      config(ids.prod, 'shop-prod', {
        ssh: { host: 'bastion-01', port: 22, user: 'ops', auth: 'key', key_path: null },
      }),
    );
    expect(screen.getByText('shop-prod · MySQL 8.0.36 · via SSH bastion-01')).toBeInTheDocument();
  });

  it('shows a short PostgreSQL version with the full string in the tooltip, and a read-only mark', () => {
    const full = 'PostgreSQL 17.11 (Debian 17.11-1.pgdg120+1) on x86_64-pc-linux-gnu, compiled by gcc, 64-bit';
    render(<StatusBar />);
    const summary = config(ids.prod, 'pg-main', { read_only: true });
    act(() => {
      useConnections.getState().setStored([], [summary]);
      useConnections.getState().setInfo(ids.prod, { ...serverInfo('Postgres', [17, 11, 0]), version_string: full });
      useTree.getState().select(connNodeId(ids.prod));
    });
    const label = screen.getByText('pg-main · PostgreSQL 17.11');
    expect(label).toHaveAttribute('title', expect.stringContaining(full));
    expect(screen.getByText('read-only')).toBeInTheDocument();
  });
  it('follows the active tab, else the tree selection', () => {
    const { container } = render(<StatusBar />);
    const prod = config(ids.prod, 'shop-prod', { env_tag: 'production' });
    const local = config(ids.analytics, 'analytics', { env_tag: 'local' });
    act(() => {
      useConnections.getState().setStored([], [prod, local]);
      useTree.getState().select(connNodeId(ids.analytics));
    });
    expect(screen.getByText(/analytics/)).toBeInTheDocument();

    const orders = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
    act(() => {
      useTabs.getState().open({ id: objectTabId(ids.prod, orders), kind: 'object', connectionId: ids.prod, objectRef: orders, title: 'orders' });
    });
    expect(screen.getByText(/shop-prod/)).toBeInTheDocument();
    expect(screen.getByTestId('status-env')).toHaveTextContent('PRODUCTION');
    expect(screen.getByTestId('status-db')).toHaveTextContent('shop');

    act(() => useTabs.getState().reset());
    expect(screen.getByText(/analytics/)).toBeInTheDocument();
    expect(screen.getByTestId('status-env')).toHaveTextContent('LOCAL');
    expect(container.querySelector('footer')).not.toHaveClass('env-production');
  });

  it('is neutral for other connections and without a selection', () => {
    const { container } = render(<StatusBar />);
    expect(screen.getByText('No connection selected')).toBeInTheDocument();
    select(config(ids.prod, 'local-db', { env_tag: 'local' }), false);
    expect(container.querySelector('footer')).not.toHaveClass('env-production');
    expect(screen.getByText('local-db · not connected')).toBeInTheDocument();
  });
});

describe('StatusBar of a query tab', () => {
  let tab: string;
  const setRun = (patch: Partial<TabRun>) =>
    act(() => useQueryRun.setState({ runs: new Map([[tab, { ...emptyRun, ...patch }]]) }));

  beforeEach(() => {
    resetStores();
    useQueryRun.setState({ runs: new Map() });
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    mockBackend({ query_tx: () => 'idle', tabs_save: () => null });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('shows the figures of the last run', () => {
    setRun({ lastRun: { rows: 12, elapsedMs: 34 } });
    render(<StatusBar />);
    expect(screen.getByText('12 rows · 34 ms')).toBeInTheDocument();
  });

  it('shows running state with elapsed time and hides the previous run', () => {
    vi.useFakeTimers({ now: 100_000 });
    setRun({
      phase: { kind: 'running', index: 1, total: 6, startedAt: 88_000 },
      lastRun: { rows: 1, elapsedMs: 10 },
    });
    render(<StatusBar />);
    expect(screen.getByTestId('status-running')).toHaveTextContent('Running… 2/6 · 12 s');
    expect(screen.queryByTestId('status-last')).toBeNull();
    expect(screen.getByTestId('status-tx')).toHaveTextContent('Auto-commit');
    const live = screen.getByRole('status');
    expect(live.textContent).toBe('Running… 2/6');
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.getByTestId('status-running')).toHaveTextContent('Running… 2/6 · 14 s');
    expect(screen.getByRole('status').textContent).toBe('Running… 2/6');
    expect(screen.getByTestId('status-running').querySelector('[aria-hidden="true"]:not(.spinner)')).toHaveTextContent('14 s');
  });

  it('says so when the user cancelled the run', () => {
    setRun({ stop: { kind: 'cancelled' }, lastRun: { rows: 1, elapsedMs: 3093 } });
    render(<StatusBar />);
    expect(screen.getByTestId('status-last')).toHaveTextContent('Cancelled by user after 3.1 s');
  });

  it('uses singular and grouped numbers', () => {
    setRun({ lastRun: { rows: 1, elapsedMs: 1200 } });
    const { unmount } = render(<StatusBar />);
    expect(screen.getByTestId('status-last')).toHaveTextContent('1 row · 1,200 ms');
    unmount();
    setRun({ lastRun: { rows: 10000, elapsedMs: 5 } });
    render(<StatusBar />);
    expect(screen.getByTestId('status-last')).toHaveTextContent('10,000 rows · 5 ms');
  });

  // The shared run text (store/queryRun `lastRunText`): affected rows and errors too.
  it('shows affected rows and errors of the last run', () => {
    setRun({ lastRun: { rows: 0, resultSets: 0, affected: 1, statements: 1, errors: 0, elapsedMs: 3 } });
    const { unmount } = render(<StatusBar />);
    expect(screen.getByTestId('status-last')).toHaveTextContent(/^1 row affected · 3 ms$/);
    unmount();
    setRun({ lastRun: { rows: 3, resultSets: 1, affected: null, statements: 4, errors: 2, elapsedMs: 12 } });
    render(<StatusBar />);
    expect(screen.getByTestId('status-last')).toHaveTextContent(/^Finished with 2 errors · 3 rows · 12 ms$/);
    expect(screen.getByTestId('status-last')).toHaveClass('errors');
  });

  it('shows_transaction_states_with_buttons', async () => {
    render(<StatusBar />);
    expect(screen.getByTestId('status-tx')).toHaveTextContent('Auto-commit');
    expect(screen.queryByRole('button', { name: 'Commit' })).toBeNull();

    setRun({ txState: 'active' });
    expect(screen.getByTestId('status-tx')).toHaveTextContent('In transaction');
    expect(screen.getByTestId('status-tx')).toHaveClass('tx-active');
    expect(screen.getByRole('button', { name: 'Rollback' })).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Commit' }));
    expect(callsOf('query_tx')).toEqual([{ tabId: tab, action: 'Commit' }]);

    setRun({ txState: 'aborted' });
    expect(screen.getByTestId('status-tx')).toHaveTextContent('Transaction aborted');
    expect(screen.getByTestId('status-tx')).toHaveClass('tx-aborted');
    expect(screen.queryByRole('button', { name: 'Commit' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Rollback' })).toBeInTheDocument();
  });

  it('stopping_counter_and_abandoning_text', () => {
    vi.useFakeTimers({ now: 100_000 });
    render(<StatusBar />);
    expect(screen.queryByTestId('status-run')).toBeNull();

    setRun({ phase: { kind: 'stopping', since: 100_000, reason: 'user' } });
    expect(screen.getByTestId('status-run')).toHaveTextContent('Stopping… 0 s');
    act(() => {
      vi.advanceTimersByTime(3000);
    });
    expect(screen.getByTestId('status-run')).toHaveTextContent('Stopping… 3 s');

    setRun({ phase: { kind: 'abandoning', since: 103_000 } });
    expect(screen.getByTestId('status-run')).toHaveTextContent('Server not responding — replacing connection… 0 s');
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.getByTestId('status-run')).toHaveTextContent('Server not responding — replacing connection… 2 s');
  });
});
