import { useEffect, useState } from 'react';
import { api } from '../../../api';
import type { TargetColumn } from '../../../bindings';
import { ChevronIcon } from '../../../components/icons';
import { EngineLogo } from '../../../components/EngineLogo';
import { SelectMenu } from '../../../components/SelectMenu';
import { plural } from '../../../lib/plural';
import { useConnections } from '../../../store/connections';
import type { StepProps } from './stepProps';
import { isKind, sqlEngine } from '../../../engine';

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

/** "Choose…" then the names, as SelectMenu options. */
const pickList = (names: string[]) => [{ value: '', label: 'Choose…' }, ...names.map((n) => ({ value: n, label: n }))];

interface Props extends StepProps {
  targetError: string | null;
}

/** Step 2: connection, database/schema and table (pre-filled from the tree), and the table's columns. */
export function TargetStep({ state, dispatch, targetError }: Props) {
  const summaries = useConnections((s) => s.summaries);
  const info = useConnections((s) => s.info);
  const open = [...info.keys()].filter((id) => summaries.has(id) || id === state.connectionId);
  const kind = summaries.get(state.connectionId)?.config.kind ?? info.get(state.connectionId)?.kind ?? 'mysql';
  const engine = sqlEngine(kind);
  const pg = engine !== null && isKind(engine, 'postgres');

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
  // NOT NULL without a default and filled by nothing else: the file must provide them.
  const required = t ? t.columns.filter((c) => !c.nullable && !c.has_default && !c.auto_increment && !c.identity && !c.generated).length : 0;

  return (
    <section className="stack" aria-label="Target table">
      <h3 className="import-wizard-heading">Target table</h3>
      <div className="import-wizard-options">
        <div className="field">
          <label htmlFor="iw-conn">Connection</label>
          <SelectMenu
            id="iw-conn"
            size="md"
            label="Connection"
            value={id}
            options={withCurrent(open, id).map((cid) => {
              const k = summaries.get(cid)?.config.kind;
              return { value: cid, label: name(cid), icon: k ? <EngineLogo engine={k} size={16} compact /> : undefined };
            })}
            onChange={(v) => {
              setDatabase(null);
              setSchema(null);
              setTables([]);
              dispatch({ type: 'connection', connectionId: v });
            }}
          />
        </div>
        <div className="field">
          <label htmlFor="iw-db">Database</label>
          <SelectMenu
            id="iw-db"
            size="md"
            label="Database"
            value={database ?? ''}
            options={pickList(withCurrent(databases, database))}
            onChange={(v) => {
              setDatabase(v || null);
              setSchema(null);
              setTables([]);
              dispatch({ type: 'table', table: null });
            }}
          />
        </div>
        {pg && (
          <div className="field">
            <label htmlFor="iw-schema">Schema</label>
            <SelectMenu
              id="iw-schema"
              size="md"
              label="Schema"
              value={schema ?? ''}
              options={pickList(withCurrent(schemas, schema))}
              onChange={(v) => {
                setSchema(v || null);
                setTables([]);
                dispatch({ type: 'table', table: null });
              }}
            />
          </div>
        )}
        <div className="field">
          <label htmlFor="iw-table">Table</label>
          <SelectMenu
            id="iw-table"
            size="md"
            mono
            label="Table"
            value={state.table?.name ?? ''}
            options={pickList(withCurrent(tables, state.table?.name ?? null))}
            onChange={pickTable}
          />
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
          {/* A summary line; the column list folds open on demand (the Mapping step lists them again). */}
          <details className="import-target-cols">
            <summary>
              <span className="import-chevron" aria-hidden="true">
                <ChevronIcon />
              </span>
              {plural(t.columns.length, 'column')}
              {required > 0 ? ` · ${required} required` : ''}
            </summary>
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
          </details>
        </>
      )}
    </section>
  );
}
