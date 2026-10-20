// Run state of each query tab (spec §6.1, §6.2, §9): a pure reducer over the `RunEvent`s
// a run streams, and the Zustand store that keeps one `TabRun` per tab.
//
// Phases: idle → running(index/n) → stopping(since) → abandoning(since) → idle (at `done`).
// Run and Explain are locked until the phase is idle again; Stop is enabled only while
// running.
import { create } from 'zustand';
import type {
  AppError,
  ColumnMeta,
  PlanTree,
  PlanView,
  RunEvent,
  StopReason,
  TxAction,
  TxState,
  Value,
} from '../bindings';
import { forgetCompletionConnection, invalidateCompletion } from './completion';
import { useConnections } from './connections';
import { asTxState, queryTxRecorded, setTxStateSource, useQueryDocs } from './queryTabs';

/** At most this many result tabs per run; later results only get their Messages line. */
export const MAX_RESULT_TABS = 50;
/** Rows are kept in chunks of this many (the backend's `batch_size`). */
export const CHUNK = 500;

export type RunPhase =
  | { kind: 'idle' }
  | { kind: 'running'; index: number; total: number; startedAt: number }
  | { kind: 'stopping'; since: number; reason: 'user' | 'timeout' }
  | { kind: 'abandoning'; since: number };

export interface ResultSet {
  /** `result_id` of the run. */
  id: number;
  /** Statement index in the plan. */
  index: number;
  columns: ColumnMeta[];
  /** Rows in chunks of `CHUNK` (only the last one may be shorter). */
  chunks: Value[][][];
  rowCount: number;
  truncated: boolean;
  affected: number | null;
  elapsedMs: number | null;
  done: boolean;
}

export interface MessageLine {
  /** Statement index in the plan; null for the final line of the run. */
  index: number | null;
  text: string;
  rows: number | null;
  affected: number | null;
  elapsedMs: number | null;
  error: AppError | null;
  tone: 'info' | 'error' | 'warning';
  /** Editor range of the statement "Retry statement" runs again (retryable errors only). */
  retryRange: [number, number] | null;
}

export interface TabRun {
  phase: RunPhase;
  runId: string | null;
  plan: PlanView | null;
  /** Results with columns, in order (at most `MAX_RESULT_TABS`). */
  results: ResultSet[];
  /** Results beyond `MAX_RESULT_TABS`: their rows were dropped. */
  hiddenResults: number;
  messages: MessageLine[];
  /** undefined while running (or before the first run), null when every statement ran. */
  stop: StopReason | null | undefined;
  txState: TxState;
  /** Error markers: UTF-16 offsets into the editor text. */
  markers: { offset: number; message: string }[];
  /** Statements that completed a result and did not fail, in order. */
  finished: number[];
  explain: PlanTree | null;
  panel: 'result' | 'messages' | 'history' | 'explain';
  activeResult: number;
  lastRun: { rows: number; elapsedMs: number } | null;
  /** The statement being run (from `statement_start`), null outside a run. */
  current: number | null;
  /** The result between its `result_start` and `result_end` (results never interleave). */
  openResult: { id: number; index: number; hasColumns: boolean } | null;
  /** When the run started (kept while stopping, to resume `running` after a timeout). */
  startedAt?: number;
  /** What the current stop is for: a `timeout` stop lets a continue-on-error run go on. */
  stopCause?: 'user' | 'timeout';
}

export const emptyRun: TabRun = {
  phase: { kind: 'idle' },
  runId: null,
  plan: null,
  results: [],
  hiddenResults: 0,
  messages: [],
  stop: undefined,
  txState: 'idle',
  markers: [],
  finished: [],
  explain: null,
  panel: 'result',
  activeResult: 0,
  lastRun: null,
  current: null,
  openResult: null,
};

export const canRun = (r: TabRun): boolean => r.phase.kind === 'idle';
export const canStop = (r: TabRun): boolean => r.phase.kind === 'running';

/** A fresh run of `plan`: keeps the tab's tx state (as the plan saw it) and the last run's figures. */
export function startRun(run: TabRun, plan: PlanView, now: number): TabRun {
  return {
    ...emptyRun,
    phase: { kind: 'running', index: 0, total: plan.stmts.length, startedAt: now },
    plan,
    txState: plan.tx_state,
    lastRun: run.lastRun,
    startedAt: now,
  };
}

