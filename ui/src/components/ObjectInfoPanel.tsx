import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { api, isCancelled, notOpenError, toAppError } from '../api';
import type {
  AppError,
  ColumnDetail,
  ForeignKeyDetail,
  NamespaceDetail,
  ObjectKind,
  ObjectRef,
  ObjectSummary,
  Scope,
  TableDataPage,
  TableDetail,
} from '../bindings';
import { isKind, sqlEngine } from '../engine';
import { quoteIdent } from '../query/completion/provider';
import { insertAtCursor } from '../store/runQuery';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { editorOf } from '../tabs/query/activeEditor';
import { useConnections } from '../store/connections';
import type { Loadable } from '../store/objectData';
import { dialectOfConnection } from '../store/queryTabs';
import { promptHandles, reopenConnection } from '../store/reopen';
import { targetName, useObjectInfo, type InfoTarget } from '../store/objectInfo';
import { useTree } from '../store/tree';
import { connectionIdOf, folderLabel, folderKindsOf, nodeKey, objectNodeKey } from '../store/treeModel';
import { pageToResult } from '../tabs/DataView';
import { ErrorBox, extraOf, formatBytes, formatCount, indexTarget, orDash, referenceOf } from '../tabs/ObjectTab';
import { ResultGrid } from '../tabs/query/ResultGrid';
import { serverLabel } from '../dialogs/parts';
import { EnvTag } from './EnvTag';
import { ChevronIcon, LockIcon, PinIcon, RefreshIcon, SearchIcon } from './icons';
import { openDesignTab, openObjectRef } from './openObject';
import './objectInfo.css';

/** Rows shown by the Preview section (the first page, unsorted, read-only). */
export const PREVIEW_ROWS = 100;
/** The backend cancels the previous preview of this key itself when the next one starts. */
const PREVIEW_KEY = 'object-info-preview';
const DASH = '—';

export type InfoSection = 'general' | 'columns' | 'ddl' | 'preview';
const SECTION_LABEL: Record<InfoSection, string> = { general: 'General', columns: 'Columns', ddl: 'DDL', preview: 'Preview' };

const hasRows = (kind: ObjectRef['kind']) => kind === 'Table' || kind === 'View' || kind === 'MaterializedView';

/** Tables and views show their columns and sample rows; other objects their DDL. */
export function sectionsOf(kind: ObjectRef['kind']): InfoSection[] {
  return hasRows(kind) ? ['general', 'columns', 'ddl', 'preview'] : ['general', 'ddl'];
}

/** `varchar(255)` -> ["varchar", "255"]; `decimal(10,2) unsigned` -> ["decimal unsigned", "10,2"]. Enum / set lists stay in the type. */
export function splitType(dbType: string): [string, string] {
  const m = /^([^(]+)\(([^)]*)\)(.*)$/.exec(dbType.trim());
  if (!m || /^(enum|set)\b/i.test(m[1]!.trim()) || m[2]!.includes("'")) return [dbType, ''];
  return [`${m[1]!.trim()}${m[3]!.trim() ? ` ${m[3]!.trim()}` : ''}`, m[2]!.trim()];
}

/** The key marks of a column: PK, FK, UQ (in a unique index), IX (first column of another index). */
export function keyMarks(c: ColumnDetail, detail: TableDetail): string[] {
  const marks: string[] = [];
  if (detail.primary_key?.columns.includes(c.name)) marks.push('PK');
  if (detail.foreign_keys.some((fk) => fk.columns.includes(c.name))) marks.push('FK');
  const others = detail.indexes.filter((i) => !i.primary);
  if (others.some((i) => i.unique && !i.partial && i.columns.includes(c.name))) marks.push('UQ');
  else if (others.some((i) => !i.unique && i.columns[0] === c.name)) marks.push('IX');
  return marks;
}

/**
 * The object to show: one picked in an editor (Ctrl+click on its name), else the selected tree
 * node when it is an object of a known connection, else null.
 */
