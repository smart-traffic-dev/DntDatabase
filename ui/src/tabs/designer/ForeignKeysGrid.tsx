import { useEffect, useId, useState } from 'react';
import type { FkDef, Scope } from '../../bindings';
import type { CompletionScope } from '../../query/completion/provider';
import { useCompletion } from '../../store/completion';
import { DraftInput } from './DraftInput';
import { ElementGrid, type ElementGridProps } from './ElementGrid';
import { parseList } from './elementModel';

const ACTIONS = ['NO ACTION', 'RESTRICT', 'CASCADE', 'SET NULL', 'SET DEFAULT'];

const content = (f: FkDef) => [f.columns, f.ref_schema, f.ref_table, f.ref_columns, f.on_update, f.on_delete];

export interface ForeignKeysGridProps extends ElementGridProps {
  connectionId: string;
  scope: Scope;
}

/** The schema cache's tables the Referenced table field completes with, for the schema `refSchema` names. */
function tablesOf(cache: CompletionScope | null, pg: boolean, scope: Scope, refSchema: string | null): string[] {
  if (!cache) return [];
  // MySQL: a referenced schema is a database, and only this one is cached here.
  if (!pg && refSchema !== null && refSchema !== scope.database) return [];
  const schema = pg ? (refSchema ?? scope.schema) : null;
  return cache.tables.filter((t) => t.kind === 'Table' && (!pg || t.schema === schema)).map((t) => t.name);
}

/** The Foreign keys grid: columns, the referenced table and columns, the actions, and PostgreSQL's deferral. */
export function ForeignKeysGrid(p: ForeignKeysGridProps) {
  const uid = useId();
  const pg = p.open.server.kind === 'postgres';
  const [cache, setCache] = useState<CompletionScope | null>(null);
  useEffect(() => {
    let live = true;
    void useCompletion.scope(p.connectionId, p.scope.database).then((s) => live && setCache(s));
    return () => {
      live = false;
    };
  }, [p.connectionId, p.scope.database]);

  const cols = ['minmax(0, 1.1fr)', 'minmax(0, 1.1fr)', 'minmax(0, 1.6fr)', 'minmax(0, 1.1fr)', '112px', '112px', ...(pg ? ['76px', '76px'] : [])].join(' ');
  const headers = ['Name', 'Columns', 'Referenced table', 'Referenced columns', 'ON UPDATE', 'ON DELETE', ...(pg ? ['Deferrable', 'Deferred'] : [])];

  // MySQL cannot change a foreign key in place: the same name with new content is refused.
  const base = new Map(p.open.def.foreign_keys.map((f) => [f.name, f]));
  const unchangeable = pg
    ? []
    : p.draft.foreign_keys.filter((f) => {
        const b = f.original === null ? undefined : base.get(f.original);
        return !!b && f.name === b.name && JSON.stringify(content(f)) !== JSON.stringify(content(b));
      });

  const action = (label: string, value: string, disabled: boolean, onChange: (v: string) => void) => (
    <div role="cell">
      <select aria-label={label} className="dz-input mono" value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)}>
        {ACTIONS.includes(value) ? null : <option value={value}>{value}</option>}
        {ACTIONS.map((a) => (
          <option key={a} value={a}>
            {a}
          </option>
        ))}
      </select>
    </div>
  );

  return (
    <ElementGrid
      {...p}
      listKey="foreign_keys"
      label="Foreign keys"
      noun="foreign key"
      cols={cols}
      headers={headers}
      fields={['name', 'columns', 'ref_schema', 'ref_table', 'ref_columns', 'on_update', 'on_delete', 'deferrable', 'initially_deferred']}
      cells={({ row, disabled, errorOf, set }) => {
        const fk = row.item as FkDef;
        const listId = `${uid}-${row.key}`;
        const tables = tablesOf(cache, pg, p.scope, fk.ref_schema);
        return (
          <>
            <div role="cell">
              <DraftInput aria-label="Name" className="mono" value={fk.name} disabled={disabled} error={errorOf('name')} onCommit={(text, key) => set({ name: text }, key)} />
            </div>
            <div role="cell">
              <DraftInput
                aria-label="Columns"
                className="mono"
                placeholder="col, col"
                value={fk.columns.join(', ')}
                disabled={disabled}
                error={errorOf('columns')}
                normalize={(t) => parseList(t).join(', ')}
                onCommit={(text, key) => set({ columns: parseList(text) }, key)}
              />
            </div>
            <div role="cell" className="dz-ref">
              <DraftInput
                aria-label="Referenced schema"
                className="mono dz-ref-schema"
                placeholder={pg ? (p.scope.schema ?? 'schema') : p.scope.database}
                value={fk.ref_schema ?? ''}
                disabled={disabled}
                error={errorOf('ref_schema')}
                onCommit={(text, key) => set({ ref_schema: text === '' ? null : text }, key)}
              />
              <DraftInput
                aria-label="Referenced table"
                className="mono"
                list={listId}
                placeholder="table"
                value={fk.ref_table}
                disabled={disabled}
                error={errorOf('ref_table')}
                onCommit={(text, key) => set({ ref_table: text }, key)}
              />
              <datalist id={listId}>
                {tables.map((t) => (
                  <option key={t} value={t} />
                ))}
              </datalist>
            </div>
            <div role="cell">
              <DraftInput
                aria-label="Referenced columns"
                className="mono"
                placeholder="col, col"
                value={fk.ref_columns.join(', ')}
                disabled={disabled}
                error={errorOf('ref_columns')}
                normalize={(t) => parseList(t).join(', ')}
                onCommit={(text, key) => set({ ref_columns: parseList(text) }, key)}
              />
            </div>
            {action('ON UPDATE', fk.on_update, disabled, (v) => set({ on_update: v }))}
            {action('ON DELETE', fk.on_delete, disabled, (v) => set({ on_delete: v }))}
            {pg ? (
              <>
                <div role="cell" className="dz-center">
                  <input
                    type="checkbox"
                    aria-label="Deferrable"
                    checked={fk.deferrable}
                    disabled={disabled}
                    onChange={(e) => set({ deferrable: e.target.checked, initially_deferred: e.target.checked && fk.initially_deferred })}
                  />
                </div>
                <div role="cell" className="dz-center">
                  <input
                    type="checkbox"
                    aria-label="Initially deferred"
                    checked={fk.initially_deferred}
                    disabled={disabled || !fk.deferrable}
                    onChange={(e) => set({ initially_deferred: e.target.checked })}
                  />
                </div>
              </>
            ) : null}
          </>
        );
      }}
    >
      {unchangeable.map((f) => (
        <p key={f.name} className="dz-hint dz-row-hint" role="note">
          <span className="mono">{f.name}</span>: Rename the foreign key to change it.
        </p>
      ))}
    </ElementGrid>
  );
}
