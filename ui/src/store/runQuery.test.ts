import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { PlanStmtView, PlanView } from '../bindings';
import { callsOf, calls, config, ids, mockBackend, openResult, resetStores, serverInfo } from '../test/backend';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { useQueryDocs } from './queryTabs';
import { useQueryRun } from './queryRun';
import { registerEditor, runFromEditor, runPlanned, runQuery, stopQuery, txAction, unregisterEditor } from './runQuery';
import { useRunPrompt } from './runPrompt';
import { currentPlanRunner } from '../tabs/query/rerun';

describe('retry and fetch all wiring', () => {
  it('registers the confirm-aware run flow as the plan runner', () => {
    expect(currentPlanRunner()).not.toBeNull();
  });
});

vi.mock('./completion', () => ({ invalidateCompletion: vi.fn(), forgetCompletionConnection: vi.fn() }));

const stmt = (index: number, kind: PlanStmtView['kind'], verdict: PlanStmtView['verdict']): PlanStmtView => ({
  index,
  range: [index * 10, index * 10 + 8],
  kind,
  verdict,
  preview: `STMT ${index}`,
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
let plan: PlanView;

function setup(opts: { open?: boolean; connection?: boolean } = {}) {
  resetStores();
  useQueryRun.setState({ runs: new Map() });
  useRunPrompt.setState({ pending: null });
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod', { env_tag: 'production' })]);
  if (opts.open ?? true) useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
  tab = useQueryDocs.getState().newQuery(opts.connection === false ? null : ids.prod, 'shop');
  useQueryDocs.getState().update(tab, { sql: 'SELECT 1; UPDATE t SET a = 1', rowLimit: 1000, continueOnError: true });
  mockBackend({
    open_connection: () => openResult(serverInfo('MySql8')),
    query_plan: () => plan,
    query_run: () => 'run-1',
    query_cancel: () => null,
    query_tx: () => 'idle',
    tabs_save: () => null,
  });
}

const messages = () => useQueryRun.getState().get(tab).messages.map((m) => m.text);

describe('runQuery', () => {
  beforeEach(() => {
    plan = planOf([stmt(0, 'read', 'allow')]);
    setup();
  });

  it('opens_connection_before_planning', async () => {
    setup({ open: false });
    await runQuery(tab, 'All');
    expect(calls().filter((c) => c === 'open_connection' || c === 'query_plan' || c === 'query_run')).toEqual([
      'open_connection',
      'query_plan',
      'query_run',
    ]);
  });

  it('stops quietly when the connection cannot be reopened', async () => {
    setup({ open: false });
    mockBackend({ open_connection: () => Promise.reject({ kind: 'connection_failed', message: 'down' }) });
    await runQuery(tab, 'All');
    expect(callsOf('query_plan')).toEqual([]);
  });

  it('says so without a connection', async () => {
    setup({ connection: false });
    await runQuery(tab, 'All');
    expect(messages()).toEqual(['Choose a connection first.']);
    expect(callsOf('query_plan')).toEqual([]);
  });

  it('plans with the target and the editor text, then runs', async () => {
    await runQuery(tab, { Cursor: 3 });
    expect(callsOf('query_plan')[0]).toMatchObject({
      tabId: tab,
      connId: ids.prod,
      database: 'shop',
      text: 'SELECT 1; UPDATE t SET a = 1',
      target: { Cursor: 3 },
    });
    expect(callsOf('query_run')).toHaveLength(1);
    expect(useQueryRun.getState().get(tab).runId).toBe('run-1');
  });

  it('an empty plan does nothing', async () => {
    plan = planOf([]);
    await runQuery(tab, 'All');
    expect(callsOf('query_run')).toEqual([]);
  });

  it('production_cancel_sends_no_query_run', async () => {
    plan = planOf([stmt(0, 'read', 'allow'), stmt(1, 'write', 'confirm')]);
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(false);
    await done;
    expect(callsOf('query_run')).toEqual([]);
    expect(useQueryRun.getState().get(tab).phase.kind).toBe('idle');
  });

  it('confirmed_run_passes_confirmed_true_and_row_limit', async () => {
    plan = planOf([stmt(0, 'write', 'confirm')]);
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true);
    await done;
    expect(callsOf('query_run')[0]).toMatchObject({
      planId: 'plan-1',
      confirmed: true,
      opts: { max_rows: 1000, continue_on_error: true },
    });
  });

  it('an allowed plan runs with confirmed false, and maxRows overrides the limit', async () => {
    await runQuery(tab, 'All', { maxRows: 50 });
    expect(callsOf('query_run')[0]).toMatchObject({ confirmed: false, opts: { max_rows: 50 } });
  });

  it('blocked_plan_never_runs', async () => {
    plan = planOf([stmt(0, 'read', 'allow'), stmt(1, 'write', 'block')]);
    const done = runQuery(tab, 'All');
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('blocked'));
    useRunPrompt.getState().settle(true);
    await done;
    expect(callsOf('query_run')).toEqual([]);
  });

  it('shows a plan error as a Messages line', async () => {
    mockBackend({
      query_plan: () => Promise.reject({ kind: 'syntax', message: 'bad plan', position: null }),
    });
    await runQuery(tab, 'All');
    expect(messages()).toEqual(['bad plan']);
    expect(useQueryRun.getState().get(tab).phase.kind).toBe('idle');
  });

  it('a failed query_run returns the tab to idle with the error', async () => {
    mockBackend({
      query_plan: () => plan,
      query_run: () => Promise.reject({ kind: 'busy', message: 'Tab is busy' }),
    });
    await runQuery(tab, 'All');
    expect(messages()).toEqual(['Tab is busy']);
    expect(useQueryRun.getState().get(tab).phase.kind).toBe('idle');
  });

  it('does not run while the tab is not idle', async () => {
    await runQuery(tab, 'All');
    await runQuery(tab, 'All');
    expect(callsOf('query_run')).toHaveLength(1);
  });

  it('closing a tab while it runs asks first', async () => {
    await runQuery(tab, 'All');
    const closing = useQueryDocs.getState().closeTab(tab);
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('stopRun'));
    const q = useDialogs.getState().question;
    if (q?.kind === 'stopRun') q.done(false);
    expect(await closing).toBe(false);
    expect(callsOf('query_close_tab')).toEqual([]);
  });

  it('events of an earlier channel are ignored by a new run', async () => {
    await runQuery(tab, 'All');
    const first = callsOf('query_run')[0]!.channel as { onmessage(ev: unknown): void };
    first.onmessage({ t: 'done', stop: null, elapsed_ms: 1 });
    plan = { ...planOf([stmt(0, 'read', 'allow')]), plan_id: 'plan-2' };
    await runQuery(tab, 'All');
    first.onmessage({ t: 'done', stop: null, elapsed_ms: 2 });
    first.onmessage({ t: 'notice', index: 0, message: 'late' });
    const run = useQueryRun.getState().get(tab);
    expect(run.phase.kind).toBe('running');
    expect(run.messages.map((m) => m.text)).not.toContain('late');
  });

  it('two rapid runs make exactly one plan and one run', async () => {
    await Promise.all([runQuery(tab, 'All'), runQuery(tab, 'All')]);
    expect(callsOf('query_plan')).toHaveLength(1);
    expect(callsOf('query_run')).toHaveLength(1);
  });

  it('runPlanned runs an already planned view through the guard dialogs', async () => {
    plan = planOf([stmt(0, 'write', 'confirm')]);
    const done = runPlanned(tab, plan);
    await vi.waitFor(() => expect(useRunPrompt.getState().pending?.kind).toBe('confirm'));
    useRunPrompt.getState().settle(true);
    await done;
    expect(callsOf('query_plan')).toEqual([]);
    expect(callsOf('query_run')[0]).toMatchObject({ planId: 'plan-1', confirmed: true });
  });

  it('a Stop before attach is sent at attach', async () => {
    let release!: (id: string) => void;
    mockBackend({
      query_plan: () => plan,
      query_run: () => new Promise((r) => (release = r)),
      query_cancel: () => null,
    });
    const running = runQuery(tab, 'All');
    await vi.waitFor(() => expect(callsOf('query_run')).toHaveLength(1));
    await stopQuery(tab);
    expect(callsOf('query_cancel')).toEqual([]);
    release('run-9');
    await running;
    expect(callsOf('query_cancel')).toEqual([{ runId: 'run-9' }]);
  });

  describe('Stop and close', () => {
    const answerStop = async () => {
      await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('stopRun'));
      const q = useDialogs.getState().question;
      if (q?.kind === 'stopRun') q.done(true);
    };

    it('waits for done, then asks about the transaction', async () => {
      await runQuery(tab, 'All');
      useQueryRun.getState().apply(tab, { t: 'tx_state', state: 'active' });
      const closing = useQueryDocs.getState().closeTab(tab);
      await answerStop();
      await vi.waitFor(() => expect(callsOf('query_cancel')).toHaveLength(1));
      expect(useDialogs.getState().question).toBeNull();
      useQueryRun.getState().apply(tab, { t: 'done', stop: { kind: 'cancelled' }, elapsed_ms: 1 });
      await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('txClose'));
      const q = useDialogs.getState().question;
      if (q?.kind === 'txClose') q.done(false);
      expect(await closing).toBe(false);
    });

    it('a run that ended while the dialog was open closes without waiting', async () => {
      mockBackend({
        query_plan: () => plan,
        query_run: () => 'run-1',
        query_close_tab: () => null,
        tabs_delete: () => null,
        tabs_save: () => null,
      });
      await runQuery(tab, 'All');
      const closing = useQueryDocs.getState().closeTab(tab);
      await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('stopRun'));
      useQueryRun.getState().apply(tab, { t: 'done', stop: null, elapsed_ms: 1 });
      const q = useDialogs.getState().question;
      if (q?.kind === 'stopRun') q.done(true);
      expect(await closing).toBe(true);
    });

    it('a run that does not stop keeps the tab open', async () => {
      await runQuery(tab, 'All');
      vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
      try {
        const closing = useQueryDocs.getState().closeTab(tab);
        await answerStop();
        await vi.advanceTimersByTimeAsync(30_000);
        expect(await closing).toBe(false);
        expect(callsOf('query_close_tab')).toEqual([]);
        expect(useDialogs.getState().notice).toMatch(/could not stop/i);
      } finally {
        vi.useRealTimers();
      }
    });
  });

  it('run selection runs only a selection', async () => {
    registerEditor(tab, {
      getModel: () => ({ getOffsetAt: () => 0 }),
      getSelection: () => ({ isEmpty: () => true, getStartPosition: () => ({ lineNumber: 1, column: 1 }), getEndPosition: () => ({ lineNumber: 1, column: 1 }) }),
      getPosition: () => ({ lineNumber: 1, column: 1 }),
    });
    await runFromEditor(tab, 'selection');
    expect(callsOf('query_plan')).toEqual([]);
    await runFromEditor(tab, 'cursor');
    expect(callsOf('query_plan')[0]).toMatchObject({ target: { Cursor: 0 } });
    unregisterEditor(tab);
  });

  it('stopQuery cancels the run and txAction settles the transaction', async () => {
    await runQuery(tab, 'All');
    await stopQuery(tab);
    expect(callsOf('query_cancel')).toEqual([{ runId: 'run-1' }]);
    useQueryRun.getState().apply(tab, { t: 'done', stop: null, elapsed_ms: 1 });
    await txAction(tab, 'Commit');
    expect(callsOf('query_tx')).toEqual([{ tabId: tab, action: 'Commit' }]);
  });
});
