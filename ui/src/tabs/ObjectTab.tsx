import { useCallback, useEffect, useId, useRef, useState, type KeyboardEvent } from 'react';
import Editor from '@monaco-editor/react';
import { needsReconnect } from '../api';
import type { AppError, ColumnDetail, ForeignKeyDetail, IndexDetail, TableDetail } from '../bindings';
import { useObjectData, type Loadable, type SubTab } from '../store/objectData';
import { DataPager, DataView } from './DataView';
import { READ_ONLY_TIP, resourceOf, subTabsOf, useSubTab } from './objectSections';
import { DDL_EDITABLE_KINDS, openDdlEditTab } from '../components/openDdlEdit';
import { useConnections } from '../store/connections';
import { useDntMonacoTheme } from '../monaco/dntTheme';
import { editorFontOptions } from '../monaco/fonts';
import { dialectOfConnection, useQueryDocs } from '../store/queryTabs';
import { useTabs, type ObjectTabRef as Tab } from '../store/tabs';
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

/** The row estimate of a loaded detail (views have none). */
const estimateOf = (detail: Loadable<TableDetail> | undefined): number | null =>
  detail?.status === 'ok' ? detail.data.row_estimate : null;

function DdlPanel({ ddl, tab }: { ddl: string; tab: Tab }) {
  const monacoTheme = useDntMonacoTheme();
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState(false);
  const [wrap, setWrap] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const conn = useConnections((s) => s.summaries.get(tab.connectionId)?.config);
  const editable = !!conn && DDL_EDITABLE_KINDS[conn.kind === 'postgres' ? 'postgres' : 'mysql'].includes(tab.objectRef.kind);

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
      <div className="ot-bar" role="toolbar" aria-label="DDL">
        <button type="button" className="btn small" aria-pressed={wrap} title="Wrap long lines" onClick={() => setWrap((w) => !w)}>
          Wrap
        </button>
        <button type="button" className="btn small" onClick={() => openQuery(tab, ddl)}>
          Open in editor
        </button>
        {editable ? (
          <button
            type="button"
            className="btn small"
            disabled={!!conn?.read_only}
            title={conn?.read_only ? READ_ONLY_TIP : 'Edit and apply this definition'}
            onClick={() => {
              const { objectRef: o } = tab;
              openDdlEditTab(tab.connectionId, { database: o.database, schema: o.schema }, o.kind, o.name, null);
            }}
          >
            Edit
          </button>
        ) : null}
        <button type="button" className="btn small" onClick={() => void copy()}>
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
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
  const sub = useSubTab(tab);
  const resource = resourceOf(sub);
  const tabRefs = useRef<Partial<Record<SubTab, HTMLButtonElement | null>>>({});

  // Lazy: fetch the data of the visible sub-tab once; the result is cached for the tab's lifetime.
  useEffect(() => {
    if (!tab.needsVerification) void useObjectData.getState().load(tab, resource);
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
  const state = stored?.[resource];
  const estimate = estimateOf(sub === 'data' ? (state as Loadable<TableDetail> | undefined) : undefined);

  if (tab.needsVerification) return <div role="alert" className="ot-error">
    <p>Object action outcome unknown. Cached data is hidden. Reload to verify the object before continuing.</p>
    <button className="btn" onClick={() => useTabs.setState((s) => ({ tabs: s.tabs.map((t) => t.id === tab.id ? { ...t, needsVerification: false } : t) }))}>Reload object</button>
  </div>;

  // No header row (editor v2 §2): the name is the tab's title, the connection and actions are in
  // the strip's cluster. The sections and the Data pager share one 28 px row.
  return (
    <div className="object-tab">
      <div className="ot-subbar">
        <div role="tablist" aria-label="Object sections" className="ot-tabs" onKeyDown={onKeyDown}>
          {subs.map((s) => (
            <button
              key={s}
              type="button"
              role="tab"
              id={`${uid}-tab-${s}`}
              aria-selected={s === sub}
              aria-controls={`${uid}-panel`}
              tabIndex={s === sub ? 0 : -1}
              className={`ot-subtab${s === sub ? ' on' : ''}`}
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
        {sub === 'data' ? <DataPager tab={tab} estimate={estimate} /> : null}
      </div>
      <div
        role="tabpanel"
        id={`${uid}-panel`}
        aria-labelledby={`${uid}-tab-${sub}`}
        className={sub === 'data' ? 'ot-panel ot-panel-data' : 'ot-panel'}
      >
        {sub === 'data' ? (
          <DataView key={`${tab.id}:${tab.resourceRevision ?? 0}`} tab={tab} />
        ) : sub === 'ddl' && state?.status === 'ok' ? (
          <DdlPanel ddl={state.data as string} tab={tab} />
        ) : (
          <>
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
