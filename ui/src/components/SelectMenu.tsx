import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { ChevronDownIcon } from './icons';
import './selectMenu.css';

/** One choice: its value, the text shown, an optional mark before it and a muted hint after it. */
export interface SelectMenuOption {
  value: string;
  label: string;
  hint?: string;
  icon?: ReactNode;
  disabled?: boolean;
}

interface Props {
  /** Accessible name of the control (the list is `<label> options`, the search `Search <label>`). */
  label: string;
  value: string;
  options: SelectMenuOption[];
  onChange(value: string): void;
  disabled?: boolean;
  title?: string;
  testId?: string;
  className?: string;
  /** A search field on top of the list; by default only for lists longer than 7. */
  searchable?: boolean;
  /** Control height: `xs` for dense grid cells, `sm` (the default) for toolbars and bars, `md` for forms. */
  size?: 'xs' | 'sm' | 'md';
  /** Shown (muted) when `value` is empty and matches no option. */
  placeholder?: string;
  /** Marks the control as invalid: a danger border and `aria-invalid`. */
  invalid?: boolean;
  /** The trigger's id, for a `<label htmlFor>`. */
  id?: string;
  /** Monospaced labels (identifiers such as column names), in the trigger and the list. */
  mono?: boolean;
  /**
   * Whether ↑ / ↓ open the closed list (the default). Off in grids, where they move between rows:
   * Alt+↓, Enter and Space still open it.
   */
  arrowsOpen?: boolean;
}

/** Marks the list in <body>: a popover that closes on outside clicks treats it as inside. */
export const SELECT_MENU_ATTR = 'data-select-menu';

/** Whether `node` is in an open SelectMenu list (rendered in <body>, outside its trigger). */
export const inSelectMenu = (node: EventTarget | null): boolean =>
  node instanceof Element && !!node.closest(`[${SELECT_MENU_ATTR}]`);

const SEARCH_FROM = 8;
const LIST_MAX_H = 300;
/** How long typed letters keep adding to the type-ahead text. */
const TYPE_AHEAD_MS = 700;

/** Whether a key press types a character (type-ahead), not a shortcut. */
const isPrintable = (e: KeyboardEvent) => e.key.length === 1 && e.key !== ' ' && !e.ctrlKey && !e.metaKey && !e.altKey;

/**
 * A themed select (the native `<select>` popup ignores the app's theme and cannot be searched):
 * a button showing the current choice that opens a list, with a search field for long lists.
 * ↓ / Enter / Space open it; ↑ / ↓ / Home / End move; Enter picks; Esc closes and keeps the focus.
 * Without a search field, typing letters jumps to the first option starting with them (as a native select).
 */
