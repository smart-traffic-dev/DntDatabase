import { useEffect, useRef } from 'react';
import { useDialogs } from '../store/dialogs';

interface Props {
  label: string;
  x: number;
  y: number;
  items: { label: string; run(): void }[];
  onClose(): void;
}

/** "Import connections…" / "Export connections…" entries shared by the sidebar and toolbar menus. */
export const transferItems = () => [
  { label: 'Import connections…', run: () => useDialogs.getState().requestDialog({ kind: 'import' }) },
  { label: 'Export connections…', run: () => useDialogs.getState().requestDialog({ kind: 'export' }) },
];

/** A small popup menu at a fixed position; closes on outside click and Esc. */
export function ActionMenu({ label, x, y, items, onClose }: Props) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>('button')?.focus();
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
      {items.map((item) => (
        <button
          key={item.label}
          type="button"
          role="menuitem"
          onClick={() => {
            onClose();
            item.run();
          }}
        >
          <span>{item.label}</span>
        </button>
      ))}
    </div>
  );
}
