import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useConnections } from '../store/connections';
import { objectTabId, useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { config, ids, resetStores, serverInfo } from '../test/backend';
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

  it('marks a production connection with env-production', () => {
    const { container } = render(<StatusBar />);
    select(config(ids.prod, 'shop-prod', { env_tag: 'production' }));
    const bar = container.querySelector('footer');
    expect(bar).toHaveClass('env-production');
    expect(screen.getByText(/shop-prod/)).toBeInTheDocument();
  });

  it('shows the connection, server version and SSH host', () => {
    render(<StatusBar />);
    select(
      config(ids.prod, 'shop-prod', {
        ssh: { host: 'bastion-01', port: 22, user: 'ops', auth: 'key', key_path: null },
      }),
    );
    expect(screen.getByText('shop-prod · 8.0.36 · via SSH bastion-01')).toBeInTheDocument();
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
    expect(container.querySelector('footer')).toHaveClass('env-production');

    act(() => useTabs.getState().reset());
    expect(screen.getByText(/analytics/)).toBeInTheDocument();
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
