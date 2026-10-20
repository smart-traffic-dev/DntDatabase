// Starting and stopping a query run (spec §5.2, §6.2): plan → guard dialogs → `query_run`.
import { api, toAppError } from '../api';
import { runChannel } from '../api/query';
import type { TxAction, Target } from '../bindings';
import { useConnections } from './connections';
import { canRun, canStop, settleTx, useQueryRun, type MessageLine } from './queryRun';
import { setRunningSource, useQueryDocs } from './queryTabs';
import { promptHandles, reopenConnection } from './reopen';
import { askRunConfirmation, showBlocked } from './runPrompt';

// Closing a tab asks first while its run is active.
setRunningSource((tabId) => !canRun(useQueryRun.getState().get(tabId)));

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
 * The run target of a tab: `selection` is the selected text (the statement at the cursor when
 * nothing is selected), `cursor` the same but ignoring none, `all` the whole tab.
 */
export function targetOf(tabId: string, mode: 'cursor' | 'selection' | 'all'): Target {
  if (mode === 'all') return 'All';
  const editor = editors.get(tabId);
  const model = editor?.getModel();
  if (editor && model) {
    const sel = editor.getSelection();
    if (sel && !sel.isEmpty()) {
      return { Selection: [model.getOffsetAt(sel.getStartPosition()), model.getOffsetAt(sel.getEndPosition())] };
    }
    const pos = editor.getPosition();
    if (pos) return { Cursor: model.getOffsetAt(pos) };
  }
  return { Cursor: useQueryDocs.getState().docs.get(tabId)?.cursor ?? 0 };
}

/** Runs the toolbar / key action of a tab. */
export const runFromEditor = (tabId: string, mode: 'cursor' | 'selection' | 'all'): Promise<void> =>
  runQuery(tabId, targetOf(tabId, mode));

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

export async function runQuery(tabId: string, target: Target, extra?: { maxRows?: number }): Promise<void> {
  const store = useQueryRun.getState();
  if (!canRun(store.get(tabId))) return;
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (!doc) return;
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

  let plan;
  try {
    plan = await api.queryPlan(tabId, connectionId, doc.database, doc.sql, target);
  } catch (e) {
    const error = toAppError(e);
    note(tabId, error.message, error);
    return;
  }
  if (plan.stmts.length === 0) return;
  if (plan.stmts.some((s) => s.verdict === 'block')) {
    await showBlocked(plan);
    return;
  }
  const confirmed = plan.stmts.some((s) => s.verdict === 'confirm');
  if (confirmed && !(await askRunConfirmation(plan))) return;

  // The tab may have been run, or closed, while the dialog was open.
  if (!canRun(useQueryRun.getState().get(tabId)) || !useQueryDocs.getState().docs.has(tabId)) return;
  useQueryRun.getState().begin(tabId, plan);
  try {
    const runId = await api.queryRun(
      plan.plan_id,
      confirmed,
      { max_rows: extra?.maxRows ?? doc.rowLimit, continue_on_error: doc.continueOnError },
      runChannel((ev) => useQueryRun.getState().apply(tabId, ev)),
    );
    useQueryRun.getState().attach(tabId, runId);
  } catch (e) {
    const error = toAppError(e);
    abort(tabId, error.message, error);
  }
}

/** Stop: asks the backend to cancel the tab's run (the `stopping` event follows). */
export async function stopQuery(tabId: string): Promise<void> {
  const run = useQueryRun.getState().get(tabId);
  if (!canStop(run) || !run.runId) return;
  try {
    await api.queryCancel(run.runId);
  } catch (e) {
    const error = toAppError(e);
    note(tabId, error.message, error);
  }
}

/** Commit / Rollback from the status bar. */
export async function txAction(tabId: string, action: TxAction): Promise<void> {
  try {
    await settleTx(tabId, action);
  } catch (e) {
    const error = toAppError(e);
    note(tabId, error.message, error);
  }
}
