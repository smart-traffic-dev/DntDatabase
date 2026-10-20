import { invoke } from '@tauri-apps/api/core';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppError, ColumnMeta, PlanStmtView, PlanTree, PlanView, RunEvent, Value } from '../bindings';
import { forgetCompletionConnection, invalidateCompletion } from './completion';
import { useConnections } from './connections';
import { useQueryDocs, type QueryDoc } from './queryTabs';
import {
  CHUNK,
  MAX_RESULT_TABS,
  canRun,
  canStop,
  emptyRun,
  reduceRun,
  settleTx,
  startRun,
  stopReasonText,
  useQueryRun,
  type TabRun,
} from './queryRun';
import { runChannel } from '../api/query';

vi.mock('./completion', () => ({ invalidateCompletion: vi.fn(), forgetCompletionConnection: vi.fn() }));

const err = (kind: string, message: string, extra: Partial<AppError> = {}): AppError => ({
  kind,
  message,
  detail: null,
  stage: null,
  timings: null,
  server_code: null,
  sqlstate: null,
  position: null,
  retryable: false,
  outcome_unknown: false,
  hint: null,
  fingerprint: null,
  key_openssh: null,
  host: null,
  port: null,
  field: null,
  tx_state: null,
  ...extra,
});

const stmt = (index: number, preview: string, range: [number, number], kind: PlanStmtView['kind'] = 'read'): PlanStmtView => ({
  index,
  range,
  kind,
  verdict: 'allow',
  preview,
});

const planOf = (stmts: PlanStmtView[], extra: Partial<PlanView> = {}): PlanView => ({
  plan_id: 'plan-1',
  stmts,
  dialect: { engine: 'postgres' },
  connection_name: 'local',
  env_tag: null,
  read_only: false,
  tx_state: 'idle',
  ...extra,
});

const col = (name: string): ColumnMeta => ({
  name,
  db_type: 'int4',
  logical: 'Integer',
  nullable: true,
  origin_table: null,
  origin_column: null,
});

const rows = (n: number, from = 0): Value[][] =>
  Array.from({ length: n }, (_, i) => [{ t: 'Int', v: from + i }] as Value[]);

const PLAN = planOf([stmt(0, 'SELECT 1', [0, 8]), stmt(1, 'UPDATE t SET a = 1', [10, 28], 'write')]);

/** Starts `plan` at t=1000 and reduces `events` (t = 1000 + 10·i). */
function play(events: RunEvent[], plan: PlanView = PLAN, from: TabRun = emptyRun): TabRun {
  let run = startRun(from, plan, 1000);
  events.forEach((ev, i) => {
    run = reduceRun(run, ev, 1010 + 10 * i);
  });
  return run;
}

const resultEnd = (result_id: number, row_count: number, extra: Partial<Extract<RunEvent, { t: 'result_end' }>> = {}): RunEvent => ({
  t: 'result_end',
  result_id,
  affected: null,
  row_count,
  truncated: false,
  elapsed_ms: 7,
  ...extra,
});