function useSelectedObject(): (InfoTarget & { label: string }) | null {
  const node = useTree((s) => (s.selectedId ? s.nodes.get(s.selectedId) : undefined));
  const target = useObjectInfo((s) => s.target);
  return useMemo(() => {
    if (target) return { ...target, label: targetName(target) };
    const connectionId = node ? connectionIdOf(node.id) : null;
    if (!node || !connectionId) return null;
    if (node.kind === 'object' && node.objectRef) return { label: node.label, connectionId, object: node.objectRef };
    // A database or a schema: its objects.
    if ((node.kind === 'database' || node.kind === 'schema') && node.scope) return { label: node.label, connectionId, scope: node.scope };
    return null;
  }, [node, target]);
}

/** What a database or schema holds: its object counts and tables, or (a PostgreSQL database) its schemas. */
export interface NamespaceSummary {
  /** Charset, collation, owner, size, comment; null where the engine has none or the call failed. */
  detail: NamespaceDetail | null;
  schemas: string[] | null;
  counts: { kind: ObjectKind; count: number | null }[];
  relations: ObjectSummary[];
}

/** Lists the objects of a database / schema (the tree's cached lists), or the schemas of a PostgreSQL database. */
export async function loadNamespace(connectionId: string, scope: Scope): Promise<NamespaceSummary> {
  const server = useConnections.getState().info.get(connectionId);
  if (!server) throw notOpenError();
  // The properties are extra: a failure (no privilege, an engine without them) leaves them out.
  const detailCall = server.capabilities.family === 'sql' ? api.namespaceDetail(connectionId, scope).catch(() => null) : Promise.resolve(null);
  if (server.capabilities.namespaces === 'db_schema' && scope.schema === null) {
    const [schemas, detail] = await Promise.all([api.listSchemas(connectionId, scope.database, false), detailCall]);
    return { detail, schemas: schemas.map((s) => s.name), counts: [], relations: [] };
  }
  const kinds = folderKindsOf(server.variant, server.capabilities);
  const [lists, detail] = await Promise.all([Promise.allSettled(kinds.map((k) => api.listObjects(connectionId, scope, k, useTree.getState().showSystem))), detailCall]);
  // Every list failed: the connection is the problem, say so; one kind failing only blanks its count.
  if (lists.length > 0 && lists.every((l) => l.status === 'rejected')) throw (lists[0] as PromiseRejectedResult).reason;
  const counts = kinds.map((kind, i) => {
    const l = lists[i]!;
    return { kind, count: l.status === 'fulfilled' ? l.value.length : null };
  });
  const relations = kinds.flatMap((kind, i) => {
    const l = lists[i]!;
    return hasRows(kind) && l.status === 'fulfilled' ? l.value.map((o) => ({ ...o, kind })) : [];
  });
  return { detail, schemas: null, counts, relations };
}

/**
 * Loads `fetch()` while `enabled`, once per `key` (and per `reload()`); a newer key or an
 * unmount drops the answer. A cancelled call is forgotten (the next visit fetches again).
 */
function useLoad<T>(key: string, enabled: boolean, fetch: () => Promise<T>, onAbandon?: () => void) {
  const [state, setState] = useState<{ key: string; nonce: number; value: Loadable<T> } | null>(null);
  const [nonce, setNonce] = useState(0);
  const fetchRef = useRef(fetch);
  fetchRef.current = fetch;
  const abandonRef = useRef(onAbandon);
  abandonRef.current = onAbandon;
  const done = useRef<{ key: string; nonce: number } | null>(null);

  useEffect(() => {
    if (!enabled) return;
    if (done.current?.key === key && done.current.nonce === nonce) return;
    let live = true;
    let settled = false;
    setState({ key, nonce, value: { status: 'loading' } });
    fetchRef.current().then(
      (data) => {
        settled = true;
        if (!live) return;
        done.current = { key, nonce };
        setState({ key, nonce, value: { status: 'ok', data } });
      },
      (e: unknown) => {
        settled = true;
        if (!live) return;
        const error = toAppError(e);
        if (isCancelled(error)) setState(null);
        else {
          done.current = { key, nonce };
          setState({ key, nonce, value: { status: 'error', error } });
        }
      },
    );
    return () => {
      live = false;
      if (!settled) abandonRef.current?.();
    };
  }, [key, enabled, nonce]);

  const value = state && state.key === key ? state.value : undefined;
  return {
    state: enabled ? (value ?? { status: 'loading' as const }) : value,
    reload: () => {
      done.current = null;
      setNonce((n) => n + 1);
    },
    /** Shows an error without fetching (a reconnect that failed). */
    fail: (error: AppError) => {
      done.current = { key, nonce };
      setState({ key, nonce, value: { status: 'error', error } });
    },
    loading: () => setState({ key, nonce, value: { status: 'loading' } }),
  };
}

