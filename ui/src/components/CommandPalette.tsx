import { useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import type { ObjectKind } from '../bindings';
import { usePalette, type PaletteMode } from '../store/palette';
import { buildMenus, type MenuState } from './appMenus';
import { KindIcon } from './icons';
import { rankItems, type Ranked } from './paletteMatch';
import { paletteCommands } from './paletteCommands';
import { recentCommands, rememberCommand } from './paletteRecent';
import { collectObjectEntries, openObjectEntry, searchObjects, type ObjectEntry } from './quickOpen';
import { useMenuState } from './useMenuState';
import './palette.css';

/** Most rows the list shows. */
const MAX_ROWS = 60;

/** The hint of a planned menu item; such items are listed last and never run. */
const COMING_SOON = 'Coming soon';

/** One command of the palette: a menu action or a registered extra command. */
export interface PaletteEntry {
  id: string;
  /** Section header: the menu name, or the "Group" of a "Group: Action" extra command. */
  group: string;
  label: string;
  /** Shortcut. */
  hint?: string;
  /** A "Coming soon" item: listed last, not runnable. */
  soon: boolean;
  run(): void;
}

/** A row of the palette. */
interface Row {
  id: string;
  label: string;
  /** Accessible name: the label with its shortcut (or "coming soon"), or the object's full path. */
  name: string;
  /** Caption after the label (the menu name of a Recent row). */
  sub?: string;
  /** Shortcut (commands). */
  hint?: string;
  /** "connection › database" (objects). */
  path?: string;
  kind?: ObjectKind;
  /** Positions in `label` to highlight. */
  indices: number[];
  disabled?: boolean;
  run(ddl: boolean): void;
}

interface Section {
  /** No title: the plain object list. */
  title?: string;
  rows: Row[];
}

/**
 * The actions of the menu bar, plus the registered extra commands ("Group: Action" labels are
 * split into group and action). The menus are the single source: a new menu item shows up here
 * with its shortcut. Disabled items are left out, except the planned "Coming soon" ones.
 */
export function commandList(state: MenuState): PaletteEntry[] {
  const out: PaletteEntry[] = [];
  for (const menu of buildMenus(state)) {
    for (const item of menu.items) {
      if ('separator' in item) continue;
      const soon = item.hint === COMING_SOON;
      if (item.disabled && !soon) continue;
      out.push({
        id: `menu:${menu.id}:${item.label}`,
        group: menu.label,
        label: item.label,
        hint: soon ? undefined : item.hint,
        soon,
        run: item.run,
      });
    }
  }
  for (const c of paletteCommands()) {
    const at = c.label.indexOf(': ');
    out.push({
      id: `cmd:${c.id}`,
      group: at > 0 ? c.label.slice(0, at) : 'Commands',
      label: at > 0 ? c.label.slice(at + 2) : c.label,
      hint: c.hint,
      soon: false,
      run: c.run,
    });
  }
  return out;
}

const commandRow = (r: Ranked<PaletteEntry>, sub?: string): Row => ({
  id: r.item.id,
  label: r.item.label,
  name: r.item.soon ? `${r.item.label}, coming soon` : r.item.hint ? `${r.item.label}, ${r.item.hint}` : r.item.label,
  sub,
  hint: r.item.hint,
  indices: r.indices,
  disabled: r.item.soon,
  run: () => r.item.run(),
});

/**
 * Commands under section headers: Recent first (no query) or wherever its best match ranks (a
 * query, so the best match is always the first row), then one section per menu, "Coming soon" last.
 */
function commandSections(entries: PaletteEntry[], query: string, recent: string[]): Section[] {
  // The action is what is shown and highlighted; "file new" still finds File › New query.
  const texts = (c: PaletteEntry) => [c.label, `${c.group} ${c.label}`];
  const ranked = rankItems(
    entries.filter((c) => !c.soon),
    query,
    texts,
  );
  const order = new Map(recent.map((id, i) => [id, i]));
  const recentRows = ranked.filter((r) => order.has(r.item.id));
  if (!query.trim()) recentRows.sort((a, b) => order.get(a.item.id)! - order.get(b.item.id)!);

  const groups = new Map<string, Ranked<PaletteEntry>[]>();
  for (const r of ranked) {
    if (order.has(r.item.id)) continue;
    groups.set(r.item.group, [...(groups.get(r.item.group) ?? []), r]);
  }
  const scored: { section: Section; best: number }[] = [];
  if (recentRows.length) {
    scored.push({ section: { title: 'Recent', rows: recentRows.map((r) => commandRow(r, r.item.group)) }, best: recentRows[0]!.score });
  }
  for (const [title, rows] of groups) scored.push({ section: { title, rows: rows.map((r) => commandRow(r)) }, best: rows[0]!.score });
  // Stable: with no query every score is 0 and the order stays Recent, then the menus.
  scored.sort((a, b) => b.best - a.best);

  const sections = scored.map((s) => s.section);
  const soon = rankItems(
    entries.filter((c) => c.soon),
    query,
    texts,
  );
  if (soon.length) sections.push({ title: COMING_SOON, rows: soon.map((r) => commandRow(r)) });

  // At most MAX_ROWS rows in all.
  let left = MAX_ROWS;
  return sections
    .map((s) => {
      const rows = s.rows.slice(0, Math.max(0, left));
      left -= rows.length;
      return { ...s, rows };
    })
    .filter((s) => s.rows.length > 0);
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

/** "Ctrl+Shift+P" as keycaps; a chord ("Ctrl+K Ctrl+W") keeps its space. */
export function Keycaps({ hint }: { hint: string }) {
  return (
    <span className="palette-keys" aria-hidden="true">
      {hint.split(' ').map((chord, c) => (
        <span key={c} className="palette-chord">
          {chord.split(/\+(?!$)/).map((key, k) => (
            <kbd key={k} className="keycap">
              {key}
            </kbd>
          ))}
        </span>
      ))}
    </span>
  );
}

const MODE_NAME: Record<PaletteMode, string> = { commands: 'Commands', objects: 'Objects' };
const other = (mode: PaletteMode): PaletteMode => (mode === 'commands' ? 'objects' : 'commands');

/**
 * Ctrl+Shift+P (commands) / Ctrl+P (objects) overlay: a top-center input over a list. It is
 * not modal: no backdrop, no `aria-modal`, so the window shortcuts keep working around it. The
 * mode is a chip before the input (click it or press Tab); a typed leading `>` still switches to
 * commands, and Backspace in an empty input goes back to objects, as in VS Code.
 */
export function CommandPalette() {
  const open = usePalette((s) => s.open);
  const nonce = usePalette((s) => s.nonce);
  const mode = usePalette((s) => s.mode);
  return open ? <PaletteOverlay key={nonce} initialMode={mode} /> : null;
}

function PaletteOverlay({ initialMode }: { initialMode: PaletteMode }) {
  const hide = usePalette((s) => s.hide);
  const state = useMenuState();
  const [mode, setMode] = useState<PaletteMode>(initialMode);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLInputElement>(null);
  // Where the focus was (usually the editor): it goes back there when the palette closes.
  const previous = useRef<Element | null>(document.activeElement);

  const commandMode = mode === 'commands';
  // Taken once per opening: the loaded objects are read from the stores, never fetched.
  const objects = useMemo(() => collectObjectEntries(), []);
  const recent = useMemo(() => recentCommands(), []);

  const sections: Section[] = commandMode
    ? commandSections(commandList(state), query, recent)
    : [
        {
          rows: searchObjects(objects, query, MAX_ROWS).map(({ item, indices }: { item: ObjectEntry; indices: number[] }) => ({
            id: item.key,
            label: item.name,
            name: `${item.path} › ${item.name}`,
            path: item.path,
            kind: item.objectRef.kind,
            indices,
            run: (ddl: boolean) => void openObjectEntry(item, ddl),
          })),
        },
      ];
  const rows = sections.flatMap((s) => s.rows);
  const selectable = rows.flatMap((r, i) => (r.disabled ? [] : [i]));
  const at = selectable.includes(active) ? active : (selectable[0] ?? -1);

  useEffect(() => {
    input.current?.focus();
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
    document.getElementById(`palette-row-${at}`)?.scrollIntoView?.({ block: 'nearest' });
  }, [at]);

  const close = () => {
    hide();
    if (previous.current instanceof HTMLElement) previous.current.focus();
  };

  const choose = (row: Row | undefined, ddl: boolean) => {
    if (!row || row.disabled) return;
    if (commandMode) rememberCommand(row.id);
    close();
    row.run(ddl);
  };

  const switchMode = (next: PaletteMode) => {
    setMode(next);
    setActive(0);
    input.current?.focus();
  };

  /** The next selectable row from `at`, `dir` steps away (wrapping); "Coming soon" rows are skipped. */
  const step = (dir: 1 | -1) => {
    if (!selectable.length) return;
    const pos = selectable.indexOf(at);
    setActive(selectable[(pos + dir + selectable.length) % selectable.length]!);
  };

  const onKeyDown = (e: ReactKeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      step(e.key === 'ArrowDown' ? 1 : -1);
    } else if (e.key === 'Home' && e.ctrlKey) {
      e.preventDefault();
      setActive(selectable[0] ?? 0);
    } else if (e.key === 'End' && e.ctrlKey) {
      e.preventDefault();
      setActive(selectable.at(-1) ?? 0);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      choose(rows[at], !commandMode && (e.ctrlKey || e.metaKey));
    } else if (e.key === 'Tab') {
      e.preventDefault();
      switchMode(other(mode));
    } else if (e.key === 'Backspace' && commandMode && query === '') {
      e.preventDefault();
      switchMode('objects');
    } else if (e.key === 'Escape') {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  };

  const label = commandMode ? 'Command palette' : 'Go to object';
  let index = -1;
  const renderRow = (row: Row) => {
    const i = ++index;
    return (
      <div
        key={row.id}
        id={`palette-row-${i}`}
        role="option"
        aria-selected={i === at}
        aria-disabled={row.disabled ? true : undefined}
        aria-label={row.name}
        className={`palette-row${i === at ? ' active' : ''}${row.disabled ? ' disabled' : ''}`}
        data-testid="palette-row"
        onMouseMove={() => !row.disabled && i !== at && setActive(i)}
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
        {row.sub && <span className="palette-sub">{row.sub}</span>}
        {row.disabled ? <span className="palette-soon">{COMING_SOON}</span> : row.hint && <Keycaps hint={row.hint} />}
      </div>
    );
  };

  return (
    <div ref={root} className="palette" role="dialog" aria-label={label} data-testid="palette">
      <div className="palette-head">
        <button
          type="button"
          className="palette-mode"
          aria-label={`Mode: ${MODE_NAME[mode]}, switch to ${MODE_NAME[other(mode)]}`}
          title="Switch mode (Tab)"
          // Keep the focus in the input.
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => switchMode(other(mode))}
        >
          {MODE_NAME[mode]}
        </button>
        <input
          ref={input}
          className="palette-input"
          type="text"
          role="combobox"
          aria-label={commandMode ? 'Type a command' : 'Search objects, or type > for commands'}
          aria-expanded="true"
          aria-controls="palette-list"
          aria-activedescendant={at >= 0 ? `palette-row-${at}` : undefined}
          aria-autocomplete="list"
          placeholder={commandMode ? 'Type a command' : 'Go to table, view or routine'}
          spellCheck={false}
          autoComplete="off"
          value={query}
          onChange={(e) => {
            const value = e.target.value;
            if (!commandMode && value.startsWith('>')) {
              setMode('commands');
              setQuery(value.slice(1));
            } else {
              setQuery(value);
            }
            setActive(0);
          }}
          onKeyDown={onKeyDown}
        />
        <span className="palette-tip" aria-hidden="true">
          Tab: {MODE_NAME[other(mode)]}
        </span>
      </div>
      <div id="palette-list" className="palette-list" role="listbox" aria-label={label}>
        {sections.map((s) =>
          s.title ? (
            <div key={s.title} role="group" aria-label={s.title} className="palette-group">
              <div className="palette-section" aria-hidden="true">
                {s.title}
              </div>
              {s.rows.map(renderRow)}
            </div>
          ) : (
            s.rows.map(renderRow)
          ),
        )}
      </div>
      {rows.length === 0 && (
        <div className="palette-empty" role="status">
          {commandMode
            ? 'No matching commands'
            : objects.length === 0
              ? 'No objects loaded yet: expand a connection in the tree first'
              : 'No matching objects'}
        </div>
      )}
      <div className="palette-foot" aria-hidden="true">
        <span>↑↓ Select</span>
        <span>{commandMode ? 'Enter Run' : 'Enter Open · Ctrl+Enter DDL'}</span>
        <span>Esc Close</span>
        <span className="palette-count">{rows.length === 1 ? '1 result' : `${rows.length} results`}</span>
      </div>
    </div>
  );
}
