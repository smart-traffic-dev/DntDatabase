import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { api, toAppError } from '../api';
import type { ActionPreview, ActionReport, DbKind, NamespaceOptions, ObjectAction } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { ChevronDownIcon } from '../components/icons';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTree } from '../store/tree';
import { connNodeId, seg } from '../store/treeModel';
import { Modal } from './Modal';
import { refreshAfterAction, type ObjectActionRequest } from './ObjectActionDialog';
import './objectAction.css';

type CreateAction = Extract<ObjectAction, { kind: 'create_namespace' }>;

const utf8Bytes = (s: string) => new TextEncoder().encode(s).length;

/** The client-side twin of the backend's identifier rules (`ddl::validate_identifier`). */
export function nameProblem(kind: DbKind, name: string, existing: readonly string[]): string | null {
  if (!name.trim()) return 'Enter a name.';
  if (/[.\0]/.test(name)) return 'Names cannot contain a dot (.).';
  if (kind === 'mysql') {
    if ([...name].length > 64) return 'MySQL names are limited to 64 characters.';
    if (/[/\\]/.test(name)) return 'Database names cannot contain / or \\.';
    if (name.endsWith(' ')) return 'MySQL names cannot end with a space.';
    if ([...name].some((c) => (c.codePointAt(0) ?? 0) > 0xffff)) return 'MySQL names cannot contain emoji or other supplementary characters.';
  } else if (utf8Bytes(name) > 63) return 'PostgreSQL names are limited to 63 bytes.';
  if (existing.includes(name)) return `${kind === 'mysql' ? 'A database' : 'A schema'} named ${name} already exists.`;
  return null;
}

function Select({ id, label, value, onChange, disabled, children }: { id: string; label: string; value: string; onChange(v: string): void; disabled?: boolean; children: ReactNode }) {
  return <div className="field">
    <label htmlFor={id}>{label}</label>
    <div className="select-wrap">
      <select id={id} value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)}>{children}</select>
      <ChevronDownIcon />
    </div>
  </div>;
}

