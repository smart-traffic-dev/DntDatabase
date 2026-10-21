import { useCallback, useEffect, useRef, useState, type MouseEvent } from 'react';
import { ActionMenu } from './ActionMenu';
import { buildMenus } from './appMenus';
import { useMenuState } from './useMenuState';
import './menuBar.css';

/**
 * Which menu is open; `keyboard`: it was opened with the keyboard, so its first item takes the focus.
 * `x` / `y`: the bottom-left corner of its label, measured when it opened (never read during render,
 * where a missing label made the menu flash in the window's top-left corner).
 */
interface Open {
  index: number;
  keyboard: boolean;
  x: number;
  y: number;
}

/** The label with its mnemonic letter underlined. */
function Label({ text, mnemonic }: { text: string; mnemonic: string }) {
  const at = text.indexOf(mnemonic);
  if (at < 0) return <>{text}</>;
  return (
    <>
      {text.slice(0, at)}
      <u>{mnemonic}</u>
      {text.slice(at + 1)}
    </>
  );
}

/**
 * Navicat / VS Code style menu bar for the title bar. Click opens a menu, hovering another
 * label switches while one is open, Left / Right move between menus, Esc closes, and
 * Alt+F / E / V / Q / T / H open File / Edit / View / Query / Tools / Help (not while a
 * modal dialog is open). Opened with the pointer, a menu leaves the editor's focus alone.
 */
export function MenuBar() {
  const state = useMenuState();
  const menus = buildMenus(state);
  const [open, setOpen] = useState<Open | null>(null);
  const buttons = useRef<(HTMLButtonElement | null)[]>([]);
  const dropdown = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setOpen(null), []);
  const count = menus.length;
  const mnemonics = menus.map((m) => m.mnemonic.toLowerCase()).join('');

  /** The menu at `index`, anchored under its label; `null` when the label is not laid out. */
  const openAt = useCallback((index: number, keyboard: boolean): Open | null => {
    const r = buttons.current[index]?.getBoundingClientRect();
    return r ? { index, keyboard, x: r.left, y: r.bottom } : null;
  }, []);

  // Alt+<letter> opens a menu (Alt alone does nothing).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || e.repeat) return;
      if (document.querySelector('[aria-modal="true"]')) return;
      const index = mnemonics.indexOf(e.key.toLowerCase());
      if (e.key.length !== 1 || index < 0) return;
      e.preventDefault();
      e.stopPropagation();
      setOpen(openAt(index, true));
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [mnemonics, openAt]);

  // While a menu is open: Left / Right switch menus; Up / Down / Home / End / Enter reach the menu
  // even when the focus is still in the editor (a pointer-opened menu does not take the focus).
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.altKey || e.ctrlKey || e.metaKey) return;
      const inMenu = dropdown.current?.contains(document.activeElement) ?? false;
      const items = () => [...(dropdown.current?.querySelectorAll<HTMLButtonElement>('button[role^="menuitem"]:not(:disabled)') ?? [])];
      if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
        e.preventDefault();
        e.stopPropagation();
        const step = e.key === 'ArrowRight' ? 1 : -1;
        setOpen((o) => (o ? (openAt((o.index + step + count) % count, true) ?? o) : o));
      } else if (!inMenu && ['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter'].includes(e.key)) {
        e.preventDefault();
        e.stopPropagation();
        const list = items();
        (e.key === 'ArrowUp' || e.key === 'End' ? list[list.length - 1] : list[0])?.focus();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [open, count, openAt]);

  /** Menu buttons never take focus on mousedown (the editor keeps its caret). The mousedown also
   * stays away from the open menu's outside-click handler, so a second click on a label closes it. */
  const onMouseDown = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
  };

  const active = open ? menus[open.index] : undefined;

  return (
    <div className="menubar" role="menubar" aria-label="Application menu">
      {menus.map((menu, i) => {
        const isOpen = open?.index === i;
        return (
          <button
            key={menu.id}
            ref={(el) => {
              buttons.current[i] = el;
            }}
            type="button"
            role="menuitem"
            className={`menubar-btn${isOpen ? ' open' : ''}`}
            aria-haspopup="menu"
            aria-expanded={isOpen}
            tabIndex={-1}
            data-testid={`menu-${menu.id}`}
            onMouseDown={onMouseDown}
            onClick={() => setOpen(isOpen ? null : openAt(i, false))}
            onMouseEnter={() => open && !isOpen && setOpen(openAt(i, false) ?? open)}
          >
            <Label text={menu.label} mnemonic={menu.mnemonic} />
          </button>
        );
      })}
      {open && active && (
        <div ref={dropdown}>
          <ActionMenu
            key={active.id}
            label={active.label}
            x={open.x}
            y={open.y}
            items={active.items}
            onClose={close}
            autoFocus={open?.keyboard ?? false}
          />
        </div>
      )}
    </div>
  );
}
