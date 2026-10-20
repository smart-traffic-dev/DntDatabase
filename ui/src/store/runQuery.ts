// Starting and stopping a query run (spec §5.2, §6.2): plan → guard dialogs → `query_run`.
import { create } from 'zustand';
import { api, toAppError } from '../api';
import { runChannel } from '../api/query';
import type { PlanView, Target, TxAction } from '../bindings';
import { useConnections } from './connections';
import { useDialogs } from './dialogs';
import { canRun, canStop, settleTx, useQueryRun, type MessageLine } from './queryRun';
import { setRunningSource, useQueryDocs } from './queryTabs';
import { promptHandles, reopenConnection } from './reopen';
import { askRunConfirmation, showBlocked } from './runPrompt';
import { setPlanRunner } from '../tabs/query/rerun';

/** How long "Stop and close" waits for the run to end. */
export const STOP_WAIT_MS = 30_000;

/** Tabs between the Run click and the run being under way (planning, dialogs, `query_run`). */
export const useStarting = create<{ ids: Set<string> }>()(() => ({ ids: new Set() }));
const setStarting = (tabId: string, on: boolean) =>
  useStarting.setState((s) => {
    if (s.ids.has(tabId) === on) return s;
    const ids = new Set(s.ids);
    if (on) ids.add(tabId);
    else ids.delete(tabId);
    return { ids };
  });
const isStarting = (tabId: string) => useStarting.getState().ids.has(tabId);

/** Tabs whose editor has selected text (Run selection needs one). */
export const useSelection = create<{ ids: Set<string> }>()(() => ({ ids: new Set() }));
export function setHasSelection(tabId: string, on: boolean) {
  useSelection.setState((s) => {
    if (s.ids.has(tabId) === on) return s;
    const ids = new Set(s.ids);
    if (on) ids.add(tabId);
    else ids.delete(tabId);
    return { ids };
  });
}

/** Tabs whose Stop came before `query_run` returned the run id. */
const pendingStop = new Set<string>();

interface Pos {
  lineNumber: number;
  column: number;
}

/** The part of a Monaco editor the run keys need. */
export interface EditorLike {
  getModel(): { getOffsetAt(p: Pos): number } | null;
  getSelection(): { isEmpty(): boolean; getStartPosition(): Pos; getEndPosition(): Pos } | null;
  getPosition(): Pos | null;
}

const editors = new Map<string, EditorLike>();

/** Remembers the mounted editor of a tab (the toolbar and the keys read the cursor from it). */
export function registerEditor(tabId: string, editor: EditorLike) {
  editors.set(tabId, editor);
}

export function unregisterEditor(tabId: string, editor?: EditorLike) {
  if (!editor || editors.get(tabId) === editor) editors.delete(tabId);
}

/**
 * The run target of a tab. `cursor` (Run, Ctrl+Enter): the selection if there is one, else the
 * statement at the cursor. `selection` (Run selection): the selection only, null without one.
 * `all` (Ctrl+Shift+Enter): the whole tab.
 */
export function targetOf(tabId: string, mode: 'cursor' | 'selection' | 'all'): Target | null {
  if (mode === 'all') return 'All';
  const editor = editors.get(tabId);
  const model = editor?.getModel();
  if (editor && model) {
    const sel = editor.getSelection();
    if (sel && !sel.isEmpty()) {
      return { Selection: [model.getOffsetAt(sel.getStartPosition()), model.getOffsetAt(sel.getEndPosition())] };
    }
    if (mode === 'selection') return null;
    const pos = editor.getPosition();
    if (pos) return { Cursor: model.getOffsetAt(pos) };
  } else if (mode === 'selection') {
    return null;
  }
  return { Cursor: useQueryDocs.getState().docs.get(tabId)?.cursor ?? 0 };
}

/** Runs the toolbar / key action of a tab. */
export async function runFromEditor(tabId: string, mode: 'cursor' | 'selection' | 'all'): Promise<void> {
  const target = targetOf(tabId, mode);
  if (target) await runQuery(tabId, target);
}

/** Appends a line to the tab's Messages and shows that panel. */
function note(tabId: string, text: string, error: ReturnType<typeof toAppError> | null = null) {
  const line: MessageLine = {
    index: null,
    text,
    rows: null,
    affected: null,
    elapsedMs: null,
    error,
    tone: 'error',
    retryRange: null,
  };
  const run = useQueryRun.getState().get(tabId);
  useQueryRun.setState((s) => ({
    runs: new Map(s.runs).set(tabId, { ...run, messages: [...run.messages, line], panel: 'messages' }),
  }));
}

/** A run that never started (`query_run` failed): the tab is usable again. */
function abort(tabId: string, text: string, error: ReturnType<typeof toAppError>) {
  const run = useQueryRun.getState().get(tabId);
  if (run.phase.kind !== 'idle') {
    useQueryRun.setState((s) => ({
      runs: new Map(s.runs).set(tabId, { ...run, phase: { kind: 'idle' }, current: null, openResult: null }),
    }));
  }
  note(tabId, text, error);
}

