import { useEffect, useRef } from 'react';

export interface TabMenuItem {
  label: string;
  /** Shortcut shown on the right. */
  hint?: string;
  run(): void;
  disabled?: boolean;
  /** Set for a "pick one" menu: the item becomes a `menuitemradio` with this state. */
  checked?: boolean;
}

/** Popup menu of the tab strip (overflow list, tab context menu); closes on outside click and Esc. */
export function TabMenu({ label, x, y, items, onClose }: { label: string; x: number; y: number; items: TabMenuItem[]; onClose(): void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
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
  }, [onClose]);
  return (
    <div ref={ref} role="menu" aria-label={label} className="context-menu" style={{ left: x, top: y }}>
      {items.map((item, i) => (
        <button
          key={`${i}:${item.label}`}
          type="button"
          role={item.checked === undefined ? 'menuitem' : 'menuitemradio'}
          aria-checked={item.checked}
          disabled={item.disabled}
          onClick={() => {
            onClose();
            item.run();
          }}
        >
          <span>{item.label}</span>
          {item.hint && <kbd aria-hidden="true">{item.hint}</kbd>}
          {item.checked && <span aria-hidden="true">●</span>}
        </button>
      ))}
    </div>
  );
}