/** The final Messages line of a run (spec §9). */
export function stopReasonText(stop: StopReason | null, statements: number, elapsedMs: number): string {
  if (stop === null) return `Done: ${statements} statement(s) in ${elapsedMs} ms.`;
  switch (stop.kind) {
    case 'error':
      return `Stopped at statement ${stop.index + 1} after an error.`;
    case 'cancelled':
      return 'Cancelled by user.';
    case 'tx_aborted':
      return 'The transaction is aborted; only ROLLBACK, ROLLBACK TO or COMMIT can run.';
    case 'late_completion':
      return 'Statement finished after the timeout; the remaining statements were not run.';
    case 'dialect_changed':
      return 'sql_mode changed; run the remaining statements again';
  }
}

const stmtOf = (run: TabRun, index: number) => run.plan?.stmts.find((s) => s.index === index);

const line = (fields: Partial<MessageLine> & Pick<MessageLine, 'text'>): MessageLine => ({
  index: null,
  rows: null,
  affected: null,
  elapsedMs: null,
  error: null,
  tone: 'info',
  retryRange: null,
  ...fields,
});

/** Appends `rows` to `chunks`, filling the last chunk up to `CHUNK` first. */
function appendRows(chunks: Value[][][], rows: Value[][]): Value[][][] {
  const out = chunks.slice();
  let at = 0;
  const last = out.at(-1);
  if (last && last.length < CHUNK) {
    const take = rows.slice(0, CHUNK - last.length);
    out[out.length - 1] = last.concat(take);
    at = take.length;
  }
  for (; at < rows.length; at += CHUNK) out.push(rows.slice(at, at + CHUNK));
  return out;
}

function stopTone(stop: StopReason | null): MessageLine['tone'] {
  if (stop === null || stop.kind === 'cancelled') return 'info';
  return stop.kind === 'error' ? 'error' : 'warning';
}

function errorLine(run: TabRun, index: number, error: AppError): MessageLine {
  if (error.kind === 'cancelled' && !error.outcome_unknown) {
    // A stop the user asked for, before the statement took effect: not an error.
    return line({ index, text: 'Cancelled.' });
  }
  if (error.outcome_unknown) {
    // Never "not applied", never retried blindly.
    return line({
      index,
      text: `${error.message} The statement may have been applied.`,
      error,
      tone: 'warning',
    });
  }
  const range = error.retryable ? stmtOf(run, index)?.range : undefined;
  return line({
    index,
    text: error.message,
    error,
    tone: 'error',
    retryRange: range ? [range[0], range[1]] : null,
  });
}

