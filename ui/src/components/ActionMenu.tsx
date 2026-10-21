import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useDialogs } from '../store/dialogs';

export interface MenuAction {
  label: string;
  /** Shortcut shown on the right. */
  hint?: string;
  danger?: boolean;
  /** Shown but not runnable (skipped by the arrow keys). */
  disabled?: boolean;
  /** A check mark column: `true` ticks the item, `false` leaves it blank (a toggle / current choice). */
  checked?: boolean;
  /** Tooltip: for a disabled item, the reason it is disabled. */
  title?: string;
  testId?: string;
  run(): void;
}

/** A divider between groups of items. */
export interface MenuSeparator {
  separator: true;
}

export type MenuEntry = MenuAction | MenuSeparator;

interface Props {
  label: string;
  x: number;
  y: number;
  items: MenuEntry[];
  onClose(): void;
  /** Move the focus into the menu when it opens (default). A pointer-opened menu bar keeps the editor's focus. */
  autoFocus?: boolean;
  /**
   * What a `checked` item is: an independent toggle (`checkbox`, default) or the current choice
   * of a "pick one" list (`radio`, e.g. the tab strip's list of open tabs).
   */
  checkRole?: 'checkbox' | 'radio';
}

/** "Import connections…" / "Export connections…" entries shared by the sidebar and toolbar menus. */
export const transferItems = () => [
  { label: 'Import connections…', run: () => useDialogs.getState().requestDialog({ kind: 'import' }) },
  { label: 'Export connections…', run: () => useDialogs.getState().requestDialog({ kind: 'export' }) },
];

/** A small popup menu at a fixed position; closes on outside click and Esc. */
export function ActionMenu({ label, x, y, items, onClose, autoFocus = true, checkRole = 'checkbox' }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  // Measured at (0, 0) first, so the box is not squeezed against the viewport edge.
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);

  // Keep the whole menu inside the viewport.
  useLayoutEffect(() => {
    const box = ref.current?.getBoundingClientRect();
    if (!box) return;
    const margin = 4;
    setPos({
      left: Math.max(margin, Math.min(x, window.innerWidth - box.width - margin)),
      top: Math.max(margin, Math.min(y, window.innerHeight - box.height - margin)),
    });
  }, [x, y, items.length]);

  // Hand the focus back to where it was when the menu closes.
  useEffect(() => {
    const previous = document.activeElement;
    return () => {
      if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
    };
  }, []);

  useEffect(() => {
    if (autoFocus) ref.current?.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
    const away = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const key = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    document.addEventListener('mousedown', away);
    document.addEventListener('keydown', key);
    return () => {
      document.removeEventListener('mousedown', away);
      document.removeEventListener('keydown', key);
    };
  }, [onClose, autoFocus]);

  const hasChecks = items.some((i) => 'label' in i && i.checked !== undefined);

  // Roving focus: arrows wrap around, Home / End jump to the ends; disabled items are skipped.
  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    const buttons = [...(ref.current?.querySelectorAll<HTMLButtonElement>('button[role^="menuitem"]:not(:disabled)') ?? [])];
    if (buttons.length === 0) return;
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    let next: number;
    switch (e.key) {
      case 'ArrowDown':
        next = at < 0 ? 0 : (at + 1) % buttons.length;
        break;
      case 'ArrowUp':
        next = at < 0 ? buttons.length - 1 : (at - 1 + buttons.length) % buttons.length;
        break;
      case 'Home':
        next = 0;
        break;
      case 'End':
        next = buttons.length - 1;
        break;
      default:
        return;
    }
    e.preventDefault();
    buttons[next]?.focus();
  };

  return (
    <div ref={ref} role="menu" aria-label={label} className="context-menu" style={pos ?? { left: 0, top: 0 }} onKeyDown={onKeyDown}>
      {items.map((item, i) =>
        'separator' in item ? (
          <div key={`sep-${i}`} role="separator" className="menu-sep" />
        ) : (
          <button
            key={`${i}:${item.label}`}
            type="button"
            role={item.checked === undefined ? 'menuitem' : `menuitem${checkRole}`}
            aria-checked={item.checked}
            disabled={item.disabled}
            title={item.title}
            data-testid={item.testId}
            className={item.danger ? 'danger' : undefined}
            onClick={() => {
              if (item.disabled) return;
              onClose();
              item.run();
            }}
          >
            {hasChecks && (
              <span className="menu-check" aria-hidden="true">
                {item.checked ? '✓' : ''}
              </span>
            )}
            <span className="menu-label">{item.label}</span>
            {item.hint && <kbd>{item.hint}</kbd>}
          </button>
        ),
      )}
    </div>
  );
}
