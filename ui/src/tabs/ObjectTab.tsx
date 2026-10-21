import { useCallback, useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import Editor from '@monaco-editor/react';
import { needsReconnect } from '../api';
import { LockIcon, RefreshIcon } from '../components/icons';
import type { AppError, ColumnDetail, ForeignKeyDetail, IndexDetail, TableDetail } from '../bindings';
import { useConnections } from '../store/connections';
import { useObjectData, type Loadable, type Resource, type SubTab } from '../store/objectData';
import { DataView } from './DataView';
import { useDntMonacoTheme } from '../monaco/dntTheme';
import { editorFontOptions } from '../monaco/fonts';
import { dialectOfConnection, useQueryDocs } from '../store/queryTabs';
import type { ObjectTabRef as Tab } from '../store/tabs';
import './objectTab.css';

const DASH = '—';

const SUB_LABEL: Record<SubTab, string> = { data: 'Data', info: 'Info', ddl: 'DDL', structure: 'Structure' };

/** Base-1024 size with one decimal: 1536 -> "1.5 KB". */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit += 1;
  } while (Number(value.toFixed(1)) >= 1024 && unit < units.length - 1);
  return `${value.toFixed(1)} ${units[unit]}`;
}

const formatCount = (n: number) => `≈ ${n.toLocaleString('en-US')}`;

/**
 * Tables and views (incl. materialized) open on their Data (like Navicat), then Info, DDL and
 * Structure; other objects have DDL only.
 */
function subTabsOf(kind: Tab['objectRef']['kind']): SubTab[] {
  return kind === 'Table' || kind === 'View' || kind === 'MaterializedView' ? ['data', 'info', 'ddl', 'structure'] : ['ddl'];
}

// Data also loads the (cached) detail: its row estimate is shown next to the page.
const resourceOf = (sub: SubTab): Resource => (sub === 'ddl' ? 'ddl' : 'detail');

interface Recovery {
  onRetry(): void;
  onReconnect(): void;
}

/** The error of a sub-tab. A closed or lost connection offers Reconnect instead of Retry. */
function ErrorBox({ error, onRetry, onReconnect }: { error: AppError } & Recovery) {
  const reconnect = needsReconnect(error);
  return (
    <div role="alert" className={`ot-error${reconnect ? ' ot-reconnect' : ''}`}>
      <strong>{error.message}</strong>
      {error.hint ? <p>{error.hint}</p> : null}
      {error.detail ? (
        <details>
          <summary>Details</summary>
          <pre>{error.detail}</pre>
        </details>
      ) : null}
      {reconnect ? (
        <button type="button" className="btn small primary" onClick={onReconnect}>
          Reconnect
        </button>
      ) : (
        <button type="button" className="btn small" onClick={onRetry}>
          Retry
        </button>
      )}
    </div>
  );
}

function Loaded<T>({
  state,
  children,
  ...recovery
}: {
  state: Loadable<T> | undefined;
  children: (data: T) => React.ReactNode;
} & Recovery) {
  if (!state || state.status === 'loading') return <p className="ot-loading muted">Loading…</p>;
  if (state.status === 'error') return <ErrorBox error={state.error} {...recovery} />;
  return <>{children(state.data)}</>;
}

function InfoPanel({ detail, kind }: { detail: TableDetail; kind: Tab['objectRef']['kind'] }) {
  const { options } = detail;
  // A plain view has no storage: its Info lists what it does have rather than a column of dashes.
  if (kind === 'View') {
    const viewRows: [string, string][] = [
      ['Columns', String(detail.columns.length)],
      ...(options.collation ? ([['Collation', options.collation]] as [string, string][]) : []),
      ['Comment', detail.comment || DASH],
    ];
    return <InfoRows rows={viewRows} />;
  }
  const rows: [string, string][] = [
    ['Row estimate', detail.row_estimate === null ? DASH : formatCount(detail.row_estimate)],
    ['Size', detail.size_bytes === null ? DASH : formatBytes(detail.size_bytes)],
    ['Engine', options.engine ?? DASH],
    ['Collation', options.collation ?? DASH],
    ['Tablespace', options.tablespace ?? DASH],
    ['Partitioned by', options.partitioned_by ?? DASH],
    ['System versioned', options.system_versioned ? 'Yes' : 'No'],
    ['Comment', detail.comment || DASH],
  ];
  return <InfoRows rows={rows} />;
}

