import { useId, useState } from 'react';
import type { ColumnDef, DefaultSpec, DesignOpen, DraftError, ElementRef, TableDef } from '../../bindings';
import { deepEqual } from '../../store/tableDesigns';
import { DraftInput } from './DraftInput';
import './columnInspector.css';
import { baseText, columnErrors, lockOf, parseParam, renameColumn, sameType, typeInfoOf, typeText, updateColumn, withBase } from './columnModel';

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
  return (
    <aside className="dz-inspector" aria-label="Column inspector">
      {column ? (
        <ColumnFields key={index} open={open} column={column} index={index} errors={errors} editable={editable} onSelect={onSelect} edit={edit} />
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

function ColumnFields({ open, column, index, errors, editable, onSelect, edit }: FieldsProps) {
  const uid = useId();
  const [mode, setMode] = useState<DefaultSpec['kind']>(column.default?.kind ?? 'value');
  const pg = open.server.kind === 'postgres';
  const base = column.original === null ? undefined : open.def.columns.find((c) => c.name === column.original);
  const typeChanged = !!base && !sameType(base.data_type, column.data_type);
  const info = typeInfoOf(open.catalog, column.data_type);
  const spec = column.data_type.kind === 'known' ? column.data_type : null;
  const disabled = !editable || column.locked;
  const lock = lockOf(open.locked, column);
  const errs = columnErrors(errors, column.name);
  const errorOf = (...fields: string[]) => errs.find((e) => e.field !== null && fields.includes(e.field))?.message;
  const set = (patch: Partial<ColumnDef>, key?: string) => edit((d) => updateColumn(d, index, patch), key);
  const setType = (patch: Partial<NonNullable<typeof spec>>, key?: string) => spec && set({ data_type: { ...spec, ...patch } }, key);
  const setDefault = (kind: DefaultSpec['kind'], text: string, key?: string) => set({ default: text === '' ? null : { kind, text } }, key);
  const state = column.original === null ? 'New' : base && !deepEqual(base, column) ? 'Modified' : null;

  const param = (field: 'length' | 'precision' | 'scale', label: string) =>
    info && info[field] !== 'None' && spec ? (
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
      {errs.filter((e) => e.field === null).map((e, i) => (
        <div key={i} className="dz-alert" role="note">
          {e.message}
        </div>
      ))}
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
            onCommit={(text, key) => set({ data_type: withBase(column.data_type, text, open.catalog) }, key)}
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
      {info?.values && spec ? (
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
      {info?.unsigned && spec ? (
        <label className="dz-check">
          <input type="checkbox" checked={spec.unsigned} disabled={disabled} onChange={(e) => setType({ unsigned: e.target.checked })} />
          Unsigned
        </label>
      ) : null}
      <label className="dz-check">
        <input type="checkbox" checked={column.nullable} disabled={disabled} onChange={(e) => set({ nullable: e.target.checked })} />
        Nullable
      </label>
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
                setMode(k);
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
      {info?.collation ? (
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
      {pg && typeChanged ? (
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
