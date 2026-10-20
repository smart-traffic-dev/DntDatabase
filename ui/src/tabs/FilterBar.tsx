import { useImperativeHandle, useRef, type KeyboardEvent, type Ref } from 'react';
import type { FilterMode, FilterOp } from '../bindings';
import { newFilterRow, opArity, type FilterDraft, type FilterRow } from '../store/tableData';

/** The operators of the bar, in menu order, with their labels. */
export const FILTER_OPS: { op: FilterOp; label: string }[] = [
  { op: 'eq', label: '=' },
  { op: 'ne', label: '<>' },
  { op: 'lt', label: '<' },
  { op: 'le', label: '<=' },
  { op: 'gt', label: '>' },
  { op: 'ge', label: '>=' },
  { op: 'like', label: 'LIKE' },
  { op: 'not_like', label: 'NOT LIKE' },
  { op: 'contains', label: 'contains' },
  { op: 'starts_with', label: 'starts with' },
  { op: 'ends_with', label: 'ends with' },
  { op: 'in', label: 'IN (list)' },
  { op: 'between', label: 'BETWEEN' },
  { op: 'is_null', label: 'IS NULL' },
  { op: 'is_not_null', label: 'IS NOT NULL' },
];

export interface FilterBarHandle {
  /** Moves the keyboard focus into the bar (F3). */
  focus(): void;
}

interface Props {
  columns: string[];
  draft: FilterDraft;
  /** Whether a filter is in effect (Clear is offered then even with an empty bar). */
  active: boolean;
  error: string | null;
  onChange(draft: FilterDraft): void;
  onApply(): void;
  onClear(): void;
  handle?: Ref<FilterBarHandle>;
}

/**
 * The data view's filter bar (Navicat Filter): condition rows of column, operator and value(s),
 * joined with AND or OR. Nothing is SQL: the rows become a structured filter on Apply.
 */
export function FilterBar({ columns, draft, active, error, onChange, onApply, onClear, handle }: Props) {
  const root = useRef<HTMLDivElement>(null);
  useImperativeHandle(handle, () => ({
    focus() {
      // The first value box, else the first column, else "+ Condition".
      for (const selector of ['.fb-raw', '.fb-value', '.fb-row select', '.fb-add']) {
        const el = root.current?.querySelector<HTMLElement>(selector);
        if (el) {
          el.focus();
          return;
        }
      }
    },
  }));

  const update = (id: number, change: Partial<FilterRow>) =>
    onChange({ ...draft, rows: draft.rows.map((r) => (r.id === id ? { ...r, ...change } : r)) });
  const remove = (id: number) => onChange({ ...draft, rows: draft.rows.filter((r) => r.id !== id) });
  const add = () => onChange({ ...draft, rows: [...draft.rows, newFilterRow(columns[0] ?? '')] });

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      onApply();
    }
  };

  const raw = draft.rawMode === true;
  const rawText = draft.rawText ?? '';

  return (
    <div className="filter-bar" role="search" aria-label="Filter" ref={root}>
      <div className="fb-head">
        {raw ? null : (
          <>
            <label className="fb-mode">
              <span>Match</span>
              <select
                className="qt-select"
                aria-label="Match conditions"
                value={draft.mode}
                onChange={(e) => onChange({ ...draft, mode: e.target.value as FilterMode })}
              >
                <option value="and">all (AND)</option>
                <option value="or">any (OR)</option>
              </select>
            </label>
            <button type="button" className="qt-btn fb-add" onClick={add} disabled={columns.length === 0}>
              + Condition
            </button>
          </>
        )}
        <button
          type="button"
          className={`qt-btn${raw ? ' dv-on' : ''}`}
          aria-pressed={raw}
          title="Type the WHERE condition as SQL"
          onClick={() => onChange({ ...draft, rawMode: !raw })}
        >
          Raw WHERE
        </button>
        <span className="spacer" />
        <button type="button" className="qt-btn" onClick={onApply}>
          Apply
        </button>
        <button type="button" className="qt-btn" onClick={onClear} disabled={!active && (raw ? rawText === '' : draft.rows.length === 0)}>
          Clear
        </button>
      </div>
      {raw ? (
        <div className="fb-row">
          <span className="fb-and">WHERE</span>
          <input
            className="fb-raw"
            aria-label="Raw WHERE"
            placeholder="total > 100 AND status = 'paid'"
            spellCheck={false}
            autoComplete="off"
            value={rawText}
            onChange={(e) => onChange({ ...draft, rawText: e.target.value })}
            onKeyDown={onKeyDown}
          />
        </div>
      ) : null}
      {(raw ? [] : draft.rows).map((row, i) => {
        const arity = opArity(row.op);
        const n = i + 1;
        return (
          <div className="fb-row" key={row.id} data-testid="filter-row">
            <select
              className="qt-select"
              aria-label={`Column ${n}`}
              value={row.column}
              onChange={(e) => update(row.id, { column: e.target.value })}
            >
              {/* A column the page no longer has stays selectable (the backend refuses it). */}
              {!columns.includes(row.column) && row.column ? <option value={row.column}>{row.column}</option> : null}
              {columns.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
            <select
              className="qt-select fb-op"
              aria-label={`Operator ${n}`}
              value={row.op}
              onChange={(e) => update(row.id, { op: e.target.value as FilterOp })}
            >
              {FILTER_OPS.map((o) => (
                <option key={o.op} value={o.op}>
                  {o.label}
                </option>
              ))}
            </select>
            {arity === 0 ? null : (
              <input
                className="fb-value"
                aria-label={arity === 2 ? `From ${n}` : `Value ${n}`}
                placeholder={arity === 'list' ? 'a, b, c' : arity === 2 ? 'from' : 'value'}
                value={row.value}
                onChange={(e) => update(row.id, { value: e.target.value })}
                onKeyDown={onKeyDown}
              />
            )}
            {arity === 2 ? (
              <>
                <span className="fb-and">and</span>
                <input
                  className="fb-value"
                  aria-label={`To ${n}`}
                  placeholder="to"
                  value={row.value2}
                  onChange={(e) => update(row.id, { value2: e.target.value })}
                  onKeyDown={onKeyDown}
                />
              </>
            ) : null}
            <button type="button" className="fb-remove" aria-label={`Remove condition ${n}`} onClick={() => remove(row.id)}>
              ×
            </button>
          </div>
        );
      })}
      {error ? (
        <p role="alert" className="fb-error">
          {error}
        </p>
      ) : null}
    </div>
  );
}
