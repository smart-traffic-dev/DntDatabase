// Starting and stopping a query run (spec §5.2, §6.2): plan → guard dialogs → `query_run`.
import { create } from 'zustand';
import { api, toAppError } from '../api';
import { runChannel } from '../api/query';
import type { PlanView, Target, TxAction } from '../bindings';
import { useConnections } from './connections';
import { exportRunOf, useExporting } from './queryExport';
import { canRun, canStop, settleTx, useQueryRun, type MessageLine } from './queryRun';
import { setRunningSource, useQueryDocs } from './queryTabs';
import { promptHandles, reopenConnection } from './reopen';
import { askRunConfirmation, showBlocked } from './runPrompt';
import { setPlanRunner } from '../tabs/query/rerun';

/** How long "Stop and close" waits for the run to end. */
export const STOP_WAIT_MS = 30_000;

/** A store holding a set of tab ids, and its setter: a new Set only when membership changes. */
function tabIdSet() {
  const store = create<{ ids: Set<string> }>()(() => ({ ids: new Set() }));
  const toggle = (tabId: string, on: boolean) =>
    store.setState((s) => {
      if (s.ids.has(tabId) === on) return s;
      const ids = new Set(s.ids);
      if (on) ids.add(tabId);
      else ids.delete(tabId);
      return { ids };
    });
  return [store, toggle] as const;
}

/** Tabs between the Run click and the run being under way (planning, dialogs, `query_run`). */
const [startingStore, setStarting] = tabIdSet();
export const useStarting = startingStore;
const isStarting = (tabId: string) => useStarting.getState().ids.has(tabId);

/** Whether a run may start on the tab: nothing starting, running or exporting there. */
const mayStart = (tabId: string) =>
  !isStarting(tabId) && canRun(useQueryRun.getState().get(tabId)) && exportRunOf(tabId) === null;

/** Tabs whose editor has selected text (Run selection needs one). */
export const [useSelection, setHasSelection] = tabIdSet();

/** Tabs whose Stop came before `query_run` returned the run id. */
const pendingStop = new Set<string>();

interface Pos {
  lineNumber: number;
  column: number;
}

interface EditorRange {
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
}

/** The part of a Monaco editor the run keys (and `revealInEditor`) need. */
export interface EditorLike {
  getModel(): { getOffsetAt(p: Pos): number; getPositionAt?(offset: number): Pos } | null;
  getSelection(): { isEmpty(): boolean; getStartPosition(): Pos; getEndPosition(): Pos } | null;
  getPosition(): Pos | null;
  executeEdits?(
    source: string,
    edits: {
      range: EditorRange;
      text: string;
      forceMoveMarkers?: boolean;
    }[],
  ): unknown;
  setSelection?(range: EditorRange): void;
  revealRangeInCenterIfOutsideViewport?(range: EditorRange): void;
  focus?(): void;
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
 * Click-to-jump (F25): selects UTF-16 offsets `from`..`to` of the tab's editor text (a caret
 * at `from` without `to`, e.g. an error position), scrolls it into view and focuses the
 * editor (`focus: false` to keep focus where it is). Offsets are those of `PlanStmtView.range`,
 * `MessageLine.retryRange` and error markers. Returns false when no editor is mounted; the
 * tab's stored cursor then moves to `from`.
 */
