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
import { formatCount, plural } from '../lib/plural';
import { stmtsByIndex } from '../query/plan';
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
  /**
   * For a result line the statement's first preview line; for an error, the server message
   * (shown under `statement`); for a notice, the notice; for the final line, the run summary.
   */
  text: string;
  /**
   * First line of the statement's preview (spec §6.2: "#2 SELECT * FORM orders" above its
   * error). Set on every line of a statement; null (or absent) for the final line and app notes.
   */
  statement?: string | null;
  rows: number | null;
  affected: number | null;
  elapsedMs: number | null;
  error: AppError | null;
  tone: 'info' | 'error' | 'warning';
  /** Editor range of the statement "Retry statement" runs again (retryable errors only). */
  retryRange: [number, number] | null;
}

/**
 * Figures of the last finished run (status bar, spec §9). Explain runs never replace them.
 * Only `rows` and `elapsedMs` are always present; format with `lastRunText`.
 */
export interface LastRun {
  /** Rows returned by the row-returning results. */
  rows: number;
  /** Whole run, from `done` (not the sum of statement times). */
  elapsedMs: number;
  /** Rows affected by statements without a result set (DML); null when none reported a count. */
  affected?: number | null;
  /** Row-returning results. */
  resultSets?: number;
  /** Statements in the plan. */
  statements?: number;
  /** Statements that failed (a plain user cancel is not one; an outcome-unknown failure is). */
  errors?: number;
}

/**
 * Run-state fields a status bar needs (all derivable, see `runElapsedMs` / `runProgress`):
 * - `phase.kind !== 'idle'`: a run is active (`running`, `stopping`, `abandoning`).
 * - `startedAt`: Date.now() at `begin`, kept while stopping; elapsed = now - startedAt.
 * - `phase.index` / `phase.total` (running) or `current` / `plan.stmts.length`: statement N of M.
 * - `phase.since` (stopping / abandoning): when that phase began.
 * - `lastRun`: figures of the last finished (non-Explain) run; hide it while a run is active.
 * - `stop`: undefined while running, null when every statement ran, else why it stopped.
 */
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
  /** Why Explain did not start (`query_explain` refused), shown in the Explain panel. */
  explainError: string | null;
  panel: 'result' | 'messages' | 'explain';
  activeResult: number;
  lastRun: LastRun | null;
  /** The statement being run (from `statement_start`), null outside a run. */
  current: number | null;
  /** The result between its `result_start` and `result_end` (results never interleave). */
  openResult: { id: number; index: number; hasColumns: boolean } | null;
  /** When the run started (kept while stopping, to resume `running` after a timeout). */
  startedAt?: number;
  /** What the current stop is for: a `timeout` stop lets a continue-on-error run go on. */
  stopCause?: 'user' | 'timeout';
  /** A normal run or an Explain (whose figures never become `lastRun`). */
  mode: 'run' | 'explain';
  /** Connection and database the run's plan was made on (null before the first run). */
  origin: { connectionId: string; database: string | null } | null;
  /**
   * The backend no longer holds `plan` (its connection closed, or `query_plan_statement`
   * answered `plan_expired`): Retry statement / Fetch all cannot use it. See `planAlive`.
   */
  planExpired: boolean;
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
  explainError: null,
  panel: 'result',
  activeResult: 0,
  lastRun: null,
  current: null,
  openResult: null,
  mode: 'run',
  origin: null,
  planExpired: false,
};

export const canRun = (r: TabRun): boolean => r.phase.kind === 'idle';
export const canStop = (r: TabRun): boolean => r.phase.kind === 'running';

/** Milliseconds since the active run started at `now`; null when no run is active. */
export function runElapsedMs(r: TabRun, now: number): number | null {
  if (r.phase.kind === 'idle') return null;
  const started = r.phase.kind === 'running' ? r.phase.startedAt : r.startedAt;
  return started === undefined ? null : Math.max(0, now - started);
}

