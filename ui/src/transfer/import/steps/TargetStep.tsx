import { useEffect, useState } from 'react';
import { api } from '../../../api';
import type { TargetColumn } from '../../../bindings';
import { useConnections } from '../../../store/connections';
import type { StepProps } from './stepProps';

/** The column's flags as the target list shows them. */
export function columnFlags(c: TargetColumn): string[] {
  const flags: string[] = [];
  if (c.auto_increment) flags.push('auto increment');
  if (c.identity) flags.push(c.identity === 'always' ? 'identity (always)' : 'identity (by default)');
  if (c.generated) flags.push('generated');
  if (c.unsigned) flags.push('unsigned');
  return flags;
}

/** Keeps `current` selectable while (or if) its list cannot be read. */
const withCurrent = (names: string[], current: string | null) => (current && !names.includes(current) ? [current, ...names] : names);

interface Props extends StepProps {
  targetError: string | null;
}

/** Step 2: connection, database/schema and table (pre-filled from the tree), and the table's columns. */
export function TargetStep({ state, dispatch, targetError }: Props) {
  const summaries = useConnections((s) => s.summaries);
  const info = useConnections((s) => s.info);
  const open = [...info.keys()].filter((id) => summaries.has(id) || id === state.connectionId);
  const kind = summaries.get(state.connectionId)?.config.kind ?? info.get(state.connectionId)?.kind ?? 'mysql';
  const pg = kind === 'postgres';

  const [database, setDatabase] = useState<string | null>(state.table?.database ?? null);
  const [schema, setSchema] = useState<string | null>(state.table?.schema ?? null);
  const [databases, setDatabases] = useState<string[]>([]);
  const [schemas, setSchemas] = useState<string[]>([]);
  const [tables, setTables] = useState<string[]>([]);
  const id = state.connectionId;

  useEffect(() => {
    let live = true;
    api.listDatabases(id, false).then(
      (list) => live && setDatabases(list.map((d) => d.name)),
      () => live && setDatabases([]),
    );
    return () => {
      live = false;
    };
  }, [id]);

  useEffect(() => {
    let live = true;
    if (!pg || !database) return;
    api.listSchemas(id, database, false).then(
      (list) => live && setSchemas(list.map((s) => s.name)),
      () => live && setSchemas([]),
    );
    return () => {
      live = false;
    };
  }, [id, pg, database]);

  useEffect(() => {
    let live = true;
    if (!database || (pg && !schema)) return;
    api.listObjects(id, { database, schema: pg ? schema : null }, 'Table').then(
      (list) => live && setTables(list.map((o) => o.name)),
      () => live && setTables([]),
    );
    return () => {
      live = false;
    };
  }, [id, pg, database, schema]);

  const pickTable = (name: string) =>
    dispatch({ type: 'table', table: name && database ? { database, schema: pg ? schema : null, name, kind: 'Table' } : null });

  const t = state.target;
  const name = (cid: string) => summaries.get(cid)?.config.name ?? cid;

  return (
    <div className="stack">
      <div className="import-wizard-options">
        <div className="field">
          <label htmlFor="iw-conn">Connection</label>
          <select
            id="iw-conn"
            value={id}
            onChange={(e) => {
              setDatabase(null);
              setSchema(null);
              setTables([]);
              dispatch({ type: 'connection', connectionId: e.target.value });
            }}
          >
            {withCurrent(open, id).map((cid) => (
              <option key={cid} value={cid}>
                {name(cid)}
              </option>
            ))}
          </select>
        </div>
        <div className="field">
          <label htmlFor="iw-db">Database</label>
          <select
            id="iw-db"
            value={database ?? ''}
            onChange={(e) => {
              setDatabase(e.target.value || null);
              setSchema(null);
              setTables([]);
              dispatch({ type: 'table', table: null });
            }}
          >
            <option value="">Choose…</option>
            {withCurrent(databases, database).map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        </div>
        {pg && (
          <div className="field">
            <label htmlFor="iw-schema">Schema</label>
            <select
              id="iw-schema"
              value={schema ?? ''}
              onChange={(e) => {
                setSchema(e.target.value || null);
                setTables([]);
                dispatch({ type: 'table', table: null });
              }}
            >
              <option value="">Choose…</option>
              {withCurrent(schemas, schema).map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </select>
          </div>
        )}
        <div className="field">
          <label htmlFor="iw-table">Table</label>
          <select id="iw-table" value={state.table?.name ?? ''} onChange={(e) => pickTable(e.target.value)}>
            <option value="">Choose…</option>
            {withCurrent(tables, state.table?.name ?? null).map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </div>
      </div>

      {targetError && (
        <p role="alert" className="field-error">
          {targetError}
        </p>
      )}
      {state.table && !t && !targetError && (
        <p role="status" className="hint">
          Reading the table…
        </p>
      )}
      {t && (
        <>
          {t.read_only && <p className="import-wizard-note">This connection is read-only: you can validate the file, but not import it.</p>}
          <div className="import-wizard-grid">
            <table className="import-table" data-testid="import-target-columns">
              <thead>
                <tr>
                  <th>Column</th>
                  <th>Type</th>
                  <th>Null</th>
                  <th>Default</th>
                  <th>Flags</th>
                </tr>
              </thead>
              <tbody>
                {t.columns.map((c) => (
                  <tr key={c.name}>
                    <td>{c.name}</td>
                    <td className="mono">{c.db_type}</td>
                    <td>{c.nullable ? 'Yes' : 'No'}</td>
                    <td>{c.has_default ? 'Yes' : 'No'}</td>
                    <td>{columnFlags(c).join(', ') || '—'}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </div>
  );
}