export function CreateNamespaceDialog({ request, onClose }: { request: ObjectActionRequest; onClose(): void }) {
  const { connectionId } = request;
  const base = request.action as CreateAction;
  const cfg = useConnections((s) => s.summaries.get(connectionId)?.config);
  const info = useConnections((s) => s.info.get(connectionId));
  const kind: DbKind = cfg?.kind ?? info?.kind ?? 'mysql';
  const mysql = kind === 'mysql';
  const what = mysql ? 'database' : 'schema';
  const parentId = mysql ? connNodeId(connectionId) : `${connNodeId(connectionId)}/db:${seg(base.database)}`;
  const childIds = useTree((s) => s.childIds.get(parentId));
  const nodes = useTree((s) => s.nodes);
  const existing = useMemo(() => (childIds ?? []).map((id) => nodes.get(id)?.label ?? ''), [childIds, nodes]);

  const [name, setName] = useState(base.name);
  const [touched, setTouched] = useState(false);
  const [charset, setCharset] = useState('');
  const [collation, setCollation] = useState('');
  const [owner, setOwner] = useState('');
  const [options, setOptions] = useState<NamespaceOptions | null>(null);
  const [optionsError, setOptionsError] = useState<string | null>(null);
  const [sql, setSql] = useState<string | null>(null);
  const [sqlError, setSqlError] = useState<string | null>(null);
  const [preview, setPreview] = useState<ActionPreview | null>(null);
  const [typed, setTyped] = useState('');
  const [report, setReport] = useState<ActionReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const renderSeq = useRef(0);

  const action = useMemo<CreateAction>(() => ({
    kind: 'create_namespace', database: base.database, name,
    charset: mysql ? charset || null : null,
    collation: mysql ? collation || null : null,
    owner: mysql ? null : owner || null,
  }), [base.database, name, charset, collation, owner, mysql]);
  const problem = nameProblem(kind, name, existing);
  // A taken name still has meaningful SQL; an invalid one has none.
  const invalidName = nameProblem(kind, name, []) !== null;
  const readOnly = !!cfg?.read_only;
  const production = cfg?.env_tag === 'production';

  useEffect(() => {
    let live = true;
    api.namespaceOptions(connectionId, base.database)
      .then((o) => { if (live) setOptions(o); })
      .catch((e) => { if (live) setOptionsError(toAppError(e).message); });
    return () => { live = false; };
  }, [connectionId, base.database]);

  // Live SQL: rendered by the backend (its quoting is the one that runs), debounced.
  useEffect(() => {
    const seq = ++renderSeq.current;
    if (invalidName) { setSql(null); setSqlError(null); return; }
    const timer = setTimeout(() => {
      api.objectActionRender(connectionId, action)
        .then((text) => { if (seq === renderSeq.current) { setSql(text); setSqlError(null); } })
        .catch((e) => { if (seq === renderSeq.current) { setSql(null); setSqlError(toAppError(e).message); } });
    }, 120);
    return () => clearTimeout(timer);
  }, [connectionId, action, invalidName]);

  const edit = <T,>(set: (v: T) => void) => (v: T) => { set(v); setPreview(null); setTyped(''); setError(null); setReport(null); };
  const close = () => { if (!busy) onClose(); };

  const run = async (plan: ActionPreview) => {
    useDialogs.setState({ objectActionRunning: true });
    try {
      const result = await api.objectActionApply(plan.plan_id, true, plan.confirm_name ? typed : null, crypto.randomUUID(), new Channel<string>());
      setPreview(null);
      let refreshError: string | null = null;
      try { refreshError = await refreshAfterAction({ connectionId, action }, result); }
      catch (e) { refreshError = toAppError(e).message; }
      if (result.outcome === 'applied' && !result.error && !refreshError && result.warnings.length === 0) {
        const tree = useTree.getState();
        if (!mysql) { tree.expand(parentId); await tree.refresh(parentId); }
        tree.select(`${parentId}/${mysql ? 'db' : 'schema'}:${seg(name)}`);
        useDialogs.getState().setNotice(`${mysql ? 'Database' : 'Schema'} ${name} created.`, 'success');
        onClose();
        return;
      }
      setReport(result);
      if (result.error) setError(result.error.message);
      else if (refreshError) setError(`Created; reloading the tree failed: ${refreshError}`);
    } finally { useDialogs.setState({ objectActionRunning: false }); }
  };

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    setTouched(true);
    if (busy || problem || readOnly || sqlError) return;
    setBusy(true); setError(null); setReport(null);
    try {
      if (preview) {
        if (preview.confirm_name !== null && typed !== preview.confirm_name) return;
        await run(preview);
        return;
      }
      const plan = await api.objectActionPreview(connectionId, action);
      if (plan.confirm_name !== null) { setPreview(plan); return; }
      await run(plan);
    } catch (err) {
      setPreview(null);
      setError(toAppError(err).message);
    } finally { setBusy(false); }
  };

  const defaultCharset = options?.default_charset ?? null;
  const effectiveCharset = charset || defaultCharset;
  const collations = (options?.collations ?? []).filter((c) => !effectiveCharset || c.charset === effectiveCharset);
  const charsetDefault = options?.charsets.find((c) => c.name === effectiveCharset)?.default_collation
    ?? (charset ? null : options?.default_collation ?? null);
  const showProblem = problem && (touched || name.length > 0);
  const needsConfirm = preview?.confirm_name != null;
  const disabled = busy || !!problem || readOnly || !!sqlError || (needsConfirm && typed !== preview?.confirm_name);
  const host = cfg?.name ?? connectionId;
  const server = info ? ` · ${info.variant === 'MariaDb' ? 'MariaDB' : mysql ? 'MySQL' : 'PostgreSQL'} ${info.version[0]}.${info.version[1]}` : '';

  return <Modal title={`Create ${what}`} onClose={close} closable={!busy} className="create-ns-dialog"
    badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}>
    <form onSubmit={(e) => void submit(e)} noValidate>
      <div className="modal-body create-ns-body">
        <p className="hint create-ns-context">{mysql ? `On ${host}` : `In ${base.database} on ${host}`}{server}</p>
        <div className="field">
          <label htmlFor="cns-name">Name</label>
          <input id="cns-name" aria-label="Name" value={name} readOnly={busy} autoComplete="off" spellCheck={false}
            placeholder={mysql ? 'e.g. shop_dev' : 'e.g. reporting'}
            aria-invalid={showProblem ? true : undefined} aria-describedby="cns-name-msg"
            onChange={(e) => edit(setName)(e.target.value)} />
          {showProblem
            ? <p className="field-error" id="cns-name-msg" role="alert">{problem}</p>
            : <p className="hint" id="cns-name-msg">{mysql ? `${[...name].length}/64 characters` : `${utf8Bytes(name)}/63 bytes`}</p>}
        </div>
        {mysql ? <div className="create-ns-row">
          <Select id="cns-charset" label="Character set" value={charset} disabled={busy || !options} onChange={(v) => { edit(setCharset)(v); setCollation(''); }}>
            <option value="">{defaultCharset ? `Server default (${defaultCharset})` : 'Server default'}</option>
            {options?.charsets.map((c) => <option key={c.name} value={c.name}>{c.name}</option>)}
          </Select>
          <Select id="cns-collation" label="Collation" value={collation} disabled={busy || !options} onChange={edit(setCollation)}>
            <option value="">{charsetDefault ? `Default (${charsetDefault})` : 'Default'}</option>
            {collations.map((c) => <option key={c.name} value={c.name}>{c.name}</option>)}
          </Select>
        </div> : <Select id="cns-owner" label="Owner" value={owner} disabled={busy || !options} onChange={edit(setOwner)}>
          <option value="">{options?.current_role ? `Current user (${options.current_role})` : 'Current user'}</option>
          {options?.roles.filter((r) => r !== options.current_role).map((r) => <option key={r} value={r}>{r}</option>)}
        </Select>}
        {optionsError && <p className="hint">Could not load {mysql ? 'character sets' : 'roles'}: {optionsError}</p>}
        <div className="field">
          <span className="field-label">SQL</span>
          <pre className="object-action-sql" aria-label="SQL preview" aria-live="polite">{sql ?? (sqlError ? '' : name.trim() ? 'Fix the name to see the SQL.' : 'Enter a name to see the SQL.')}</pre>
          {sqlError && <p className="field-error" role="alert">{sqlError}</p>}
        </div>
        {needsConfirm && <div className="field">
          <label htmlFor="cns-confirm">This is a production connection. Type {preview?.confirm_name} to confirm</label>
          <input id="cns-confirm" aria-label="Confirmation name" value={typed} readOnly={busy} autoComplete="off" onChange={(e) => setTyped(e.target.value)} />
        </div>}
        {readOnly && <p className="hint" role="note">This connection is read-only. Turn off read-only in its settings to create a {what}.</p>}
        {error && <p className="field-error" role="alert">{error}</p>}
        {report && <p role="status">{report.outcome === 'unknown' ? 'Outcome unknown. Do not retry; refresh the tree to check what happened.' : report.outcome === 'applied' ? `${mysql ? 'Database' : 'Schema'} created.` : 'Nothing was created.'}</p>}
        {report?.warnings.map((w) => <p className="hint" key={w}>{w}</p>)}
      </div>
      <footer className="modal-foot">
        <button type="button" data-cancel className="btn" disabled={busy} onClick={close}>{report ? 'Close' : 'Cancel'}</button>
        {!report && <button type="submit" className="btn primary" disabled={disabled}>{busy ? 'Creating…' : `Create ${what}`}</button>}
      </footer>
    </form>
  </Modal>;
}