export function reduceRun(run: TabRun, ev: RunEvent, now: number): TabRun {
  switch (ev.t) {
    case 'statement_start': {
      if (run.phase.kind === 'running') {
        return { ...run, phase: { ...run.phase, index: ev.index }, current: ev.index };
      }
      // Continue on error after a timeout: the run goes on with the next statement. A
      // user stop never starts another statement, so it stays stopping.
      if (run.phase.kind !== 'idle' && run.stopCause === 'timeout') {
        const phase: RunPhase = {
          kind: 'running',
          index: ev.index,
          total: run.plan?.stmts.length ?? 0,
          startedAt: run.startedAt ?? now,
        };
        return { ...run, phase, stopCause: undefined, current: ev.index };
      }
      return { ...run, current: ev.index };
    }
    case 'result_start': {
      const openResult = { id: ev.result_id, index: ev.index, hasColumns: ev.columns.length > 0 };
      // A result without columns (DML, DDL…) only gets its Messages line.
      if (!openResult.hasColumns) return { ...run, openResult };
      if (run.results.length >= MAX_RESULT_TABS) return { ...run, openResult, hiddenResults: run.hiddenResults + 1 };
      const result: ResultSet = {
        id: ev.result_id,
        index: ev.index,
        columns: ev.columns,
        chunks: [],
        rowCount: 0,
        truncated: false,
        affected: null,
        elapsedMs: null,
        done: false,
      };
      return { ...run, openResult, results: [...run.results, result] };
    }
    case 'rows': {
      const at = run.results.findIndex((r) => r.id === ev.result_id);
      if (at < 0 || ev.rows.length === 0) return run;
      const r = run.results[at]!;
      const results = run.results.slice();
      results[at] = { ...r, chunks: appendRows(r.chunks, ev.rows), rowCount: r.rowCount + ev.rows.length };
      return { ...run, results };
    }
    case 'result_end': {
      const open = run.openResult?.id === ev.result_id ? run.openResult : null;
      const at = run.results.findIndex((r) => r.id === ev.result_id);
      let results = run.results;
      const index = open?.index ?? (at >= 0 ? run.results[at]!.index : run.current);
      const hasRows = open?.hasColumns ?? at >= 0;
      if (at >= 0) {
        const r = run.results[at]!;
        results = run.results.slice();
        results[at] = { ...r, truncated: ev.truncated, affected: ev.affected, elapsedMs: ev.elapsed_ms, done: true };
      }
      const text = (index !== null ? stmtOf(run, index)?.preview : undefined)?.split(/\r?\n/)[0] ?? '';
      const message = line({
        index,
        text,
        rows: hasRows ? ev.row_count : null,
        affected: ev.affected,
        elapsedMs: ev.elapsed_ms,
      });
      const finished = index !== null && !run.finished.includes(index) ? [...run.finished, index] : run.finished;
      return { ...run, results, openResult: null, messages: [...run.messages, message], finished };
    }
    case 'statement_error': {
      const { index, error } = ev;
      const message = errorLine(run, index, error);
      const real = message.tone !== 'info';
      const markers =
        real && error.position !== null
          ? [...run.markers, { offset: error.position, message: error.message }]
          : run.markers;
      return {
        ...run,
        messages: [...run.messages, message],
        markers,
        finished: run.finished.filter((i) => i !== index),
        txState: asTxState(error.tx_state) ?? run.txState,
      };
    }
    case 'notice':
      return { ...run, messages: [...run.messages, line({ index: ev.index, text: ev.message, tone: 'warning' })] };
    case 'explain':
      return { ...run, explain: ev.plan, panel: 'explain' };
    case 'stopping':
      if (run.phase.kind !== 'running') return run;
      return { ...run, phase: { kind: 'stopping', since: now, reason: ev.reason }, stopCause: ev.reason };
    case 'abandoning':
      if (run.phase.kind === 'abandoning' || run.phase.kind === 'idle') return run;
      return { ...run, phase: { kind: 'abandoning', since: now } };
    case 'tx_state':
      return { ...run, txState: ev.state };
    case 'done': {
      const statements = run.plan?.stmts.length ?? 0;
      const rows = run.messages.reduce((n, m) => n + (m.index !== null && m.rows !== null ? m.rows : 0), 0);
      const final = line({
        text: stopReasonText(ev.stop, statements, ev.elapsed_ms),
        elapsedMs: ev.elapsed_ms,
        tone: stopTone(ev.stop),
      });
      const panel = run.results.length === 0 && run.explain === null ? 'messages' : run.panel;
      return {
        ...run,
        phase: { kind: 'idle' },
        stop: ev.stop,
        current: null,
        openResult: null,
        stopCause: undefined,
        messages: [...run.messages, final],
        panel,
        lastRun: { rows, elapsedMs: ev.elapsed_ms },
      };
    }
  }
}

interface QueryRunState {
  runs: Map<string, TabRun>;
  /** The tab's run, `emptyRun` when it never ran. */
  get(tabId: string): TabRun;
  /** Starts a run of `plan` (before `query_run` returns its id). */
  begin(tabId: string, plan: PlanView): void;
  attach(tabId: string, runId: string): void;
  /**
   * Reduces one event of the tab's run; events of a cleared tab are dropped.
   *
   * The caller must bind each run's Channel callback to that run: pass the `plan_id` of
   * the plan given to `begin` as `token`, so late events of an earlier run of the same
   * tab (its channel can still deliver after a new `begin`) are ignored.
   */
  apply(tabId: string, ev: RunEvent, token?: string): void;
  setPanel(tabId: string, p: TabRun['panel']): void;
  setTxState(tabId: string, s: TxState): void;
  clear(tabId: string): void;
}

