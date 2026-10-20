import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanView } from '../../bindings';
import { useQueryDocs } from '../../store/queryTabs';
import { useQueryRun } from '../../store/queryRun';
import { callsOf, mockBackend, resetStores } from '../../test/backend';
import { rerunStatement, setPlanRunner } from './rerun';

const plan = (id: string, verdict: 'allow' | 'confirm' | 'block' = 'allow'): PlanView => ({
  plan_id: id,
  stmts: [{ index: 0, range: [0, 8], kind: 'read', verdict, preview: 'SELECT 1' }],
  dialect: { engine: 'postgres' },
  connection_name: 'c',
  env_tag: null,
  read_only: false,
  tx_state: 'idle',
});

describe('rerunStatement', () => {
  beforeEach(() => {
    resetStores();
    setPlanRunner(null);
    useQueryRun.setState({ runs: new Map() });
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { rowLimit: 1000, continueOnError: true });
    useQueryRun.getState().begin(id, plan('source'));
    useQueryRun.getState().apply(id, { t: 'done', stop: null, elapsed_ms: 1 });
    (globalThis as { tab?: string }).tab = id;
  });
  const tab = () => (globalThis as { tab?: string }).tab!;

  it('derives_a_plan_of_the_stored_statement_and_runs_it', async () => {
    mockBackend({
      query_plan_statement: () => plan('derived'),
      query_run: () => 'run-2',
    });
    await rerunStatement(tab(), 0, { maxRows: 1_000_000 });
    expect(callsOf('query_plan_statement')).toEqual([{ planId: 'source', index: 0 }]);
    const [run] = callsOf('query_run');
    expect(run).toMatchObject({ planId: 'derived', confirmed: false, opts: { max_rows: 1_000_000, continue_on_error: true } });
    const state = useQueryRun.getState().get(tab());
    expect(state.plan?.plan_id).toBe('derived');
    expect(state.runId).toBe('run-2');
  });

  it('uses_the_tab_row_limit_by_default', async () => {
    mockBackend({ query_plan_statement: () => plan('d'), query_run: () => 'r' });
    await rerunStatement(tab(), 0);
    expect(callsOf('query_run')[0]).toMatchObject({ opts: { max_rows: 1000 } });
  });

  it('confirm_verdict_goes_to_the_registered_runner', async () => {
    mockBackend({ query_plan_statement: () => plan('d', 'confirm'), query_run: () => 'r' });
    const runner = vi.fn(async () => {});
    setPlanRunner(runner);
    await rerunStatement(tab(), 0, { maxRows: 5 });
    expect(runner).toHaveBeenCalledWith(tab(), expect.objectContaining({ plan_id: 'd' }), { maxRows: 5 });
    expect(callsOf('query_run')).toEqual([]);
  });

  it('confirm_without_runner_is_not_run_silently', async () => {
    mockBackend({ query_plan_statement: () => plan('d', 'confirm'), query_run: () => 'r' });
    await rerunStatement(tab(), 0);
    expect(callsOf('query_run')).toEqual([]);
    expect(useQueryRun.getState().get(tab()).plan?.plan_id).toBe('source');
  });

  it('ignored_while_a_run_is_active', async () => {
    mockBackend({ query_plan_statement: () => plan('d'), query_run: () => 'r' });
    useQueryRun.getState().begin(tab(), plan('busy'));
    await rerunStatement(tab(), 0);
    expect(callsOf('query_plan_statement')).toEqual([]);
  });
});
