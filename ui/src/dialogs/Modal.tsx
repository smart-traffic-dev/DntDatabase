import { useEffect, useId, useRef, type KeyboardEvent, type ReactNode } from 'react';
import { CloseIcon } from '../components/icons';
import './dialogs.css';
import './parts.css';

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

export type ModalTone = 'neutral' | 'accent' | 'warning' | 'danger' | 'success';

interface Props {
  title: ReactNode;
  /** Header content before the title (e.g. the Production chip). */
  badge?: ReactNode;
  /** Icon tile before the title; `tone` colours it (danger for destructive actions). */
  icon?: ReactNode;
  tone?: ModalTone;
  /** One line under the title: the consequence of the action, or where it runs. */
  subtitle?: ReactNode;
  /** A strip under the header (`ContextBar`: environment, connection › object). */
  context?: ReactNode;
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
export function Modal({ title, badge, icon, tone = 'neutral', subtitle, context, onClose, children, className, initialFocus, role = 'dialog', testId, closable = true }: Props) {
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

  const closeRef = useRef(onClose);
  useEffect(() => {
    closeRef.current = onClose;
  });
  // A focused control that gets disabled (a busy dialog's Confirm) drops focus to <body>, out of
  // the dialog's own key handling: the next key brings it back to the topmost dialog.
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      const root = ref.current;
      const active = document.activeElement as HTMLElement | null;
      const lost = !active || active === document.body || (root?.contains(active) && active.matches(':disabled'));
      if (!root || !lost) return;
      const modals = document.querySelectorAll('[aria-modal="true"]');
      if (modals[modals.length - 1] !== root) return;
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        root.focus();
        closeRef.current();
      } else if (e.key === 'Tab') {
        e.preventDefault();
        e.stopPropagation();
        (root.querySelector<HTMLElement>(FOCUSABLE) ?? root).focus();
      }
    };
    document.addEventListener('keydown', onKey, true);
    return () => document.removeEventListener('keydown', onKey, true);
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
      // In a text box (a typed confirmation name) Enter does nothing: cancelling there would drop
      // what the user just typed, with nothing run and nothing said.
      if (target.closest('input:not([type="checkbox"], [type="radio"])')) {
        e.preventDefault();
        return;
      }
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
          {icon && (
            <span className={`modal-icon tone-${tone}`} aria-hidden="true">
              {icon}
            </span>
          )}
          {badge}
          {subtitle ? (
            <div className="modal-title">
              <h2 id={titleId}>{title}</h2>
              <p className="modal-sub">{subtitle}</p>
            </div>
          ) : (
            <h2 id={titleId}>{title}</h2>
          )}
          {closable && (
            // Out of the Tab order: Esc is the keyboard way out, so Tab cycles only the dialog's own controls.
            <button type="button" className="modal-close" aria-label="Close dialog" title="Close (Esc)" tabIndex={-1} onClick={onClose}>
              <CloseIcon />
            </button>
          )}
        </header>
        {context}
        {children}
      </div>
    </div>
  );
}
