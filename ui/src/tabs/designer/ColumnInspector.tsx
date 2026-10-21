import { useId, useState } from 'react';
import type { ColumnDef, DefaultSpec, DesignOpen, DraftError, ElementRef, IdentityKind, TableDef, TypeSpec } from '../../bindings';
import { deepEqual } from '../../store/tableDesigns';
import { DraftInput } from './DraftInput';
import {
  baseText,
  columnErrors,
  columnKey,
  isInteger,
  lockOf,
  onUpdateText,
  parseParam,
  renameColumn,
  sameType,
  takesOnUpdate,
  typeInfoOf,
  typeText,
  updateColumn,
  withBase,
  withType,
} from './columnModel';
import './columnInspector.css';

type Edit = (recipe: (d: TableDef) => TableDef, coalesce?: string) => void;

export interface ColumnInspectorProps {
  open: DesignOpen;
  draft: TableDef;
  errors: DraftError[];
  editable: boolean;
  selection: ElementRef | null;
  onSelect(el: ElementRef | null): void;
  edit: Edit;
}

const numText = (n: number | null) => (n === null ? '' : String(n));
const normalizeNum = (t: string) => numText(parseParam(t));

/** The fields of the selected column, as its type allows; the original type when it changed. */
export function ColumnInspector({ open, draft, errors, editable, selection, onSelect, edit }: ColumnInspectorProps) {
  const pg = open.server.kind === 'postgres';
  const index = selection?.kind === 'column' ? draft.columns.findIndex((c) => c.name === selection.name) : -1;
  const column = index >= 0 ? draft.columns[index] : undefined;
  const inBase = !!column && column.original !== null && open.def.columns.some((c) => c.name === column.original);
  return (
    <aside className="dz-inspector" aria-label="Column inspector">
      {column ? (
        <ColumnFields
          key={columnKey(column, inBase)}
          open={open}
          column={column}
          index={index}
          errors={errors}
          editable={editable}
          onSelect={onSelect}
          edit={edit}
        />
      ) : (
        <p className="dz-subtle">Select a column to see its details.</p>
      )}
      <span className="dz-spacer" />
      <div className="dz-hint">
        {pg ? '' : 'Ctrl+↑/↓ reorders columns · '}Delete marks a column for drop · Ctrl+Z undoes in the designer
      </div>
    </aside>
  );
}

interface FieldsProps {
  open: DesignOpen;
  column: ColumnDef;
  index: number;
  errors: DraftError[];
  editable: boolean;
  onSelect(el: ElementRef | null): void;
  edit: Edit;
}

const IDENTITY_LABEL: Record<IdentityKind, string> = { always: 'Always', by_default: 'By default' };