/** Guard dialogs, then `begin` + `query_run` + `attach` for a plan (the caller holds the starting flag). */
async function execute(tabId: string, plan: PlanView, extra?: { maxRows?: number }): Promise<void> {
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (!doc || plan.stmts.length === 0) return;
  if (plan.stmts.some((s) => s.verdict === 'block')) {
    await showBlocked(plan);
    return;
  }
  const confirmed = plan.stmts.some((s) => s.verdict === 'confirm');
  if (confirmed && !(await askRunConfirmation(plan))) return;

  // The tab may have been closed while the dialog was open.
  if (!canRun(useQueryRun.getState().get(tabId)) || !useQueryDocs.getState().docs.has(tabId)) return;
  pendingStop.delete(tabId);
  useQueryRun.getState().begin(tabId, plan);
  try {
    const runId = await api.queryRun(
      plan.plan_id,
      confirmed,
      { max_rows: extra?.maxRows ?? doc.rowLimit, continue_on_error: doc.continueOnError },
      // Bound to this run: late events of an earlier channel are dropped.
      runChannel((ev) => useQueryRun.getState().apply(tabId, ev, plan.plan_id)),
    );
    useQueryRun.getState().attach(tabId, runId);
    if (pendingStop.delete(tabId)) await cancelRun(tabId, runId);
  } catch (e) {
    pendingStop.delete(tabId);
    const error = toAppError(e);
    abort(tabId, error.message, error);
  }
}

/** Runs the tab's `target`: opens the connection, plans, asks the guards, runs. */
export async function runQuery(tabId: string, target: Target, extra?: { maxRows?: number }): Promise<void> {
  if (isStarting(tabId) || !canRun(useQueryRun.getState().get(tabId))) return;
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (!doc) return;
  setStarting(tabId, true);
  try {
    if (!doc.connectionId) {
      note(tabId, 'Choose a connection first.');
      return;
    }
    const connectionId = doc.connectionId;
    if (!useConnections.getState().info.has(connectionId)) {
      const failure = await reopenConnection(connectionId);
      if (failure) {
        // A password / host key prompt takes over; any other failure is shown here.
        if (!promptHandles(failure)) note(tabId, failure.message, failure);
        return;
      }
    }
    let plan: PlanView;
    try {
      plan = await api.queryPlan(tabId, connectionId, doc.database, doc.sql, target);
    } catch (e) {
      const error = toAppError(e);
      note(tabId, error.message, error);
      return;
    }
    await execute(tabId, plan, extra);
  } finally {
    setStarting(tabId, false);
  }
}

/** Runs a plan that already exists (Retry, Fetch all) through the same guards and run flow. */
export async function runPlanned(tabId: string, plan: PlanView, extra?: { maxRows?: number }): Promise<void> {
  if (isStarting(tabId) || !canRun(useQueryRun.getState().get(tabId))) return;
  setStarting(tabId, true);
  try {
    await execute(tabId, plan, extra);
  } finally {
    setStarting(tabId, false);
  }
}

async function cancelRun(tabId: string, runId: string) {
  try {
    await api.queryCancel(runId);
  } catch (e) {
    const error = toAppError(e);
    note(tabId, error.message, error);
  }
}

/** Stop: asks the backend to cancel the tab's run (the `stopping` event follows). */
export async function stopQuery(tabId: string): Promise<void> {
  const run = useQueryRun.getState().get(tabId);
  if (!canStop(run)) return;
  if (!run.runId) {
    // Between `begin` and `attach`: sent as soon as the run id is known.
    pendingStop.add(tabId);
    return;
  }
  await cancelRun(tabId, run.runId);
}

/** Stops the tab's run and waits for it to end; false when it is still running after `STOP_WAIT_MS`. */
function stopAndWait(tabId: string): Promise<boolean> {
  if (canRun(useQueryRun.getState().get(tabId))) return Promise.resolve(true);
  return new Promise((resolve) => {
    const finish = (ok: boolean) => {
      clearTimeout(timer);
      unsubscribe();
      resolve(ok);
    };
    const unsubscribe = useQueryRun.subscribe((s) => {
      if (canRun(s.runs.get(tabId) ?? useQueryRun.getState().get(tabId))) finish(true);
    });
    const timer = setTimeout(() => finish(false), STOP_WAIT_MS);
    void stopQuery(tabId);
  });
}

// Closing a tab asks first while its run is active, then stops it and waits.
setRunningSource(
  (tabId) => !canRun(useQueryRun.getState().get(tabId)),
  async (tabId) => {
    if (await stopAndWait(tabId)) return true;
    useDialogs.getState().setNotice('Could not stop the query; the tab stays open.');
    return false;
  },
);

// Retry statement and Fetch all (rerun.ts) run their derived plans through this flow, so a
// Confirm verdict asks and a Block verdict is refused exactly as for a normal run.
setPlanRunner((tabId, plan, opts) => runPlanned(tabId, plan, { maxRows: opts.maxRows }));

/** Commit / Rollback from the status bar. */
export async function txAction(tabId: string, action: TxAction): Promise<void> {
  try {
    await settleTx(tabId, action);
  } catch (e) {
    const error = toAppError(e);
    note(tabId, error.message, error);
  }
}
