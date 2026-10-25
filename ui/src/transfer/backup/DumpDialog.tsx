import { useEffect, useRef, useState } from 'react';
import { api, toAppError } from '../../api';
import type {
  DumpObject,
  DumpObjects,
  DumpRequest,
  MysqlDumpOptions,
  ObjectKind,
  PgDumpFormat,
  PgDumpOptions,
  Tool,
  ToolDumpRequest,
  ToolsList,
} from '../../bindings';
import { showJobs } from '../../components/windowActions';
import { Modal } from '../../dialogs/Modal';
import { useConnections } from '../../store/connections';
import type { UiRequest } from '../../store/dialogs';
import { useJobs } from '../../store/jobs';
import { TOOL_LABEL } from './backupFormat';
import './backup.css';

/** What "Run again" hands the dialog: the stored request of a built-in or a tool dump. */
export type DumpParams = NonNullable<Extract<UiRequest, { kind: 'dump' }>['params']>;

type Method = 'builtin' | Tool;

interface Props {
  connectionId: string;
  database: string;
  /** PostgreSQL: the schema the dump starts with (every schema when absent). */
  schema?: string | null;
  /** "Dump table…": the only object pre-selected. */
  table?: string;
  params?: DumpParams;
  onClose(): void;
}

interface Pick {
  on: boolean;
  /** Tables: dump the rows too. */
  data: boolean;
}

const keyOf = (o: { schema: string | null; name: string; kind: ObjectKind }) => `${o.schema ?? ''}\u0000${o.kind}\u0000${o.name}`;

const GROUPS: { kind: ObjectKind; label: string }[] = [
  { kind: 'Table', label: 'Tables' },
  { kind: 'View', label: 'Views' },
  { kind: 'Sequence', label: 'Sequences' },
  { kind: 'Type', label: 'Types' },
];

const MYSQL_DEFAULTS: MysqlDumpOptions = {
  tables: [],
  structure: true,
  data: true,
  routines: false,
  events: false,
  triggers: true,
  single_transaction: true,
};

const PG_DEFAULTS: PgDumpOptions = {
  schemas: [],
  tables: [],
  structure: true,
  data: true,
  format: 'plain',
  no_owner: false,
  no_privileges: false,
};

/**
 * The dump dialog (spec §7.4): the built-in dump or an external tool, the objects to dump (with
 * the data toggle of each table and the objects the built-in dump skips), the options and the
 * output file. A dump is a read: **Start dump** starts the job without a confirmation.
 */
