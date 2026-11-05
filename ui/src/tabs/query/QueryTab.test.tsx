import { act, render, renderHook, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { TabBar } from '../../components/TabBar';
import { queryDocTarget, registerCompletion } from '../../query/completion/monacoProvider';
import { useConnections } from '../../store/connections';
import { emptyRun } from '../../store/queryRun';
import { ROW_LIMITS, useQueryDocs } from '../../store/queryTabs';
import { useTabs, type QueryTabRef } from '../../store/tabs';
import { useTree } from '../../store/tree';
import { calls, callsOf, config, ids, mockBackend, resetStores } from '../../test/backend';
import { useEditorMarks } from './editorMarks';
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
    expect(limits).toEqual(['Display limit 1,000', '✓Display limit 10,000', 'Display limit 100,000', 'Display limit 1,000,000']);
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

  it('command_tab_speaks_its_language_without_a_mysql_fallback', () => {
    act(() =>
      useConnections.getState().setStored([], [
        config(ids.prod, 'shop-prod'),
        config(ids.legacy, 'cache', { kind: 'redis', port: 6379 }),
        config(ids.analytics, 'docs', { kind: 'mongo', port: 27017 }),
      ]),
    );
    const mount = (connectionId: string) => {
      const docId = useQueryDocs.getState().newQuery(connectionId, null);
      return { docId, view: render(<QueryTab tab={{ id: docId, kind: 'query', connectionId, title: 'Query' }} />) };
    };
    const redis = mount(ids.legacy);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-language', 'redis');
    expect(useQueryDocs.getState().docs.get(redis.docId)?.dialect).toBe('redis');
    redis.view.unmount();
    mount(ids.analytics);
    expect(screen.getByTestId('monaco-mock')).toHaveAttribute('data-language', 'mongo');
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

describe('query tab of a Command connection', () => {
  beforeEach(() => {
    resetStores();
    useConnections.getState().setStored([], [
      config(ids.prod, 'shop-prod', { default_db: 'shop' }),
      config(ids.legacy, 'cache', { kind: 'redis', port: 6379 }),
    ]);
  });

  /** An editor over a growing text, with its cursor on `line` and the highlight decorations recorded. */
  function fakeEditor() {
    let text = '';
    let line = 1;
    let onContent: (e: { changes: { rangeOffset: number }[] }) => void = () => {};
    let onCursor: () => void = () => {};
    const highlights: unknown[][] = [];
    let collections = 0;
    const lines = () => text.split('\n');
    const model = {
      getValue: () => text,
      getValueLength: () => text.length,
      getOffsetAt: (p: { lineNumber: number; column: number }) =>
        lines().slice(0, p.lineNumber - 1).reduce((n, l) => n + l.length + 1, 0) + p.column - 1,
      getPositionAt: (o: number) => {
        const before = text.slice(0, o);
        return { lineNumber: before.split('\n').length, column: before.length - before.lastIndexOf('\n') };
      },
    };
    const editor = {
      updateOptions: vi.fn(),
      // The first collection is the current-statement highlight, the second the glyphs.
      createDecorationsCollection: () => {
        const mine = collections++ === 0;
        return { set: (d: unknown[]) => void (mine && highlights.push(d)), clear: vi.fn() };
      },
      getModel: () => model,
      getPosition: () => ({ lineNumber: line, column: 1 }),
      onDidChangeModelContent: (cb: typeof onContent) => {
        onContent = cb;
        return { dispose: vi.fn() };
      },
      onDidChangeCursorPosition: (cb: () => void) => {
        onCursor = cb;
        return { dispose: vi.fn() };
      },
    };
    const monaco = { editor: { setModelMarkers: vi.fn() }, MarkerSeverity: { Error: 8, Warning: 4 } };
    return {
      mounted: { editor, monaco } as never,
      highlights,
      type(s: string) {
        for (const ch of s) {
          const at = text.length;
          text += ch;
          onContent({ changes: [{ rangeOffset: at }] });
        }
      },
      moveTo(l: number) {
        line = l;
        onCursor();
      },
    };
  }

  const highlightLine = (d: unknown[] | undefined) =>
    (d?.[0] as { range: { startLineNumber: number; endLineNumber: number; endColumn: number } } | undefined)?.range;

  it('command_tab_uses_backend_ranges', async () => {
    vi.useFakeTimers();
    try {
      const ranges = vi.fn((): [number, number][] => [
        [0, 5],
        [6, 13],
      ]);
      mockBackend({ statement_ranges: () => ranges() });
      const f = fakeEditor();
      renderHook(() => useEditorMarks(f.mounted, 'redis', emptyRun, { tabId: 'q-redis', connectionId: ids.legacy }));
      f.type('GET a\nSET b 1');
      await vi.advanceTimersByTimeAsync(149);
      expect(ranges).not.toHaveBeenCalled();
      await vi.advanceTimersByTimeAsync(1);
      expect(ranges).toHaveBeenCalledTimes(1);
      expect(callsOf('statement_ranges')).toEqual([
        { text: 'GET a\nSET b 1', dialect: { engine: 'postgres' }, connId: ids.legacy },
      ]);
      // The two ranges the backend cut: the cursor's line picks one.
      expect(highlightLine(f.highlights.at(-1))).toEqual({ startLineNumber: 1, startColumn: 1, endLineNumber: 1, endColumn: 6 });
      act(() => f.moveTo(2));
      expect(highlightLine(f.highlights.at(-1))).toEqual({ startLineNumber: 2, startColumn: 1, endLineNumber: 2, endColumn: 8 });
      // Nothing more was asked (no IPC per cursor move).
      expect(ranges).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it('stale_range_response_is_dropped', async () => {
    vi.useFakeTimers();
    try {
      const pending: ((r: [number, number][]) => void)[] = [];
      mockBackend({ statement_ranges: () => new Promise((resolve) => pending.push(resolve)) });
      const f = fakeEditor();
      renderHook(() => useEditorMarks(f.mounted, 'redis', emptyRun, { tabId: 'q-stale', connectionId: ids.legacy }));
      f.type('GET a');
      await vi.advanceTimersByTimeAsync(150);
      f.type('\nSET b 1');
      await vi.advanceTimersByTimeAsync(150);
      expect(pending).toHaveLength(2);
      // The newer answer first, then the older one: the older must not win.
      await act(async () => pending[1]!([[0, 5], [6, 13]]));
      await act(async () => pending[0]!([[0, 13]]));
      expect(highlightLine(f.highlights.at(-1))).toEqual({ startLineNumber: 1, startColumn: 1, endLineNumber: 1, endColumn: 6 });
    } finally {
      vi.useRealTimers();
    }
  });

  it('command_tab_has_no_sql_completion', () => {
    mockBackend({ tabs_save: () => null });
    const redisDoc = useQueryDocs.getState().newQuery(ids.legacy, null);
    const sqlDoc = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    const model = (id: string) => ({ uri: { scheme: 'query', authority: id } });
    // No SQL target: neither the SQL completion nor the AI ghost text serve the tab.
    expect(queryDocTarget(model(redisDoc))).toBeNull();
    expect(queryDocTarget(model(sqlDoc))).toMatchObject({ engine: 'mysql', connId: ids.prod });
    // The provider is registered for the SQL languages only.
    const registerCompletionItemProvider = vi.fn(() => ({ dispose: vi.fn() }));
    const monaco = { languages: { registerCompletionItemProvider } } as never;
    registerCompletion(monaco, () => null).dispose();
    expect(registerCompletionItemProvider.mock.calls.map((c) => (c as unknown[])[0])).toEqual(['mysql', 'pgsql']);
    // Opening the editor loads no SQL autocomplete data.
    render(<QueryTab tab={{ id: redisDoc, kind: 'query', connectionId: ids.legacy, title: 'Query 1' }} />);
    expect(calls().filter((c) => c.startsWith('completion_'))).toEqual([]);
  });
});
