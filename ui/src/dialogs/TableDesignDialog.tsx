import { useEffect, useRef, useState } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { api, toAppError } from '../api';
import type { AppError, CountedImpact, DesignPreview, DesignReport, Impact } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { formatCompact } from '../lib/plural';
import { invalidateCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { invalidateObjectData } from '../store/objectData';
import { invalidateTableData } from '../store/tableData';
import { isDesignLost, useTableDesigns } from '../store/tableDesigns';
import { designTabId, objectTabId, useTabs, type DesignTabRef, type ObjectTabRef } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { reportGridSelection } from '../store/gridSelection';
import { SqlPreview } from '../tabs/designer/SqlPreview';
import { Modal } from './Modal';
import './tableDesign.css';

export const LOST_MESSAGE = 'The connection to this table was lost. Your changes are kept; reopen the design to continue.';
export const STALE_MESSAGE = 'The table changed since you opened it.';
const UNKNOWN_MESSAGE = 'The result is unknown. The table structure was reloaded; check it before continuing.';
const UNKNOWN_NEW_MESSAGE = 'The result is unknown. The table may or may not have been created; check the connection tree before continuing.';
const NOT_APPLIED: DesignReport = { outcome: 'not_applied', error: null, warnings: [], statements_sent: 0 };

const isStale = (e: AppError) => e.kind === 'plan_stale';

/**
 * An impact line with its counted rows ("412 rows have a value") or "Not counted"; a rebuild
 * warning with the table's row estimate ("≈ 1.2M rows").
 */
function describe({ impact, rows, counted }: CountedImpact, estimate: number | null): Impact {
  if (impact.kind === 'rebuild' && estimate !== null) {
    return { ...impact, message: `${impact.message} (≈ ${formatCompact(estimate)} ${estimate === 1 ? 'row' : 'rows'})` };
  }
  if (impact.count_sql === null) return impact;
  const count = counted && rows !== null ? `${rows} ${rows === 1 ? 'row has' : 'rows have'} a value` : 'Not counted';
  return { ...impact, message: `${impact.message} (${count})` };
}

/**
 * After DDL was sent: forget what is cached for the table, follow a rename in the open object tabs
 * (an unknown result only marks them for verification), refresh the connection's tree node.
 * Returns the tree refresh error, if any.
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
  // The design tab is re-keyed when its table is created or renamed: follow it.
  const [id, setId] = useState(tabId);
  const tab = useTabs((s) => s.tabs.find((t): t is DesignTabRef => t.kind === 'design' && t.id === id));
  const design = useTableDesigns((s) => s.byTab[id]);
  const cfg = useConnections((s) => (tab ? s.summaries.get(tab.connectionId)?.config : undefined));
  const [preview, setPreview] = useState<DesignPreview | null>(null);
  const [report, setReport] = useState<DesignReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState(false);
  const [loading, setLoading] = useState(true);
  const [applying, setApplying] = useState(false);
  const [stage, setStage] = useState('');
  const [typed, setTyped] = useState('');
  const [unknownText, setUnknownText] = useState(UNKNOWN_MESSAGE);
  const operation = useRef<string | null>(null);
  const inflight = useRef(false);
  const busy = loading || applying;
  const engine = design?.open?.server.kind;
  const env = design?.open?.env ?? cfg?.env_tag ?? null;

  const lost = () => {
    useTableDesigns.getState().markLost(id);
    useDialogs.getState().setNotice(LOST_MESSAGE, 'error');
    onClose();
  };

  /** A failed command: a lost design closes the dialog, a stale plan offers Reopen, anything else is shown. */
  const fail = (e: AppError) => {
    if (isDesignLost(e)) return lost();
    if (isStale(e)) {
      const draft = useTableDesigns.getState().byTab[id]?.draft ?? null;
      useTableDesigns.getState().setStaleDraft(id, draft);
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
      setLoading(false);
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
        if (live) setLoading(false);
      });
    return () => {
      live = false;
    };
    // The review is of the draft as it is when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Closing during the preview is fine (nothing is written); not while an apply runs.
  const close = () => {
    if (!applying) onClose();
  };

  /**
   * Opens the design again under the first of `names` that opens, and moves the tab to that table's
   * id when it differs (a created or renamed table). A tab that already designs the target keeps its
   * draft: it is focused and this one, just applied, is closed.
   */
  const follow = async (current: DesignTabRef, names: string[], merge: boolean) => {
    const designs = useTableDesigns.getState();
    const target = names[0];
    const next = target === undefined ? id : designTabId(current.connectionId, current.scope, target);
    if (merge && next !== id && useTabs.getState().tabs.some((t) => t.id === next)) {
      useTabs.getState().activate(next);
      useTabs.getState().close(id);
      return;
    }
    let opened: string | null = null;
    for (const name of names) {
      await designs.load(id, current.connectionId, current.scope, name);
      if (!useTableDesigns.getState().byTab[id]?.error) {
        opened = name;
        break;
      }
    }
    if (opened === null || opened === current.table) return;
    const moved = designTabId(current.connectionId, current.scope, opened);
    if (useTabs.getState().tabs.some((t) => t.id === moved)) {
      useTabs.getState().activate(moved);
      useTabs.getState().close(id);
      return;
    }
    designs.rekey(id, moved);
    useTabs.getState().moveDesignTab(id, opened);
    setId(moved);
  };

  const apply = async () => {
    const state = useTableDesigns.getState().byTab[id];
    if (inflight.current || !preview || busy || !tab || !state?.draft || !state.open) return;
    inflight.current = true;
    const to = state.draft.name;
    const from = tab.table;
    setApplying(true);
    setError(null);
    const operationId = crypto.randomUUID();
    operation.current = operationId;
    useDialogs.setState({ tableDesignRunning: true });
    let result: DesignReport | null = null;
    try {
      const channel = new Channel<string>();
      channel.onmessage = setStage;
      result = await api.tableDesignApply(preview.plan_id, true, typed || null, operationId, channel);
    } catch (e) {
      // Nothing was sent (a lock wait, a lost session, a stale plan): the draft is untouched.
      setPreview(null);
      setReport(NOT_APPLIED);
      fail(toAppError(e));
    } finally {
      operation.current = null;
      useDialogs.setState({ tableDesignRunning: false });
    }
    if (result) {
      setPreview(null);
      setReport(result);
      const reportError = result.error;
      if (result.outcome === 'unknown') {
        // Whatever the error says (the connection dropped, the plan went stale after a send), DDL may have run.
        if (reportError) setError(reportError.message);
        setUnknownText(from === null ? UNKNOWN_NEW_MESSAGE : UNKNOWN_MESSAGE);
        const refreshError = await refreshAfterApply(tab, from, to, true).catch((e) => toAppError(e).message);
        if (refreshError) setError(`Changes may have been sent; metadata refresh failed: ${refreshError}`);
        // A rename may have gone through: the new name first, then the old one. A table never created has nothing to reload.
        if (from !== null) await follow(tab, to !== from ? [to, from] : [from], false);
      } else if (result.outcome === 'applied') {
        if (reportError) setError(reportError.message);
        useTableDesigns.getState().setStaleDraft(id, null);
        const refreshError = await refreshAfterApply(tab, from, to, false).catch((e) => toAppError(e).message);
        if (refreshError) setError(`Changes were applied; metadata refresh failed: ${refreshError}`);
        await follow(tab, [to], true);
      } else if (reportError) {
        // Not applied / rolled back: the draft stays. Lost or stale routes only when nothing was sent.
        if (result.statements_sent === 0 && (isDesignLost(reportError) || isStale(reportError))) fail(reportError);
        else setError(reportError.message);
      }
    }
    inflight.current = false;
    setApplying(false);
    setStage('');
    if (result?.outcome === 'applied' && !result.error && result.warnings.length === 0) {
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
    void useTableDesigns.getState().reload(id);
    onClose();
  };

  const production = env === 'production';
  const locked = preview !== null && preview.confirm_name !== null && typed !== preview.confirm_name;
  const outcome = report
    ? report.outcome === 'applied'
      ? 'Changes applied.'
      : report.outcome === 'unknown'
        ? unknownText
        : report.outcome === 'rolled_back'
          ? 'The changes were rolled back. Your draft is kept.'
          : 'The changes were not applied. Your draft is kept.'
    : null;

  return (
    <Modal
      title="Review & apply"
      badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}
      onClose={close}
      closable={!applying}
      className={`wide table-design-dialog${production ? ' env-production' : ''}`}
    >
      <div className="modal-body">
        <p>
          {cfg?.name ?? tab?.connectionId} · {tab?.table ?? design?.draft?.name ?? 'new table'}
        </p>
        {preview ? (
          <>
            <SqlPreview statements={preview.statements} impacts={preview.impacts.map((i) => describe(i, preview.row_estimate))} engine={engine} />
            {preview.confirm_name !== null ? (
              <label>
                Type {preview.confirm_name} to apply
                <input aria-label="Confirmation name" value={typed} disabled={busy} autoComplete="off" onChange={(e) => setTyped(e.target.value)} />
              </label>
            ) : null}
          </>
        ) : null}
        {loading ? <p role="status">Preparing the review…</p> : null}
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
        <button type="button" data-cancel className="btn" disabled={applying} onClick={close}>
          Close
        </button>
        {applying && operation.current ? (
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
