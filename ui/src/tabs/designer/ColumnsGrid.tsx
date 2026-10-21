import { useEffect, useId, useRef, type DragEvent, type KeyboardEvent } from 'react';
import type { ColumnDef, DesignOpen, DraftError, ElementRef, TableDef, TypeSpec } from '../../bindings';
import { DraftInput } from './DraftInput';
import {
  baseText,
  columnErrors,
  columnRows,
  dropColumn,
  lockOf,
  moveColumn,
  paramsText,
  renameColumn,
  restoreColumn,
  togglePrimaryKey,
  typeInfoOf,
  updateColumn,
  withBase,
  withParamsText,
  withType,
  type ColumnRow,
} from './columnModel';
import { RowStateCell } from './RowStateCell';
import './columnsGrid.css';

type Edit = (recipe: (d: TableDef) => TableDef, coalesce?: string) => void;

export interface ColumnsGridProps {
  open: DesignOpen;
  draft: TableDef;
  errors: DraftError[];
  /** False while read-only, lost or (re)opening: every field is disabled. */
  editable: boolean;
  selection: ElementRef | null;
  onSelect(el: ElementRef | null): void;
  edit: Edit;
  onAdd(): void;
}

const COLUMN_DRAG_TYPE = 'application/x-dnt-design-column';

/**
 * Which grid cell shows a draft error of `field` (the inspector has the field itself); anything
 * else (no field, `locked`, …) shows on the row's state mark.
 */
const CELL_OF: Record<string, 'name' | 'type' | 'length' | 'default' | 'comment'> = {
  name: 'name',
  data_type: 'type',
  values: 'type',
  unsigned: 'type',
  collation: 'type',
  charset: 'type',
  using: 'type',
  length: 'length',
  precision: 'length',
  scale: 'length',
  default: 'default',
  auto_increment: 'default',
  identity: 'default',
  on_update: 'default',
  generated: 'default',
  comment: 'comment',
};

/** What the Default cell shows (muted) for a column whose value the server generates. */
function generatedText(c: ColumnDef): string | null {
  if (c.auto_increment) return 'AUTO_INCREMENT';
  if (c.identity) return c.identity === 'always' ? 'IDENTITY ALWAYS' : 'IDENTITY BY DEFAULT';
  return null;
}

function KeyIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true">
      <circle cx="5.5" cy="10.5" r="3" />
      <path d="m7.8 8.2 6-6M11.5 4.5l2 2" />
    </svg>
  );
}

