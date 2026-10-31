import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { api, toAppError } from '../api';
import type { ActionPreview, ActionReport, AppError, ObjectAction, ObjectKind, TableDetail } from '../bindings';
import { TableIcon, TrashIcon } from '../components/icons';
import { formatBytes } from '../jobs/jobFormat';
import { formatCount, plural } from '../lib/plural';
import { useQueryDocs } from '../store/queryTabs';
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
import { CreateNamespaceDialog } from './CreateNamespaceDialog';
import { Callout, ContextBar, FooterNote, ImpactTiles, OutcomeReport, SqlBlock, TypedConfirm, type Impact } from './parts';
import './objectAction.css';

export interface ObjectActionRequest {
  connectionId: string;
  action: ObjectAction;
}
const KIND_WORD: Record<ObjectKind, string> = {
  Table: 'table',
  View: 'view',
  MaterializedView: 'materialized view',
  Function: 'function',
  Procedure: 'procedure',
  Trigger: 'trigger',
  Event: 'event',
  Sequence: 'sequence',
  Type: 'type',
  Collection: 'collection',
  Key: 'key',
};

/** The words the tree menu uses: "Drop table orders", "Drop database shop", "Rename table orders". */
export function actionTitle(action: ObjectAction, postgres = false): string {
  const ns = postgres ? 'schema' : 'database';
  switch (action.kind) {
    case 'create_namespace': return `Create ${ns}`;
    case 'drop_namespace': return `Drop ${ns} ${action.name}`;
    case 'rename_table': return `Rename table ${action.object.name}`;
    case 'drop_object': return `Drop ${KIND_WORD[action.object.kind]} ${action.object.name}`;
    case 'truncate_table': return `Truncate table ${action.object.name}`;
  }
}

/** The primary button: the verb and the kind of object, never "Apply". */
function actionVerb(action: ObjectAction, postgres: boolean, busy: boolean): string {
  const ns = postgres ? 'schema' : 'database';
  switch (action.kind) {
    case 'create_namespace': return busy ? 'Creating…' : `Create ${ns}`;
    case 'drop_namespace': return busy ? 'Dropping…' : `Drop ${ns}`;
    case 'rename_table': return busy ? 'Renaming…' : 'Rename table';
    case 'drop_object': return busy ? 'Dropping…' : `Drop ${KIND_WORD[action.object.kind]}`;
    case 'truncate_table': return busy ? 'Truncating…' : 'Truncate table';
  }
}

/** One line under the title: what the action does to the data. */
function actionConsequence(action: ObjectAction, postgres: boolean): string {
  switch (action.kind) {
    case 'create_namespace': return '';
    case 'drop_namespace': return `Removes the ${postgres ? 'schema' : 'database'} and every object in it`;
    case 'rename_table': return 'Data, indexes and triggers move with it';
    case 'truncate_table': return 'Deletes every row; the table and its structure stay';
    case 'drop_object':
      switch (action.object.kind) {
        case 'Table': return 'Removes the table with its data, indexes and triggers';
        case 'View': return 'Removes the view; the tables it reads stay';
        case 'MaterializedView': return 'Removes the materialized view and its stored rows';
        default: return `Removes the ${KIND_WORD[action.object.kind]}`;
      }
  }
}

/** The toast after a clean apply. */
function actionDone(action: ObjectAction, postgres: boolean): string {
  switch (action.kind) {
    case 'create_namespace': return `Created ${postgres ? 'schema' : 'database'} ${action.name}.`;
    case 'drop_namespace': return `Dropped ${postgres ? 'schema' : 'database'} ${action.name}.`;
    case 'rename_table': return `Renamed ${action.object.name} to ${action.name}.`;
    case 'drop_object': return `Dropped ${KIND_WORD[action.object.kind]} ${action.object.name}.`;
    case 'truncate_table': return `Truncated table ${action.object.name}.`;
  }
}

