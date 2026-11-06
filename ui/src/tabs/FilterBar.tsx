import { useImperativeHandle, useRef, type KeyboardEvent, type Ref } from 'react';
import type { EngineFamily, FilterMode, FilterOp } from '../bindings';
import { CloseIcon } from '../components/icons';
import { SelectMenu, type SelectMenuOption } from '../components/SelectMenu';
import { assertNever } from '../engine';
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

const OP_OPTIONS: SelectMenuOption[] = FILTER_OPS.map((o) => ({ value: o.op, label: o.label }));

const MODE_OPTIONS: SelectMenuOption[] = [
  { value: 'and', label: 'all (AND)' },
  { value: 'or', label: 'any (OR)' },
];

export interface FilterBarHandle {
  /** Moves the keyboard focus into the bar (F3). */
  focus(): void;
}

interface Props {
  columns: string[];
  /** The columns' types, shown as hints in the column list (optional). */
  columnTypes?: Record<string, string>;
  draft: FilterDraft;
  /** Whether a filter is in effect (Clear is offered then even with an empty bar). */
  active: boolean;
  error: string | null;
  onChange(draft: FilterDraft): void;
  onApply(): void;
  onClear(): void;
  handle?: Ref<FilterBarHandle>;
  /** The raw mode is a SQL WHERE condition (`sql`, the default) or a JSON filter (`command`). */
  family?: EngineFamily;
}

/** How the raw mode reads per family: its toggle / input name, tooltip, placeholder and the word before the input. */
function rawCopy(family: EngineFamily): { label: string; title: string; placeholder: string; prefix: string | null } {
  switch (family) {
    case 'sql':
      return { label: 'Raw WHERE', title: 'Type the WHERE condition as SQL', placeholder: "total > 100 AND status = 'paid'", prefix: 'WHERE' };
    case 'command':
      return { label: 'JSON filter', title: 'Type the filter as a JSON object', placeholder: '{ "status": "paid" }', prefix: null };
    default:
      return assertNever(family);
  }
}

/**
 * The data view's filter bar (Navicat Filter): condition rows of column, operator and value(s),
 * joined with AND or OR, then one line of actions under them (add, match, raw mode | Apply, Clear).
 * Nothing is SQL: the rows become a structured filter on Apply.
 */
export function FilterBar({ columns, columnTypes, draft, active, error, onChange, onApply, onClear, handle, family = 'sql' }: Props) {
  const copy = rawCopy(family);
  const root = useRef<HTMLDivElement>(null);
  useImperativeHandle(handle, () => ({
    focus() {
      // The first value box, else the first column, else "+ Condition".
      for (const selector of ['.fb-raw', '.fb-value', '.fb-row .select-menu', '.fb-add']) {
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
  const columnOptions = (current: string): SelectMenuOption[] => [
    // A column the page no longer has stays selectable (the backend refuses it).
    ...(!columns.includes(current) && current ? [{ value: current, label: current }] : []),
    ...columns.map((c) => ({ value: c, label: c, hint: columnTypes?.[c] })),
  ];

  return (
    <div className="filter-bar" role="search" aria-label="Filter" ref={root}>
      {raw ? (
        <div className="fb-row">
          <span className="fb-join">{copy.prefix}</span>
          <input
            className="fb-raw"
            aria-label={copy.label}
            placeholder={copy.placeholder}
            spellCheck={false}
            autoComplete="off"
            value={rawText}
            onChange={(e) => onChange({ ...draft, rawText: e.target.value })}
            onKeyDown={onKeyDown}
          />
        </div>
      ) : (
        draft.rows.map((row, i) => {
          const arity = opArity(row.op);
          const n = i + 1;
          return (
            <div className="fb-row" key={row.id} data-testid="filter-row">
              {/* How the row joins the ones above: a reading aid; the Match menu sets it. */}
              <span className="fb-join" aria-hidden="true">
                {i === 0 ? 'Where' : draft.mode === 'or' ? 'or' : 'and'}
              </span>
              <SelectMenu
                label={`Column ${n}`}
                className="fb-column"
                mono
                value={row.column}
                options={columnOptions(row.column)}
                onChange={(column) => update(row.id, { column })}
              />
              <SelectMenu
                label={`Operator ${n}`}
                className="fb-op"
                searchable={false}
                value={row.op}
                options={OP_OPTIONS}
                onChange={(op) => update(row.id, { op: op as FilterOp })}
              />
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
              <button
                type="button"
                className="icon-btn fb-remove"
                aria-label={`Remove condition ${n}`}
                title="Remove this condition"
                onClick={() => remove(row.id)}
              >
                <CloseIcon />
              </button>
            </div>
          );
        })
      )}
      <div className="fb-actions">
        {raw ? null : (
          <>
            <button type="button" className="qt-btn fb-add" onClick={add} disabled={columns.length === 0}>
              + Condition
            </button>
            <span className="fb-mode">
              <span aria-hidden="true">Match</span>
              <SelectMenu
                label="Match conditions"
                className="fb-mode-menu"
                value={draft.mode}
                options={MODE_OPTIONS}
                onChange={(mode) => onChange({ ...draft, mode: mode as FilterMode })}
              />
            </span>
          </>
        )}
        <button
          type="button"
          className={`qt-btn${raw ? ' dv-on' : ''}`}
          aria-pressed={raw}
          title={copy.title}
          onClick={() => onChange({ ...draft, rawMode: !raw })}
        >
          {copy.label}
        </button>
        <span className="fb-sep" aria-hidden="true" />
        <button type="button" className="qt-btn qt-primary" onClick={onApply}>
          Apply
        </button>
        <button type="button" className="qt-btn" onClick={onClear} disabled={!active && (raw ? rawText === '' : draft.rows.length === 0)}>
          Clear
        </button>
      </div>
      {error ? (
        <p role="alert" className="fb-error">
          {error}
        </p>
      ) : null}
    </div>
  );
}