type Load<T> = ReturnType<typeof useLoad<T>>;

function Section<T>({ load, onReconnect, children }: { load: Load<T>; onReconnect(): void; children: (data: T) => React.ReactNode }) {
  const { state } = load;
  if (!state || state.status === 'loading')
    return (
      <p className="oi-loading muted" role="status">
        Loading…
      </p>
    );
  if (state.status === 'error')
    return (
      <div className="oi-pad">
        <ErrorBox error={state.error} onRetry={load.reload} onReconnect={onReconnect} />
      </div>
    );
  return <>{children(state.data)}</>;
}

function Rows({ rows }: { rows: [string, string][] }) {
  return (
    <dl className="oi-props">
      {rows.map(([label, value]) => (
        <div key={label} className="oi-prop">
          <dt>{label}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

/**
 * Where the object lives: environment, connection › database › schema, server version and a
 * read-only mark. Editors can be on many connections at once; this says which one is shown.
 * Tinted on Production.
 */
function ObjectContext({ connectionId, object }: { connectionId: string; object: Scope }) {
  const cfg = useConnections((s) => s.summaries.get(connectionId)?.config);
  const info = useConnections((s) => s.info.get(connectionId));
  const env = cfg?.env_tag ?? null;
  const server = serverLabel(info);
  const name = cfg?.name ?? connectionId;
  const path = [name, object.database, object.schema].filter((p): p is string => !!p);
  const address = cfg ? `${cfg.host}${cfg.port ? `:${cfg.port}` : ''}` : '';
  return (
    <div
      className={`oi-context${env === 'production' ? ' oi-prod' : ''}`}
      data-testid="object-info-context"
      title={[path.join(' › '), address, server].filter(Boolean).join('\n')}
    >
      {env && <EnvTag env={env} />}
      <span className="oi-path">
        <strong className="oi-conn">{name}</strong>
        {path.slice(1).map((p, i) => (
          <span key={i}>
            <span className="oi-sep" aria-hidden="true">
              {' › '}
            </span>
            {p}
          </span>
        ))}
      </span>
      <span className="spacer" />
      {cfg?.read_only && (
        <span className="oi-ro" title="Read-only connection" aria-label="Read-only">
          <LockIcon />
        </span>
      )}
      {server && <span className="oi-server muted">{server}</span>}
    </div>
  );
}

function General({ object, detail }: { object: ObjectRef; detail: TableDetail | null }) {
  const place: [string, string][] = [
    ['Name', object.name],
    ['Type', object.kind === 'MaterializedView' ? 'Materialized view' : object.kind],
    ['Database', object.database],
    ...(object.schema ? ([['Schema', object.schema]] as [string, string][]) : []),
  ];
  if (!detail) return <Rows rows={place} />;
  const o = detail.options;
  const rows: [string, string][] = [...place, ['Columns', String(detail.columns.length)]];
  if (object.kind !== 'View') {
    rows.push(
      ['Rows (estimate)', detail.row_estimate === null ? DASH : formatCount(detail.row_estimate)],
      ['Size', detail.size_bytes === null ? DASH : formatBytes(detail.size_bytes)],
    );
    if (o.engine) rows.push(['Engine', o.engine]);
    if (o.auto_increment != null) rows.push(['Auto increment', String(o.auto_increment)]);
    if (o.row_format) rows.push(['Row format', o.row_format]);
    if (o.tablespace) rows.push(['Tablespace', o.tablespace]);
    if (o.partitioned_by) rows.push(['Partitioned by', o.partitioned_by]);
    rows.push(['Primary key', detail.primary_key ? detail.primary_key.columns.join(', ') : DASH]);
    rows.push(['Indexes', String(detail.indexes.filter((i) => !i.primary).length)]);
    rows.push(['Foreign keys', String(detail.foreign_keys.length)]);
  }
  if (o.charset) rows.push(['Charset', o.charset]);
  if (o.collation) rows.push(['Collation', o.collation]);
  rows.push(['Comment', detail.comment || DASH]);
  return <Rows rows={rows} />;
}

/** The columns whose name, type or comment contains `filter` (any case). */
export function filterColumns(columns: ColumnDetail[], filter: string): ColumnDetail[] {
  const q = filter.trim().toLowerCase();
  if (!q) return columns;
  return columns.filter((c) => c.name.toLowerCase().includes(q) || c.db_type.toLowerCase().includes(q) || (c.comment ?? '').toLowerCase().includes(q));
}

/** The table a foreign key points at: a database on MySQL / MariaDB, a schema on PostgreSQL. */
export function referencedTable(fk: ForeignKeyDetail, object: ObjectRef, postgres: boolean): ObjectRef {
  return postgres
    ? { database: object.database, schema: fk.ref_schema ?? object.schema, name: fk.ref_table, kind: 'Table' }
    : { database: fk.ref_schema ?? object.database, schema: null, name: fk.ref_table, kind: 'Table' };
}

/** The query tab with the focus, whose editor Insert writes to. */
function useInsertTab(): string | null {
  return useTabs((s) => {
    const tab = s.tabs.find((t) => t.id === s.activeId);
    return tab?.kind === 'query' ? tab.id : null;
  });
}

function Columns({
  detail,
  object,
  connectionId,
  onFollow,
}: {
  detail: TableDetail;
  object: ObjectRef;
  connectionId: string;
  onFollow(to: ObjectRef): void;
}) {
  const kind = object.kind;
  const indexes = detail.indexes.filter((i) => !i.primary);
  const [filter, setFilter] = useState('');
  const [note, setNote] = useState<string | null>(null);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const insertTab = useInsertTab();
  const engine = sqlEngine(dialectOfConnection(connectionId)) ?? 'mysql';
  const postgres = isKind(engine, 'postgres');
  const columns = filterColumns(detail.columns, filter);
  const quoted = (name: string) => quoteIdent(name, engine);

  const say = (text: string) => {
    setNote(text);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setNote(null), 2000);
  };
  const insert = (text: string) => {
    if (!insertTab) return say('Open a query tab to insert');
    insertAtCursor(insertTab, text);
    editorOf(insertTab)?.focus();
    say(`Inserted ${text.length > 40 ? `${text.slice(0, 40)}…` : text}`);
  };
  const copyNames = () => {
    const text = columns.map((c) => quoted(c.name)).join(', ');
    void navigator.clipboard?.writeText(text).then(
      () => say(`Copied ${columns.length} column name${columns.length === 1 ? '' : 's'}`),
      () => undefined,
    );
  };

  return (
    <div className="oi-columns">
      <div className="oi-colbar">
        <label className="oi-filter">
          <SearchIcon />
          <input
            type="search"
            value={filter}
            placeholder="Filter columns"
            aria-label="Filter columns"
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape' && filter) {
                e.stopPropagation();
                setFilter('');
              }
            }}
          />
        </label>
        <button type="button" className="btn small" disabled={columns.length === 0} title="Copy the names, comma-separated" onClick={copyNames}>
          Copy names
        </button>
        <button
          type="button"
          className="btn small"
          disabled={columns.length === 0}
          title={insertTab ? 'Insert the names at the cursor of the focused editor' : 'Open a query tab to insert'}
          onClick={() => insert(columns.map((c) => quoted(c.name)).join(', '))}
        >
          Insert
        </button>
      </div>
      <div className="oi-colnote muted" role="status" aria-live="polite">
        {note ?? (filter.trim() ? `${columns.length} of ${detail.columns.length} columns` : 'Double-click a column to insert its name')}
      </div>
      <table className="oi-table" aria-label="Columns">
        <thead>
          <tr>
            <th>Name</th>
            <th>Type</th>
            <th>Length</th>
            <th>Null</th>
            <th>Key</th>
            <th>Default</th>
            <th>Extra</th>
            <th>Comment</th>
          </tr>
        </thead>
        <tbody>
          {columns.length === 0 && (
            <tr>
              <td colSpan={8} className="muted">
                No column matches “{filter.trim()}”
              </td>
            </tr>
          )}
          {columns.map((c) => {
            const [type, length] = splitType(c.db_type);
            return (
              <tr key={c.name} className="oi-col" title={`Double-click to insert ${quoted(c.name)}`} onDoubleClick={() => insert(quoted(c.name))}>
                <td className="oi-name">{c.name}</td>
                <td className="mono">{type}</td>
                <td className="mono">{length}</td>
                <td>{c.nullable ? 'YES' : 'NO'}</td>
                <td>
                  {keyMarks(c, detail).map((m) => (
                    <span key={m} className={`chip oi-key oi-key-${m.toLowerCase()}`}>
                      {m}
                    </span>
                  ))}
                </td>
                <td className="mono">{orDash(c.default)}</td>
                <td>{extraOf(c)}</td>
                <td>{c.comment || ''}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {kind !== 'View' && (
        <>
          <h3 className="oi-h">Indexes</h3>
          {indexes.length === 0 && !detail.primary_key ? (
            <p className="muted oi-none">None</p>
          ) : (
            <table className="oi-table" aria-label="Indexes">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Columns</th>
                  <th>Unique</th>
                  <th>Method</th>
                </tr>
              </thead>
              <tbody>
                {[...(detail.primary_key ? [detail.primary_key] : []), ...indexes].map((i) => (
                  <tr key={`${i.primary ? 'pk' : 'ix'}:${i.name}`}>
                    <td className="oi-name">{i.name}</td>
                    <td className="mono">{indexTarget(i)}</td>
                    <td>{i.unique || i.primary ? 'YES' : 'NO'}</td>
                    <td>{i.method ?? DASH}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          <h3 className="oi-h">Foreign keys</h3>
          {detail.foreign_keys.length === 0 ? (
            <p className="muted oi-none">None</p>
          ) : (
            <table className="oi-table" aria-label="Foreign keys">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Reference</th>
                  <th>On update</th>
                  <th>On delete</th>
                </tr>
              </thead>
              <tbody>
                {detail.foreign_keys.map((fk, i) => (
                  <tr key={`${fk.name}:${i}`}>
                    <td className="oi-name">{fk.name}</td>
                    <td className="mono">
                      <button
                        type="button"
                        className="oi-link"
                        title={`Show ${fk.ref_table} here`}
                        onClick={() => onFollow(referencedTable(fk, object, postgres))}
                      >
                        {referenceOf(fk)}
                      </button>
                    </td>
                    <td>{fk.on_update}</td>
                    <td>{fk.on_delete}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </>
      )}
    </div>
  );
}

function Ddl({ ddl }: { ddl: string }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  return (
    <div className="oi-ddl">
      <div className="oi-bar">
        <button
          type="button"
          className="btn small"
          onClick={() =>
            void navigator.clipboard?.writeText(ddl).then(
              () => {
                setCopied(true);
                window.clearTimeout(timer.current);
                timer.current = window.setTimeout(() => setCopied(false), 1500);
              },
              () => undefined,
            )
          }
        >
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
      <pre className="oi-ddl-text mono" data-testid="object-info-ddl">
        {ddl}
      </pre>
    </div>
  );
}

function Preview({ page, connectionId, object }: { page: TableDataPage; connectionId: string; object: ObjectRef }) {
  const result = useMemo(() => pageToResult(page), [page]);
  if (page.rows.length === 0) return <p className="muted oi-pad">No rows</p>;
  return (
    <div className="oi-preview">
      <div className="oi-grid" data-testid="object-info-preview">
        <ResultGrid result={result} engine={dialectOfConnection(connectionId)} tableRef={object} cellsCapped={page.truncated_cells > 0} />
      </div>
      <div className="oi-foot muted">
        {page.has_more ? `First ${page.rows.length.toLocaleString('en-US')} rows` : `${page.rows.length.toLocaleString('en-US')} rows`}
        {' · read-only'}
      </div>
    </div>
  );
}

const KIND_NAME: Partial<Record<ObjectKind, string>> = { MaterializedView: 'Materialized view' };

/**
 * A database or schema: its object counts and its tables and views (a click shows one here,
 * Back returns), or the schemas of a PostgreSQL database (a click shows one).
 */
function NamespaceView({
  summary,
  scope,
  onFollow,
}: {
  summary: NamespaceSummary;
  scope: Scope;
  onFollow(to: { object: ObjectRef } | { scope: Scope }): void;
}) {
  const [filter, setFilter] = useState('');
  const q = filter.trim().toLowerCase();
  const rows: [string, string][] = [
    ['Name', scope.schema ?? scope.database],
    ['Type', scope.schema !== null ? 'Schema' : 'Database'],
    ...(scope.schema !== null ? ([['Database', scope.database]] as [string, string][]) : []),
  ];
  const d = summary.detail;
  const pg = summary.schemas !== null || scope.schema !== null;
  if (d?.owner) rows.push(['Owner', d.owner]);
  if (d?.charset) rows.push([pg ? 'Encoding' : 'Charset', d.charset]);
  if (d?.collation) rows.push(['Collation', d.collation]);
  if (d && d.size_bytes !== null) rows.push(['Size', formatBytes(d.size_bytes)]);
  if (summary.schemas) rows.push(['Schemas', String(summary.schemas.length)]);
  // Exact counts (the lists themselves), not estimates.
  for (const c of summary.counts) rows.push([folderLabel(c.kind), c.count === null ? DASH : c.count.toLocaleString('en-US')]);
  const tables = summary.relations.filter((r) => r.kind === 'Table');
  const estimates = tables.map((t) => t.row_estimate).filter((n): n is number => n !== null);
  if (tables.length > 0) rows.push(['Rows (estimate)', estimates.length === 0 ? DASH : formatCount(estimates.reduce((a, b) => a + b, 0))]);
  if (d?.comment) rows.push(['Comment', d.comment]);

  const names = summary.schemas ?? summary.relations.map((r) => r.name);
  const listed = summary.schemas
    ? summary.schemas.filter((s) => s.toLowerCase().includes(q))
    : summary.relations.filter((r) => r.name.toLowerCase().includes(q) || (r.comment ?? '').toLowerCase().includes(q));
  const what = summary.schemas ? 'schemas' : 'tables and views';

  return (
    <div className="oi-columns">
      <Rows rows={rows} />
      <h3 className="oi-h">{summary.schemas ? 'Schemas' : 'Tables and views'}</h3>
      {names.length === 0 ? (
        <p className="muted oi-none">None</p>
      ) : (
        <>
          <div className="oi-colbar">
            <label className="oi-filter">
              <SearchIcon />
              <input
                type="search"
                value={filter}
                placeholder={`Filter ${what}`}
                aria-label={`Filter ${what}`}
                onChange={(e) => setFilter(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Escape' && filter) {
                    e.stopPropagation();
                    setFilter('');
                  }
                }}
              />
            </label>
          </div>
          <div className="oi-colnote muted" role="status">
            {q ? `${listed.length} of ${names.length} ${what}` : `Click a name to see it here`}
          </div>
          <table className="oi-table" aria-label={summary.schemas ? 'Schemas' : 'Tables and views'}>
            <thead>
              <tr>
                <th>Name</th>
                {!summary.schemas && (
                  <>
                    <th>Type</th>
                    <th>Rows</th>
                    <th>Comment</th>
                  </>
                )}
              </tr>
            </thead>
            <tbody>
              {summary.schemas
                ? (listed as string[]).map((s) => (
                    <tr key={s}>
                      <td>
                        <button type="button" className="oi-link" onClick={() => onFollow({ scope: { database: scope.database, schema: s } })}>
                          {s}
                        </button>
                      </td>
                    </tr>
                  ))
                : (listed as ObjectSummary[]).map((r) => (
                    <tr key={`${r.kind}:${r.name}`}>
                      <td>
                        <button
                          type="button"
                          className="oi-link"
                          onClick={() => onFollow({ object: { database: scope.database, schema: scope.schema, name: r.name, kind: r.kind } })}
                        >
                          {r.name}
                        </button>
                      </td>
                      <td>{KIND_NAME[r.kind] ?? r.kind}</td>
                      <td className="mono">{r.row_estimate === null || r.kind !== 'Table' ? '' : formatCount(r.row_estimate)}</td>
                      <td>{r.comment || ''}</td>
                    </tr>
                  ))}
              {listed.length === 0 && (
                <tr>
                  <td colSpan={4} className="muted">
                    Nothing matches “{filter.trim()}”
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </>
      )}
    </div>
  );
}

/**
 * The Object info panel of the right sidebar (Navicat's information pane): the object selected
 * in the connection tree, as General, Columns (with indexes and foreign keys), DDL and a
 * read-only Preview of its first rows. Each section loads when shown; selecting another object
 * switches the panel.
 */
export function ObjectInfoPanel() {
  const selected = useSelectedObject();
  const [section, setSection] = useState<InfoSection>('general');
  const refs = useRef<Partial<Record<InfoSection, HTMLButtonElement | null>>>({});

  const pinned = useObjectInfo((s) => s.pinned);
  const history = useObjectInfo((s) => s.history);
  const connectionId = selected?.connectionId ?? '';
  const object = selected?.object;
  const ns = selected?.scope;
  const current: InfoTarget | null = object ? { connectionId, object } : ns ? { connectionId, scope: ns } : null;
  /** A foreign key's table, a database's table or schema: shown here, Back returns. */
  const followTo = (to: { object: ObjectRef } | { scope: Scope }) => current && useObjectInfo.getState().follow(current, { connectionId, ...to } as InfoTarget);
  const follow = (to: ObjectRef) => followTo({ object: to });
  const key = object ? `${connectionId}\u0000${objectNodeKey(object)}` : '';
  const nsKey = ns ? `${connectionId}\u0000${ns.database}\u0000${ns.schema ?? ''}` : '';
  const namespace = useLoad<NamespaceSummary>(nsKey, !!ns, () => loadNamespace(connectionId, ns!));
  const sections = object ? sectionsOf(object.kind) : [];
  const shown: InfoSection | undefined = sections.includes(section) ? section : sections[0];
  const rowsKind = !!object && hasRows(object.kind);

  const detail = useLoad<TableDetail>(key, !!object && rowsKind && (shown === 'general' || shown === 'columns'), () => api.tableDetail(connectionId, object!));
  const ddl = useLoad<string>(key, !!object && shown === 'ddl', () => api.objectDdl(connectionId, object!));
  const preview = useLoad<TableDataPage>(
    key,
    !!object && rowsKind && shown === 'preview',
    () => api.tableData(connectionId, object!, PREVIEW_ROWS, 0, [], null, PREVIEW_KEY),
    () => void api.tableDataCancel(connectionId, PREVIEW_KEY).catch(() => undefined),
  );
  const active = ns ? namespace : shown === 'ddl' ? ddl : shown === 'preview' ? preview : detail;
  const busy = active.state?.status === 'loading';
  const connected = useConnections((s) => s.info.has(connectionId));

  const refresh = async () => {
    if (ns) {
      // Drop the cached lists of the database / schema first (as the tree's Refresh does).
      const id = ['conn:-', `db:${encodeURIComponent(ns.database)}`, ...(ns.schema === null ? [] : [`schema:${encodeURIComponent(ns.schema)}`])].join('/');
      await api.refreshNode(connectionId, nodeKey(id)).catch(() => undefined);
      namespace.reload();
      return;
    }
    if (!object) return;
    // Metadata is cached by the backend: drop it first (only an optimisation when it fails).
    if (shown !== 'preview') await api.refreshNode(connectionId, objectNodeKey(object)).catch(() => undefined);
    active.reload();
  };

  const reconnect = async (load: Load<unknown>) => {
    load.loading();
    const error = await reopenConnection(connectionId);
    if (!error) load.reload();
    else load.fail(promptHandles(error) ? notOpenError() : error);
  };

  const select = (next: InfoSection) => {
    setSection(next);
    refs.current[next]?.focus();
  };
  const onTabKey = (e: KeyboardEvent) => {
    if (!shown) return;
    const at = sections.indexOf(shown);
    const target =
      e.key === 'ArrowRight' ? sections[(at + 1) % sections.length]
      : e.key === 'ArrowLeft' ? sections[(at - 1 + sections.length) % sections.length]
      : e.key === 'Home' ? sections[0]
      : e.key === 'End' ? sections[sections.length - 1]
      : undefined;
    if (!target) return;
    e.preventDefault();
    select(target);
  };

  return (
    <section className="hs-view oi-view" aria-label="Object info" data-testid="object-info-view">
      <div className="rs-head">
        <strong className="rs-title">Object info</strong>
        <span className="spacer" />
        {history.length > 0 && (
          <button
            type="button"
            className="icon-btn oi-back"
            aria-label="Back"
            title={`Back to ${targetName(history[history.length - 1]!)}`}
            onClick={() => useObjectInfo.getState().back()}
          >
            <ChevronIcon />
          </button>
        )}
        {current && (
          <button
            type="button"
            className={`icon-btn oi-pin${pinned ? ' on' : ''}`}
            aria-label="Pin"
            aria-pressed={pinned}
            title={pinned ? 'Unpin: follow the selection in the connection tree again' : 'Pin: keep this object while you select others in the tree'}
            onClick={() => useObjectInfo.getState().setPinned(!pinned, current)}
          >
            <PinIcon />
          </button>
        )}
        {current && (
          <button
            type="button"
            className="icon-btn"
            aria-label={busy ? 'Refreshing…' : 'Refresh'}
            title="Reload this section"
            disabled={busy}
            onClick={() => void refresh()}
          >
            <RefreshIcon />
          </button>
        )}
      </div>
      {selected && ns ? (
        <>
          <div className="oi-title">
            <span className="oi-object" title={selected.label}>
              {ns.schema ?? ns.database}
            </span>
            <span className="spacer" />
            <button
              type="button"
              className="btn small"
              title={`New query on ${ns.database}`}
              onClick={() => useQueryDocs.getState().newQuery(connectionId, ns.database)}
            >
              New query
            </button>
          </div>
          <ObjectContext connectionId={connectionId} object={ns} />
          <div className="oi-body">
            <Section load={namespace} onReconnect={() => void reconnect(namespace as Load<unknown>)}>
              {(summary) => <NamespaceView summary={summary} scope={ns} onFollow={followTo} />}
            </Section>
          </div>
        </>
      ) : !selected || !object ? (
        <p className="muted oi-empty" role="status">
          Select a database, table or view in the connection tree, or Ctrl+click a table name in a SQL editor,
          to see its details here.
        </p>
      ) : (
        <>
          <div className="oi-title">
            <span className="oi-object" title={selected.label}>
              {object.name}
            </span>
            <span className="spacer" />
            {rowsKind && (
              <button
                type="button"
                className="btn small"
                title="Open the data in a tab"
                onClick={() => openObjectRef(connectionId, object, selected.label)}
              >
                Open
              </button>
            )}
            {object.kind === 'Table' && connected && (
              <button
                type="button"
                className="btn small"
                onClick={() => void openDesignTab(connectionId, { database: object.database, schema: object.schema }, object.name)}
              >
                Design
              </button>
            )}
          </div>
          <ObjectContext connectionId={connectionId} object={object} />
          <div role="tablist" aria-label="Object info sections" className="oi-tabs" onKeyDown={onTabKey}>
            {sections.map((s) => (
              <button
                key={s}
                type="button"
                role="tab"
                id={`oi-tab-${s}`}
                aria-selected={s === shown}
                aria-controls="oi-panel"
                tabIndex={s === shown ? 0 : -1}
                className={`ot-subtab${s === shown ? ' on' : ''}`}
                ref={(el) => {
                  refs.current[s] = el;
                }}
                onClick={() => select(s)}
              >
                {SECTION_LABEL[s]}
              </button>
            ))}
          </div>
          <div role="tabpanel" id="oi-panel" aria-labelledby={`oi-tab-${shown}`} className={`oi-body oi-body-${shown}`}>
            {shown === 'general' ? (
              rowsKind ? (
                <Section load={detail} onReconnect={() => void reconnect(detail as Load<unknown>)}>
                  {(d) => <General object={object} detail={d} />}
                </Section>
              ) : (
                <General object={object} detail={null} />
              )
            ) : shown === 'columns' ? (
              <Section load={detail} onReconnect={() => void reconnect(detail as Load<unknown>)}>
                {(d) => <Columns detail={d} object={object} connectionId={connectionId} onFollow={follow} />}
              </Section>
            ) : shown === 'ddl' ? (
              <Section load={ddl} onReconnect={() => void reconnect(ddl as Load<unknown>)}>
                {(text) => <Ddl ddl={text} />}
              </Section>
            ) : (
              <Section load={preview} onReconnect={() => void reconnect(preview as Load<unknown>)}>
                {(page) => <Preview page={page} connectionId={connectionId} object={object} />}
              </Section>
            )}
          </div>
        </>
      )}
    </section>
  );
}