/** A new table name the backend would refuse, or that changes nothing. */
export function renameProblem(postgres: boolean, name: string, current: string): string | null {
  if (!name.trim()) return 'Enter a name.';
  if (name === current) return 'That is the current name.';
  if (/[.\0]/.test(name)) return 'Names cannot contain a dot (.).';
  if (postgres) {
    if (new TextEncoder().encode(name).length > 63) return 'PostgreSQL names are limited to 63 bytes.';
  } else {
    if ([...name].length > 64) return 'MySQL names are limited to 64 characters.';
    if (name.endsWith(' ')) return 'MySQL names cannot end with a space.';
  }
  return null;
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

/** The backend's `object_actions::OBJECT_MISSING_FIELD`: the target is no longer on the server. */
export const isObjectMissing = (e: AppError) => e.kind === 'config' && e.field === 'object_missing';

/** A target gone from the server (dropped elsewhere): its tabs close and the tree reloads. */
export async function forgetMissingObject(connectionId: string, action: ObjectAction): Promise<void> {
  if (!('object' in action)) return;
  await refreshAfterAction({ connectionId, action: { kind: 'drop_object', object: action.object } }, { outcome: 'applied', error: null, warnings: [] });
}

export function ObjectActionDialog(props: { request: ObjectActionRequest; onClose(): void }) {
  return props.request.action.kind === 'create_namespace' ? <CreateNamespaceDialog {...props} /> : <ObjectActionForm {...props} />;
}

function ObjectActionForm({ request, onClose }: { request: ObjectActionRequest; onClose(): void }) {
  const { connectionId } = request;
  const cfg = useConnections((s) => s.summaries.get(connectionId)?.config);
  const info = useConnections((s) => s.info.get(connectionId));
  const postgres = (cfg?.kind ?? info?.kind) === 'postgres';
  const rename = request.action.kind === 'rename_table';
  const original = rename && 'object' in request.action ? request.action.object.name : '';
  const [name, setName] = useState(rename && 'name' in request.action ? request.action.name : '');
  const [touched, setTouched] = useState(false);
  const [typed, setTyped] = useState('');
  const [preview, setPreview] = useState<ActionPreview | null>(null);
  const [liveSql, setLiveSql] = useState<string | null>(null);
  const [report, setReport] = useState<ActionReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [running, setRunning] = useState(false);
  const [stage, setStage] = useState('');
  const [missing, setMissing] = useState(false);
  const [detail, setDetail] = useState<TableDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const operation = useRef<string | null>(null);
  const sequence = useRef(0);
  const renderSeq = useRef(0);
  const confirmInput = useRef<HTMLDivElement>(null);
  const action = useMemo<ObjectAction>(() => ('name' in request.action && rename ? { ...request.action, name } : request.action), [request.action, name, rename]);
  const readOnly = !!cfg?.read_only;
  const problem = rename ? renameProblem(postgres, name, original) : null;
  const target = 'object' in request.action ? request.action.object : null;
  const tableTarget = target?.kind === 'Table' && request.action.kind !== 'rename_table' ? target : null;

  const close = () => { if (!busy) onClose(); };
  const check = () => {
    if (stagedOnConnection(connectionId)) throw new Error('Apply or discard staged data edits on this connection first.');
  };
  /** Shows `e`; a target gone from the server also closes its tabs and reloads the tree. */
  const failed = async (e: unknown) => {
    const err = toAppError(e);
    setError(err.message);
    if (!isObjectMissing(err)) return;
    setMissing(true);
    try {
      await forgetMissingObject(connectionId, action);
      setError(`${err.message} The tree was refreshed.`);
    } catch (refresh) { setError(`${err.message} Refreshing the tree failed: ${toAppError(refresh).message}`); }
  };

  const plan = async (): Promise<ActionPreview | null> => {
    const seq = ++sequence.current;
    setError(null); setPreview(null); setTyped(''); setReport(null);
    try {
      check();
      const next = await api.objectActionPreview(connectionId, action);
      if (seq !== sequence.current) return null;
      setPreview(next);
      return next;
    } catch (e) {
      if (seq === sequence.current) await failed(e);
      return null;
    }
  };

  const review = async () => {
    setBusy(true);
    try { await plan(); } finally { setBusy(false); }
  };

  const apply = async (chosen: ActionPreview) => {
    setBusy(true); setRunning(true); setError(null);
    const id = crypto.randomUUID(); operation.current = id;
    useDialogs.setState({ objectActionRunning: true });
    try {
      check();
      const channel = new Channel<string>();
      channel.onmessage = setStage;
      const result = await api.objectActionApply(chosen.plan_id, true, chosen.confirm_name !== null ? typed : null, id, channel);
      setPreview(null);
      let refreshError: string | null = null;
      try { refreshError = await refreshAfterAction({ ...request, action }, result); }
      catch (e) { refreshError = toAppError(e).message; }
      if (result.outcome === 'applied' && !result.error && !refreshError && result.warnings.length === 0) {
        useDialogs.getState().setNotice(actionDone(action, postgres), 'success');
        onClose();
        return;
      }
      setReport(result);
      if (result.error) setError(result.error.message);
      else if (refreshError) setError(`Operation finished; metadata refresh failed: ${refreshError}`);
    } catch (e) {
      setPreview(null);
      await failed(e);
    } finally { operation.current = null; useDialogs.setState({ objectActionRunning: false }); setBusy(false); setRunning(false); setStage(''); }
  };

  const stop = async () => {
    if (!operation.current) return;
    setStage('stopping');
    try { await api.objectActionCancel(operation.current); }
    catch (e) { setError(`Could not request Stop: ${toAppError(e).message}`); }
  };

  /** Rename: Enter or the button plans, then applies at once unless the plan wants a typed name. */
  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    setTouched(true);
    if (busy || readOnly || missing) return;
    if (preview) {
      if (preview.confirm_name === null || typed === preview.confirm_name) await apply(preview);
      return;
    }
    if (!rename) { await review(); return; }
    if (problem) return;
    setBusy(true);
    const next = await plan();
    setBusy(false);
    if (next && next.confirm_name === null) await apply(next);
  };

  // Destructive actions show their SQL at once: no "Preview SQL" step.
  useEffect(() => {
    if (!rename && !readOnly) void review();
    // Planned once, on open.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The table's size and parts, for the impact tiles (best effort: a failure just hides them).
  useEffect(() => {
    if (!tableTarget) return;
    let live = true;
    setDetailLoading(true);
    api.tableDetail(connectionId, tableTarget)
      .then((d) => { if (live) setDetail(d); })
      .catch(() => {})
      .finally(() => { if (live) setDetailLoading(false); });
    return () => { live = false; };
  }, [connectionId, tableTarget]);

  // Rename: the SQL follows the name as it is typed (rendered by the backend, debounced).
  useEffect(() => {
    if (!rename) return;
    const seq = ++renderSeq.current;
    if (problem) { setLiveSql(null); return; }
    const timer = setTimeout(() => {
      api.objectActionRender(connectionId, action)
        .then((sql) => { if (seq === renderSeq.current) setLiveSql(sql); })
        .catch(() => { if (seq === renderSeq.current) setLiveSql(null); });
    }, 120);
    return () => clearTimeout(timer);
  }, [rename, connectionId, action, problem]);

  // The typed confirmation takes the focus as soon as it appears.
  useEffect(() => {
    if (preview?.confirm_name != null) confirmInput.current?.querySelector('input')?.focus();
  }, [preview]);

  const openInQuery = (sql: string) => {
    const database = 'object' in action ? action.object.database : postgres ? action.database : action.name;
    const docs = useQueryDocs.getState();
    const id = docs.newQuery(connectionId, database || null);
    docs.update(id, { sql: sql.trimEnd().endsWith(';') ? sql : `${sql};` });
    onClose();
  };

  const path = 'object' in action
    ? [action.object.database, action.object.schema, action.object.name]
    : postgres ? [action.database, action.name] : [action.name];
  const destructive = !rename;
  const needsTyped = preview?.confirm_name != null;
  const typedOk = !needsTyped || typed === preview?.confirm_name;
  const sql = preview?.sql ?? (rename ? liveSql : null);
  const showProblem = rename && problem && (touched || name !== original);

  const impacts: Impact[] = [];
  if (detail) {
    if (detail.row_estimate !== null) impacts.push({ value: `≈ ${formatCount(detail.row_estimate)}`, label: action.kind === 'truncate_table' ? 'rows deleted (estimate)' : 'rows (estimate)', tone: 'danger' });
    if (detail.size_bytes !== null) impacts.push({ value: formatBytes(detail.size_bytes), label: 'data + indexes' });
    if (action.kind === 'drop_object') impacts.push({ value: String(detail.indexes.length + (detail.primary_key ? 1 : 0)), label: `indexes · ${plural(detail.triggers.length, 'trigger')}` });
  }

  let note: string | null = null;
  if (readOnly) note = 'This connection is read-only.';
  else if (running) note = stage ? `${stage}… Stop does not undo DDL already sent.` : 'Running…';
  else if (busy && !preview) note = 'Preparing the SQL…';
  else if (needsTyped && !typedOk) note = 'Type the name to confirm';

  const primaryLabel = actionVerb(action, postgres, running);
  const showPrimary = !report && !missing;
  const primaryDisabled = busy || readOnly || (rename ? (!preview && !!problem) : !preview) || !typedOk;
  const offerPreviewAgain = destructive && !preview && !busy && !report && !missing && !readOnly && error !== null;

  return (
    <Modal
      title={actionTitle(request.action, postgres)}
      subtitle={actionConsequence(request.action, postgres)}
      icon={rename ? <TableIcon /> : <TrashIcon />}
      tone={rename ? 'accent' : 'danger'}
      context={<ContextBar connectionId={connectionId} path={path} />}
      onClose={close}
      closable={!busy}
      className="size-m object-action-dialog"
      initialFocus={rename ? '#oa-name' : '[data-cancel]'}
      role={destructive ? 'alertdialog' : 'dialog'}
    >
      <form onSubmit={(e) => void submit(e)} noValidate>
        <div className="modal-body flow">
          {destructive && tableTarget && <ImpactTiles items={impacts} loading={detailLoading} />}
          {rename && (
            <div className="field">
              <label htmlFor="oa-name">New name</label>
              <input
                id="oa-name"
                aria-label="Name"
                value={name}
                readOnly={busy}
                autoComplete="off"
                spellCheck={false}
                aria-invalid={showProblem ? true : undefined}
                aria-describedby="oa-name-msg"
                onFocus={(e) => e.currentTarget.select()}
                onChange={(e) => { setName(e.target.value); setPreview(null); setTyped(''); setReport(null); setError(null); }}
              />
              {showProblem
                ? <p className="field-error" id="oa-name-msg" role="alert">{problem}</p>
                : <p className="hint" id="oa-name-msg">{postgres ? `${new TextEncoder().encode(name).length}/63 bytes` : `${[...name].length}/64 characters`} · same {postgres ? 'schema' : 'database'}</p>}
            </div>
          )}
          <SqlBlock
            sql={sql}
            caption={preview ? 'exactly what runs' : rename && liveSql ? 'live' : undefined}
            placeholder={readOnly ? 'Read-only connection: nothing can run.' : rename ? (name === original || !name.trim() ? 'Type a new name to see the SQL.' : problem ? 'Fix the name to see the SQL.' : 'Rendering…') : busy ? 'Preparing the SQL…' : error ? 'No SQL.' : ''}
            onOpenInQuery={readOnly ? undefined : openInQuery}
          />
          {preview && preview.warnings.length > 0 && (
            <Callout tone="warning" role="note">
              {preview.warnings.map((w) => <p key={w}>{w}</p>)}
            </Callout>
          )}
          {needsTyped && preview?.confirm_name && (
            <div ref={confirmInput}>
              <TypedConfirm name={preview.confirm_name} value={typed} onChange={setTyped} disabled={busy} prefix={cfg?.env_tag === 'production' ? 'Production ·' : undefined} />
            </div>
          )}
          {readOnly && <Callout tone="neutral" role="note">Turn off read-only in the connection's settings to change objects.</Callout>}
          {error && <Callout tone="danger" role="alert">{error}</Callout>}
          {report && <OutcomeReport report={report} />}
        </div>
        <footer className="modal-foot">
          <FooterNote>{note}</FooterNote>
          {running && <button type="button" className="btn" onClick={() => void stop()}>Stop</button>}
          <button type="button" data-cancel className="btn" disabled={busy} onClick={close}>{report || missing ? 'Close' : 'Cancel'}</button>
          {offerPreviewAgain && <button type="button" className="btn" onClick={() => void review()}>Preview again</button>}
          {showPrimary && !offerPreviewAgain && (
            <button type="submit" className={`btn ${destructive ? 'danger' : 'primary'}`} disabled={primaryDisabled}>
              {primaryLabel}
            </button>
          )}
        </footer>
      </form>
    </Modal>
  );
}
