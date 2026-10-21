import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { TabBar } from '../../components/TabBar';
import { useConnections } from '../../store/connections';
import { ROW_LIMITS, useQueryDocs } from '../../store/queryTabs';
import { useTabs, type QueryTabRef } from '../../store/tabs';
import { useTree } from '../../store/tree';
import { config, ids, resetStores } from '../../test/backend';
import { QueryTab } from './QueryTab';

function seed() {
  useConnections.getState().setStored([], [
    config(ids.prod, 'shop-prod', { default_db: 'shop' }),
    config(ids.analytics, 'analytics', { kind: 'postgres', port: 5432 }),
  ]);
}

function selectDatabase(connectionId: string, database: string) {
  const id = `conn:${connectionId}/db:${database}`;
  useTree.setState({
    nodes: new Map([[id, { id, kind: 'database', label: database, scope: { database, schema: null } }]]),
    selectedId: id,
  });
}

const activeTab = () => useTabs.getState().tabs.find((t) => t.id === useTabs.getState().activeId);

describe('query tab shell', () => {
  beforeEach(() => {
    resetStores();
    seed();
  });

  it('new_query_from_tree_selection_binds_connection_and_database', async () => {
    act(() => selectDatabase(ids.prod, 'shop'));
    render(<TabBar />);
    await userEvent.setup().click(screen.getByTestId('new-query'));
    const tab = activeTab();
    expect(tab).toMatchObject({ kind: 'query', title: 'Query 1', connectionId: ids.prod });
    const doc = useQueryDocs.getState().docs.get(tab!.id);
    expect(doc).toMatchObject({ connectionId: ids.prod, database: 'shop', title: 'Query 1', dialect: 'mysql', rowLimit: 10000 });
  });

  it('new_query_titles_increment', async () => {
    act(() => selectDatabase(ids.prod, 'shop'));
    render(<TabBar />);
    const user = userEvent.setup();
    await user.click(screen.getByTestId('new-query'));
    await user.click(screen.getByTestId('new-query'));
    expect(useTabs.getState().tabs.map((t) => t.title)).toEqual(['Query 1', 'Query 2']);
  });

  it('a connection node gives its default database', async () => {
    const id = `conn:${ids.prod}`;
    act(() => useTree.setState({ nodes: new Map([[id, { id, kind: 'connection', label: 'shop-prod' }]]), selectedId: id }));
    render(<TabBar />);
    await userEvent.setup().click(screen.getByTestId('new-query'));
    expect(useQueryDocs.getState().docs.get(activeTab()!.id)).toMatchObject({ connectionId: ids.prod, database: 'shop' });
  });

  it('the strip shows run, explain, context and ⋯ (format, limit); the tab itself has no toolbar', async () => {
    const docId = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    const tab: QueryTabRef = { id: docId, kind: 'query', connectionId: ids.prod, title: 'Query 1' };
    const { container } = render(<QueryTab tab={tab} />);
    expect(container.querySelector('[role="toolbar"]')).toBeNull();
    render(<TabBar />);
    for (const name of ['Export…', 'Copy as…']) {
      expect(screen.queryByRole('button', { name })).toBeNull();
    }
    for (const testId of ['query-run', 'query-run-menu', 'query-explain', 'query-context', 'query-more']) {
      expect(screen.getByTestId(testId)).toBeInTheDocument();
    }
    await userEvent.setup().click(screen.getByTestId('query-more'));
    expect(screen.getByTestId('query-format')).toBeInTheDocument();
    const limits = ROW_LIMITS.map((n) => screen.getByTestId(`query-row-limit-${n}`).textContent);
    expect(limits).toEqual(['Limit 1,000', '✓Limit 10,000', 'Limit 100,000', 'Limit 1,000,000']);
  });

  it('closeConnection closes object tabs but keeps query tabs', () => {
    const orders = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
    act(() => {
      useTabs.getState().open({ id: 'o1', kind: 'object', connectionId: ids.prod, objectRef: orders, title: 'orders' });
      useQueryDocs.getState().newQuery(ids.prod, 'shop');
      useTabs.getState().closeConnection(ids.prod);
    });
    expect(useTabs.getState().tabs.map((t) => t.kind)).toEqual(['query']);
    expect(useTabs.getState().activeId).toBe(useTabs.getState().tabs[0]!.id);
  });

  it('a query tab survives deletion of its connection and shows a placeholder', async () => {
    const docId = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    act(() => {
      useConnections.getState().setStored([], [config(ids.analytics, 'analytics', { kind: 'postgres' })]);
      useTabs.getState().closeConnection(ids.prod);
    });
    expect(useTabs.getState().tabs.map((t) => t.id)).toEqual([docId]);
    render(
      <>
        <TabBar />
        <QueryTab tab={{ id: docId, kind: 'query', connectionId: ids.prod, title: 'Query 1' }} />
      </>,
    );
    await userEvent.setup().click(screen.getByTestId('query-context'));
    const select = screen.getByTestId('query-connection') as HTMLSelectElement;
    expect(select.value).toBe(ids.prod);
    expect(select.selectedOptions[0]).toHaveTextContent('(deleted connection)');
    expect(select.selectedOptions[0]).toBeDisabled();
  });

  it('editor_language_follows_engine', () => {
    const mount = (connectionId: string) => {
      const docId = useQueryDocs.getState().newQuery(connectionId, null);
      return render(<QueryTab tab={{ id: docId, kind: 'query', connectionId, title: 'Query' }} />);
    };
    const mysql = mount(ids.prod);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-language', 'mysql');
    mysql.unmount();
    mount(ids.analytics);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-language', 'pgsql');
    expect(document.querySelector('[data-testid="query-editor"] [data-testid="monaco-mock"]')).not.toBeNull();
  });
});
