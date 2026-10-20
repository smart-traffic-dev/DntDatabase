import { act, renderHook } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanView } from '../../bindings';
import { useQueryDocs } from '../../store/queryTabs';
import { planAlive, useQueryRun } from '../../store/queryRun';
import { callsOf, mockBackend, resetStores } from '../../test/backend';
import { PLAN_GONE, rerunStatement, setPlanRunner, usePlanAlive } from './rerun';

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

  it('double_click_while_pending_runs_once', async () => {
    mockBackend({ query_plan_statement: () => plan('d'), query_run: () => 'r' });
    await Promise.all([rerunStatement(tab(), 0), rerunStatement(tab(), 0)]);
    expect(callsOf('query_plan_statement')).toHaveLength(1);
    expect(callsOf('query_run')).toHaveLength(1);
  });

  it('run_started_meanwhile_wins', async () => {
    mockBackend({
      query_plan_statement: () => {
        useQueryRun.getState().begin(tab(), plan('other'));
        return plan('d');
      },
      query_run: () => 'r',
    });
    await rerunStatement(tab(), 0);
    expect(callsOf('query_run')).toEqual([]);
    expect(useQueryRun.getState().get(tab()).plan?.plan_id).toBe('other');
  });

  // F12: once the backend says the plan is gone, Retry / Fetch all go away.
  it('plan_expired_marks_the_plan_gone', async () => {
    mockBackend({
      query_plan_statement: () => Promise.reject({ kind: 'plan_expired', message: 'The run plan expired; run it again.' }),
    });
    expect(planAlive(useQueryRun.getState().get(tab()), useQueryDocs.getState().docs.get(tab()))).toBe(true);
    await rerunStatement(tab(), 0);
    const run = useQueryRun.getState().get(tab());
    expect(run.planExpired).toBe(true);
    expect(run.messages.at(-1)?.text).toBe('The run plan expired; run it again.');
    expect(planAlive(run, useQueryDocs.getState().docs.get(tab()))).toBe(false);
    // A later click asks the backend nothing and says why.
    await rerunStatement(tab(), 0);
    expect(callsOf('query_plan_statement')).toHaveLength(1);
    expect(useQueryRun.getState().get(tab()).messages.at(-1)?.text).toBe(PLAN_GONE);
  });

  it('other_errors_keep_the_plan', async () => {
    mockBackend({ query_plan_statement: () => Promise.reject({ kind: 'busy', message: 'Busy.' }) });
    await rerunStatement(tab(), 0);
    expect(useQueryRun.getState().get(tab()).planExpired).toBe(false);
  });

  it('plan_is_gone_when_the_tab_moved_to_another_database_or_connection', async () => {
    const id =useQueryDocs.getState().newQuery('conn-1', 'shop');
    useQueryRun.getState().begin(id, plan('p'));
    useQueryRun.getState().apply(id, { t: 'done', stop: null, elapsed_ms: 1 });
    const alive = () => planAlive(useQueryRun.getState().get(id), useQueryDocs.getState().docs.get(id));
    expect(alive()).toBe(true);
    const { result } = renderHook(() => usePlanAlive(id));
    expect(result.current).toBe(true);
    act(() => useQueryDocs.getState().update(id, { database: 'other' }));
    expect(alive()).toBe(false);
    expect(result.current).toBe(false);
    act(() => useQueryDocs.getState().update(id, { database: 'shop', connectionId: 'conn-2' }));
    expect(alive()).toBe(false);
    act(() => useQueryDocs.getState().update(id, { connectionId: 'conn-1' }));
    expect(result.current).toBe(true);
    mockBackend({ query_plan_statement: () => plan('d'), query_run: () => 'r' });
    act(() => useQueryDocs.getState().update(id, { database: 'other' }));
    await rerunStatement(id, 0);
    expect(callsOf('query_plan_statement')).toEqual([]);
    // No plan at all: nothing to retry.
    expect(planAlive(useQueryRun.getState().get('none'), undefined)).toBe(false);
  });

  it('ignored_while_a_run_is_active', async () => {
    mockBackend({ query_plan_statement: () => plan('d'), query_run: () => 'r' });
    useQueryRun.getState().begin(tab(), plan('busy'));
    await rerunStatement(tab(), 0);
    expect(callsOf('query_plan_statement')).toEqual([]);
  });
});