export function revealInEditor(
  tabId: string,
  range: { from: number; to?: number },
  opts: { focus?: boolean } = {},
): boolean {
  const from = Math.max(0, range.from);
  const to = Math.max(from, range.to ?? from);
  const editor = editors.get(tabId);
  const model = editor?.getModel();
  if (editor && model?.getPositionAt && editor.setSelection) {
    const start = model.getPositionAt(from);
    const end = model.getPositionAt(to);
    const r: EditorRange = {
      startLineNumber: start.lineNumber,
      startColumn: start.column,
      endLineNumber: end.lineNumber,
      endColumn: end.column,
    };
    editor.setSelection(r);
    editor.revealRangeInCenterIfOutsideViewport?.(r);
    if (opts.focus ?? true) editor.focus?.();
    return true;
  }
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (doc) useQueryDocs.getState().update(tabId, { cursor: Math.min(from, doc.sql.length) });
  return false;
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
    statement: null,
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
async function execute(
  tabId: string,
  plan: PlanView,
  extra?: { maxRows?: number; mode?: 'run' | 'explain' },
): Promise<void> {
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
  useQueryRun.getState().begin(tabId, plan, { mode: extra?.mode });
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

/** The tab's connection id once it is open (reopening it if needed); null when the run cannot go on. */
async function openedConnection(tabId: string, connectionId: string | null): Promise<string | null> {
  if (!connectionId) {
    note(tabId, 'Choose a connection first.');
    return null;
  }
  if (!useConnections.getState().info.has(connectionId)) {
    const failure = await reopenConnection(connectionId);
    if (failure) {
      // A password / host key prompt takes over; any other failure is shown here.
      if (!promptHandles(failure)) note(tabId, failure.message, failure);
      return null;
    }
  }
  return connectionId;
}

/** Runs the tab's `target`: opens the connection, plans, asks the guards, runs. */
export async function runQuery(tabId: string, target: Target, extra?: { maxRows?: number }): Promise<void> {
  if (!mayStart(tabId)) return;
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (!doc) return;
  setStarting(tabId, true);
  try {
    const connectionId = await openedConnection(tabId, doc.connectionId);
    if (!connectionId) return;
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

/** Sets the Explain panel's refusal text (null clears it) and, for a refusal, shows that panel. */
function setExplainError(tabId: string, text: string | null) {
  const run = useQueryRun.getState().get(tabId);
  if (run.explainError === text && (text === null || run.panel === 'explain')) return;
  useQueryRun.setState((s) => ({
    runs: new Map(s.runs).set(tabId, { ...run, explainError: text, panel: text === null ? run.panel : 'explain' }),
  }));
}

/**
 * Explain (or Explain Analyze) of the statement at the cursor (spec 8.2): `query_explain` builds
 * the plan, which then goes through the normal run flow, so Analyze on Production asks first.
 */
export async function explainQuery(tabId: string, analyze: boolean): Promise<void> {
  if (!mayStart(tabId)) return;
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (!doc) return;
  setStarting(tabId, true);
  try {
    const connectionId = await openedConnection(tabId, doc.connectionId);
    if (!connectionId) return;
    const target = targetOf(tabId, 'cursor');
    const cursor = target === null || target === 'All' ? 0 : 'Cursor' in target ? target.Cursor : target.Selection[0];
    let plan: PlanView;
    try {
      plan = await api.queryExplain(tabId, connectionId, doc.database, doc.sql, cursor, analyze);
    } catch (e) {
      setExplainError(tabId, toAppError(e).message);
      return;
    }
    setExplainError(tabId, null);
    await execute(tabId, plan, { mode: 'explain' });
  } finally {
    setStarting(tabId, false);
  }
}

/** Inserts `text` at the cursor of the tab's editor (replacing the selection). */
export function insertAtCursor(tabId: string, text: string): void {
  const editor = editors.get(tabId);
  const sel = editor?.getSelection();
  const pos = editor?.getPosition();
  if (editor?.executeEdits && (sel || pos)) {
    const start = sel?.getStartPosition() ?? pos!;
    const end = sel?.getEndPosition() ?? pos!;
    editor.executeEdits('history', [
      {
        range: {
          startLineNumber: start.lineNumber,
          startColumn: start.column,
          endLineNumber: end.lineNumber,
          endColumn: end.column,
        },
        text,
        forceMoveMarkers: true,
      },
    ]);
    return;
  }
  // No editor mounted: edit the document at its stored cursor.
  const doc = useQueryDocs.getState().docs.get(tabId);
  if (!doc) return;
  const at = Math.min(Math.max(doc.cursor, 0), doc.sql.length);
  useQueryDocs
    .getState()
    .update(tabId, { sql: doc.sql.slice(0, at) + text + doc.sql.slice(at), cursor: at + text.length, dirty: true });
}

/** Runs a plan that already exists (Retry, Fetch all) through the same guards and run flow. */
export async function runPlanned(tabId: string, plan: PlanView, extra?: { maxRows?: number }): Promise<void> {
  if (!mayStart(tabId)) return;
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

/** Stop: asks the backend to cancel the tab's run (the `stopping` event follows), or its export. */
export async function stopQuery(tabId: string): Promise<void> {
  const exporting = exportRunOf(tabId);
  if (exporting !== null) {
    await cancelRun(tabId, exporting);
    return;
  }
  const run = useQueryRun.getState().get(tabId);
  if (!canStop(run)) return;
  if (!run.runId) {
    // Between `begin` and `attach`: sent as soon as the run id is known.
    pendingStop.add(tabId);
    return;
  }
  await cancelRun(tabId, run.runId);
}

/** Whether the tab neither runs nor exports. */
const tabIdle = (tabId: string) => canRun(useQueryRun.getState().get(tabId)) && exportRunOf(tabId) === null;

/** Stops the tab's run (or export) and waits for it to end; false when it is still running after `STOP_WAIT_MS`. */
function stopAndWait(tabId: string): Promise<boolean> {
  if (tabIdle(tabId)) return Promise.resolve(true);
  return new Promise((resolve) => {
    const finish = (ok: boolean) => {
      clearTimeout(timer);
      unsubscribeRun();
      unsubscribeExport();
      resolve(ok);
    };
    const check = () => {
      if (tabIdle(tabId)) finish(true);
    };
    const unsubscribeRun = useQueryRun.subscribe(check);
    const unsubscribeExport = useExporting.subscribe(check);
    const timer = setTimeout(() => finish(false), STOP_WAIT_MS);
    void stopQuery(tabId);
  });
}

// Closing a tab (or quitting) asks first while its run (or export) is active, then stops it and waits.
setRunningSource((tabId) => !tabIdle(tabId), stopAndWait);

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