export function DumpDialog({ connectionId, database, schema, table, params, onClose }: Props) {
  const cfg = useConnections((s) => s.summaries.get(connectionId)?.config);
  const pg = cfg?.kind === 'postgres';
  const builtinParams = params?.kind === 'dump' ? params.request : null;
  const toolParams = params?.kind === 'tool_dump' ? params.request : null;

  const [method, setMethod] = useState<Method>(toolParams?.tool ?? 'builtin');
  const [tools, setTools] = useState<ToolsList | null>(null);
  const [allSchemas, setAllSchemas] = useState<string[] | null>(null);
  // PostgreSQL: the schemas dumped; `null` until the schema list says "all of them".
  const [schemas, setSchemas] = useState<string[] | null>(() => {
    if (!pg) return [];
    const stored = builtinParams?.schemas ?? toolParams?.pg?.schemas ?? [];
    if (stored.length > 0) return stored;
    return schema ? [schema] : null;
  });
  const [listing, setListing] = useState<DumpObjects | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [picks, setPicks] = useState<Record<string, Pick>>({});
  const firstListing = useRef(true);
  const [includeDrop, setIncludeDrop] = useState(builtinParams?.include_drop ?? false);
  const [compress, setCompress] = useState(params?.request.compress ?? false);
  const [mysql, setMysql] = useState<MysqlDumpOptions>(toolParams?.mysql ?? MYSQL_DEFAULTS);
  const [pgOpts, setPgOpts] = useState<PgDumpOptions>(toolParams?.pg ?? PG_DEFAULTS);
  const [path, setPath] = useState<string | null>(params?.request.path ?? null);
  const [picking, setPicking] = useState(false);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api.toolsList().then(
      (result) => live && setTools(result),
      // No tool list: the tool methods say "not found"; the built-in dump still works.
      () => live && setTools({ tools: [], missing: [], searched: [] }),
    );
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    if (!pg) return;
    let live = true;
    api.listSchemas(connectionId, database, false).then(
      (result) => {
        if (!live) return;
        const names = result.map((s) => s.name);
        setAllSchemas(names);
        setSchemas((current) => current ?? names);
      },
      (e) => live && setListError(toAppError(e).message),
    );
    return () => {
      live = false;
    };
  }, [pg, connectionId, database]);

  useEffect(() => {
    if (schemas === null) return;
    if (pg && schemas.length === 0) {
      setListing({ objects: [], skipped: [] });
      return;
    }
    let live = true;
    setListError(null);
    api.dumpObjects(connectionId, database, schemas).then(
      (result) => {
        if (!live) return;
        const first = firstListing.current;
        firstListing.current = false;
        setPicks((current) => {
          const next: Record<string, Pick> = {};
          for (const o of result.objects) {
            const key = keyOf(o);
            next[key] = current[key] ?? (first ? initialPick(o) : { on: true, data: true });
          }
          return next;
        });
        setListing(result);
      },
      (e) => live && setListError(toAppError(e).message),
    );
    return () => {
      live = false;
    };
    // `initialPick` reads only the props (fixed for the dialog's life).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionId, database, pg, schemas]);

  /** The first listing's selection: the stored request, the table of "Dump table…", or everything. */
  function initialPick(o: DumpObject): Pick {
    if (builtinParams) {
      const found = builtinParams.objects.find((s) => keyOf(s) === keyOf(o));
      return { on: !!found, data: found ? found.data : true };
    }
    if (toolParams?.mysql) {
      const t = toolParams.mysql.tables;
      return { on: t.length === 0 || t.includes(o.name), data: true };
    }
    if (toolParams?.pg) {
      const t = toolParams.pg.tables;
      return { on: t.length === 0 || t.some(([s, n]) => s === o.schema && n === o.name), data: true };
    }
    if (table) return { on: o.kind === 'Table' && o.name === table && (!schema || o.schema === schema), data: true };
    return { on: true, data: true };
  }

  const objects = listing?.objects ?? [];
  const selected = objects.filter((o) => picks[keyOf(o)]?.on);
  const allOn = selected.length === objects.length;
  const builtin = method === 'builtin';
  const toolInfo = builtin ? null : (tools?.tools.find((t) => t.tool === method) ?? null);
  const archive = method === 'pg_dump' && pgOpts.format !== 'plain';
  const toolMethods: Tool[] = pg ? ['pg_dump'] : ['mysql_dump', 'mariadb_dump'];
  const toolOutputEmpty = !builtin && (pg ? !pgOpts.structure && !pgOpts.data : !mysql.structure && !mysql.data);

  const display = (o: { schema: string | null; name: string }) => (pg && o.schema ? `${o.schema}.${o.name}` : o.name);

  const setPick = (o: DumpObject, change: Partial<Pick>) => {
    const key = keyOf(o);
    setPicks((p) => ({ ...p, [key]: { ...(p[key] ?? { on: false, data: true }), ...change } }));
  };

  const selectAll = (on: boolean) =>
    setPicks((p) => Object.fromEntries(Object.entries(p).map(([k, v]) => [k, { ...v, on }])));

  const toggleSchema = (name: string) => {
    const current = schemas ?? [];
    const next = current.includes(name) ? current.filter((s) => s !== name) : [...current, name];
    const order = allSchemas ?? [];
    const rank = (s: string) => (order.includes(s) ? order.indexOf(s) : order.length);
    setSchemas(next.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b)));
  };

  /** The picked file's extension follows Compress: changing it asks for a file again. */
  const changeCompress = (value: boolean) => {
    if (value === compress) return;
    setCompress(value);
    setPath(null);
  };

  const baseName = () => {
    if (table) return table;
    if (pg && schemas?.length === 1) return `${database}-${schemas[0]}`;
    return database;
  };

  const browse = async () => {
    setPicking(true);
    setError(null);
    try {
      const picked = await api.dumpPickPath(baseName(), compress && !archive);
      if (picked) setPath(picked);
    } catch (e) {
      setError(toAppError(e).message);
    } finally {
      setPicking(false);
    }
  };

  const builtinRequest = (target: string): DumpRequest => ({
    connection_id: connectionId,
    database,
    schemas: pg ? (schemas ?? []) : [],
    objects: selected.map((o) => ({ schema: o.schema, name: o.name, kind: o.kind, data: o.kind === 'Table' ? !!picks[keyOf(o)]?.data : false })),
    include_drop: includeDrop,
    compress,
    path: target,
  });

  const toolRequest = (tool: Tool, target: string): ToolDumpRequest => {
    const named = selected.filter((o) => o.kind !== 'Type');
    return {
      connection_id: connectionId,
      database,
      tool,
      mysql: pg ? null : { ...mysql, tables: allOn ? [] : named.map((o) => o.name) },
      pg: pg
        ? { ...pgOpts, schemas: schemas ?? [], tables: allOn ? [] : named.map((o) => [o.schema ?? 'public', o.name] as [string, string]) }
        : null,
      compress: compress && !archive,
      path: target,
    };
  };

  const canStart =
    !!path && !!listing && selected.length > 0 && !starting && !picking && (builtin || !!toolInfo) && !toolOutputEmpty;

  const start = async () => {
    if (!path || !canStart) return;
    setStarting(true);
    setError(null);
    try {
      if (builtin) await api.dumpStart(builtinRequest(path));
      else await api.toolDumpStart(toolRequest(method, path));
      setStarting(false);
      onClose();
      showJobs();
      void useJobs.getState().load();
    } catch (e) {
      setError(toAppError(e).message);
      setStarting(false);
    }
  };

  const close = () => {
    if (!starting) onClose();
  };

  const methodLabel = (m: Method) => (m === 'builtin' ? 'Built-in' : TOOL_LABEL[m]);

  return (
    <Modal title={table ? `Dump table ${table}` : `Dump ${database}`} onClose={close} className="backup-dialog" testId="dump-dialog">
      <div className="modal-body stack backup-body">
        <section className="stack" aria-label="Method">
          <div className="segmented backup-methods" role="radiogroup" aria-label="Dump method">
            {(['builtin', ...toolMethods] as Method[]).map((m) => (
              <button
                key={m}
                type="button"
                role="radio"
                aria-checked={method === m}
                className={`segment${method === m ? ' on' : ''}`}
                onClick={() => setMethod(m)}
              >
                {methodLabel(m)}
              </button>
            ))}
          </div>
          {builtin ? (
            <p className="hint">
              DntDatabase&apos;s own dump: tables (structure and data) and views{pg ? ', with the schemas, types and sequences they need' : ''}. Use an external tool for full fidelity.
            </p>
          ) : (
            <div className="backup-tool-status" data-testid="dump-tool-status" role="status">
              {!tools && <p className="hint">Looking for {methodLabel(method)}…</p>}
              {tools && toolInfo && (
                <>
                  <p>{toolInfo.version}</p>
                  <p className="hint mono">{toolInfo.path}</p>
                </>
              )}
              {tools && !toolInfo && (
                <>
                  <p className="field-error">{methodLabel(method)} was not found.</p>
                  {tools.searched.length > 0 ? (
                    <>
                      <p className="hint">Folders searched:</p>
                      <ul className="backup-folders">
                        {tools.searched.map((f) => (
                          <li key={f} className="mono">
                            {f}
                          </li>
                        ))}
                      </ul>
                    </>
                  ) : (
                    <p className="hint">No folder was searched.</p>
                  )}
                  <p className="hint">Install the client tools or set the External tools folder in Settings.</p>
                </>
              )}
            </div>
          )}
        </section>

        {pg && (
          <fieldset className="backup-fieldset">
            <legend>Schemas</legend>
            {allSchemas === null && <p className="hint">Loading schemas…</p>}
            <div className="backup-schemas">
              {(allSchemas ?? schemas ?? []).map((s) => (
                <label key={s} className="check">
                  <input type="checkbox" aria-label={`Schema ${s}`} checked={!!schemas?.includes(s)} onChange={() => toggleSchema(s)} />
                  <span>{s}</span>
                </label>
              ))}
            </div>
            {schemas?.length === 0 && <p className="field-error">Choose at least one schema.</p>}
          </fieldset>
        )}

        <section className="stack" aria-label="Objects">
          <div className="backup-row">
            <h3 className="backup-heading">Objects</h3>
            <span className="spacer" />
            <button type="button" className="btn small" disabled={!listing} onClick={() => selectAll(true)}>
              Select all
            </button>
            <button type="button" className="btn small" disabled={!listing} onClick={() => selectAll(false)}>
              Select none
            </button>
          </div>
          {listError && (
            <p role="alert" className="field-error">
              {listError}
            </p>
          )}
          {!listing && !listError && <p className="hint">Listing the objects…</p>}
          {listing && (
            <div className="backup-objects" data-testid="dump-objects">
              {objects.length === 0 && <p className="hint">Nothing to dump here.</p>}
              {GROUPS.map(({ kind, label }) => {
                const group = objects.filter((o) => o.kind === kind);
                if (group.length === 0) return null;
                const dataColumn = builtin && kind === 'Table';
                return (
                  <div key={kind} className="backup-group" role="group" aria-label={label}>
                    <div className="backup-group-head">
                      <span>
                        {label} ({group.length})
                      </span>
                      {dataColumn && <span className="backup-data-col">Data</span>}
                    </div>
                    {group.map((o) => {
                      const p = picks[keyOf(o)];
                      return (
                        <div key={keyOf(o)} className="backup-object">
                          <label className="check">
                            <input type="checkbox" checked={!!p?.on} onChange={(e) => setPick(o, { on: e.target.checked })} />
                            <span className="mono">{display(o)}</span>
                          </label>
                          {dataColumn && (
                            <input
                              type="checkbox"
                              className="backup-data-col"
                              aria-label={`Data of ${display(o)}`}
                              checked={!!p?.data}
                              disabled={!p?.on}
                              onChange={(e) => setPick(o, { data: e.target.checked })}
                            />
                          )}
                        </div>
                      );
                    })}
                  </div>
                );
              })}
            </div>
          )}
          {listing && objects.length > 0 && selected.length === 0 && <p className="field-error">Choose at least one object.</p>}
          {builtin && listing && listing.skipped.length > 0 && (
            <details className="backup-skipped" data-testid="dump-skipped">
              <summary>Not in the built-in dump ({listing.skipped.length})</summary>
              <ul>
                {listing.skipped.map((s) => (
                  <li key={keyOf(s)}>
                    <span className="mono">{display(s)}</span> ({s.kind}): {s.reason}
                  </li>
                ))}
              </ul>
            </details>
          )}
        </section>

        <section className="stack" aria-label="Options">
          <h3 className="backup-heading">Options</h3>
          <div className="backup-options">
            {builtin && (
              <label className="check">
                <input type="checkbox" checked={includeDrop} onChange={(e) => setIncludeDrop(e.target.checked)} />
                <span>Drop statements</span>
              </label>
            )}
            {!builtin && !pg && (
              <>
                {(
                  [
                    ['structure', 'Structure'],
                    ['data', 'Data'],
                    ['routines', 'Routines'],
                    ['events', 'Events'],
                    ['triggers', 'Triggers'],
                    ['single_transaction', 'Single transaction'],
                  ] as const
                ).map(([field, label]) => (
                  <label key={field} className="check">
                    <input type="checkbox" checked={mysql[field]} onChange={(e) => setMysql({ ...mysql, [field]: e.target.checked })} />
                    <span>{label}</span>
                  </label>
                ))}
              </>
            )}
            {!builtin && pg && (
              <>
                {(
                  [
                    ['structure', 'Structure'],
                    ['data', 'Data'],
                    ['no_owner', 'No owner'],
                    ['no_privileges', 'No privileges'],
                  ] as const
                ).map(([field, label]) => (
                  <label key={field} className="check">
                    <input type="checkbox" checked={pgOpts[field]} onChange={(e) => setPgOpts({ ...pgOpts, [field]: e.target.checked })} />
                    <span>{label}</span>
                  </label>
                ))}
                <label className="field backup-format">
                  <span className="field-label">Format</span>
                  <select
                    value={pgOpts.format}
                    onChange={(e) => {
                      const format = e.target.value as PgDumpFormat;
                      setPgOpts({ ...pgOpts, format });
                      if (format !== 'plain') changeCompress(false);
                    }}
                  >
                    <option value="plain">Plain SQL</option>
                    <option value="custom">Custom archive</option>
                    <option value="tar">Tar archive</option>
                  </select>
                </label>
              </>
            )}
            <label className="check">
              <input type="checkbox" checked={compress && !archive} disabled={archive} onChange={(e) => changeCompress(e.target.checked)} />
              <span>Compress (.sql.gz)</span>
            </label>
          </div>
          {toolOutputEmpty && <p className="field-error">Choose structure, data or both.</p>}
        </section>

        <section className="stack" aria-label="Output file">
          <h3 className="backup-heading">Output file</h3>
          <div className="backup-row">
            {path ? (
              <span className="backup-path mono" title={path}>
                {path}
              </span>
            ) : (
              <span className="hint">No file chosen.</span>
            )}
            <span className="spacer" />
            <button type="button" className="btn" disabled={picking || starting} onClick={() => void browse()}>
              Browse…
            </button>
          </div>
        </section>

        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" disabled={starting} onClick={close}>
          Cancel
        </button>
        <span className="spacer" />
        <button type="button" className="btn primary" disabled={!canStart} onClick={() => void start()}>
          Start dump
        </button>
      </footer>
    </Modal>
  );
}