/** The Columns grid: one row per draft column, plus the opened table's columns the draft drops. */
export function ColumnsGrid({ open, draft, errors, editable, selection, onSelect, edit, onAdd }: ColumnsGridProps) {
  const typesId = useId();
  const pg = open.server.kind === 'postgres';
  const rows = columnRows(open.def, draft);
  const rowEls = useRef(new Map<string, HTMLDivElement>());
  const dragFrom = useRef<number | null>(null);
  const prevCount = useRef(draft.columns.length);
  const selected = selection?.kind === 'column' ? selection.name : null;

  // A column just added (and selected) gets its name field focused.
  const count = draft.columns.length;
  const last = draft.columns[count - 1];
  useEffect(() => {
    const grew = count > prevCount.current;
    prevCount.current = count;
    if (!grew || !last || last.original !== null || last.name !== selected) return;
    rowEls.current.get(rows[rows.length - 1]?.key ?? '')?.querySelector<HTMLInputElement>('input[aria-label="Name"]')?.focus();
  });

  const pk = draft.primary_key;

  const onRowKey = (e: KeyboardEvent<HTMLDivElement>, row: ColumnRow) => {
    if (e.target !== e.currentTarget || row.index === null || !editable || row.column.locked) return;
    const index = row.index;
    if (e.key === 'Delete') {
      e.preventDefault();
      edit((d) => dropColumn(d, index));
      onSelect(null);
    } else if ((e.ctrlKey || e.metaKey) && (e.key === 'ArrowUp' || e.key === 'ArrowDown') && !pg) {
      e.preventDefault();
      edit((d) => moveColumn(d, index, index + (e.key === 'ArrowUp' ? -1 : 1)));
    }
  };

  const onDrop = (e: DragEvent<HTMLDivElement>, row: ColumnRow) => {
    const from = dragFrom.current;
    dragFrom.current = null;
    if (from === null || row.index === null) return;
    e.preventDefault();
    const to = row.index;
    edit((d) => moveColumn(d, from, to));
  };

  return (
    <div className="dz-columns">
      <div className={`dz-grid${pg ? ' dz-grid-pg' : ''}`} role="table" aria-label="Columns" aria-rowcount={rows.length + 1}>
        <div role="row" className="dz-row dz-head">
          {pg ? null : <div role="columnheader" aria-label="Order" />}
          <div role="columnheader" aria-label="State" />
          <div role="columnheader">PK</div>
          <div role="columnheader">Name</div>
          <div role="columnheader">Type</div>
          <div role="columnheader">Length</div>
          <div role="columnheader">Null</div>
          <div role="columnheader">Default</div>
          <div role="columnheader">Comment</div>
        </div>
        {rows.map((row) => {
          const { column, index, state } = row;
          const dropped = state === 'drop';
          const lock = lockOf(open.locked, column);
          const disabled = !editable || column.locked || dropped;
          const errs = dropped ? [] : columnErrors(errors, column.name);
          const errorOf = (cell: string) => errs.find((e) => e.field !== null && CELL_OF[e.field] === cell)?.message;
          const rowErrors = errs.filter((e) => e.field === null || !CELL_OF[e.field]);
          const info = typeInfoOf(open.catalog, column.data_type);
          const takesParams = !!info && (info.length !== 'None' || info.precision !== 'None');
          const inPk = !dropped && !!pk?.columns.includes(column.name);
          const isSelected = !dropped && selected === column.name;
          const set = (patch: Partial<ColumnDef>, key?: string) => index !== null && edit((d) => updateColumn(d, index, patch), key);
          const retype = (spec: TypeSpec, key: string) => set(withType(column, spec, open.catalog, open.server.kind, row.base), key);
          const generated = generatedText(column);
          return (
            <div
              key={row.key}
              ref={(el) => {
                if (el) rowEls.current.set(row.key, el);
                else rowEls.current.delete(row.key);
              }}
              role="row"
              data-column={column.name}
              aria-current={isSelected ? true : undefined}
              tabIndex={isSelected ? 0 : -1}
              className={`dz-row dz-${state}${column.locked ? ' dz-locked' : ''}${isSelected ? ' dz-selected' : ''}`}
              onFocus={() => !dropped && !isSelected && onSelect({ kind: 'column', name: column.name })}
              onKeyDown={(e) => onRowKey(e, row)}
              onDragOver={(e) => {
                if (dragFrom.current !== null && !dropped) e.preventDefault();
              }}
              onDrop={(e) => onDrop(e, row)}
            >
              {pg ? null : (
                <div role="cell" className="dz-handle">
                  {dropped ? null : (
                    <span
                      role="img"
                      aria-label="Drag to reorder"
                      title="Drag to reorder (or Ctrl+↑/↓)"
                      draggable={!disabled}
                      onDragStart={(e) => {
                        dragFrom.current = index;
                        e.dataTransfer.effectAllowed = 'move';
                        e.dataTransfer.setData(COLUMN_DRAG_TYPE, String(index));
                      }}
                      onDragEnd={() => {
                        dragFrom.current = null;
                      }}
                    >
                      ⋮⋮
                    </span>
                  )}
                </div>
              )}
              <RowStateCell locked={column.locked} lock={lock} rowErrors={rowErrors.map((e) => e.message)} state={state} />
              <div role="cell" className="dz-pk">
                <button
                  type="button"
                  className={`dz-pk-btn${inPk ? ' on' : ''}`}
                  aria-label="Primary key"
                  aria-pressed={inPk}
                  disabled={disabled || !!pk?.locked}
                  onClick={() => edit((d) => togglePrimaryKey(d, column.name))}
                >
                  {inPk ? <KeyIcon /> : null}
                </button>
              </div>
              <div role="cell">
                <DraftInput
                  aria-label="Name"
                  className="mono"
                  value={column.name}
                  disabled={disabled}
                  error={errorOf('name')}
                  onCommit={(text, key) => {
                    if (index === null) return;
                    edit((d) => renameColumn(d, index, text), key);
                    // The selection names the column: it follows the rename.
                    if (isSelected) onSelect({ kind: 'column', name: text });
                  }}
                />
              </div>
              <div role="cell">
                <DraftInput
                  aria-label="Type"
                  className="mono"
                  list={typesId}
                  value={baseText(column.data_type)}
                  disabled={disabled}
                  error={errorOf('type')}
                  onCommit={(text, key) => retype(withBase(column.data_type, text, open.catalog), key)}
                />
              </div>
              <div role="cell">
                <DraftInput
                  aria-label="Length"
                  className="mono"
                  value={paramsText(column.data_type)}
                  disabled={disabled || !takesParams}
                  error={errorOf('length')}
                  normalize={(text) => paramsText(withParamsText(column.data_type, info, text))}
                  onCommit={(text, key) => retype(withParamsText(column.data_type, info, text), key)}
                />
              </div>
              <div role="cell" className="dz-center">
                <input
                  type="checkbox"
                  aria-label="Null"
                  checked={column.nullable}
                  disabled={disabled}
                  onChange={(e) => set({ nullable: e.target.checked })}
                />
              </div>
              <div role="cell">
                {generated && !column.default ? (
                  <span className="mono dz-generated" title={errorOf('default')}>
                    {generated}
                  </span>
                ) : (
                <DraftInput
                  aria-label="Default"
                  className={`mono${column.default?.kind === 'expression' ? ' dz-expr' : ''}`}
                  value={column.default?.text ?? ''}
                  placeholder={column.nullable && !dropped ? 'NULL' : undefined}
                  disabled={disabled}
                  error={errorOf('default')}
                  onCommit={(text, key) => set({ default: text === '' ? null : { kind: column.default?.kind ?? 'value', text } }, key)}
                />
                )}
              </div>
              <div role="cell">
                {dropped ? (
                  <span className="dz-drop-note">
                    To drop ·{' '}
                    <button type="button" className="dz-link" disabled={!editable} onClick={() => edit((d) => restoreColumn(open.def, d, column.name))}>
                      Undo
                    </button>
                  </span>
                ) : (
                  <DraftInput
                    aria-label="Comment"
                    value={column.comment ?? ''}
                    disabled={disabled}
                    error={errorOf('comment')}
                    onCommit={(text, key) => set({ comment: text === '' ? null : text }, key)}
                  />
                )}
              </div>
            </div>
          );
        })}
      </div>
      <datalist id={typesId}>
        {open.catalog.map((t) => (
          <option key={t.base} value={t.base} />
        ))}
      </datalist>
      <button type="button" className="dz-add" disabled={!editable} aria-keyshortcuts="Control+Enter" onClick={onAdd}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M8 3v10M3 8h10" />
        </svg>
        Add column
        <span className="dz-kbd" aria-hidden="true">
          Ctrl+Enter
        </span>
      </button>
    </div>
  );
}
