import { useEffect, useId, useRef, type KeyboardEvent, type ReactNode } from 'react';
import { CloseIcon } from '../components/icons';
import './dialogs.css';

/** The Tab stops of a dialog (an element taken out with `tabindex=-1`, like the header ×, is not one). */
const FOCUSABLE = [
  'a[href]',
  'button:not([disabled])',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]',
]
  .map((s) => `${s}:not([tabindex="-1"])`)
  .join(', ');

interface Props {
  title: ReactNode;
  /** Header content before the title (e.g. the Production chip). */
  badge?: ReactNode;
  onClose(): void;
  children: ReactNode;
  className?: string;
  /** Element to focus first (default: the first focusable control below the header). */
  initialFocus?: string;
  role?: 'dialog' | 'alertdialog';
  testId?: string;
  /** Shows the × in the header that calls `onClose` (default on). */
  closable?: boolean;
}

/**
 * Accessible modal (spec §6): header (badge, title, ×), body, footer; `role=dialog`, focus trap,
 * Esc closes, focus returns to the opener. In an `alertdialog` (destructive) Enter outside a
 * button presses the safe default, the `[data-cancel]` button.
 */
export function Modal({ title, badge, onClose, children, className, initialFocus, role = 'dialog', testId, closable = true }: Props) {
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
    if (e.key === 'Enter' && role === 'alertdialog') {
      const target = e.target as HTMLElement;
      if (target.closest('button, a[href], textarea, select')) return;
      const safe = ref.current?.querySelector<HTMLElement>('[data-cancel]');
      if (safe) {
        e.preventDefault();
        e.stopPropagation();
        safe.click();
      }
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
          {badge}
          <h2 id={titleId}>{title}</h2>
          {closable && (
            // Out of the Tab order: Esc is the keyboard way out, so Tab cycles only the dialog's own controls.
            <button type="button" className="modal-close" aria-label="Close dialog" title="Close (Esc)" tabIndex={-1} onClick={onClose}>
              <CloseIcon />
            </button>
          )}
        </header>
        {children}
      </div>
    </div>
  );
}