/** "Statement `current` of `total`" (1-based) of the active run; null when no run is active. */
export function runProgress(r: TabRun): { current: number; total: number } | null {
  if (r.phase.kind === 'idle') return null;
  const total = r.phase.kind === 'running' ? r.phase.total : (r.plan?.stmts.length ?? 0);
  const index = r.phase.kind === 'running' ? r.phase.index : (r.current ?? 0);
  return { current: Math.min(index + 1, Math.max(total, 1)), total };
}

/**
 * Whether Retry statement / Fetch all can still derive from the run's plan (F12): there is
 * one, the backend has not dropped it, and the tab is still on the connection and database
 * it was made on (the backend's plan is bound to both). `doc`: the tab's current doc.
 * A run without a recorded `origin` (begun outside `begin`, or on a tab without a connection)
 * counts as alive: the backend's `plan_expired` answer is then the authority.
 */
export function planAlive(
  r: TabRun,
  doc: { connectionId: string | null; database: string | null } | undefined,
): boolean {
  if (!r.plan || r.planExpired) return false;
  // No recorded origin (a run begun outside `begin`): only the backend can tell.
  if (!r.origin) return true;
  if (!doc) return false;
  return doc.connectionId === r.origin.connectionId && doc.database === r.origin.database;
}

/** Server messages that only inform: PostgreSQL NOTICE / INFO / LOG / DEBUG, MySQL Note. */
const INFO_NOTICE = /^(?:NOTICE|INFO|LOG|DEBUG\d?|Note)(?: \d+)?: /;
const OMITTED_NOTICES = /^\d[\d,]* more server messages? not shown$/;

/**
 * Tone of a `notice` line. The backend prefixes server messages with their level
 * ("NOTICE: …", "WARNING: …", "Warning 1292: …", dnt_core `ServerNotice::line`): informative
 * levels and the "N more server messages not shown" tally are info; WARNING / Warning, and
 * the app's own notes (reconnected, plan gone…), are warnings.
 */
export function noticeTone(message: string): 'info' | 'warning' {
  return INFO_NOTICE.test(message) || OMITTED_NOTICES.test(message) ? 'info' : 'warning';
}

/**
 * Status bar text of a finished run: "1 row affected · 3 ms", "10,000 rows · 1,234 ms", and
 * with failed statements (continue on error) "Finished with 2 errors · 3 rows · 12 ms", as
 * the Messages' final line says it (`stopReasonText`).
 */
export function lastRunText(last: LastRun): string {
  const parts: string[] = [];
  const affected = last.affected ?? null;
  if ((last.errors ?? 0) > 0) parts.push(`Finished with ${plural(last.errors!, 'error')}`);
  if ((last.resultSets ?? 1) > 0 || affected === null) parts.push(plural(last.rows, 'row'));
  if (affected !== null) parts.push(`${plural(affected, 'row')} affected`);
  parts.push(`${formatCount(last.elapsedMs)} ms`);
  return parts.join(' · ');
}

export interface StartOptions {
  /** `explain`: an Explain run, which keeps `lastRun` (F46). Default `run`. */
  mode?: TabRun['mode'];
  origin?: TabRun['origin'];
}

/** A fresh run of `plan`: keeps the tab's tx state (as the plan saw it) and the last run's figures. */
export function startRun(run: TabRun, plan: PlanView, now: number, opts: StartOptions = {}): TabRun {
  return {
    ...emptyRun,
    phase: { kind: 'running', index: 0, total: plan.stmts.length, startedAt: now },
    plan,
    txState: plan.tx_state,
    lastRun: run.lastRun,
    startedAt: now,
    mode: opts.mode ?? 'run',
    origin: opts.origin ?? null,
  };
}

/**
 * The final Messages line of a run (spec §9). `errors`: failed statements of a run that went
 * on (continue on error); with any, the run "finished with errors" rather than "Done".
 */
