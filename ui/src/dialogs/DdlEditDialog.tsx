import { useEffect, useRef, useState } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { api, needsReconnect, toAppError } from '../api';
import type { AppError, DdlEditPreview, DdlEditReport, ObjectState } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { sqlEngine } from '../query/engine';
import { invalidateCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { useDdlEdits, type DdlTarget } from '../store/ddlEdits';
import { useDialogs } from '../store/dialogs';
import { reportGridSelection } from '../store/gridSelection';
import { invalidateObjectData } from '../store/objectData';
import { invalidateTableData } from '../store/tableData';
import { useTabs, type ObjectTabRef } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { DDL_EDIT_PG_HEADER, SqlPreview } from '../tabs/designer/SqlPreview';
import { Modal } from './Modal';
import './ddlEdit.css';

export const DDL_LOST_MESSAGE = 'The connection to this object was lost. Your text is kept; reopen the edit to continue.';
export const DDL_STALE_MESSAGE = 'The object changed since you opened it.';
/** The backend's `plan_stale` reason when the object itself changed (`ddl_edit::OBJECT_CHANGED`). */
const OBJECT_CHANGED_REASON = 'The object changed since the editor opened it.';
const UNKNOWN_MESSAGE = 'The result is unknown; check the object before continuing.';
/** The warning of a non-transactional DROP + CREATE (`build.rs`): shown in danger colour. */
const GAP_PREFIX = 'Between the DROP and the CREATE';
const NOT_APPLIED: DdlEditReport = {
  outcome: 'not_applied',
  error: null,
  warnings: [],
  statements_sent: 0,
  executed: [],
  failed: null,
  object_state: 'unchanged',
  restore_text: null,
  object: null,
  text: null,
};

const OUTCOME_TEXT: Record<DdlEditReport['outcome'], string> = {
  applied: 'Changes applied.',
  not_applied: 'The changes were not applied. Your text is kept.',
  rolled_back: 'The changes were rolled back. Your text is kept.',
  partial: 'The changes were applied only in part.',
  unknown: UNKNOWN_MESSAGE,
};

const STATE_TEXT: Record<ObjectState, string> = {
  unchanged: 'The object is unchanged.',
  changed: 'The object was changed: it is neither as it was nor as your text defines it.',
  missing: 'The object no longer exists on the server.',
  unknown: 'The state of the object is unknown.',
};

/** The edit is gone: its backend edit was closed, or the connection needs a reconnect. */
export const isDdlEditLost = (e: AppError) => (e.kind === 'config' && e.field === 'edit_id') || needsReconnect(e);

/** An error as shown: an unsupported feature adds what is missing and the version that has it. */
const messageOf = (e: AppError) => (e.kind === 'unsupported' && e.detail ? `${e.message} ${e.detail}` : e.message);

/**
 * After DDL was sent: forget what is cached for the object's open object tabs and bump their
 * resource revision (they reload the DDL; an unknown result marks them for verification),
 * invalidate completion and refresh the connection's tree node. Returns the tree refresh error.
 */
async function refreshAfterApply(target: DdlTarget, name: string | null, unknown: boolean): Promise<string | null> {
  const replacements: Record<string, ObjectTabRef | null> = {};
  for (const t of useTabs.getState().tabs) {
    if (t.kind !== 'object' || t.connectionId !== target.connectionId || name === null) continue;
    const ref = t.objectRef;
    if (ref.database !== target.scope.database || (ref.schema ?? null) !== (target.scope.schema ?? null)) continue;
    if (ref.name !== name || ref.kind !== target.kind) continue;
    invalidateObjectData(t.id);
    invalidateTableData(t.id);
    reportGridSelection(t.id, null);
    replacements[t.id] = { ...t, resourceRevision: (t.resourceRevision ?? 0) + 1, needsVerification: unknown };
  }
  useTabs.getState().replaceObjectTabs(replacements);
  invalidateCompletion(target.connectionId);
  await useTree.getState().refresh(connNodeId(target.connectionId));
  return useTree.getState().nodes.get(connNodeId(target.connectionId))?.error?.message ?? null;
}

/**
 * Review & apply of a DDL editor tab: previews the text as it is when the dialog opens, shows the
 * statements exactly as they are sent, applies the stored plan and reports the outcome.
 */
export function DdlEditDialog({ tabId, onClose }: { tabId: string; onClose(): void }) {
  const edit = useDdlEdits((s) => s.byTab[tabId]);
  const connectionId = edit?.target.connectionId;
  const cfg = useConnections((s) => (connectionId ? s.summaries.get(connectionId)?.config : undefined));
  const [preview, setPreview] = useState<DdlEditPreview | null>(null);
  const [report, setReport] = useState<DdlEditReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stale, setStale] = useState(false);
  const [expired, setExpired] = useState(false);
  const [loading, setLoading] = useState(true);
  const [applying, setApplying] = useState(false);
  const [stage, setStage] = useState('');
  const [typed, setTyped] = useState('');
  const operation = useRef<string | null>(null);
  const inflight = useRef(false);
  const mounted = useRef(true);
  const previewSeq = useRef(0);
  const busy = loading || applying;
  const env = edit?.open?.env ?? cfg?.env_tag ?? null;
  const engine = sqlEngine(edit?.open?.server.kind) ?? 'mysql';

  const lost = () => {
    useDdlEdits.getState().markLost(tabId);
    useDialogs.getState().setNotice(DDL_LOST_MESSAGE, 'error');
    onClose();
  };

  /**
   * A failed command: a lost edit closes the dialog, a stale plan offers Reopen, an expired plan
   * offers Preview again, anything else (a validation error) is shown.
   */
  const fail = (e: AppError) => {
    if (isDdlEditLost(e)) return lost();
    if (e.kind === 'plan_expired') setExpired(true);
    if (e.kind === 'plan_stale') {
      useDdlEdits.getState().markStale(tabId);
      setStale(true);
      // Another staleness (the connection, its policy, the dialect) says what changed.
      setError(!e.detail || e.detail === OBJECT_CHANGED_REASON ? DDL_STALE_MESSAGE : e.detail);
      return;
    }
    setError(messageOf(e));
  };

  /** Previews the text as it is now; only the newest preview of a mounted dialog lands. */
  const runPreview = () => {
    const seq = ++previewSeq.current;
    const current = () => mounted.current && previewSeq.current === seq;
    const state = useDdlEdits.getState().byTab[tabId];
    setExpired(false);
    setReport(null);
    setTyped('');
    if (!state?.open) {
      setLoading(false);
      setError('The edit is not open.');
      return;
    }
    setError(null);
    setLoading(true);
    api
      .ddlEditPreview(state.open.edit_id, state.text, state.strategy)
      .then((plan) => {
        if (current()) setPreview(plan);
      })
      .catch((e) => {
        if (current()) fail(toAppError(e));
      })
      .finally(() => {
        if (current()) setLoading(false);
      });
  };

  useEffect(() => {
    mounted.current = true;
    runPreview();
    return () => {
      mounted.current = false;
    };
    // The review is of the text as it is when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Closing during the preview is fine (nothing is written); not while an apply runs.
  const close = () => {
    if (!applying) onClose();
  };

  const apply = async () => {
    const state = useDdlEdits.getState().byTab[tabId];
    if (inflight.current || !preview || busy || !state?.open) return;
    inflight.current = true;
    const target = state.target;
    setApplying(true);
    setError(null);
    const operationId = crypto.randomUUID();
    operation.current = operationId;
    useDialogs.setState({ ddlEditRunning: true });
    let result: DdlEditReport | null = null;
    try {
      const channel = new Channel<string>();
      channel.onmessage = setStage;
      result = await api.ddlEditApply(preview.plan_id, true, typed || null, operationId, channel);
    } catch (e) {
      // Nothing was sent (a lock wait, a lost session, a stale plan): the text is untouched.
      setPreview(null);
      setReport(NOT_APPLIED);
      fail(toAppError(e));
    } finally {
      operation.current = null;
      useDialogs.setState({ ddlEditRunning: false });
    }
    if (result) {
      setPreview(null);
      setReport(result);
      // The backend made the edit a create of the same name: so does the tab, restore click or not.
      if (result.object_state === 'missing') useDdlEdits.getState().missing(tabId, result.restore_text);
      const reportError = result.error;
      const name = result.object?.name ?? target.name;
      if (result.outcome === 'applied') {
        if (reportError) setError(messageOf(reportError));
        void useDdlEdits.getState().applied(tabId, result);
        const refreshError = await refreshAfterApply(target, name, false).catch((e) => toAppError(e).message);
        if (refreshError) setError(`Changes were applied; metadata refresh failed: ${refreshError}`);
      } else if (result.outcome === 'unknown' || result.outcome === 'partial') {
        // Statements ran (or may have): whatever the error says, the object is not as before.
        const unknown = result.outcome === 'unknown';
        if (reportError) setError(messageOf(reportError));
        const refreshError = await refreshAfterApply(target, name, unknown).catch((e) => toAppError(e).message);
        if (refreshError) setError(`Changes may have been sent; metadata refresh failed: ${refreshError}`);
      } else if (reportError) {
        // Not applied / rolled back: the text stays. Lost or stale routes only when nothing was sent.
        if (result.statements_sent === 0 && (isDdlEditLost(reportError) || reportError.kind === 'plan_stale')) fail(reportError);
        else setError(messageOf(reportError));
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
      await api.ddlEditCancel(operation.current);
    } catch (e) {
      setError(`Could not request Stop: ${toAppError(e).message}`);
    }
  };

  const reopen = () => {
    useDdlEdits.getState().markStale(tabId);
    void useDdlEdits.getState().reopen(tabId);
    onClose();
  };

  const restore = (text: string) => {
    useDdlEdits.getState().restore(tabId, text);
    onClose();
  };

  const production = env === 'production';
  const locked = preview !== null && preview.confirm_name !== null && typed !== preview.confirm_name;
  const objectName = edit?.target.name ?? edit?.missing?.name ?? 'new object';
  const failedList = report && (report.executed.length > 0 || report.failed !== null);

  return (
    <Modal
      title="Review & apply"
      badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}
      onClose={close}
      closable={!applying}
      className={`wide ddl-edit-dialog${production ? ' env-production' : ''}`}
    >
      <div className="modal-body">
        <p>
          {cfg?.name ?? connectionId} · {objectName}
        </p>
        {preview ? (
          <>
            <SqlPreview
              statements={preview.statements}
              impacts={[]}
              showImpacts={false}
              engine={engine}
              script={{ pgHeader: DDL_EDIT_PG_HEADER, delimited: engine === 'mysql' && edit?.target.kind !== 'View' }}
            />
            <p>{preview.transactional ? 'Runs in one transaction' : 'Not transactional: statements run one by one'}</p>
            {preview.warnings.length > 0 ? (
              <ul className="ddl-edit-list" aria-label="Warnings">
                {preview.warnings.map((w) => (
                  <li key={w} className={preview.gap && w.startsWith(GAP_PREFIX) ? 'danger' : undefined}>
                    {w}
                  </li>
                ))}
              </ul>
            ) : null}
            {preview.checks.length > 0 ? (
              <ul className="ddl-edit-list" aria-label="Checks before commit">
                {preview.checks.map((c) => (
                  <li key={c}>{c}</li>
                ))}
              </ul>
            ) : null}
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
        {report ? (
          <div role="status">
            <p>{OUTCOME_TEXT[report.outcome]}</p>
            {report.outcome === 'partial' ? <p>{STATE_TEXT[report.object_state]}</p> : null}
            {failedList ? (
              <div className="ddl-edit-stmts">
                {report.executed.length > 0 ? (
                  <>
                    <strong>Executed</strong>
                    <ol className="ddl-edit-list" aria-label="Executed statements">
                      {report.executed.map((s, i) => (
                        <li key={i}>
                          <code>{s}</code>
                        </li>
                      ))}
                    </ol>
                  </>
                ) : null}
                {report.failed !== null ? (
                  <>
                    <strong>Failed</strong>
                    <p aria-label="Failed statement">
                      <code>{report.failed}</code>
                    </p>
                  </>
                ) : null}
              </div>
            ) : null}
            {report.warnings.map((w) => (
              <p key={w}>{w}</p>
            ))}
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
        {report?.restore_text != null && !applying ? (
          <button type="button" className="btn primary" onClick={() => restore(report.restore_text!)}>
            Put the original definition in the editor
          </button>
        ) : null}
        {stale ? (
          <button type="button" className="btn primary" onClick={reopen}>
            Reopen
          </button>
        ) : null}
        {expired && !busy ? (
          <button type="button" className="btn primary" onClick={runPreview}>
            Preview again
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
