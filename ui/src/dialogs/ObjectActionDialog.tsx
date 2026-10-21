import { useMemo, useRef, useState } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { api, toAppError } from '../api';
import type { ActionPreview, ActionReport, ObjectAction } from '../bindings';
import { invalidateObjectData } from '../store/objectData';
import { invalidateTableData } from '../store/tableData';
import { reportGridSelection } from '../store/gridSelection';
import { invalidateCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useDataEdits } from '../store/dataEdits';
import { objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import { useTree } from '../store/tree';
import { connNodeId } from '../store/treeModel';
import { Modal } from './Modal';
import './objectAction.css';

export interface ObjectActionRequest {
  connectionId: string;
  action: ObjectAction;
}
export function actionTitle(action: ObjectAction): string {
  switch (action.kind) {
    case 'create_namespace': return 'Create namespace';
    case 'drop_namespace': return 'Drop namespace';
    case 'rename_table': return 'Rename table';
    case 'drop_object': return 'Drop object';
    case 'truncate_table': return 'Truncate table';
  }
}
export function stagedOnConnection(connectionId: string): boolean {
  const edits = useDataEdits.getState();
  return useTabs.getState().tabs.some((tab) => tab.kind === 'object' && tab.connectionId === connectionId &&
    (edits.hasStaged(tab.id) || edits.byTab[tab.id]?.applying?.busy));
}
function affects(action: ObjectAction, tab: ObjectTabRef, postgres: boolean): boolean {
  if ('object' in action) return objectTabId(tab.connectionId, action.object) === tab.id;
  return action.kind === 'drop_namespace' && (postgres
    ? tab.objectRef.database === action.database && tab.objectRef.schema === action.name
    : tab.objectRef.database === action.name);
}
/** Remove old resources through the normal tab lifecycle; reopen only known live objects. */
export async function refreshAfterAction(request: ObjectActionRequest, report: ActionReport): Promise<string | null> {
  const { connectionId, action } = request;
  if (report.outcome !== 'applied' && report.outcome !== 'unknown') return null;
  const postgres = useConnections.getState().summaries.get(connectionId)?.config.kind === 'postgres';
  const state = useTabs.getState();
  const affected = state.tabs.filter((t): t is ObjectTabRef => t.kind === 'object' && t.connectionId === connectionId && affects(action, t, postgres));
  const replacements: Record<string, ObjectTabRef | null> = {};
  for (const tab of affected) {
    invalidateObjectData(tab.id);
    invalidateTableData(tab.id);
    reportGridSelection(tab.id, null);
    useDataEdits.setState((s) => ({ byTab: Object.fromEntries(Object.entries(s.byTab).filter(([id]) => id !== tab.id)) }));
    const fresh = { ...tab, resourceRevision: (tab.resourceRevision ?? 0) + 1 };
    if (report.outcome === 'unknown') replacements[tab.id] = { ...fresh, needsVerification: true };
    else if (action.kind === 'rename_table') {
      const objectRef = { ...tab.objectRef, name: action.name };
      replacements[tab.id] = { ...fresh, id: objectTabId(connectionId, objectRef), objectRef, title: action.name, needsVerification: false };
    } else replacements[tab.id] = action.kind === 'truncate_table' ? { ...fresh, needsVerification: false } : null;
  }
  useTabs.getState().replaceObjectTabs(replacements);
  invalidateCompletion(connectionId);
  await useTree.getState().refresh(connNodeId(connectionId));
  return useTree.getState().nodes.get(connNodeId(connectionId))?.error?.message ?? null;
}

export function ObjectActionDialog({ request, onClose }: { request: ObjectActionRequest; onClose(): void }) {
  const cfg = useConnections((s) => s.summaries.get(request.connectionId)?.config);
  const initial = 'name' in request.action ? request.action.name : '';
  const [name, setName] = useState(initial);
  const [typed, setTyped] = useState('');
  const [preview, setPreview] = useState<ActionPreview | null>(null);
  const [report, setReport] = useState<ActionReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [stage, setStage] = useState('');
  const operation = useRef<string | null>(null);
  const sequence = useRef(0);
  const action = useMemo<ObjectAction>(() => 'name' in request.action ? { ...request.action, name } : request.action, [request.action, name]);
  const editableName = action.kind === 'create_namespace' || action.kind === 'rename_table';
  const close = () => { if (!busy) onClose(); };
  const check = () => {
    if (stagedOnConnection(request.connectionId)) throw new Error('Apply or discard staged data edits on this connection first.');
  };
  const review = async () => {
    const seq = ++sequence.current;
    setError(null); setPreview(null); setTyped(''); setReport(null); setBusy(true);
    try {
      check();
      const plan = await api.objectActionPreview(request.connectionId, action);
      if (seq === sequence.current) setPreview(plan);
    } catch (e) { if (seq === sequence.current) setError(toAppError(e).message); }
    finally { if (seq === sequence.current) setBusy(false); }
  };
  const apply = async () => {
    if (!preview || busy) return;
    setBusy(true); setError(null);
    const id = crypto.randomUUID(); operation.current = id;
    useDialogs.setState({ objectActionRunning: true });
    try {
      check();
      const channel = new Channel<string>();
      channel.onmessage = setStage;
      const result = await api.objectActionApply(preview.plan_id, true, typed || null, id, channel);
      setReport(result); setPreview(null);
      if (result.error) setError(result.error.message);
      try {
        const refreshError = await refreshAfterAction({ ...request, action }, result);
        if (refreshError) setError(`Operation finished; metadata refresh failed: ${refreshError}`);
      } catch (e) { setError(`Operation finished; metadata refresh failed: ${toAppError(e).message}`); }
    } catch (e) {
      setPreview(null);
      setError(toAppError(e).message);
    } finally { operation.current = null; useDialogs.setState({ objectActionRunning: false }); setBusy(false); setStage(''); }
  };
  const stop = async () => {
    if (!operation.current) return;
    setStage('stopping');
    try { await api.objectActionCancel(operation.current); }
    catch (e) { setError(`Could not request Stop: ${toAppError(e).message}`); }
  };
  const scope = 'object' in action ? [action.object.database, action.object.schema, action.object.name].filter(Boolean).join(' / ') : [action.database, action.name].filter(Boolean).join(' / ');
  return <Modal title={actionTitle(action)} onClose={close} className="wide object-action-dialog" closable={!busy}>
    <div className="modal-body">
    <p>{cfg?.name ?? request.connectionId} · {cfg?.env_tag ?? 'untagged'} · {scope || 'New namespace'}</p>
    {editableName && <label>Name<input aria-label="Name" value={name} disabled={busy} onChange={(e) => { setName(e.target.value); setPreview(null); setTyped(''); setReport(null); }} /></label>}
    {preview && <>
      <pre className="object-action-sql" aria-label="SQL preview">{preview.sql}</pre>
      {preview.warnings.map((w) => <p className="hint" key={w}>{w}</p>)}
      {preview.confirm_name !== null && <label>Type {preview.confirm_name} to confirm<input aria-label="Confirmation name" value={typed} disabled={busy} onChange={(e) => setTyped(e.target.value)} autoComplete="off" /></label>}
    </>}
    {error && <p role="alert">{error}</p>}
    {report && <div role="status">
      <p>{report.outcome === 'applied' ? 'Operation applied.' : report.outcome === 'unknown' ? 'Outcome unknown. Do not retry. Reload metadata to check what happened.' : report.outcome === 'rolled_back' ? 'The operation was rolled back.' : 'The operation was not applied.'}</p>
      {report.warnings.map((w) => <p key={w}>{w}</p>)}
    </div>}
    {stage && <p role="status">{stage}… Stop does not guarantee undoing DDL already sent.</p>}
    </div>
    <footer className="modal-foot">
      <button type="button" data-cancel className="btn" disabled={busy} onClick={close}>Close</button>
      {busy && operation.current ? <button className="btn" onClick={() => void stop()}>Stop</button> : null}
      {!report && !preview && <button className="btn primary" disabled={busy || (editableName && !name.trim()) || !!cfg?.read_only} onClick={() => void review()}>Preview SQL</button>}
      {preview && <button className="btn primary" disabled={busy || (preview.confirm_name !== null && typed !== preview.confirm_name)} onClick={() => void apply()}>Apply</button>}
    </footer>
  </Modal>;
}
