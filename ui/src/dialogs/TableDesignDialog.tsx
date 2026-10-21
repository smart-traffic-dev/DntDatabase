import { useEffect, useRef, useState } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { api, toAppError } from '../api';
import type { AppError, CountedImpact, DesignPreview, DesignReport, Impact } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { invalidateCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { invalidateObjectData } from '../store/objectData';
import { invalidateTableData } from '../store/tableData';
import { isDesignLost, useTableDesigns } from '../store/tableDesigns';
import { objectTabId, useTabs, type DesignTabRef, type ObjectTabRef } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { reportGridSelection } from '../store/gridSelection';
import { SqlPreview } from '../tabs/designer/SqlPreview';
import { Modal } from './Modal';
import './tableDesign.css';

export const LOST_MESSAGE = 'The connection to this table was lost. Your changes are kept; reopen the design to continue.';
export const STALE_MESSAGE = 'The table changed since you opened it.';
const UNKNOWN_MESSAGE = 'The result is unknown. The table structure was reloaded; check it before continuing.';

const isStale = (e: AppError) => e.kind === 'plan_stale';

/** An impact line with its counted rows ("412 rows have a value") or "Not counted". */
function describe({ impact, rows, counted }: CountedImpact): Impact {
  if (impact.count_sql === null) return impact;
  const count = counted && rows !== null ? `${rows} ${rows === 1 ? 'row has' : 'rows have'} a value` : 'Not counted';
  return { ...impact, message: `${impact.message} (${count})` };
}

/**
 * After a statement was sent: forget what is cached for the table, follow a rename in the open object
 * tabs, refresh the connection's tree node. Returns the tree refresh error, if any.
 */
async function refreshAfterApply(tab: DesignTabRef, from: string | null, to: string, unknown: boolean): Promise<string | null> {
  const replacements: Record<string, ObjectTabRef | null> = {};
  for (const t of useTabs.getState().tabs) {
    if (t.kind !== 'object' || t.connectionId !== tab.connectionId || from === null) continue;
    const ref = t.objectRef;
    if (ref.database !== tab.scope.database || ref.schema !== tab.scope.schema || ref.name !== from) continue;
    invalidateObjectData(t.id);
    invalidateTableData(t.id);
    reportGridSelection(t.id, null);
    const fresh = { ...t, resourceRevision: (t.resourceRevision ?? 0) + 1 };
    if (unknown || to === from) replacements[t.id] = { ...fresh, needsVerification: unknown };
    else {
      const objectRef = { ...ref, name: to };
      replacements[t.id] = { ...fresh, id: objectTabId(tab.connectionId, objectRef), objectRef, title: to, needsVerification: false };
    }
  }
  useTabs.getState().replaceObjectTabs(replacements);
  invalidateCompletion(tab.connectionId);
  await useTree.getState().refresh(connNodeId(tab.connectionId));
  return useTree.getState().nodes.get(connNodeId(tab.connectionId))?.error?.message ?? null;
}

export function TableDesignDialog({ tabId, onClose }: { tabId: string; onClose(): void }) {
  const tab = useTabs((s) => s.tabs.find((t): t is DesignTabRef => t.kind === 'design' && t.id === tabId));
  const design = useTableDesigns((s) => s.byTab[tabId]);
  const cfg = useConnections((s) => (tab ? s.summaries.get(tab.connectionId)?.config : undefined));
  const [preview, setPreview] = useState<DesignPreview | null>(null);
  const [report, setReport] = useState<DesignReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState(false);
  const [busy, setBusy] = useState(true);
  const [stage, setStage] = useState('');
  const [typed, setTyped] = useState('');
  const operation = useRef<string | null>(null);
  const engine = design?.open?.server.kind;
  const env = design?.open?.env ?? cfg?.env_tag ?? null;

  const lost = () => {
    useTableDesigns.getState().markLost(tabId);
    useDialogs.getState().setNotice(LOST_MESSAGE, 'error');
    onClose();
  };

  /** A failed command: a lost design closes the dialog, a stale plan offers Reopen, anything else is shown. */
  const fail = (e: AppError) => {
    if (isDesignLost(e)) return lost();
    if (isStale(e)) {
      const draft = useTableDesigns.getState().byTab[tabId]?.draft ?? null;
      useTableDesigns.getState().setStaleDraft(tabId, draft);
      setStale(true);
      setError(STALE_MESSAGE);
      return;
    }
    setError(e.message);
  };

  useEffect(() => {
    const state = useTableDesigns.getState().byTab[tabId];
    let live = true;
    if (!state?.open || !state.draft) {
      setBusy(false);
      setError('The design is not open.');
      return;
    }
    api
      .tableDesignPreview(state.open.design_id, state.draft)
      .then((plan) => {
        if (live) setPreview(plan);
      })
      .catch((e) => {
        if (live) fail(toAppError(e));
      })
      .finally(() => {
        if (live) setBusy(false);
      });
    return () => {
      live = false;
    };
    // The review is of the draft as it is when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const close = () => {
    if (!busy) onClose();
  };

  const apply = async () => {
    const state = useTableDesigns.getState().byTab[tabId];
    if (!preview || busy || !tab || !state?.draft || !state.open) return;
    const to = state.draft.name;
    const from = tab.table;
    setBusy(true);
    setError(null);
    const id = crypto.randomUUID();
    operation.current = id;
    useDialogs.setState({ tableDesignRunning: true });
    let result: DesignReport | null = null;
    try {
      const channel = new Channel<string>();
      channel.onmessage = setStage;
      result = await api.tableDesignApply(preview.plan_id, true, typed || null, id, channel);
    } catch (e) {
      setPreview(null);
      fail(toAppError(e));
    } finally {
      operation.current = null;
      useDialogs.setState({ tableDesignRunning: false });
    }
    if (!result) {
      setBusy(false);
      setStage('');
      return;
    }
    setPreview(null);
    setReport(result);
    const reportError = result.error;
    if (reportError && (isDesignLost(reportError) || isStale(reportError))) {
      setBusy(false);
      setStage('');
      return fail(reportError);
    }
    if (reportError) setError(reportError.message);
    const sent = result.outcome === 'applied' || result.outcome === 'unknown' || (result.outcome === 'rolled_back' && result.statements_sent > 0);
    if (sent) {
      let refreshError: string | null;
      try {
        refreshError = await refreshAfterApply(tab, from, to, result.outcome === 'unknown');
      } catch (e) {
        refreshError = toAppError(e).message;
      }
      if (refreshError) setError(`Changes were sent; metadata refresh failed: ${refreshError}`);
      // The designer follows the table: a new or renamed table is opened under its new name.
      const applied = result.outcome === 'applied';
      if (applied && to !== tab.table) {
        useTabs.setState((s) => ({ tabs: s.tabs.map((t) => (t.id === tabId && t.kind === 'design' ? { ...t, table: to, title: to } : t)) }));
        await useTableDesigns.getState().load(tabId, tab.connectionId, tab.scope, to);
      } else if (tab.table !== null) await useTableDesigns.getState().reload(tabId);
    }
    setBusy(false);
    setStage('');
    if (result.outcome === 'applied' && !reportError && result.warnings.length === 0) {
      useDialogs.getState().setNotice('Changes applied.', 'success');
      onClose();
    }
  };

  const stop = async () => {
    if (!operation.current) return;
    setStage('stopping');
    try {
      await api.tableDesignCancel(operation.current);
    } catch (e) {
      setError(`Could not request Stop: ${toAppError(e).message}`);
    }
  };

  const reopen = () => {
    void useTableDesigns.getState().reload(tabId);
    onClose();
  };

  const production = env === 'production';
  const locked = preview !== null && preview.confirm_name !== null && typed !== preview.confirm_name;
  const outcome = report
    ? report.outcome === 'applied'
      ? 'Changes applied.'
      : report.outcome === 'unknown'
        ? UNKNOWN_MESSAGE
        : report.outcome === 'rolled_back'
          ? 'The changes were rolled back. Your draft is kept.'
          : 'The changes were not applied. Your draft is kept.'
    : null;

  return (
    <Modal
      title="Review & apply"
      badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}
      onClose={close}
      closable={!busy}
      className={`wide table-design-dialog${production ? ' env-production' : ''}`}
    >
      <div className="modal-body">
        <p>
          {cfg?.name ?? tab?.connectionId} · {tab?.table ?? design?.draft?.name ?? 'new table'}
        </p>
        {preview ? (
          <>
            <SqlPreview statements={preview.statements} impacts={preview.impacts.map(describe)} engine={engine} />
            {preview.confirm_name !== null ? (
              <label>
                Type {preview.confirm_name} to apply
                <input aria-label="Confirmation name" value={typed} disabled={busy} autoComplete="off" onChange={(e) => setTyped(e.target.value)} />
              </label>
            ) : null}
          </>
        ) : null}
        {busy && !preview && !report && !stage ? <p role="status">Preparing the review…</p> : null}
        {error ? <p role="alert">{error}</p> : null}
        {outcome ? (
          <div role="status">
            <p>{outcome}</p>
            {report?.warnings.map((w) => <p key={w}>{w}</p>)}
          </div>
        ) : null}
        {stage ? <p role="status">{stage}… Stop does not guarantee undoing DDL already sent.</p> : null}
      </div>
      <footer className="modal-foot">
        <button type="button" data-cancel className="btn" disabled={busy} onClick={close}>
          Close
        </button>
        {busy && operation.current ? (
          <button type="button" className="btn" onClick={() => void stop()}>
            Stop
          </button>
        ) : null}
        {stale ? (
          <button type="button" className="btn primary" onClick={reopen}>
            Reopen
          </button>
        ) : null}
        {preview ? (
          <button type="button" className="btn primary" disabled={busy || locked} onClick={() => void apply()}>
            Apply
          </button>
        ) : null}
      </footer>
    </Modal>
  );
}
