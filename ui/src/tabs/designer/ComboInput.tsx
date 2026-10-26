import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import { DraftInput } from './DraftInput';
import './comboInput.css';

/** One entry of the list: its value, a muted hint after it, and the group it is listed under. */
export interface ComboOption {
  value: string;
  hint?: string;
  group?: string;
}

/** Options matching `query`: prefix matches first, then the rest containing it, each in list order. */
export function filterOptions<T extends { value: string }>(options: T[], query: string): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return options;
  const has = (o: T) => o.value.toLowerCase();
  const prefix = options.filter((o) => has(o).startsWith(q));
  return [...prefix, ...options.filter((o) => !has(o).startsWith(q) && has(o).includes(q))];
}

interface Props {
  /** The field's accessible name (and the list's: `<label> options`). */
  label: string;
  value: string;
  options: ComboOption[];
  placeholder?: string;
  disabled?: boolean;
  error?: string;
  /** Shown when nothing matches what was typed. */
  emptyText?: string;
  className?: string;
  onCommit(text: string, coalesce: string): void;
}

let pickCount = 0;

/**
 * A free-text field with a themed, filterable list (the designer's replacement for `<datalist>`,
 * whose native popup ignores the app theme). ↓ / click open it, ↑ / ↓ move, Enter picks, Esc closes.
 * Grouped while nothing is typed; a click on the chevron shows the whole list.
 */
export function ComboInput({ label, value, options, placeholder, disabled, error, emptyText = 'No match.', className, onCommit }: Props) {
  const listId = useId();
  const wrap = useRef<HTMLDivElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const [rect, setRect] = useState<{ left: number; top: number; width: number; above: boolean } | null>(null);
  const session = useRef('');

  const grouped = !query.trim() && options.some((o) => o.group);
  const ordered = useMemo(() => {
    const items = filterOptions(options, query);
    if (!grouped) return items;
    const order = [...new Set(items.map((o) => o.group ?? ''))];
    return order.flatMap((g) => items.filter((o) => (o.group ?? '') === g));
  }, [options, query, grouped]);
  const current = value.trim().toLowerCase();

  const place = () => {
    const r = wrap.current?.getBoundingClientRect();
    if (!r) return;
    const height = 280;
    const above = r.bottom + height > window.innerHeight && r.top > height;
    setRect({ left: r.left, top: above ? r.top - 4 : r.bottom + 4, width: Math.max(r.width, 220), above });
  };
  const show = (q: string) => {
    place();
    const at = ordered.findIndex((o) => o.value.toLowerCase() === current);
    setActive(q.trim() ? 0 : Math.max(at, 0));
    setOpen(true);
  };
  const pick = (o: ComboOption | undefined) => {
    if (!o) return;
    onCommit(o.value, session.current || `pick${++pickCount}`);
    setQuery('');
    setOpen(false);
  };

  useLayoutEffect(() => {
    if (!open) return;
    list.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView?.({ block: 'nearest' });
  }, [open, active]);
  useEffect(() => {
    if (!open) return;
    const reposition = () => place();
    window.addEventListener('resize', reposition);
    // Any scrolling container (the grid, the tab) moves the field.
    window.addEventListener('scroll', reposition, true);
    return () => {
      window.removeEventListener('resize', reposition);
      window.removeEventListener('scroll', reposition, true);
    };
  }, [open]);

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (!open) show('');
      else setActive((i) => Math.max(0, Math.min(i + 1, ordered.length - 1)));
    } else if (e.key === 'ArrowUp' && open) {
      e.preventDefault();
      setActive((i) => Math.max(i - 1, 0));
    } else if (e.key === 'Enter' && open && !ordered[active]) {
      // Nothing listed (a custom type): Enter commits the text as typed.
      setOpen(false);
    } else if (e.key === 'Enter' && open && !e.ctrlKey && !e.metaKey) {
      e.preventDefault();
      e.stopPropagation();
      pick(ordered[active]);
    } else if (e.key === 'Escape' && open) {
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    } else if (e.key === 'Tab' && open) {
      setOpen(false);
    }
  };

  let lastGroup: string | undefined;
  return (
    <div className="dz-combo" ref={wrap}>
      <DraftInput
        aria-label={label}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-activedescendant={open && ordered[active] ? `${listId}-${active}` : undefined}
        autoComplete="off"
        spellCheck={false}
        className={className ?? 'mono'}
        placeholder={placeholder}
        value={value}
        disabled={disabled}
        error={error}
        onFocus={() => {
          session.current = `combo${++pickCount}`;
          setQuery('');
        }}
        onBlur={() => setOpen(false)}
        onClick={() => !open && show('')}
        onKeyDown={onKeyDown}
        onCommit={(text, key) => {
          session.current = key;
          setQuery(text);
          if (!open) show(text);
          else setActive(0);
          onCommit(text, key);
        }}
      />
      <button
        type="button"
        className="dz-combo-chevron"
        tabIndex={-1}
        aria-label={`Show ${label.toLowerCase()} options`}
        disabled={disabled}
        // Keeps the focus in the field.
        onMouseDown={(e) => e.preventDefault()}
        onClick={() => {
          if (open) setOpen(false);
          else {
            wrap.current?.querySelector('input')?.focus();
            setQuery('');
            show('');
          }
        }}
      >
        <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true">
          <path d="M6 9l6 6 6-6" />
        </svg>
      </button>
      {open && rect
        ? createPortal(
            <div
              ref={list}
              id={listId}
              role="listbox"
              aria-label={`${label} options`}
              className="dz-combo-list"
              style={{ left: rect.left, width: rect.width, ...(rect.above ? { bottom: window.innerHeight - rect.top } : { top: rect.top }) }}
              // A click on the list must not blur the field (that would close the list first).
              onMouseDown={(e) => e.preventDefault()}
            >
              {ordered.length === 0 ? (
                <div className="dz-combo-empty">{emptyText}</div>
              ) : (
                ordered.map((o, i) => {
                  const header = grouped && o.group !== lastGroup ? o.group : null;
                  lastGroup = o.group;
                  return (
                    <div key={o.value} role="presentation">
                      {header ? (
                        <div className="dz-combo-group" role="presentation">
                          {header}
                        </div>
                      ) : null}
                      <div
                        id={`${listId}-${i}`}
                        data-index={i}
                        role="option"
                        aria-selected={i === active}
                        className={`dz-combo-option${i === active ? ' active' : ''}${o.value.toLowerCase() === current ? ' current' : ''}`}
                        onMouseEnter={() => setActive(i)}
                        onClick={() => pick(o)}
                      >
                        <span className="mono">{o.value}</span>
                        {o.hint ? <span className="dz-combo-hint mono">{o.hint}</span> : null}
                      </div>
                    </div>
                  );
                })
              )}
            </div>,
            document.body,
          )
        : null}
    </div>
  );
}
