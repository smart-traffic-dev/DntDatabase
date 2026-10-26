import type { DesignOpen, DraftError, TableDef, TableOptionsDef } from '../../bindings';
import { DraftInput } from './DraftInput';
import './elementsGrid.css';

type Edit = (recipe: (d: TableDef) => TableDef, coalesce?: string) => void;

export interface OptionsPanelProps {
  open: DesignOpen;
  draft: TableDef;
  errors: DraftError[];
  editable: boolean;
  edit: Edit;
}

const ENGINES = ['InnoDB', 'MyISAM', 'MEMORY', 'ARCHIVE'];
const ROW_FORMATS = ['DYNAMIC', 'COMPACT', 'REDUNDANT', 'COMPRESSED', 'FIXED'];

/** A select over `values`, plus the current value when it is not one of them. */
function Choice({ label, value, values, disabled, error, onChange }: {
  label: string;
  value: string | null;
  values: string[];
  disabled: boolean;
  error?: string;
  onChange(v: string | null): void;
}) {
  return (
    <label className="dz-field">
      <span>{label}</span>
      <select
        aria-label={label}
        className={`dz-input${error ? ' dz-invalid' : ''}`}
        value={value ?? ''}
        disabled={disabled}
        title={error}
        onChange={(e) => onChange(e.target.value === '' ? null : e.target.value)}
      >
        <option value="">Server default</option>
        {value !== null && !values.includes(value) ? <option value={value}>{value}</option> : null}
        {values.map((v) => (
          <option key={v} value={v}>
            {v}
          </option>
        ))}
      </select>
    </label>
  );
}

/** The Options section: the table's name; engine, charset, collation and row format on MySQL / MariaDB. */
export function OptionsPanel({ open, draft, errors, editable, edit }: OptionsPanelProps) {
  const mysql = open.server.kind === 'mysql';
  const opts = draft.options;
  const optionErrors = errors.filter((e) => e.element.kind === 'options');
  const errorOf = (field: string) => optionErrors.find((e) => e.field === field)?.message;
  const nameError = errors.find((e) => e.element.kind === 'table' && e.field === 'name')?.message;
  const other = optionErrors.filter((e) => e.field === null || !['engine', 'charset', 'collation', 'row_format'].includes(e.field));
  const frozen = !editable || opts.locked;
  const setOption = (patch: Partial<TableOptionsDef>, key?: string) => edit((d) => ({ ...d, options: { ...d.options, ...patch } }), key);
  const engines = open.server.variant === 'MariaDb' ? [...ENGINES, 'Aria'] : ENGINES;

  return (
    <div className="dz-options">
      {/* The table's name is edited in the designer's header. */}
      {nameError ? <p className="dz-alert">{nameError}</p> : null}
      {mysql ? null : <p className="dz-subtle">PostgreSQL tables have no storage options here. Rename the table in the header above.</p>}
      {mysql ? (
        <>
          <Choice label="Engine" value={opts.engine} values={engines} disabled={frozen} error={errorOf('engine')} onChange={(v) => setOption({ engine: v })} />
          <label className="dz-field">
            <span>Charset</span>
            <DraftInput
              aria-label="Charset"
              className="mono"
              placeholder="Server default"
              value={opts.charset ?? ''}
              disabled={frozen}
              error={errorOf('charset')}
              onCommit={(text, key) => setOption({ charset: text === '' ? null : text }, key)}
            />
          </label>
          <label className="dz-field">
            <span>Collation</span>
            <DraftInput
              aria-label="Collation"
              className="mono"
              placeholder="Server default"
              value={opts.collation ?? ''}
              disabled={frozen}
              error={errorOf('collation')}
              onCommit={(text, key) => setOption({ collation: text === '' ? null : text }, key)}
            />
          </label>
          <Choice label="Row format" value={opts.row_format} values={ROW_FORMATS} disabled={frozen} error={errorOf('row_format')} onChange={(v) => setOption({ row_format: v })} />
        </>
      ) : null}
      {other.length > 0 ? (
        <ul className="dz-problems dz-options-problems">
          {other.map((e, i) => (
            <li key={i}>{e.message}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
