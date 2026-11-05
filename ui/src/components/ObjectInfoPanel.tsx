import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { api, isCancelled, notOpenError, toAppError } from '../api';
import type { AppError, ColumnDetail, ObjectRef, TableDataPage, TableDetail } from '../bindings';
import { useConnections } from '../store/connections';
import type { Loadable } from '../store/objectData';
import { dialectOfConnection } from '../store/queryTabs';
import { promptHandles, reopenConnection } from '../store/reopen';
import { useTree, type TreeNode } from '../store/tree';
import { connectionIdOf, objectNodeKey } from '../store/treeModel';
import { pageToResult } from '../tabs/DataView';
import { ErrorBox, extraOf, formatBytes, formatCount, indexTarget, orDash, referenceOf } from '../tabs/ObjectTab';
import { ResultGrid } from '../tabs/query/ResultGrid';
import { ChevronIcon, RefreshIcon } from './icons';
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

/** The selected tree node when it is an object of a known connection, else null. */
function useSelectedObject(): { node: TreeNode; connectionId: string; object: ObjectRef } | null {
  const node = useTree((s) => (s.selectedId ? s.nodes.get(s.selectedId) : undefined));
  return useMemo(() => {
    const connectionId = node ? connectionIdOf(node.id) : null;
    if (!node || node.kind !== 'object' || !node.objectRef || !connectionId) return null;
    return { node, connectionId, object: node.objectRef };
  }, [node]);
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

function Columns({ detail, kind }: { detail: TableDetail; kind: ObjectRef['kind'] }) {
  const indexes = detail.indexes.filter((i) => !i.primary);
  return (
    <div className="oi-columns">
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
          {detail.columns.map((c) => {
            const [type, length] = splitType(c.db_type);
            return (
              <tr key={c.name}>
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
                    <td className="mono">{referenceOf(fk)}</td>
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

/**
 * The Object info panel of the right sidebar (Navicat's information pane): the object selected
 * in the connection tree, as General, Columns (with indexes and foreign keys), DDL and a
 * read-only Preview of its first rows. Each section loads when shown; selecting another object
 * switches the panel.
 */
export function ObjectInfoPanel({ onCollapse }: { onCollapse?: () => void }) {
  const selected = useSelectedObject();
  const [section, setSection] = useState<InfoSection>('general');
  const refs = useRef<Partial<Record<InfoSection, HTMLButtonElement | null>>>({});

  const connectionId = selected?.connectionId ?? '';
  const object = selected?.object;
  const key = object ? `${connectionId}\u0000${objectNodeKey(object)}` : '';
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
  const active = shown === 'ddl' ? ddl : shown === 'preview' ? preview : detail;
  const busy = active.state?.status === 'loading';
  const connected = useConnections((s) => s.info.has(connectionId));

  const refresh = async () => {
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
        {object && (
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
        {onCollapse && (
          <button
            type="button"
            className="icon-btn"
            data-testid="right-sidebar-collapse"
            aria-label="Collapse right sidebar"
            title="Collapse right sidebar (Ctrl+Alt+B)"
            onClick={onCollapse}
          >
            <ChevronIcon />
          </button>
        )}
      </div>
      {!selected || !object ? (
        <p className="muted oi-empty" role="status">
          Select a table or view in the connection tree to see its columns, DDL and sample rows.
        </p>
      ) : (
        <>
          <div className="oi-title">
            <span className="oi-object" title={selected.node.label}>
              {object.name}
            </span>
            <span className="oi-where muted">{object.schema ? `${object.database}.${object.schema}` : object.database}</span>
            <span className="spacer" />
            {rowsKind && (
              <button
                type="button"
                className="btn small"
                title="Open the data in a tab"
                onClick={() => openObjectRef(connectionId, object, selected.node.label)}
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
                {(d) => <Columns detail={d} kind={object.kind} />}
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
