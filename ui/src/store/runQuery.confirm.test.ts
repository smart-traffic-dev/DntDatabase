import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, PlanView } from '../bindings';
import { backendError, callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import { useConnections } from './connections';
import { useQueryRun } from './queryRun';
import { useQueryDocs } from './queryTabs';
import { runQuery } from './runQuery';
import { useRunPrompt } from './runPrompt';
import { useTxMode } from './txMode';

vi.mock('./completion', () => ({ invalidateCompletion: vi.fn(), forgetCompletionConnection: vi.fn() }));

const stmt = (index: number, extra: Partial<PlanStmtView>): PlanStmtView => ({
  index,
  range: [index * 10, index * 10 + 8],
  kind: 'write',
  verdict: 'confirm',
  preview: `STMT ${index}`,
  ...extra,
});

const planOf = (stmts: PlanStmtView[]): PlanView => ({
  plan_id: 'plan-1',
  stmts,
  dialect: { engine: 'mysql' } as PlanView['dialect'],
  connection_name: 'shop-prod',
  env_tag: 'production',
  read_only: false,
  tx_state: 'idle',
});

let tab: string;

function setup(plan: PlanView, run: () => unknown = () => 'run-1') {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
  useRunPrompt.setState({ pending: null });
  useTxMode.getState().reset();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  tab = useQueryDocs.getState().newQuery(ids.prod, 'shop');
  mockBackend({ query_plan: () => plan, query_run: run, query_cancel: () => null, tabs_save: () => null });
}

describe('runQuery typed confirmation and lost transactions', () => {
  beforeEach(() => vi.clearAllMocks());

  it('sends_the_typed_names_to_query_run', async () => {
    setup(planOf([stmt(0, { confirm_name: 'orders' }), stmt(1, {}), stmt(2, { confirm_name: 'orders' })]));
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true, ['orders']);
    await done;
    expect(callsOf('query_run')[0]).toMatchObject({ confirmed: true, confirmNames: ['orders'] });
  });

  it('settling_without_names_sends_the_plans_names_once_each_in_order', async () => {
    setup(planOf([stmt(0, { confirm_name: 'b' }), stmt(1, { confirm_name: 'a' }), stmt(2, { confirm_name: 'b' })]));
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true);
    await done;
    expect(callsOf('query_run')[0]).toMatchObject({ confirmNames: ['b', 'a'] });
  });

  it('a_plan_without_names_sends_an_empty_list', async () => {
    setup(planOf([stmt(0, {})]));
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true);
    await done;
    expect(callsOf('query_run')[0]).toMatchObject({ confirmNames: [] });
  });

  it('an_allowed_implicit_commit_asks_first_and_cancel_runs_nothing', async () => {
    setup(planOf([stmt(0, { verdict: 'allow', kind: 'ddl', implicit_commit: true })]));
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(false);
    await done;
    expect(callsOf('query_run')).toEqual([]);
  });

  it('an_allowed_implicit_commit_runs_with_confirmed_false_and_no_names', async () => {
    setup(planOf([stmt(0, { verdict: 'allow', kind: 'ddl', implicit_commit: true })]));
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true);
    await done;
    expect(callsOf('query_run')[0]).toMatchObject({ confirmed: false, confirmNames: [] });
  });

  it('a_run_refused_for_a_lost_manual_transaction_asks_for_a_rollback', async () => {
    setup(planOf([stmt(0, { verdict: 'allow', kind: 'read' })]), () => {
      throw backendError('connection_lost_outcome_unknown');
    });
    useTxMode.getState().patch(tab, { mode: 'Manual' });
    await runQuery(tab, 'All');
    expect(useTxMode.getState().tabs.get(tab)?.lost).toEqual({ outcomeUnknown: true });
  });

  it('a_lost_connection_in_auto_mode_is_not_a_pending_acknowledgement', async () => {
    setup(planOf([stmt(0, { verdict: 'allow', kind: 'read' })]), () => {
      throw backendError('connection_lost_in_transaction');
    });
    await runQuery(tab, 'All');
    expect(useTxMode.getState().tabs.get(tab)?.lost ?? null).toBeNull();
  });
});