export function SelectMenu({
  label,
  value,
  options,
  onChange,
  disabled,
  title,
  testId,
  className,
  searchable,
  size = 'sm',
  placeholder,
  invalid,
  id,
  mono,
  arrowsOpen = true,
}: Props) {
  const listId = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const typed = useRef({ text: '', at: 0 });
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const [rect, setRect] = useState<{ left: number; top: number; width: number; above: boolean } | null>(null);

  const withSearch = searchable ?? options.length >= SEARCH_FROM;
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return options;
    const hit = (o: SelectMenuOption) => `${o.label} ${o.hint ?? ''}`.toLowerCase();
    return [...options.filter((o) => hit(o).startsWith(q)), ...options.filter((o) => !hit(o).startsWith(q) && hit(o).includes(q))];
  }, [options, query]);
  const current = options.find((o) => o.value === value);

  const place = () => {
    const r = trigger.current?.getBoundingClientRect();
    if (!r) return;
    const above = r.bottom + LIST_MAX_H > window.innerHeight && r.top > LIST_MAX_H;
    setRect({ left: r.left, top: above ? r.top - 4 : r.bottom + 4, width: Math.max(r.width, 200), above });
  };
  const show = () => {
    if (disabled) return;
    place();
    typed.current = { text: '', at: 0 };
    setQuery('');
    setActive(Math.max(0, options.findIndex((o) => o.value === value)));
    setOpen(true);
  };

  /** Type-ahead: the next option (after `from` for a repeated single letter) starting with the typed text. */
  const typeAhead = (key: string, from: number) => {
    const now = Date.now();
    const prev = now - typed.current.at < TYPE_AHEAD_MS ? typed.current.text : '';
    const text = (prev + key).toLowerCase();
    typed.current = { text, at: now };
    // Repeating one letter cycles through the options starting with it.
    const cycle = text.length > 1 && [...text].every((c) => c === text[0]) ? text[0]! : text;
    const start = cycle.length === 1 ? from + 1 : from;
    for (let k = 0; k < options.length; k++) {
      const i = (start + k) % options.length;
      const o = options[i]!;
      if (!o.disabled && o.label.toLowerCase().startsWith(cycle)) return i;
    }
    return -1;
  };
  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) trigger.current?.focus();
  };
  const pick = (o: SelectMenuOption | undefined) => {
    if (!o || o.disabled) return;
    close(true);
    if (o.value !== value) onChange(o.value);
  };

  // The search field takes the focus; without one the list does (its keys move the choice).
  useEffect(() => {
    if (!open) return;
    (withSearch ? search.current : list.current)?.focus();
  }, [open, withSearch]);

  useLayoutEffect(() => {
    if (!open) return;
    list.current?.querySelector<HTMLElement>(`[data-index="${active}"]`)?.scrollIntoView?.({ block: 'nearest' });
  }, [open, active]);

  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => {
      const t = e.target as Node;
      // The whole popup (search field included) is inside.
      if (!pop.current?.contains(t) && !trigger.current?.contains(t)) setOpen(false);
    };
    const reposition = () => place();
    document.addEventListener('mousedown', away, true);
    window.addEventListener('resize', reposition);
    window.addEventListener('scroll', reposition, true);
    return () => {
      document.removeEventListener('mousedown', away, true);
      window.removeEventListener('resize', reposition);
      window.removeEventListener('scroll', reposition, true);
    };
  }, [open]);

  const step = (from: number, dir: 1 | -1) => {
    for (let i = from + dir; i >= 0 && i < shown.length; i += dir) if (!shown[i]?.disabled) return i;
    return from;
  };
  const onListKey = (e: KeyboardEvent) => {
    const keys: Record<string, () => void> = {
      ArrowDown: () => setActive((i) => step(i, 1)),
      ArrowUp: () => setActive((i) => step(i, -1)),
      Home: () => setActive(step(-1, 1)),
      End: () => setActive(step(shown.length, -1)),
      Enter: () => pick(shown[active]),
      Escape: () => close(true),
      Tab: () => close(false),
    };
    if (!withSearch && isPrintable(e)) {
      e.preventDefault();
      const i = typeAhead(e.key, active);
      if (i >= 0) setActive(i);
      return;
    }
    const run = keys[e.key];
    if (!run) return;
    // Tab moves on as usual; the rest stay here (Esc must not also close a popover around us).
    if (e.key !== 'Tab') {
      e.preventDefault();
      e.stopPropagation();
    }
    run();
  };

  return (
    <>
      <button
        ref={trigger}
        type="button"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-label={label}
        aria-invalid={invalid || undefined}
        id={id}
        data-testid={testId}
        data-value={value}
        title={title}
        disabled={disabled}
        className={`select-menu select-menu-${size}${mono ? ' select-menu-mono' : ''}${invalid ? ' invalid' : ''}${className ? ` ${className}` : ''}`}
        onClick={() => (open ? close(false) : show())}
        onKeyDown={(e) => {
          if (open) return;
          const arrow = e.key === 'ArrowDown' || e.key === 'ArrowUp';
          if ((arrow && (arrowsOpen || e.altKey)) || e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            show();
          } else if (isPrintable(e) && !disabled) {
            // Typing on the closed control opens it there: the search starts with the letter, or the list jumps to it.
            e.preventDefault();
            show();
            if (withSearch) setQuery(e.key);
            else {
              const i = typeAhead(e.key, Math.max(-1, options.findIndex((o) => o.value === value)));
              if (i >= 0) setActive(i);
            }
          }
        }}
      >
        {current?.icon ? <span className="select-menu-icon">{current.icon}</span> : null}
        <span className={`select-menu-text${current ? '' : ' placeholder'}`}>{current?.label ?? (value === '' && placeholder ? placeholder : value)}</span>
        <span className="select-menu-caret" aria-hidden="true">
          <ChevronDownIcon />
        </span>
      </button>
      {open && rect
        ? createPortal(
            <div
              ref={pop}
              {...{ [SELECT_MENU_ATTR]: '' }}
              className={`select-menu-pop${mono ? ' select-menu-mono' : ''}`}
              style={{ left: rect.left, width: rect.width, ...(rect.above ? { bottom: window.innerHeight - rect.top } : { top: rect.top }) }}
              onKeyDown={onListKey}
            >
              {withSearch ? (
                <input
                  ref={search}
                  className="select-menu-search"
                  aria-label={`Search ${label.toLowerCase()}`}
                  aria-controls={listId}
                  aria-activedescendant={shown[active] ? `${listId}-${active}` : undefined}
                  placeholder="Search…"
                  autoComplete="off"
                  spellCheck={false}
                  value={query}
                  onChange={(e) => {
                    setQuery(e.target.value);
                    setActive(0);
                  }}
                />
              ) : null}
              <div
                ref={list}
                id={listId}
                role="listbox"
                aria-label={`${label} options`}
                tabIndex={-1}
                aria-activedescendant={!withSearch && shown[active] ? `${listId}-${active}` : undefined}
                className="select-menu-list"
              >
                {shown.length === 0 ? (
                  <div className="select-menu-empty">No match.</div>
                ) : (
                  shown.map((o, i) => (
                    <div
                      key={o.value}
                      id={`${listId}-${i}`}
                      data-index={i}
                      role="option"
                      // Named by its label alone; the hint is a visual aside (its tooltip too).
                      aria-label={o.label}
                      title={o.hint ? `${o.label} · ${o.hint}` : undefined}
                      aria-selected={o.value === value}
                      aria-disabled={o.disabled || undefined}
                      className={`select-menu-option${i === active ? ' active' : ''}${o.value === value ? ' current' : ''}`}
                      onMouseEnter={() => !o.disabled && setActive(i)}
                      // The field keeps the focus (a click must not blur the search first).
                      onMouseDown={(e) => e.preventDefault()}
                      onClick={() => pick(o)}
                    >
                      {o.icon ? <span className="select-menu-icon">{o.icon}</span> : null}
                      <span className="select-menu-label">{o.label}</span>
                      {o.hint ? <span className="select-menu-hint">{o.hint}</span> : null}
                    </div>
                  ))
                )}
              </div>
            </div>,
            document.body,
          )
        : null}
    </>
  );
}