export function stopReasonText(stop: StopReason | null, statements: number, elapsedMs: number, errors = 0): string {
  if (stop === null) {
    const what = `${plural(statements, 'statement')} in ${formatCount(elapsedMs)} ms.`;
    return errors > 0 ? `Finished with ${plural(errors, 'error')} — ${what}` : `Done: ${what}`;
  }
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

const stmtOf = (run: TabRun, index: number) => (run.plan ? stmtsByIndex(run.plan).get(index) : undefined);

/** First line of statement `index`'s preview; null when the plan does not know it. */
const statementOf = (run: TabRun, index: number | null): string | null => {
  if (index === null) return null;
  const preview = stmtOf(run, index)?.preview;
  if (preview === undefined) return null;
  // Previews have their line breaks turned into spaces; older plans may still hold one.
  const cut = preview.search(/[\r\n]/);
  return cut < 0 ? preview : preview.slice(0, cut);
};

const line = (fields: Partial<MessageLine> & Pick<MessageLine, 'text'>): MessageLine => ({
  index: null,
  statement: null,
  rows: null,
  affected: null,
  elapsedMs: null,
  error: null,
  tone: 'info',
  retryRange: null,
  ...fields,
});

/** A copy of `list` with `item` at the end (`slice` + `push`: a flat copy, much cheaper than a spread). */
function append<T>(list: readonly T[], item: T): T[] {
  const out = list.slice();
  out.push(item);
  return out;
}

/**
 * `finished` with `index` added. Statements run one after another, so a repeat (a statement with
 * several results) is the last entry: an O(1) check, with a scan only for an out-of-order index.
 */
function addFinished(finished: number[], index: number): number[] {
  const last = finished.at(-1);
  if (last === index) return finished;
  if (last !== undefined && index < last && finished.includes(index)) return finished;
  return append(finished, index);
}

/** `finished` without `index` (an error after a result of that statement): normally its last entry. */
function dropFinished(finished: number[], index: number): number[] {
  const last = finished.at(-1);
  if (last === index) return finished.slice(0, -1);
  if (last === undefined || index > last || !finished.includes(index)) return finished;
  return finished.filter((i) => i !== index);
}

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

function stopTone(stop: StopReason | null, errors: number): MessageLine['tone'] {
  if (stop === null) return errors > 0 ? 'warning' : 'info';
  if (stop.kind === 'cancelled') return 'info';
  return stop.kind === 'error' ? 'error' : 'warning';
}

/** A statement failure (not a plain user cancel): what the final line counts as an error. */
const isFailure = (m: MessageLine): boolean => m.index !== null && m.error !== null && m.tone !== 'info';

function errorLine(run: TabRun, index: number, error: AppError): MessageLine {
  const statement = statementOf(run, index);
  if (error.kind === 'cancelled' && !error.outcome_unknown) {
    // A stop the user asked for, before the statement took effect: not an error.
    return line({ index, statement, text: 'Cancelled.' });
  }
  if (error.outcome_unknown) {
    // Never "not applied", never retried blindly.
    return line({
      index,
      statement,
      text: `${error.message} The statement may have been applied.`,
      error,
      tone: 'warning',
    });
  }
  const range = error.retryable ? stmtOf(run, index)?.range : undefined;
  return line({
    index,
    statement,
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
      // Every piece gets its own line, also a no-column piece after a result set of the
      // same statement and a piece whose `result_start` never came (F14).
      const statement = statementOf(run, index);
      const message = line({
        index,
        statement,
        text: statement ?? '',
        rows: hasRows ? ev.row_count : null,
        affected: ev.affected,
        elapsedMs: ev.elapsed_ms,
      });
      const finished = index !== null ? addFinished(run.finished, index) : run.finished;
      return { ...run, results, openResult: null, messages: append(run.messages, message), finished };
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
        messages: append(run.messages, message),
        markers,
        finished: dropFinished(run.finished, index),
        txState: asTxState(error.tx_state) ?? run.txState,
      };
    }
    case 'notice':
      return {
        ...run,
        messages: append(
          run.messages,
          line({ index: ev.index, statement: statementOf(run, ev.index), text: ev.message, tone: noticeTone(ev.message) }),
        ),
      };
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
      let rows = 0;
      let resultSets = 0;
      let affected: number | null = null;
      let errors = 0;
      for (const m of run.messages) {
        if (m.index === null) continue;
        if (m.rows !== null) {
          rows += m.rows;
          resultSets += 1;
        } else if (m.affected !== null) {
          affected = (affected ?? 0) + m.affected;
        }
        if (isFailure(m)) errors += 1;
      }
      const final = line({
        text: stopReasonText(ev.stop, statements, ev.elapsed_ms, errors),
        elapsedMs: ev.elapsed_ms,
        tone: stopTone(ev.stop, errors),
      });
      const lastRun: LastRun | null =
        run.mode === 'explain' ? run.lastRun : { rows, affected, resultSets, statements, errors, elapsedMs: ev.elapsed_ms };
      const panel = run.results.length === 0 && run.explain === null ? 'messages' : run.panel;
      return {
        ...run,
        phase: { kind: 'idle' },
        stop: ev.stop,
        current: null,
        openResult: null,
        stopCause: undefined,
        messages: append(run.messages, final),
        panel,
        lastRun,
      };
    }
  }
}

