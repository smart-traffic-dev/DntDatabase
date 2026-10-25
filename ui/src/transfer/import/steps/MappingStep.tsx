import type { MappingIssue } from '../../../bindings';
import type { StepProps } from './stepProps';

interface Props extends StepProps {
  /** "Auto map" again (`import_auto_map`). */
  onAutoMap(): void;
  checkError: string | null;
}

export function IssueList({ issues, tone }: { issues: MappingIssue[]; tone: 'error' | 'warn' }) {
  if (issues.length === 0) return null;
  return (
    <ul className={`import-issues ${tone}`}>
      {issues.map((i, n) => (
        <li key={n} role={tone === 'error' ? 'alert' : undefined}>
          {i.message}
        </li>
      ))}
    </ul>
  );
}

/** Step 3: one row per target column with its source column; errors and warnings come from the backend. */
export function MappingStep({ state, dispatch, onAutoMap, checkError }: Props) {
  const target = state.target;
  const columns = state.inspection?.columns ?? [];
  if (!target) return null;
  const sourceOf = (name: string) => state.mapping.find((m) => m.target === name)?.source ?? null;
  const report = state.report;
  const about = (column: string | null, list: MappingIssue[] | undefined) => (list ?? []).filter((i) => i.column === column);
  const general = (list: MappingIssue[] | undefined) => (list ?? []).filter((i) => i.column === null || !target.columns.some((c) => c.name === i.column));

  return (
    <div className="stack">
      <div className="row import-wizard-actions">
        <button type="button" className="btn" onClick={onAutoMap}>
          Auto map
        </button>
        <button type="button" className="btn" onClick={() => dispatch({ type: 'mapping', mapping: target.columns.map((c) => ({ target: c.name, source: null })) })}>
          Clear
        </button>
        <span className="hint">Columns left unmapped get their default value.</span>
      </div>
      <IssueList issues={general(report?.errors)} tone="error" />
      <IssueList issues={general(report?.warnings)} tone="warn" />
      {checkError && (
        <p role="alert" className="field-error">
          {checkError}
        </p>
      )}
      <div className="import-wizard-grid">
        <table className="import-table import-mapping">
          <thead>
            <tr>
              <th>Target column</th>
              <th>Type</th>
              <th>Source column</th>
            </tr>
          </thead>
          <tbody>
            {target.columns.map((c) => {
              const required = !c.nullable && !c.has_default && !c.generated;
              const src = sourceOf(c.name);
              const errors = about(c.name, report?.errors);
              const warnings = about(c.name, report?.warnings);
              return (
                <tr key={c.name} className={errors.length > 0 ? 'has-error' : undefined}>
                  <td>
                    {c.name}
                    {required && (
                      <span className="import-required" title="Required: NOT NULL without a default">
                        {' '}
                        *
                      </span>
                    )}
                  </td>
                  <td className="mono">{c.db_type}</td>
                  <td>
                    <select
                      aria-label={`Source for ${c.name}`}
                      aria-invalid={errors.length > 0}
                      value={src === null ? '' : String(src)}
                      onChange={(e) => dispatch({ type: 'mapOne', target: c.name, source: e.target.value === '' ? null : Number(e.target.value) })}
                    >
                      <option value="">Not mapped (default)</option>
                      {columns.map((name, i) => (
                        <option key={`${name}:${i}`} value={String(i)}>
                          {name}
                        </option>
                      ))}
                    </select>
                    <IssueList issues={errors} tone="error" />
                    <IssueList issues={warnings} tone="warn" />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {!report && !checkError && (
        <p role="status" className="hint">
          Checking the mapping…
        </p>
      )}
    </div>
  );
}
