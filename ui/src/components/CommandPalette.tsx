import { useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import type { ObjectKind } from '../bindings';
import { usePalette } from '../store/palette';
import { buildMenus, type MenuState } from './appMenus';
import { EyeIcon, FunctionIcon, ObjectIcon, TableIcon } from './icons';
import { rankItems } from './paletteMatch';
import { paletteCommands } from './paletteCommands';
import { collectObjectEntries, openObjectEntry, searchObjects, type ObjectEntry } from './quickOpen';
import { useMenuState } from './useMenuState';
import './palette.css';

/** Most rows the list shows. */
const MAX_ROWS = 50;

/** A runnable row of the palette. */
interface Row {
  id: string;
  label: string;
  /** Shortcut (commands). */
  hint?: string;
  /** "connection › database" (objects). */
  path?: string;
  kind?: ObjectKind;
  /** Positions in `label` to highlight. */
  indices: number[];
  run(ddl: boolean): void;
}

/**
 * The enabled actions of the menu bar as "Menu: Action", plus the registered extra commands.
 * The menus are the single source: a new menu item shows up here with its shortcut.
 */
export function commandList(state: MenuState): { id: string; label: string; hint?: string; run(): void }[] {
  const out: { id: string; label: string; hint?: string; run(): void }[] = [];
  for (const menu of buildMenus(state)) {
    for (const item of menu.items) {
      if ('separator' in item || item.disabled) continue;
      out.push({ id: `menu:${menu.id}:${item.label}`, label: `${menu.label}: ${item.label}`, hint: item.hint, run: item.run });
    }
  }
  for (const c of paletteCommands()) out.push({ id: `cmd:${c.id}`, label: c.label, hint: c.hint, run: c.run });
  return out;
}

function KindIcon({ kind }: { kind: ObjectKind }) {
  if (kind === 'Table') return <TableIcon />;
  if (kind === 'View' || kind === 'MaterializedView') return <EyeIcon />;
  if (kind === 'Function' || kind === 'Procedure') return <FunctionIcon />;
  return <ObjectIcon />;
}

/** `text` with the characters at `indices` emphasised. */
function Highlighted({ text, indices }: { text: string; indices: number[] }): ReactNode {
  if (!indices.length) return text;
  const hit = new Set(indices);
  const parts: ReactNode[] = [];
  let run = '';
  let inMatch = false;
  const flush = (key: number) => {
    if (run) parts.push(inMatch ? <b key={key} className="palette-match">{run}</b> : run);
    run = '';
  };
  [...text].forEach((ch, i) => {
    if (hit.has(i) !== inMatch) {
      flush(i);
      inMatch = hit.has(i);
    }
    run += ch;
  });
  flush(text.length);
  return parts;
}

/**
 * Ctrl+Shift+P (commands) / Ctrl+P (objects) overlay: a top-center input over a list. It is
 * not modal: no backdrop, no `aria-modal`, so the window shortcuts keep working around it. A
 * leading `>` in the input means commands, as in VS Code.
 */
export function CommandPalette() {
  const open = usePalette((s) => s.open);
  const nonce = usePalette((s) => s.nonce);
  const initial = usePalette((s) => s.initial);
  return open ? <PaletteOverlay key={nonce} initial={initial} /> : null;
}

function PaletteOverlay({ initial }: { initial: string }) {
  const hide = usePalette((s) => s.hide);
  const state = useMenuState();
  const [text, setText] = useState(initial);
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);
  // Where the focus was (usually the editor): it goes back there when the palette closes.
  const previous = useRef<Element | null>(document.activeElement);

  const commandMode = text.startsWith('>');
  const query = commandMode ? text.slice(1) : text;
  // Taken once per opening: the loaded objects are read from the stores, never fetched.
  const objects = useMemo(() => collectObjectEntries(), []);

  const rows: Row[] = commandMode
    ? rankItems(commandList(state), query, (c) => [c.label])
        .slice(0, MAX_ROWS)
        .map(({ item, indices }) => ({ id: item.id, label: item.label, hint: item.hint, indices, run: () => item.run() }))
    : searchObjects(objects, query, MAX_ROWS).map(({ item, indices }: { item: ObjectEntry; indices: number[] }) => ({
        id: item.key,
        label: item.name,
        path: item.path,
        kind: item.objectRef.kind,
        indices,
        run: (ddl: boolean) => void openObjectEntry(item, ddl),
      }));
  const at = Math.min(active, Math.max(0, rows.length - 1));

  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);

  // Clicking anywhere outside closes it (the click then does what it was aimed at).
  useEffect(() => {
    const away = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) hide();
    };
    document.addEventListener('mousedown', away);
    return () => document.removeEventListener('mousedown', away);
  }, [hide]);

  useEffect(() => {
    list.current?.children[at]?.scrollIntoView?.({ block: 'nearest' });
  }, [at]);

  const close = () => {
    hide();
    if (previous.current instanceof HTMLElement) previous.current.focus();
  };

  const choose = (row: Row | undefined, ddl: boolean) => {
    if (!row) return;
    close();
    row.run(ddl);
  };

  const onKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (rows.length) setActive((at + (e.key === 'ArrowDown' ? 1 : -1) + rows.length) % rows.length);
    } else if (e.key === 'Home' && e.ctrlKey) {
      e.preventDefault();
      setActive(0);
    } else if (e.key === 'End' && e.ctrlKey) {
      e.preventDefault();
      setActive(Math.max(0, rows.length - 1));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      choose(rows[at], !commandMode && (e.ctrlKey || e.metaKey));
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  };

  const label = commandMode ? 'Command palette' : 'Go to object';
  return (
    <div ref={root} className="palette" role="dialog" aria-label={label} data-testid="palette">
      <input
        ref={input}
        className="palette-input"
        type="text"
        role="combobox"
        aria-label={commandMode ? 'Type a command' : 'Search objects, or type > for commands'}
        aria-expanded="true"
        aria-controls="palette-list"
        aria-activedescendant={rows[at] ? `palette-row-${at}` : undefined}
        aria-autocomplete="list"
        placeholder={commandMode ? 'Type a command' : 'Go to table, view or routine (type > for commands)'}
        spellCheck={false}
        autoComplete="off"
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setActive(0);
        }}
        onKeyDown={onKeyDown}
      />
      <ul id="palette-list" ref={list} className="palette-list" role="listbox" aria-label={label}>
        {rows.map((row, i) => (
          <li
            key={row.id}
            id={`palette-row-${i}`}
            role="option"
            aria-selected={i === at}
            className={`palette-row${i === at ? ' active' : ''}`}
            data-testid="palette-row"
            onMouseMove={() => i !== at && setActive(i)}
            onClick={(e) => choose(row, !commandMode && (e.ctrlKey || e.metaKey))}
          >
            {row.kind && (
              <span className="palette-icon">
                <KindIcon kind={row.kind} />
              </span>
            )}
            {row.path && <span className="palette-path">{`${row.path} ›`}</span>}
            <span className="palette-label">
              <Highlighted text={row.label} indices={row.indices} />
            </span>
            {row.hint && <kbd className="palette-hint">{row.hint}</kbd>}
          </li>
        ))}
      </ul>
      {rows.length === 0 && (
        <div className="palette-empty" role="status">
          {commandMode
            ? 'No matching commands'
            : objects.length === 0
              ? 'No objects loaded yet: expand a connection in the tree first'
              : 'No matching objects'}
        </div>
      )}
    </div>
  );
}