interface QueryRunState {
  runs: Map<string, TabRun>;
  /** The tab's run, `emptyRun` when it never ran. */
  get(tabId: string): TabRun;
  /** Starts a run of `plan` (before `query_run` returns its id); `mode: 'explain'` for Explain. */
  begin(tabId: string, plan: PlanView, opts?: { mode?: TabRun['mode'] }): void;
  /** The backend dropped plan `planId` (`plan_expired`): marks the tab's run if it is that plan. */
  expirePlan(tabId: string, planId: string): void;
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
  const byIndex = stmtsByIndex(run.plan);
  const ddl = run.finished.some((i) => byIndex.get(i)?.kind === 'ddl');
  if (ddl) invalidateCompletion(origin.connectionId, origin.database ?? undefined);
}

export const useQueryRun = create<QueryRunState>()((set, get) => {
  const put = (tabId: string, run: TabRun) => set((s) => ({ runs: new Map(s.runs).set(tabId, run) }));
  const patch = (tabId: string, p: Partial<TabRun>) => put(tabId, { ...get().get(tabId), ...p });

  return {
    runs: new Map(),
    get: (tabId) => get().runs.get(tabId) ?? emptyRun,
    begin: (tabId, plan, opts) => {
      const doc = useQueryDocs.getState().docs.get(tabId);
      if (doc?.connectionId) {
        const open = useConnections.getState().info.has(doc.connectionId);
        origins.set(tabId, { connectionId: doc.connectionId, database: doc.database, open });
      }
      else origins.delete(tabId);
      const origin = doc?.connectionId ? { connectionId: doc.connectionId, database: doc.database } : null;
      put(tabId, startRun(get().get(tabId), plan, Date.now(), { mode: opts?.mode, origin }));
    },
    expirePlan: (tabId, planId) => {
      const run = get().runs.get(tabId);
      if (run && run.plan?.plan_id === planId && !run.planExpired) put(tabId, { ...run, planExpired: true });
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
  if (s.info !== prev.info) {
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
  }
  // A deleted connection: its runtime, and the plans in it, are gone.
  if (s.summaries !== prev.summaries) {
    for (const conn of prev.summaries.keys()) if (!s.summaries.has(conn)) expireConnectionPlans(conn);
  }
});

/**
 * The backend runtime of `connectionId` is gone, and the plans of its tabs with it (F12).
 * Called by the tree's `forgetConnection` (Close after `close_connection`; Import after
 * replacing connections) and on delete (above). Dropping the server info alone is not that:
 * the tree's Reconnect clears it and keeps the runtime and its plans. Anything else the
 * backend dropped answers `plan_expired` (see rerun.ts).
 */
export function expireConnectionPlans(connectionId: string) {
  for (const [tabId, run] of useQueryRun.getState().runs) {
    if (run.origin?.connectionId === connectionId && run.plan) useQueryRun.getState().expirePlan(tabId, run.plan.plan_id);
  }
}

import.meta.hot?.dispose(() => {
  unsubscribeDocs();
  unsubscribeConnections();
});
