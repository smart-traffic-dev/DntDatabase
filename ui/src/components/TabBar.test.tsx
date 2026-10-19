import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import type { EnvTag } from '../bindings';
import userEvent from '@testing-library/user-event';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs } from '../store/tabs';
import { config, ids, resetStores } from '../test/backend';
import { TabBar } from './TabBar';

const orders = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };

function openTab(connectionId: string, title: string) {
  useTabs.getState().open({ id: objectTabId(connectionId, { ...orders, name: title }), kind: 'object', connectionId, objectRef: { ...orders, name: title }, title });
}

describe('TabBar', () => {
  beforeEach(resetStores);

  it('shows the environment colour of each tab connection', () => {
    const tagged = (id: string, name: string, env_tag: EnvTag | null) => config(id, name, { env_tag });
    act(() => {
      useConnections.getState().setStored([], [tagged(ids.prod, 'prod', 'production'), tagged(ids.analytics, 'stats', 'staging'), tagged(ids.legacy, 'crm', null)]);
      openTab(ids.prod, 'p_orders');
      openTab(ids.analytics, 's_orders');
      openTab(ids.legacy, 'c_orders');
    });
    render(<TabBar />);
    const tab = (title: string) => screen.getByRole('tab', { name: new RegExp(title) }).closest('.tab')!;
    expect(tab('p_orders')).toHaveClass('env-production');
    expect(tab('p_orders').querySelector('.env-dot.env-production')).toHaveAttribute('title', 'Environment: production');
    expect(tab('s_orders').querySelector('.env-dot.env-staging')).not.toBeNull();
    expect(tab('c_orders').querySelector('.env-dot')).toBeNull();
    expect(tab('c_orders')).not.toHaveClass('env-production');
  });

  it('closing a query tab removes its doc', async () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    render(<TabBar />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Close Query 1' }));
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useQueryDocs.getState().docs.has(id)).toBe(false);
  });

  it('query_tab_shows_sql_label', () => {
    act(() => {
      useConnections.getState().setStored([], [config(ids.prod, 'prod', { env_tag: 'production' })]);
      useTabs.getState().open({ id: 'q1', kind: 'query', connectionId: ids.prod, title: 'Query 1' });
      useTabs.getState().open({ id: 'q2', kind: 'query', connectionId: null, title: 'Query 2' });
    });
    render(<TabBar />);
    const q1 = screen.getByRole('tab', { name: /Query 1/ });
    expect(q1).toHaveTextContent('SQL');
    expect(q1.closest('.tab')).toHaveClass('env-production');
    expect(screen.getByRole('tab', { name: /Query 2/ })).toHaveTextContent('SQL');
  });
});