describe('reduceRun', () => {
  it('exports the limits', () => {
    expect(MAX_RESULT_TABS).toBe(50);
    expect(CHUNK).toBe(500);
  });

  it('statement_and_result_flow', () => {
    const started = startRun(emptyRun, PLAN, 1000);
    expect(started.phase).toEqual({ kind: 'running', index: 0, total: 2, startedAt: 1000 });
    expect(canRun(started)).toBe(false);

    const run = play([
      { t: 'statement_start', index: 0, range: [0, 8] },
      { t: 'result_start', result_id: 0, index: 0, columns: [col('a')] },
      { t: 'rows', result_id: 0, rows: rows(3) },
      resultEnd(0, 3, { elapsed_ms: 12 }),
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'result_start', result_id: 1, index: 1, columns: [] },
      resultEnd(1, 0, { affected: 4, elapsed_ms: 5 }),
      { t: 'done', stop: null, elapsed_ms: 40 },
    ]);

    expect(run.phase).toEqual({ kind: 'idle' });
    expect(run.stop).toBeNull();
    // Only the row-returning result gets a result tab.
    expect(run.results).toHaveLength(1);
    expect(run.results[0]).toMatchObject({ id: 0, index: 0, rowCount: 3, done: true, elapsedMs: 12, truncated: false });
    expect(run.results[0]!.columns.map((c) => c.name)).toEqual(['a']);
    expect(run.messages.map((m) => [m.index, m.text, m.rows, m.affected, m.elapsedMs, m.tone])).toEqual([
      [0, 'SELECT 1', 3, null, 12, 'info'],
      [1, 'UPDATE t SET a = 1', null, 4, 5, 'info'],
      [null, 'Done: 2 statement(s) in 40 ms.', null, null, 40, 'info'],
    ]);
    expect(run.finished).toEqual([0, 1]);
    expect(run.panel).toBe('result');
    expect(run.lastRun).toEqual({ rows: 3, elapsedMs: 40 });
    expect(canRun(run)).toBe(true);
  });

  it('message text is the first line of the preview', () => {
    const plan = planOf([stmt(0, 'SELECT 1\nFROM dual', [0, 18])]);
    const run = play(
      [
        { t: 'statement_start', index: 0, range: [0, 18] },
        { t: 'result_start', result_id: 0, index: 0, columns: [col('a')] },
        resultEnd(0, 0),
      ],
      plan,
    );
    expect(run.messages[0]!.text).toBe('SELECT 1');
  });

  it('rows_are_kept_in_500_chunks', () => {
    const run = play([
      { t: 'statement_start', index: 0, range: [0, 8] },
      { t: 'result_start', result_id: 0, index: 0, columns: [col('a')] },
      { t: 'rows', result_id: 0, rows: rows(500) },
      { t: 'rows', result_id: 0, rows: rows(500, 500) },
      { t: 'rows', result_id: 0, rows: rows(200, 1000) },
      resultEnd(0, 1200),
    ]);
    const r = run.results[0]!;
    expect(r.chunks.map((c) => c.length)).toEqual([500, 500, 200]);
    expect(r.rowCount).toBe(1200);
  });

  it('coalesced partial batches refill the last chunk', () => {
    const run = play([
      { t: 'statement_start', index: 0, range: [0, 8] },
      { t: 'result_start', result_id: 0, index: 0, columns: [col('a')] },
      { t: 'rows', result_id: 0, rows: rows(200) },
      { t: 'rows', result_id: 0, rows: rows(450, 200) },
      { t: 'rows', result_id: 0, rows: rows(500, 650) },
    ]);
    const r = run.results[0]!;
    expect(r.chunks.map((c) => c.length)).toEqual([500, 500, 150]);
    expect(r.rowCount).toBe(1150);
    // Order is kept across chunk boundaries.
    expect(r.chunks.flat().map((row) => (row[0] as { v: number }).v)).toEqual(Array.from({ length: 1150 }, (_, i) => i));
  });

  it('truncated_result_end', () => {
    const run = play([
      { t: 'statement_start', index: 0, range: [0, 8] },
      { t: 'result_start', result_id: 0, index: 0, columns: [col('a')] },
      { t: 'rows', result_id: 0, rows: rows(500) },
      resultEnd(0, 500, { truncated: true }),
    ]);
    expect(run.results[0]).toMatchObject({ truncated: true, done: true, rowCount: 500 });
    expect(run.messages[0]!.rows).toBe(500);
  });

  it('fifty_one_results_overflow_to_messages', () => {
    const events: RunEvent[] = [{ t: 'statement_start', index: 0, range: [0, 8] }];
    for (let id = 0; id < 51; id++) {
      events.push({ t: 'result_start', result_id: id, index: 0, columns: [col('a')] });
      events.push({ t: 'rows', result_id: id, rows: rows(2) });
      events.push(resultEnd(id, 2));
    }
    events.push({ t: 'done', stop: null, elapsed_ms: 99 });
    const run = play(events);
    expect(run.results).toHaveLength(50);
    expect(run.hiddenResults).toBe(1);
    expect(run.results.some((r) => r.id === 50)).toBe(false);
    // The hidden result still has its summary line.
    expect(run.messages.filter((m) => m.index === 0)).toHaveLength(51);
    expect(run.messages[50]).toMatchObject({ index: 0, rows: 2 });
  });

  it('error_marker_uses_editor_offset', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('query', 'syntax error at or near "SET"', { position: 19 }) },
      { t: 'done', stop: { kind: 'error', index: 1 }, elapsed_ms: 3 },
    ]);
    expect(run.markers).toEqual([{ offset: 19, message: 'syntax error at or near "SET"' }]);
    const line = run.messages[0]!;
    expect(line).toMatchObject({ index: 1, tone: 'error', retryRange: null });
    expect(line.text).toContain('syntax error');
    expect(line.error?.kind).toBe('query');
    expect(run.messages.at(-1)!.text).toBe('Stopped at statement 2 after an error.');
    expect(run.finished).toEqual([]);
  });

  it('outcome_unknown_is_warning_without_retry', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      {
        t: 'statement_error',
        index: 1,
        error: err('connection_lost', 'Connection lost', { outcome_unknown: true, retryable: true }),
      },
    ]);
    const line = run.messages[0]!;
    expect(line.tone).toBe('warning');
    expect(line.text).toContain('may have been applied');
    expect(line.retryRange).toBeNull();
  });

  it('cancelled with outcome_unknown is shown, not as a plain cancel', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('cancelled', 'Cancelled', { outcome_unknown: true }) },
    ]);
    expect(run.messages[0]).toMatchObject({ tone: 'warning', retryRange: null });
    expect(run.messages[0]!.text).toContain('may have been applied');
  });

  it('a plain cancel is an info line', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('cancelled', 'Cancelled', { position: 12 }) },
    ]);
    expect(run.messages[0]).toMatchObject({ tone: 'info', text: 'Cancelled.', retryRange: null });
    expect(run.markers).toEqual([]);
  });

  it('retryable_error_offers_retry_range', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('timeout', 'Timed out', { retryable: true }) },
    ]);
    expect(run.messages[0]!.retryRange).toEqual([10, 28]);
    const plain = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('query', 'Duplicate entry') },
    ]);
    expect(plain.messages[0]!.retryRange).toBeNull();
  });

  it('statement_error carries the tx state', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('query', 'boom', { tx_state: 'aborted' }) },
    ]);
    expect(run.txState).toBe('aborted');
  });

  it('notices become Messages lines', () => {
    const run = play([
      { t: 'statement_start', index: 0, range: [0, 8] },
      { t: 'notice', index: 0, message: 'Transaction was rolled back' },
    ]);
    expect(run.messages[0]).toMatchObject({ index: 0, text: 'Transaction was rolled back', error: null });
  });

  it('messages_panel_selected_when_no_results', () => {
    const run = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'result_start', result_id: 0, index: 1, columns: [] },
      resultEnd(0, 0, { affected: 2 }),
      { t: 'done', stop: null, elapsed_ms: 4 },
    ]);
    expect(run.results).toEqual([]);
    expect(run.panel).toBe('messages');
  });

  it('stopping_then_abandoning_then_idle', () => {
    let run = startRun(emptyRun, PLAN, 1000);
    run = reduceRun(run, { t: 'statement_start', index: 0, range: [0, 8] }, 1001);
    run = reduceRun(run, { t: 'stopping', reason: 'timeout' }, 2000);
    expect(run.phase).toEqual({ kind: 'stopping', since: 2000, reason: 'timeout' });
    // A second stopping does not restart the clock.
    run = reduceRun(run, { t: 'stopping', reason: 'user' }, 2500);
    expect(run.phase).toEqual({ kind: 'stopping', since: 2000, reason: 'timeout' });
    run = reduceRun(run, { t: 'abandoning' }, 12000);
    expect(run.phase).toEqual({ kind: 'abandoning', since: 12000 });
    run = reduceRun(run, { t: 'stopping', reason: 'user' }, 13000);
    expect(run.phase).toEqual({ kind: 'abandoning', since: 12000 });
    run = reduceRun(
      run,
      { t: 'statement_error', index: 0, error: err('connection_lost', 'Connection lost', { outcome_unknown: true }) },
      20000,
    );
    run = reduceRun(run, { t: 'done', stop: { kind: 'cancelled' }, elapsed_ms: 19000 }, 20001);
    expect(run.phase).toEqual({ kind: 'idle' });
    expect(run.stop).toEqual({ kind: 'cancelled' });
    expect(run.messages.at(-1)!.text).toBe('Cancelled by user.');
  });

  it('run_locked_until_done', () => {
    let run = startRun(emptyRun, PLAN, 1000);
    expect(canRun(run)).toBe(false);
    run = reduceRun(run, { t: 'stopping', reason: 'user' }, 1100);
    expect(canRun(run)).toBe(false);
    run = reduceRun(run, { t: 'abandoning' }, 1200);
    expect(canRun(run)).toBe(false);
    run = reduceRun(run, { t: 'done', stop: { kind: 'cancelled' }, elapsed_ms: 300 }, 1300);
    expect(canRun(run)).toBe(true);
    expect(canRun(emptyRun)).toBe(true);
  });

  it('stop_enabled_only_while_running', () => {
    let run = startRun(emptyRun, PLAN, 1000);
    expect(canStop(emptyRun)).toBe(false);
    expect(canStop(run)).toBe(true);
    run = reduceRun(run, { t: 'stopping', reason: 'user' }, 1100);
    expect(canStop(run)).toBe(false);
    run = reduceRun(run, { t: 'abandoning' }, 1200);
    expect(canStop(run)).toBe(false);
    run = reduceRun(run, { t: 'done', stop: null, elapsed_ms: 300 }, 1300);
    expect(canStop(run)).toBe(false);
  });

  it('late_completion_text_matches_spec', () => {
    expect(stopReasonText({ kind: 'late_completion' }, 3, 10)).toBe(
      'Statement finished after the timeout; the remaining statements were not run.',
    );
  });

  it('dialect_changed_text_matches_spec', () => {
    expect(stopReasonText({ kind: 'dialect_changed', index: 2 }, 3, 10)).toBe(
      'sql_mode changed; run the remaining statements again',
    );
  });

  it('other stop texts match the spec', () => {
    expect(stopReasonText(null, 3, 120)).toBe('Done: 3 statement(s) in 120 ms.');
    expect(stopReasonText({ kind: 'error', index: 0 }, 3, 1)).toBe('Stopped at statement 1 after an error.');
    expect(stopReasonText({ kind: 'cancelled' }, 3, 1)).toBe('Cancelled by user.');
    expect(stopReasonText({ kind: 'tx_aborted' }, 3, 1)).toBe(
      'The transaction is aborted; only ROLLBACK, ROLLBACK TO or COMMIT can run.',
    );
  });

  it('tx_state_events_update_status', () => {
    const started = startRun(emptyRun, planOf(PLAN.stmts, { tx_state: 'active' }), 1000);
    expect(started.txState).toBe('active');
    let run = reduceRun(started, { t: 'tx_state', state: 'aborted' }, 1001);
    expect(run.txState).toBe('aborted');
    run = reduceRun(run, { t: 'tx_state', state: 'idle' }, 1002);
    expect(run.txState).toBe('idle');
  });

  it('explain_event_switches_panel', () => {
    const tree: PlanTree = { kind: 'table', columns: ['id'], rows: [['1']], raw: 'x' };
    const run = play([
      { t: 'statement_start', index: 0, range: [0, 8] },
      { t: 'explain', index: 0, plan: tree },
      { t: 'done', stop: null, elapsed_ms: 2 },
    ]);
    expect(run.explain).toEqual(tree);
    expect(run.panel).toBe('explain');
  });

  it('a new run starts clean but keeps the tx state and last run', () => {
    const first = play([
      { t: 'statement_start', index: 1, range: [10, 28] },
      { t: 'statement_error', index: 1, error: err('query', 'x', { position: 11 }) },
      { t: 'done', stop: { kind: 'error', index: 1 }, elapsed_ms: 3 },
    ]);
    const next = startRun({ ...first, runId: 'old' }, planOf(PLAN.stmts, { tx_state: 'active' }), 5000);
    expect(next).toMatchObject({
      runId: null,
      results: [],
      hiddenResults: 0,
      messages: [],
      markers: [],
      finished: [],
      explain: null,
      stop: undefined,
      txState: 'active',
      lastRun: first.lastRun,
    });
  });
});

