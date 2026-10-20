import { useEffect, useId, useRef, type KeyboardEvent, type ReactNode } from 'react';
import './dialogs.css';

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

interface Props {
  title: ReactNode;
  /** Extra header content (e.g. the Production chip). */
  badge?: ReactNode;
  onClose(): void;
  children: ReactNode;
  className?: string;
  /** Element to focus first (default: the first focusable control). */
  initialFocus?: string;
  role?: 'dialog' | 'alertdialog';
  testId?: string;
  /** Shows a × button in the header that calls `onClose`. */
  closable?: boolean;
}

/** Accessible modal: `role=dialog`, focus trap, Esc closes, focus returns to the opener. */
export function Modal({ title, badge, onClose, children, className, initialFocus, role = 'dialog', testId, closable = false }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    const root = ref.current;
    const first = initialFocus ? root?.querySelector<HTMLElement>(initialFocus) : null;
    (first ?? root?.querySelector<HTMLElement>(FOCUSABLE) ?? root)?.focus();
    return () => {
      const target =
        opener && opener.isConnected && opener !== document.body
          ? opener
          : document.querySelector<HTMLElement>('[role="treeitem"][aria-selected="true"], [role="tree"]');
      target?.focus();
    };
    // Focus handling happens once, on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      // Only the innermost dialog reacts.
      e.stopPropagation();
      onClose();
      return;
    }
    if (e.key !== 'Tab') return;
    const items = [...(ref.current?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? [])];
    const head = items[0];
    const tail = items[items.length - 1];
    if (!head || !tail) {
      e.preventDefault();
      return;
    }
    if (e.shiftKey && (document.activeElement === head || document.activeElement === ref.current)) {
      e.preventDefault();
      tail.focus();
    } else if (!e.shiftKey && document.activeElement === tail) {
      e.preventDefault();
      head.focus();
    }
  };

  return (
    <div className="modal-backdrop">
      <div
        ref={ref}
        role={role}
        data-testid={testId}
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        className={`modal${className ? ` ${className}` : ''}`}
        onKeyDown={onKeyDown}
      >
        <header className="modal-head">
          <h2 id={titleId}>{title}</h2>
          {badge}
          {closable && (
            <button type="button" className="modal-close" aria-label="Close" onClick={onClose}>
              ×
            </button>
          )}
        </header>
        {children}
      </div>
    </div>
  );
}
