import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, PlanView } from '../bindings';
import { callsOf, config, ids, mockBackend, openResult, resetStores, serverInfo } from '../test/backend';
import { useConnections } from './connections';
import { useQueryDocs } from './queryTabs';
import { useQueryRun } from './queryRun';
import { explainQuery, registerEditor, unregisterEditor } from './runQuery';
import { useRunPrompt } from './runPrompt';

vi.mock('./completion', () => ({ invalidateCompletion: vi.fn(), forgetCompletionConnection: vi.fn() }));

const stmt = (verdict: PlanStmtView['verdict']): PlanStmtView => ({
  index: 0,
  range: [0, 8],
  kind: 'read',
  verdict,
  preview: 'EXPLAIN SELECT 1',
});

const planOf = (verdict: PlanStmtView['verdict']): PlanView => ({
  plan_id: 'plan-x',
  stmts: [stmt(verdict)],
  dialect: { engine: 'mysql' } as PlanView['dialect'],
  connection_name: 'shop-prod',
  env_tag: 'production',
  read_only: false,
  tx_state: 'idle',
});

let tab: string;
let explainResult: () => PlanView;

beforeEach(() => {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
  useRunPrompt.setState({ pending: null });
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  useQueryDocs.getState().update(tab, { sql: 'SELECT 1; SELECT 2', cursor: 12 });
  explainResult = () => planOf('allow');
  mockBackend({
    open_connection: () => openResult(serverInfo('MySql8')),
    query_explain: () => explainResult(),
    query_run: () => 'run-1',
    query_cancel: () => null,
    tabs_save: () => null,
  });
});

describe('explainQuery', () => {
  it('plans the statement at the cursor, then runs the plan', async () => {
    await explainQuery(tab, false);
    expect(callsOf('query_explain')[0]).toMatchObject({
      tabId: tab,
      connId: ids.prod,
      database: 'shop',
      text: 'SELECT 1; SELECT 2',
      cursor: 12,
      analyze: false,
    });
    expect(callsOf('query_run')[0]).toMatchObject({ planId: 'plan-x', confirmed: false });
    expect(useQueryRun.getState().get(tab).runId).toBe('run-1');
    // F46: an Explain run is marked, so its `done` keeps the status bar figures.
    expect(useQueryRun.getState().get(tab).mode).toBe('explain');
    const before = { rows: 7, elapsedMs: 21 };
    useQueryRun.setState({ runs: new Map([[tab, { ...useQueryRun.getState().get(tab), lastRun: before }]]) });
    useQueryRun.getState().apply(tab, { t: 'done', stop: null, elapsed_ms: 6 }, 'plan-x');
    expect(useQueryRun.getState().get(tab).lastRun).toBe(before);
  });

  it('uses the editor cursor when the editor is mounted', async () => {
    const editor = {
      getModel: () => ({ getOffsetAt: () => 4 }),
      getSelection: () => ({
        isEmpty: () => true,
        getStartPosition: () => ({ lineNumber: 1, column: 5 }),
        getEndPosition: () => ({ lineNumber: 1, column: 5 }),
      }),
      getPosition: () => ({ lineNumber: 1, column: 5 }),
    };
    registerEditor(tab, editor);
    await explainQuery(tab, true);
    unregisterEditor(tab, editor);
    expect(callsOf('query_explain')[0]).toMatchObject({ cursor: 4, analyze: true });
  });

  it('shows a refusal in the Explain panel and runs nothing', async () => {
    mockBackend({
      query_explain: () => Promise.reject({ kind: 'unsupported', message: 'Explain is not available for this statement.' }),
      query_run: () => 'run-1',
    });
    await explainQuery(tab, false);
    const run = useQueryRun.getState().get(tab);
    expect(run.explainError).toBe('Explain is not available for this statement.');
    expect(run.panel).toBe('explain');
    expect(callsOf('query_run')).toEqual([]);
  });

  it('a successful explain clears the earlier error', async () => {
    useQueryRun.setState({
      runs: new Map([[tab, { ...useQueryRun.getState().get(tab), explainError: 'old', panel: 'explain' as const }]]),
    });
    await explainQuery(tab, false);
    expect(useQueryRun.getState().get(tab).explainError).toBeNull();
  });

  it('explain_analyze_on_production_asks_first', async () => {
    explainResult = () => planOf('confirm');
    const pending = explainQuery(tab, true);
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    expect(callsOf('query_run')).toEqual([]);
    useRunPrompt.getState().settle(false);
    await pending;
    expect(callsOf('query_run')).toEqual([]);
  });

  it('explain_analyze_confirmed_runs_with_confirmation', async () => {
    explainResult = () => planOf('confirm');
    const pending = explainQuery(tab, true);
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true);
    await pending;
    expect(callsOf('query_run')[0]).toMatchObject({ confirmed: true });
  });

  it('says so without a connection', async () => {
    useQueryDocs.getState().update(tab, { connectionId: null });
    await explainQuery(tab, false);
    expect(callsOf('query_explain')).toEqual([]);
    expect(useQueryRun.getState().get(tab).messages.map((m) => m.text)).toEqual(['Choose a connection first.']);
  });
});