describe('useQueryRun', () => {
  const TAB = 'tab-1';
  const doc = (patch: Partial<QueryDoc> = {}): QueryDoc => ({
    id: TAB,
    title: 'Query 1',
    connectionId: 'conn-1',
    database: 'shop',
    sql: '',
    cursor: 0,
    filePath: null,
    dirty: false,
    continueOnError: false,
    rowLimit: 10000,
    sortOrder: 0,
    dialect: 'postgres',
    ...patch,
  });

  beforeEach(() => {
    for (const id of [...useQueryRun.getState().runs.keys()]) useQueryRun.getState().clear(id);
    useQueryDocs.setState({ docs: new Map([[TAB, doc()]]) });
    vi.mocked(invalidateCompletion).mockReset();
    vi.mocked(forgetCompletionConnection).mockReset();
    vi.mocked(invoke).mockReset();
  });

  it('begin, attach, apply and clear', () => {
    const s = useQueryRun.getState();
    expect(s.get(TAB)).toBe(emptyRun);
    s.begin(TAB, PLAN);
    expect(useQueryRun.getState().get(TAB).phase.kind).toBe('running');
    useQueryRun.getState().attach(TAB, 'run-9');
    expect(useQueryRun.getState().get(TAB).runId).toBe('run-9');
    useQueryRun.getState().apply(TAB, { t: 'done', stop: null, elapsed_ms: 1 });
    expect(useQueryRun.getState().get(TAB).phase.kind).toBe('idle');
    useQueryRun.getState().setPanel(TAB, 'history');
    expect(useQueryRun.getState().get(TAB).panel).toBe('history');
    useQueryRun.getState().clear(TAB);
    expect(useQueryRun.getState().runs.has(TAB)).toBe(false);
  });

  it('events of a cleared tab are dropped', () => {
    useQueryRun.getState().apply('gone', { t: 'tx_state', state: 'active' });
    expect(useQueryRun.getState().runs.has('gone')).toBe(false);
  });

  it('registers itself as the tx state source of the close guard', async () => {
    useQueryRun.getState().setTxState(TAB, 'active');
    vi.mocked(invoke).mockImplementation(async () => {
      throw err('internal', 'stop here');
    });
    const { useDialogs } = await import('./dialogs');
    const closing = useQueryDocs.getState().closeTab(TAB);
    // An active transaction asks Commit / Rollback before anything is sent.
    await vi.waitFor(() => expect(useDialogs.getState().question?.kind).toBe('txClose'));
    expect(vi.mocked(invoke)).not.toHaveBeenCalled();
    const q = useDialogs.getState().question as { settle(a: 'Rollback'): Promise<void>; done(closed: boolean): void };
    // Settling through the dialog records the new state.
    vi.mocked(invoke).mockResolvedValueOnce('idle');
    await q.settle('Rollback');
    expect(useQueryRun.getState().get(TAB).txState).toBe('idle');
    q.done(false);
    expect(await closing).toBe(false);
  });

  it('a closed tab loses its run state', () => {
    useQueryRun.getState().setTxState(TAB, 'active');
    useQueryDocs.getState().remove(TAB);
    expect(useQueryRun.getState().runs.has(TAB)).toBe(false);
  });

  it('settleTx records the new tx state, or the one a failure reports', async () => {
    useQueryRun.getState().setTxState(TAB, 'active');
    vi.mocked(invoke).mockResolvedValueOnce('idle');
    expect(await settleTx(TAB, 'Commit')).toBe('idle');
    expect(vi.mocked(invoke)).toHaveBeenCalledWith('query_tx', { tabId: TAB, action: 'Commit' });
    expect(useQueryRun.getState().get(TAB).txState).toBe('idle');

    useQueryRun.getState().setTxState(TAB, 'active');
    vi.mocked(invoke).mockRejectedValueOnce(err('query', 'commit failed', { tx_state: 'aborted' }));
    await expect(settleTx(TAB, 'Commit')).rejects.toMatchObject({ kind: 'query' });
    expect(useQueryRun.getState().get(TAB).txState).toBe('aborted');
  });

  it('invalidates completion after a ddl statement ran', () => {
    const plan = planOf([stmt(0, 'CREATE TABLE t (a int)', [0, 22], 'ddl'), stmt(1, 'DROP TABLE x', [23, 35], 'ddl')]);
    const s = useQueryRun.getState();
    s.begin(TAB, plan);
    // The tab's database changes while it runs: the run's database counts.
    useQueryDocs.setState({ docs: new Map([[TAB, doc({ database: 'other' })]]) });
    for (const ev of [
      { t: 'statement_start', index: 0, range: [0, 22] },
      { t: 'result_start', result_id: 0, index: 0, columns: [] },
      resultEnd(0, 0),
    ] as RunEvent[]) {
      useQueryRun.getState().apply(TAB, ev);
    }
    expect(invalidateCompletion).not.toHaveBeenCalled();
    useQueryRun.getState().apply(TAB, { t: 'done', stop: null, elapsed_ms: 5 });
    expect(invalidateCompletion).toHaveBeenCalledTimes(1);
    expect(invalidateCompletion).toHaveBeenCalledWith('conn-1', 'shop');
  });

  it('does not invalidate completion when the ddl failed or for reads', () => {
    const plan = planOf([stmt(0, 'CREATE TABLE t (a int)', [0, 22], 'ddl'), stmt(1, 'SELECT 1', [23, 31])]);
    useQueryRun.getState().begin(TAB, plan);
    for (const ev of [
      { t: 'statement_start', index: 0, range: [0, 22] },
      { t: 'statement_error', index: 0, error: err('query', 'exists') },
      { t: 'statement_start', index: 1, range: [23, 31] },
      { t: 'result_start', result_id: 0, index: 1, columns: [col('a')] },
      resultEnd(0, 0),
      { t: 'done', stop: null, elapsed_ms: 5 },
    ] as RunEvent[]) {
      useQueryRun.getState().apply(TAB, ev);
    }
    expect(invalidateCompletion).not.toHaveBeenCalled();
  });

  it('a ddl run without a database invalidates the whole connection', () => {
    useQueryDocs.setState({ docs: new Map([[TAB, doc({ database: null })]]) });
    useQueryRun.getState().begin(TAB, planOf([stmt(0, 'CREATE TABLE t (a int)', [0, 22], 'ddl')]));
    for (const ev of [
      { t: 'statement_start', index: 0, range: [0, 22] },
      { t: 'result_start', result_id: 0, index: 0, columns: [] },
      resultEnd(0, 0),
      { t: 'done', stop: null, elapsed_ms: 5 },
    ] as RunEvent[]) {
      useQueryRun.getState().apply(TAB, ev);
    }
    expect(invalidateCompletion).toHaveBeenCalledWith('conn-1', undefined);
  });

  it('closing a connection forgets its completion data and idle tx states', () => {
    useConnections.setState({
      info: new Map([['conn-1', { kind: 'postgres', variant: 'Postgres', version: [16, 0, 0], version_string: '16' }]]),
    });
    useQueryRun.getState().setTxState(TAB, 'active');
    useConnections.getState().clearInfo('conn-1');
    expect(forgetCompletionConnection).toHaveBeenCalledWith('conn-1');
    expect(useQueryRun.getState().get(TAB).txState).toBe('idle');
  });
});

describe('runChannel', () => {
  it('delivers every message to the callback', () => {
    const seen: RunEvent[] = [];
    const channel = runChannel((ev) => seen.push(ev));
    channel.onmessage({ t: 'abandoning' });
    expect(seen).toEqual([{ t: 'abandoning' }]);
  });
});
