// Typing must stay cheap (user report: laggy typing): a keystroke updates the document in
// the store, and only the editor may care. The toolbar, the result panel and its messages
// must not re-render for it.
import { act, render, screen } from '@testing-library/react';
import { Profiler } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { emptyRun, startRun, useQueryRun } from '../../store/queryRun';
import { useQueryDocs } from '../../store/queryTabs';
import { config, ids, resetStores } from '../../test/backend';
import { useConnections } from '../../store/connections';
import { TabStripActions } from '../TabStripActions';
import { QueryTab } from './QueryTab';

const counts = vi.hoisted(() => ({ toolbar: 0, panel: 0 }));

// Plain (non-memo) counters: they render whenever QueryTab renders them. The toolbar one
// renders the real toolbar inside, so its buttons can be checked too.
vi.mock('./QueryToolbar', async (importOriginal) => {
  const { createElement } = await import('react');
  const actual = await importOriginal<typeof import('./QueryToolbar')>();
  return {
    QueryToolbar: (props: Parameters<typeof actual.QueryToolbar>[0]) => {
      counts.toolbar++;
      return createElement(actual.QueryToolbar, props);
    },
  };
});
vi.mock('./ResultPanel', () => ({
  ResultPanel: () => {
    counts.panel++;
    return null;
  },
}));

/** The real result panel (the module itself is replaced by a counter above). */
const realPanel = async () => (await vi.importActual<typeof import('./ResultPanel')>('./ResultPanel')).ResultPanel;

const PLAN = {
  plan_id: 'p1',
  stmts: [{ index: 0, range: [0, 8] as [number, number], kind: 'read' as const, verdict: 'allow' as const, preview: 'S0' }],
  dialect: { engine: 'mysql' as const },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle' as const,
};

const typeChars =(id: string, n: number) => {
  for (let i = 1; i <= n; i++) {
    act(() => useQueryDocs.getState().update(id, { sql: 'x'.repeat(i) }));
  }
};

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { default_db: 'shop' })]);
  counts.toolbar = 0;
  counts.panel = 0;
});

describe('typing re-renders', () => {
  it('fifty_keystrokes_do_not_re_render_the_toolbar_or_the_result_panel', async () => {
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    // The toolbar is the tab strip's action cluster for the tab.
    const tab = { id, kind: 'query' as const, connectionId: ids.prod, title: 'Query 1' };
    render(
      <>
        <TabStripActions groupId="g1" tab={tab} />
        <QueryTab tab={tab} />
      </>,
    );
    const toolbarBefore = counts.toolbar;
    const panelBefore = counts.panel;
    expect(toolbarBefore).toBeGreaterThan(0);
    typeChars(id, 50);
    expect(useQueryDocs.getState().docs.get(id)?.sql).toHaveLength(50);
    expect(counts.toolbar - toolbarBefore).toBe(0);
    expect(counts.panel - panelBefore).toBe(0);
    // A field the toolbar shows still reaches it.
    act(() => useQueryDocs.getState().update(id, { rowLimit: 1000 }));
    expect(counts.toolbar - toolbarBefore).toBe(1);
    // So does the run state: a started run disables Run and enables Stop.
    expect(screen.getByTestId('query-run')).toBeEnabled();
    expect(screen.queryByTestId('query-stop')).toBeNull();
    act(() => useQueryRun.setState({ runs: new Map([[id, startRun(emptyRun, PLAN as never, 0)]]) }));
    // The Run slot shows Stop while the statement runs.
    expect(screen.queryByTestId('query-run')).toBeNull();
    expect(screen.getByTestId('query-stop')).toBeEnabled();
  });

  it('messages_view_ignores_keystrokes', async () => {
    const ResultPanel = await realPanel();
    const id = useQueryDocs.getState().newQuery(ids.prod, 'shop');
    // A run with a plan: the Messages tab (and its plan-alive check) is mounted.
    const run = { ...startRun(emptyRun, PLAN as never, 0), origin: { connectionId: ids.prod, database: 'shop' } };
    useQueryRun.setState({ runs: new Map([[id, run]]) });
    let commits = 0;
    render(
      <Profiler id="panel" onRender={() => void commits++}>
        <ResultPanel tabId={id} />
      </Profiler>,
    );
    const before = commits;
    typeChars(id, 50);
    expect(commits - before).toBe(0);
    // A change the panel does show (the database: plan no longer alive) still reaches it.
    act(() => useQueryDocs.getState().update(id, { database: 'other' }));
    expect(commits - before).toBeGreaterThan(0);
  });
});