function InfoRows({ rows }: { rows: [string, string][] }) {
  return (
    <dl className="ot-info">
      {rows.map(([label, value]) => (
        <div key={label} className="ot-info-row">
          <dt>{label}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

/** Opens a new query tab on the object's connection and database, holding `sql`. */
function openQuery(tab: Tab, sql: string) {
  const docs = useQueryDocs.getState();
  const id = docs.newQuery(tab.connectionId, tab.objectRef.database);
  docs.update(id, { sql });
}

const READ_ONLY_TIP = 'This connection is read-only. Change it in Edit connection.';

/** A section's toolbar (Info, DDL, Structure): its own actions, then Refresh at the right end. */
function SectionBar({ label, refresh, children }: { label: string; refresh: ReactNode; children?: ReactNode }) {
  return (
    <div className="ot-bar" role="toolbar" aria-label={label}>
      {children}
      {refresh}
    </div>
  );
}

/** Reloads the visible section (its metadata / DDL); busy while that section loads. */
function RefreshButton({ busy, onRefresh }: { busy: boolean; onRefresh(): void }) {
  return (
    <button
      type="button"
      className="icon-btn"
      aria-label={busy ? 'Refreshing…' : 'Refresh'}
      title={busy ? 'Loading this section…' : 'Reload this section'}
      disabled={busy}
      aria-busy={busy}
      onClick={onRefresh}
    >
      <RefreshIcon />
    </button>
  );
}

/** The row estimate of a loaded detail (views have none). */
const estimateOf = (detail: Loadable<TableDetail> | undefined): number | null =>
  detail?.status === 'ok' ? detail.data.row_estimate : null;

function DdlPanel({ ddl, tab, refresh }: { ddl: string; tab: Tab; refresh: ReactNode }) {
  const monacoTheme = useDntMonacoTheme();
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const [wrap, setWrap] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(ddl);
    } catch {
      setCopied(false);
      setCopyError(true);
      return;
    }
    setCopyError(false);
    setCopied(true);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1500);
  };

  return (
    <>
      <SectionBar label="DDL" refresh={refresh}>
        <button type="button" className="btn small" aria-pressed={wrap} title="Wrap long lines" onClick={() => setWrap((w) => !w)}>
          Wrap
        </button>
        <button type="button" className="btn small" onClick={() => openQuery(tab, ddl)}>
          Open in editor
        </button>
        <button type="button" className="btn small" onClick={() => void copy()}>
          {copied ? 'Copied' : 'Copy'}
        </button>
      </SectionBar>
      <div className="ot-body ot-ddl">
        {copyError ? (
          <div role="alert" className="ot-error">
            Couldn't copy — select the text and press Ctrl+C.
          </div>
        ) : null}
        <div className="ot-ddl-text" data-testid="object-ddl-text">
          <Editor
            language={dialectOfConnection(tab.connectionId) === 'postgres' ? 'pgsql' : 'mysql'}
            value={ddl}
            theme={monacoTheme.theme}
            beforeMount={monacoTheme.beforeMount}
            options={{
              readOnly: true,
              domReadOnly: true,
              automaticLayout: true,
              minimap: { enabled: false },
              ...editorFontOptions(),
              scrollBeyondLastLine: false,
              folding: false,
              renderLineHighlight: 'none',
              wordWrap: wrap ? 'on' : 'off',
              lightbulb: { enabled: 'off' as never },
              ariaLabel: 'DDL',
            }}
          />
        </div>
      </div>
    </>
  );
}

const orDash = (s: string | null | undefined) => (s === null || s === undefined || s === '' || s.toUpperCase() === 'NULL' ? DASH : s);

function extraOf(c: ColumnDetail): string {
  return [
    c.auto_increment ? 'auto_increment' : null,
    c.identity ? `identity (${c.identity})` : null,
    c.generated ? `generated (${c.generated})` : null,
    c.invisible ? 'invisible' : null,
  ]
    .filter(Boolean)
    .join(', ');
}

const indexTarget = (i: IndexDetail) => (i.columns.length > 0 ? i.columns.join(', ') : (i.expression ?? DASH));

function referenceOf(fk: ForeignKeyDetail): string {
  const table = fk.ref_schema ? `${fk.ref_schema}.${fk.ref_table}` : fk.ref_table;
  return `${fk.columns.join(', ')} → ${table}(${fk.ref_columns.join(', ')})`;
}

function StructurePanel({ detail }: { detail: TableDetail }) {
  const indexes = [
    ...(detail.primary_key ? [detail.primary_key] : []),
    ...detail.indexes.filter((i) => !(detail.primary_key && i.primary)),
  ];
  return (
    <div className="ot-structure">
      <section aria-label="Columns">
        <h3>Columns</h3>
        <table className="ot-table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Type</th>
              <th>Nullable</th>
              <th>Default</th>
              <th>Extra</th>
              <th>Comment</th>
            </tr>
          </thead>
          <tbody>
            {detail.columns.map((c) => (
              <tr key={c.name}>
                <td>{c.name}</td>
                <td className="mono">{c.db_type}</td>
                <td>{c.nullable ? 'YES' : 'NO'}</td>
                <td className="mono">{orDash(c.default)}</td>
                <td>{extraOf(c)}</td>
                <td>{c.comment || ''}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
      <section aria-label="Indexes">
        <h3>Indexes</h3>
        {indexes.length === 0 ? (
          <p className="muted">None</p>
        ) : (
          <table className="ot-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Columns</th>
                <th>Unique</th>
                <th>Method</th>
              </tr>
            </thead>
            <tbody>
              {indexes.map((i) => (
                <tr key={`${i.primary ? 'pk' : 'ix'}:${i.name}`}>
                  <td>
                    {i.name}
                    {i.primary ? <span className="chip ot-pk">PK</span> : null}
                  </td>
                  <td className="mono">{indexTarget(i)}</td>
                  <td>{i.unique || i.primary ? 'YES' : 'NO'}</td>
                  <td>{i.method ?? DASH}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      <section aria-label="Foreign keys">
        <h3>Foreign keys</h3>
        {detail.foreign_keys.length === 0 ? (
          <p className="muted">None</p>
        ) : (
          <table className="ot-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Columns</th>
                <th>On update</th>
                <th>On delete</th>
              </tr>
            </thead>
            <tbody>
              {detail.foreign_keys.map((fk, i) => (
                <tr key={`${fk.name}:${i}`}>
                  <td>{fk.name}</td>
                  <td className="mono">{referenceOf(fk)}</td>
                  <td>{fk.on_update}</td>
                  <td>{fk.on_delete}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      <section aria-label="Checks">
        <h3>Checks</h3>
        {detail.checks.length === 0 ? (
          <p className="muted">None</p>
        ) : (
          <ul className="ot-list">
            {detail.checks.map((c, i) => (
              <li key={`${c.name}:${i}`}>
                <span>{c.name}</span> <code className="mono">{c.expression}</code>
              </li>
            ))}
          </ul>
        )}
      </section>
      <section aria-label="Triggers">
        <h3>Triggers</h3>
        {detail.triggers.length === 0 ? (
          <p className="muted">None</p>
        ) : (
          <ul className="ot-list">
            {detail.triggers.map((t, i) => (
              <li key={`${t.name}:${i}`}>
                <span>{t.name}</span> <span className="muted">{`${t.timing} ${t.event}`}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

export function ObjectTab({ tab }: { tab: Tab }) {
  const uid = useId();
  const subs = subTabsOf(tab.objectRef.kind);
  const stored = useObjectData((s) => s.byTab[tab.id]);
  const sub = stored?.sub && subs.includes(stored.sub) ? stored.sub : subs[0]!;
  const resource = resourceOf(sub);
  const tabRefs = useRef<Partial<Record<SubTab, HTMLButtonElement | null>>>({});

  // Lazy: fetch the data of the visible sub-tab once; the result is cached for the tab's lifetime.
  useEffect(() => {
    void useObjectData.getState().load(tab, resource);
  }, [tab, resource]);

  const select = useCallback(
    (next: SubTab) => {
      useObjectData.getState().setSub(tab.id, next);
      tabRefs.current[next]?.focus();
    },
    [tab.id],
  );

  const onKeyDown = (e: KeyboardEvent) => {
    const at = subs.indexOf(sub);
    const target =
      e.key === 'ArrowRight' ? subs[(at + 1) % subs.length]
      : e.key === 'ArrowLeft' ? subs[(at - 1 + subs.length) % subs.length]
      : e.key === 'Home' ? subs[0]
      : e.key === 'End' ? subs[subs.length - 1]
      : undefined;
    if (!target) return;
    e.preventDefault();
    select(target);
  };

  const retry = () => void useObjectData.getState().load(tab, resource, true);
  const reconnect = () => void useObjectData.getState().reconnect(tab, resource);
  const { objectRef } = tab;
  const conn = useConnections((s) => s.summaries.get(tab.connectionId)?.config);
  const path = [conn?.name, objectRef.database, objectRef.schema].filter(Boolean).join(' › ');
  const state = stored?.[resource];
  // The Data sub-tab has its own Refresh (rows and metadata) in its toolbar.
  const refresh = (
    <RefreshButton busy={state?.status === 'loading'} onRefresh={() => void useObjectData.getState().refresh(tab, resource)} />
  );

  return (
    <div className="object-tab">
      <header className="ot-head">
        <strong className="ot-name" title={objectRef.name}>
          {objectRef.name}
        </strong>
        <span className="ot-path" title={path}>
          {path}
        </span>
        {conn?.read_only ? (
          <span className="ot-ro" title={READ_ONLY_TIP}>
            <LockIcon />
            Read-only
          </span>
        ) : null}
        <div role="tablist" aria-label="Object sections" className="segmented ot-tabs" onKeyDown={onKeyDown}>
          {subs.map((s) => (
            <button
              key={s}
              type="button"
              role="tab"
              id={`${uid}-tab-${s}`}
              aria-selected={s === sub}
              aria-controls={`${uid}-panel`}
              tabIndex={s === sub ? 0 : -1}
              className={`segment${s === sub ? ' on' : ''}`}
              data-testid={s === 'ddl' ? 'object-tab-ddl' : undefined}
              ref={(el) => {
                tabRefs.current[s] = el;
              }}
              onClick={() => select(s)}
            >
              {SUB_LABEL[s]}
            </button>
          ))}
        </div>
      </header>
      <div
        role="tabpanel"
        id={`${uid}-panel`}
        aria-labelledby={`${uid}-tab-${sub}`}
        className={sub === 'data' ? 'ot-panel ot-panel-data' : 'ot-panel'}
      >
        {sub === 'data' ? (
          <DataView tab={tab} estimate={estimateOf(state as Loadable<TableDetail> | undefined)} />
        ) : sub === 'ddl' && state?.status === 'ok' ? (
          <DdlPanel ddl={state.data as string} tab={tab} refresh={refresh} />
        ) : (
          <>
            <SectionBar label={SUB_LABEL[sub]} refresh={refresh} />
            <div className="ot-body">
              {sub === 'ddl' ? (
                <Loaded state={state as Loadable<string> | undefined} onRetry={retry} onReconnect={reconnect}>
                  {() => null}
                </Loaded>
              ) : (
                <Loaded state={state as Loadable<TableDetail> | undefined} onRetry={retry} onReconnect={reconnect}>
                  {(detail) => (sub === 'info' ? <InfoPanel detail={detail} kind={objectRef.kind} /> : <StructurePanel detail={detail} />)}
                </Loaded>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