function ColumnFields({ open, column, index, errors, editable, onSelect, edit }: FieldsProps) {
  const uid = useId();
  // Only used while there is no default: an existing default carries its own kind.
  const [pendingMode, setPendingMode] = useState<DefaultSpec['kind']>('value');
  const mode = column.default?.kind ?? pendingMode;
  const engine = open.server.kind;
  const pg = engine === 'postgres';
  const base = column.original === null ? undefined : open.def.columns.find((c) => c.name === column.original);
  const typeChanged = !!base && !sameType(base.data_type, column.data_type);
  const info = typeInfoOf(open.catalog, column.data_type);
  const spec = column.data_type.kind === 'known' ? column.data_type : null;
  const disabled = !editable || column.locked;
  const lock = lockOf(open.locked, column);
  const set = (patch: Partial<ColumnDef>, key?: string) => edit((d) => updateColumn(d, index, patch), key);
  const retype = (next: TypeSpec, key?: string) => set(withType(column, next, open.catalog, engine, base), key);
  const setType = (patch: Partial<NonNullable<typeof spec>>, key?: string) => spec && retype({ ...spec, ...patch }, key);
  const setDefault = (kind: DefaultSpec['kind'], text: string, key?: string) => set({ default: text === '' ? null : { kind, text } }, key);
  const state = column.original === null ? 'New' : base && !deepEqual(base, column) ? 'Modified' : null;

  const shows = {
    length: !!spec && !!info && info.length !== 'None',
    precision: !!spec && !!info && info.precision !== 'None',
    scale: !!spec && !!info && info.scale !== 'None',
    values: !!spec && !!info?.values,
    unsigned: !!spec && !!info?.unsigned,
    collation: !!info?.collation,
    using: pg && typeChanged,
    auto_increment: !pg && isInteger(column.data_type, engine),
    identity: pg && isInteger(column.data_type, engine),
    on_update: takesOnUpdate(column.data_type, engine),
  };
  const visible = new Set<string>(['name', 'data_type', 'default', 'comment', ...Object.entries(shows).filter(([, v]) => v).map(([k]) => k)]);
  const errs = columnErrors(errors, column.name);
  const errorOf = (field: string) => errs.find((e) => e.field === field)?.message;
  // Errors of fields this column does not show (or of no field) are listed, never hidden.
  const others = errs.filter((e) => e.field === null || !visible.has(e.field));

  const param = (field: 'length' | 'precision' | 'scale', label: string) =>
    shows[field] && spec ? (
      <label className="dz-field dz-field-num">
        <span>{label}</span>
        <DraftInput
          type="number"
          min={0}
          aria-label={label}
          className="mono"
          value={numText(spec[field])}
          disabled={disabled}
          error={errorOf(field)}
          normalize={normalizeNum}
          onCommit={(text, key) => setType({ [field]: parseParam(text) }, key)}
        />
      </label>
    ) : null;

  return (
    <div className="dz-fields">
      <div className="dz-inspector-title">
        <strong>{column.name || ' '}</strong>
        {state ? <span className={`dz-badge${state === 'New' ? ' dz-badge-new' : ''}`}>{state}</span> : null}
      </div>
      {lock ? (
        <div className="dz-lock-note" role="note">
          <strong>{lock.reason}</strong>
          <pre>{lock.server_text}</pre>
        </div>
      ) : null}
      {others.length > 0 ? (
        <ul className="dz-alert dz-other-errors" aria-label="Other problems">
          {others.map((e, i) => (
            <li key={i}>{e.message}</li>
          ))}
        </ul>
      ) : null}
      <label className="dz-field">
        <span>Name</span>
        <DraftInput
          aria-label="Name"
          className="mono"
          value={column.name}
          disabled={disabled}
          error={errorOf('name')}
          onCommit={(text, key) => {
            edit((d) => renameColumn(d, index, text), key);
            onSelect({ kind: 'column', name: text });
          }}
        />
      </label>
      <div className="dz-type-row">
        <label className="dz-field">
          <span>Type</span>
          <DraftInput
            aria-label="Type"
            className="mono"
            list={`${uid}-types`}
            value={baseText(column.data_type)}
            disabled={disabled}
            error={errorOf('data_type')}
            onCommit={(text, key) => retype(withBase(column.data_type, text, open.catalog), key)}
          />
          <datalist id={`${uid}-types`}>
            {open.catalog.map((t) => (
              <option key={t.base} value={t.base} />
            ))}
          </datalist>
        </label>
        {param('length', 'Length')}
        {param('precision', 'Precision')}
        {param('scale', 'Scale')}
      </div>
      {typeChanged && base ? <span className="dz-subtle dz-before">Before: {typeText(base.data_type)}</span> : null}
      {shows.values && spec ? (
        <label className="dz-field">
          <span>Values (one per line)</span>
          <textarea
            aria-label="Values"
            className={`dz-input mono${errorOf('values') ? ' dz-invalid' : ''}`}
            rows={3}
            value={spec.values.join('\n')}
            disabled={disabled}
            title={errorOf('values')}
            aria-invalid={errorOf('values') ? true : undefined}
            onChange={(e) => setType({ values: e.target.value.split('\n') }, `${uid}-values`)}
          />
        </label>
      ) : null}
      {shows.unsigned && spec ? (
        <label className="dz-check" title={errorOf('unsigned')}>
          <input type="checkbox" checked={spec.unsigned} disabled={disabled} onChange={(e) => setType({ unsigned: e.target.checked })} />
          Unsigned
        </label>
      ) : null}
      <label className="dz-check">
        <input type="checkbox" checked={column.nullable} disabled={disabled} onChange={(e) => set({ nullable: e.target.checked })} />
        Nullable
      </label>
      {shows.auto_increment ? (
        <label className="dz-check" title={errorOf('auto_increment')}>
          <input type="checkbox" checked={column.auto_increment} disabled={disabled} onChange={(e) => set({ auto_increment: e.target.checked })} />
          Auto increment
        </label>
      ) : null}
      {shows.identity ? (
        <label className="dz-field">
          <span>Identity</span>
          <select
            aria-label="Identity"
            className={`dz-input${errorOf('identity') ? ' dz-invalid' : ''}`}
            value={column.identity ?? ''}
            disabled={disabled}
            title={errorOf('identity')}
            onChange={(e) => set({ identity: e.target.value === '' ? null : (e.target.value as IdentityKind) })}
          >
            <option value="">None</option>
            {(['always', 'by_default'] as const).map((k) => (
              <option key={k} value={k}>
                {IDENTITY_LABEL[k]}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      {shows.on_update ? (
        <label className="dz-check" title={errorOf('on_update')}>
          <input
            type="checkbox"
            checked={column.on_update !== null}
            disabled={disabled}
            onChange={(e) => set({ on_update: e.target.checked ? onUpdateText(column.data_type) : null })}
          />
          On update {onUpdateText(column.data_type)}
        </label>
      ) : null}
      <div className="dz-field">
        <span id={`${uid}-default`}>Default</span>
        <div className="segmented dz-mode" role="group" aria-labelledby={`${uid}-default`}>
          {(['value', 'expression'] as const).map((k) => (
            <button
              key={k}
              type="button"
              className={`segment${mode === k ? ' on' : ''}`}
              aria-pressed={mode === k}
              disabled={disabled}
              onClick={() => {
                setPendingMode(k);
                if (column.default && column.default.kind !== k) setDefault(k, column.default.text);
              }}
            >
              {k === 'value' ? 'Value' : 'Expression'}
            </button>
          ))}
        </div>
        <DraftInput
          aria-label="Default"
          className="mono"
          value={column.default?.text ?? ''}
          placeholder={column.nullable ? 'NULL' : undefined}
          disabled={disabled}
          error={errorOf('default')}
          onCommit={(text, key) => setDefault(mode, text, key)}
        />
      </div>
      {shows.collation ? (
        <label className="dz-field">
          <span>Collation</span>
          <DraftInput
            aria-label="Collation"
            className="mono"
            value={column.collation ?? ''}
            placeholder="Table default"
            disabled={disabled}
            error={errorOf('collation')}
            onCommit={(text, key) => set({ collation: text === '' ? null : text }, key)}
          />
        </label>
      ) : null}
      {shows.using ? (
        <label className="dz-field">
          <span>USING</span>
          <DraftInput
            aria-label="USING"
            className="mono"
            value={column.using ?? ''}
            placeholder={`${column.name}::${typeText(column.data_type)}`}
            disabled={disabled}
            error={errorOf('using')}
            onCommit={(text, key) => set({ using: text === '' ? null : text }, key)}
          />
        </label>
      ) : null}
      <label className="dz-field">
        <span>Comment</span>
        <DraftInput
          aria-label="Comment"
          value={column.comment ?? ''}
          placeholder="Add a comment…"
          disabled={disabled}
          error={errorOf('comment')}
          onCommit={(text, key) => set({ comment: text === '' ? null : text }, key)}
        />
      </label>
    </div>
  );
}