/** Connection and database each tab's current run was started on. */
const origins = new Map<string, { connectionId: string; database: string | null; open: boolean }>();

/** Spec §7.3: a DDL statement that completed without error drops the completion cache. */
function afterDone(tabId: string, run: TabRun) {
  const origin = origins.get(tabId);
  origins.delete(tabId);
  if (!origin || !run.plan) return;
  const ddl = run.plan.stmts.some((s) => s.kind === 'ddl' && run.finished.includes(s.index));
  if (ddl) invalidateCompletion(origin.connectionId, origin.database ?? undefined);
}

export const useQueryRun = create<QueryRunState>()((set, get) => {
  const put = (tabId: string, run: TabRun) => set((s) => ({ runs: new Map(s.runs).set(tabId, run) }));
  const patch = (tabId: string, p: Partial<TabRun>) => put(tabId, { ...get().get(tabId), ...p });

  return {
    runs: new Map(),
    get: (tabId) => get().runs.get(tabId) ?? emptyRun,
    begin: (tabId, plan) => {
      const doc = useQueryDocs.getState().docs.get(tabId);
      if (doc?.connectionId) {
        const open = useConnections.getState().info.has(doc.connectionId);
        origins.set(tabId, { connectionId: doc.connectionId, database: doc.database, open });
      }
      else origins.delete(tabId);
      put(tabId, startRun(get().get(tabId), plan, Date.now()));
    },
    attach: (tabId, runId) => patch(tabId, { runId }),
    apply: (tabId, ev, token) => {
      const run = get().runs.get(tabId);
      if (!run) return;
      if (token !== undefined && run.plan?.plan_id !== token) return;
      let next = reduceRun(run, ev, Date.now());
      if (ev.t === 'done') {
        // The connection closed during the run: its session (and transaction) went with it.
        const origin = origins.get(tabId);
        if (origin?.open && !useConnections.getState().info.has(origin.connectionId)) next = { ...next, txState: 'idle' };
      }
      put(tabId, next);
      if (ev.t === 'done') afterDone(tabId, next);
    },
    setPanel: (tabId, panel) => patch(tabId, { panel }),
    setTxState: (tabId, txState) => patch(tabId, { txState }),
    clear: (tabId) => {
      origins.delete(tabId);
      set((s) => {
        if (!s.runs.has(tabId)) return s;
        const runs = new Map(s.runs);
        runs.delete(tabId);
        return { runs };
      });
    },
  };
});

/** Commit / Rollback of the tab's transaction (`query_tx`), recording the state it leaves. */
export const settleTx = (tabId: string, action: TxAction): Promise<TxState> => queryTxRecorded(tabId, action);

// The close guard asks this store whether a tab has an open transaction (spec §7.2).
setTxStateSource(
  (tabId) => useQueryRun.getState().get(tabId).txState,
  (tabId, state) => useQueryRun.getState().setTxState(tabId, state),
);

// A closed tab drops its run state.
const unsubscribeDocs = useQueryDocs.subscribe((s, prev) => {
  if (s.docs === prev.docs) return;
  for (const id of prev.docs.keys()) if (!s.docs.has(id)) useQueryRun.getState().clear(id);
});

// A closed connection: its completion data goes, and its tabs' sessions closed with it.
// A tab still running keeps its state until its `done` (see `apply`).
const unsubscribeConnections = useConnections.subscribe((s, prev) => {
  if (s.info === prev.info) return;
  for (const conn of prev.info.keys()) {
    if (s.info.has(conn)) continue;
    forgetCompletionConnection(conn);
    const { runs } = useQueryRun.getState();
    for (const [tabId, run] of runs) {
      const doc = useQueryDocs.getState().docs.get(tabId);
      if (doc?.connectionId === conn && canRun(run) && run.txState !== 'idle') {
        useQueryRun.getState().setTxState(tabId, 'idle');
      }
    }
  }
});

import.meta.hot?.dispose(() => {
  unsubscribeDocs();
  unsubscribeConnections();
});
