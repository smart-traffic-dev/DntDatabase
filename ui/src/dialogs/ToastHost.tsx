import { useEffect, useRef, useState, type ReactNode } from 'react';
import { api } from '../api';
import type { AppError } from '../bindings';
import { CheckIcon, CloseIcon, ErrorIcon, InfoIcon, WarningIcon } from '../components/icons';
import { useDialogs } from '../store/dialogs';
import { AUTO_HIDE_MS, MAX_TOASTS, autoHides, useToasts, type ToastAction, type ToastKind } from '../store/toasts';
import './toasts.css';

const ICON: Record<ToastKind, () => ReactNode> = {
  success: CheckIcon,
  info: InfoIcon,
  warning: WarningIcon,
  error: ErrorIcon,
  progress: InfoIcon,
};

interface ViewProps {
  kind: ToastKind;
  title: string;
  message?: string;
  details?: string;
  progress?: number | null;
  actions?: (ToastAction & { keepOpen?: boolean })[];
  /** A change restarts the auto-hide timer. */
  seq: number;
  /** Accessible name of the toast (default: none, the content speaks). */
  label?: string;
  onDismiss(): void;
}

/** "Copy details" of an error toast: the title, the message and the details, one per line. */
function CopyDetails({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(t);
  }, [copied]);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
    } catch {
      // No clipboard access: the text is still on screen.
    }
  };
  return (
    <button type="button" className="toast-link" onClick={() => void copy()}>
      {copied ? 'Copied' : 'Copy details'}
    </button>
  );
}

/** One toast; success / info hide after AUTO_HIDE_MS, paused while hovered or focused. */
function ToastView({ kind, title, message, details, progress, actions = [], seq, label, onDismiss }: ViewProps) {
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const remaining = useRef(AUTO_HIDE_MS);
  const dismiss = useRef(onDismiss);
  dismiss.current = onDismiss;

  // Shown again or updated: a fresh timer.
  useEffect(() => {
    remaining.current = AUTO_HIDE_MS;
  }, [seq]);

  useEffect(() => {
    if (!autoHides(kind) || hovered || focused) return;
    const started = Date.now();
    const t = setTimeout(() => dismiss.current(), Math.max(0, remaining.current));
    return () => {
      clearTimeout(t);
      remaining.current -= Date.now() - started;
    };
  }, [kind, hovered, focused, seq]);

  const Icon = ICON[kind];
  const percent = typeof progress === 'number' ? Math.round(Math.min(1, Math.max(0, progress)) * 100) : null;
  return (
    <div
      className={`toast ${kind}`}
      role={kind === 'error' ? 'alert' : 'status'}
      aria-label={label}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      onFocus={() => setFocused(true)}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocused(false);
      }}
    >
      <span className="toast-icon">
        <Icon />
      </span>
      <div className="toast-body">
        <strong className="toast-title">{title}</strong>
        {kind === 'progress' && (
          <div
            className={`toast-progress${percent === null ? ' indeterminate' : ''}`}
            role="progressbar"
            aria-label={title}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent ?? undefined}
          >
            <span style={percent === null ? undefined : { width: `${percent}%` }} />
          </div>
        )}
        {message && <span className="toast-message">{message}</span>}
        {(actions.length > 0 || kind === 'error') && (
          <span className="toast-actions">
            {actions.map((a) => (
              <button
                key={a.label}
                type="button"
                className="toast-link"
                onClick={() => {
                  a.run();
                  if (!a.keepOpen) onDismiss();
                }}
              >
                {a.label}
              </button>
            ))}
            {kind === 'error' && <CopyDetails text={[title, message, details].filter(Boolean).join('\n')} />}
          </span>
        )}
      </div>
      {kind !== 'progress' && (
        <button type="button" className="toast-dismiss" aria-label="Dismiss" title="Dismiss" onClick={onDismiss}>
          <CloseIcon />
        </button>
      )}
    </div>
  );
}

/** Spec §7: an internal error is a toast that points at the log folder. */
function InternalErrorToast({ error }: { error: AppError }) {
  const [openFailed, setOpenFailed] = useState(false);
  const openLogs = async () => {
    try {
      await api.openLogFolder();
      setOpenFailed(false);
    } catch {
      setOpenFailed(true);
    }
  };
  return (
    <ToastView
      kind="error"
      label="Internal error"
      title={error.message}
      message={openFailed ? 'The log folder could not be opened.' : (error.hint ?? undefined)}
      details={error.detail ?? undefined}
      actions={[{ label: 'Open log folder', keepOpen: true, run: () => void openLogs() }]}
      seq={0}
      onDismiss={() => useDialogs.getState().dismissInternal()}
    />
  );
}

/**
 * The toast stack, bottom right: the internal error (if any) and the newest toasts, at most
 * MAX_TOASTS in all. Notices (`useDialogs.setNotice`) arrive here as toasts too.
 */
export function ToastHost() {
  const toasts = useToasts((s) => s.toasts);
  const internal = useDialogs((s) => s.internalError);
  // The store already keeps the stack within the room the internal error leaves (`reserve`).
  const shown = toasts.slice(Math.max(0, toasts.length - (MAX_TOASTS - (internal ? 1 : 0))));
  if (!internal && shown.length === 0) return null;
  return (
    <div className="toast-stack">
      {internal && <InternalErrorToast error={internal} />}
      {shown.map((t) => (
        <ToastView key={t.id} {...t} onDismiss={() => useToasts.getState().dismiss(t.id)} />
      ))}
    </div>
  );
}
