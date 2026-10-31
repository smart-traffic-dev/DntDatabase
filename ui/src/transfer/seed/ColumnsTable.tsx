import { Fragment } from 'react';
import type { ColumnIssue, ColumnPlan, Generator, SeedColumn } from '../../bindings';
import { GeneratorOptions } from './GeneratorOptions';
import { GENERATOR_LABEL, generatorsFor, uniqueApplies, type GeneratorKind } from './seedFormat';
import { defaultGenerator } from './seedState';

interface Props {
  columns: SeedColumn[];
  plans: ColumnPlan[];
  issues: ColumnIssue[];
  keyCounts: Map<string, number>;
  disabled?: boolean;
  onGenerator(column: string, g: Generator): void;
  onPlan(column: string, patch: Partial<ColumnPlan>): void;
}

const LOCKED_TITLE = 'Generated column: the server computes it.';

/** "int · PK · auto" — the column's type and what makes it special. */
function typeText(c: SeedColumn): string {
  const parts = [c.column.db_type];
  if (c.primary) parts.push('PK');
  if (c.foreign_key) parts.push('FK');
  if (c.column.auto_increment || c.column.identity) parts.push('auto');
  if (c.locked) parts.push('generated');
  return parts.join(' · ');
}

/** One row per column: its generator, the generator's options, NULL % and Unique (spec §6.2). */
export function ColumnsTable({ columns, plans, issues, keyCounts, disabled, onGenerator, onPlan }: Props) {
  return (
    <div className="seed-columns" data-testid="seed-columns">
      <table className="seed-table">
        <thead>
          <tr>
            <th scope="col">Column</th>
            <th scope="col">Type</th>
            <th scope="col">Generator</th>
            <th scope="col">Options</th>
            <th scope="col">NULL %</th>
            <th scope="col">Unique</th>
          </tr>
        </thead>
        <tbody>
          {columns.map((c, i) => {
            const name = c.column.name;
            const plan = plans[i];
            if (!plan) return null;
            const g = plan.generator;
            const kinds = generatorsFor(c);
            const issue = issues.filter((x) => x.column === name);
            const fk = c.foreign_key;
            return (
              <Fragment key={name}>
                <tr data-testid={`seed-col-${name}`} className={issue.length ? 'has-issue' : undefined}>
                  <th scope="row" className="mono">
                    {name}
                    {!c.column.nullable && !c.column.has_default && <span className="seed-required" title="NOT NULL without a default"> *</span>}
                  </th>
                  <td className="seed-type">{typeText(c)}</td>
                  <td>
                    <select
                      aria-label={`Generator for ${name}`}
                      value={g.kind}
                      disabled={disabled || c.locked}
                      title={c.locked ? LOCKED_TITLE : undefined}
                      aria-invalid={issue.length > 0 || undefined}
                      onChange={(e) => onGenerator(name, defaultGenerator(e.target.value as GeneratorKind, c))}
                    >
                      {kinds.map((k) => (
                        <option key={k} value={k}>
                          {GENERATOR_LABEL[k]}
                        </option>
                      ))}
                    </select>
                  </td>
                  <td>
                    <GeneratorOptions
                      column={name}
                      generator={g}
                      disabled={disabled || c.locked}
                      reference={fk ? `${fk.ref_table}.${fk.ref_columns.join(', ')}` : undefined}
                      keyCount={fk ? keyCounts.get(fk.constraint) : undefined}
                      onChange={(next) => onGenerator(name, next)}
                    />
                  </td>
                  <td>
                    {c.column.nullable && g.kind !== 'skip' ? (
                      <input
                        type="number"
                        className="seed-percent"
                        aria-label={`NULL % for ${name}`}
                        min={0}
                        max={100}
                        value={plan.null_percent}
                        disabled={disabled}
                        onChange={(e) => {
                          const n = e.target.valueAsNumber;
                          if (Number.isFinite(n)) onPlan(name, { null_percent: Math.min(100, Math.max(0, Math.trunc(n))) });
                        }}
                      />
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td>
                    {uniqueApplies(g.kind) ? (
                      <input
                        type="checkbox"
                        aria-label={`Unique ${name}`}
                        checked={plan.unique}
                        disabled={disabled}
                        onChange={(e) => onPlan(name, { unique: e.target.checked })}
                      />
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                </tr>
                {issue.length > 0 && (
                  <tr className="seed-issue">
                    <td colSpan={6} role="alert">
                      {issue.map((x) => x.message).join(' ')}
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
