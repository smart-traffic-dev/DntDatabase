import { useEffect, useRef, type KeyboardEvent, type ReactNode } from 'react';
import type { DesignOpen, DraftError, ElementRef, TableDef } from '../../bindings';
import {
  ADD,
  elementErrors,
  elementRows,
  KIND_OF,
  lockFor,
  removeElement,
  restoreElement,
  updateElement,
  type ElementKey,
  type ElementRow,
} from './elementModel';
import { RowStateCell } from './RowStateCell';
import './columnsGrid.css';
import './elementsGrid.css';

type Edit = (recipe: (d: TableDef) => TableDef, coalesce?: string) => void;
type Item<K extends ElementKey> = TableDef[K][number];

export interface ElementGridProps {
  open: DesignOpen;
  draft: TableDef;
  errors: DraftError[];
  /** False while read-only, lost or (re)opening: every field is disabled. */
  editable: boolean;
  selection: ElementRef | null;
  onSelect(el: ElementRef | null): void;
  edit: Edit;
}

/** What a grid hands each cell of a row. */
export interface CellContext<K extends ElementKey> {
  row: ElementRow<Item<K>>;
  /** The row's fields are read-only (locked, dropped, or the designer is not editable). */
  disabled: boolean;
  /** The message of a draft error on `field` of this element. */
  errorOf(field: string): string | undefined;
  /** Edits this element (typing coalesces into one undo step per `key`). */
  set(patch: Partial<Item<K>>, key?: string): void;
}

interface FrameProps<K extends ElementKey> extends ElementGridProps {
  listKey: K;
  /** Accessible name of the table: `Indexes`. */
  label: string;
  /** Singular noun for the buttons: `index`. */
  noun: string;
  /** The grid's tracks between the state mark and the actions. */
  cols: string;
  headers: string[];
  /** The fields the cells show; a draft error on any other field shows on the row's state mark. */
  fields: string[];
  cells(c: CellContext<K>): ReactNode;
  /** Notes under the grid (hints about rows). */
  children?: ReactNode;
}

/**
 * The frame the Indexes, Foreign keys and Checks grids share: header, rows with the same state
 * styling, lock mark and Delete key as Columns, the Remove / Undo actions and the Add button.
 */
export function ElementGrid<K extends ElementKey>(p: FrameProps<K>) {
  const { open, draft, errors, editable, selection, onSelect, edit, listKey } = p;
  const kind = KIND_OF[listKey];
  const rows = elementRows(open.def[listKey] as Item<K>[], draft[listKey] as Item<K>[]);
  const rowEls = useRef(new Map<string, HTMLDivElement>());
  const count = draft[listKey].length;
  const prevCount = useRef(count);
  const selected = selection?.kind === kind ? selection.name : null;

  // An element just added (and selected by the add button) gets its name field focused.
  useEffect(() => {
    const grew = count > prevCount.current;
    prevCount.current = count;
    if (!grew) return;
    rowEls.current.get(`new:${count - 1}`)?.querySelector<HTMLInputElement>('input[aria-label="Name"]')?.focus();
  });

  const onRowKey = (e: KeyboardEvent<HTMLDivElement>, row: ElementRow<Item<K>>) => {
    if (e.key !== 'Delete' || e.target !== e.currentTarget || row.index === null || !editable || row.item.locked) return;
    e.preventDefault();
    remove(row.index);
  };
  const remove = (index: number) => {
    edit((d) => removeElement(d, listKey, index));
    onSelect(null);
  };
  const add = () => {
    edit((d) => ADD[listKey](d));
    const next = ADD[listKey](draft)[listKey].at(-1);
    if (next) onSelect({ kind, name: next.name } as ElementRef);
  };

  return (
    <div className="dz-columns">
      <div className="dz-grid dz-egrid" style={{ ['--dz-cols' as string]: `28px ${p.cols} 76px` }} role="table" aria-label={p.label} aria-rowcount={rows.length + 1}>
        <div role="row" className="dz-row dz-head">
          <div role="columnheader" aria-label="State" />
          {p.headers.map((h) => (
            <div key={h} role="columnheader">
              {h}
            </div>
          ))}
          <div role="columnheader" aria-label="Actions" />
        </div>
        {rows.map((row) => {
          const { item, index, state } = row;
          const dropped = state === 'drop';
          const lock = lockFor(open.locked, kind, item);
          const disabled = !editable || item.locked || dropped;
          const errs = dropped ? [] : elementErrors(errors, kind, item.name);
          const isSelected = !dropped && selected === item.name;
          const set = (patch: Partial<Item<K>>, key?: string) => index !== null && edit((d) => updateElement(d, listKey, index, patch), key);
          return (
            <div
              key={row.key}
              ref={(el) => {
                if (el) rowEls.current.set(row.key, el);
                else rowEls.current.delete(row.key);
              }}
              role="row"
              data-element={item.name}
              aria-current={isSelected ? true : undefined}
              tabIndex={isSelected ? 0 : -1}
              className={`dz-row dz-${state}${item.locked ? ' dz-locked' : ''}${isSelected ? ' dz-selected' : ''}`}
              onFocus={() => !dropped && !isSelected && onSelect({ kind, name: item.name } as ElementRef)}
              onKeyDown={(e) => onRowKey(e, row)}
            >
              <RowStateCell
                locked={item.locked}
                lock={lock}
                rowErrors={errs.filter((e) => e.field === null || !p.fields.includes(e.field)).map((e) => e.message)}
                state={state}
              />
              {p.cells({
                row,
                disabled,
                errorOf: (field) => errs.find((e) => e.field === field)?.message,
                set,
              })}
              <div role="cell" className="dz-actions">
                {dropped ? (
                  <button
                    type="button"
                    className="dz-link"
                    disabled={!editable}
                    onClick={() => edit((d) => restoreElement(open.def, d, listKey, item.name))}
                  >
                    Undo
                  </button>
                ) : (
                  <button
                    type="button"
                    className="dz-link"
                    aria-label={`Remove ${p.noun}`}
                    disabled={disabled}
                    onClick={() => index !== null && remove(index)}
                  >
                    Remove
                  </button>
                )}
              </div>
            </div>
          );
        })}
      </div>
      {p.children}
      <button type="button" className="dz-add" disabled={!editable} onClick={add}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M8 3v10M3 8h10" />
        </svg>
        Add {p.noun}
      </button>
    </div>
  );
}
