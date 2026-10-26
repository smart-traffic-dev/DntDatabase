import { useState, type FormEvent, type ReactNode } from 'react';
import { toAppError } from '../api';
import { TrashIcon, WarningIcon } from '../components/icons';
import { Modal } from './Modal';
import { Callout } from './parts';

interface NameProps {
  title: string;
  label: string;
  initial?: string;
  submitLabel: string;
  /** Rejects with the backend error; the dialog stays open and shows it. */
  onSubmit(name: string): Promise<void>;
  onClose(): void;
}

/** One-field dialog: new group / rename group. */
export function NameDialog({ title, label, initial = '', submitLabel, onSubmit, onClose }: NameProps) {
  const [name, setName] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!name.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      await onSubmit(name.trim());
    } catch (err) {
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  const close = () => !busy && onClose();

  return (
    <Modal title={title} onClose={close} closable={!busy} className="size-s">
      <form onSubmit={(e) => void submit(e)}>
        <div className="modal-body stack">
          <div className="field">
            <label htmlFor="nd-name">{label}</label>
            <input id="nd-name" value={name} readOnly={busy} onFocus={(e) => e.currentTarget.select()} onChange={(e) => setName(e.target.value)} />
          </div>
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" disabled={busy} onClick={close}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy || !name.trim()}>
            {submitLabel}
          </button>
        </footer>
      </form>
    </Modal>
  );
}

interface ConfirmProps {
  /** A question: "Delete connection shop-dev?". */
  title: string;
  message?: ReactNode;
  /** What else happens (a list of consequences, the affected tabs). */
  details?: ReactNode;
  confirmLabel: string;
  /** The confirm button while `onConfirm` runs ("Deleting…"). */
  busyLabel?: string;
  /** The label of the button that closes without confirming (default "Cancel"). */
  cancelLabel?: string;
  /** `danger` (default) for destructive answers, `primary` for safe ones. */
  tone?: 'danger' | 'primary';
  /** A third answer on the left ("Don't save"). */
  secondary?: { label: string; onClick(): Promise<void> | void };
  onConfirm(): Promise<void>;
  onClose(): void;
}

/**
 * A question with up to three answers. Cancel has the focus and answers Enter; while the confirmed
 * action runs, Cancel / Esc / × cannot answer it a second time.
 */
export function ConfirmDialog({ title, message, details, confirmLabel, busyLabel, cancelLabel = 'Cancel', tone = 'danger', secondary, onConfirm, onClose }: ConfirmProps) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (fn: () => Promise<void> | void) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (err) {
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  const close = () => !busy && onClose();

  return (
    <Modal
      title={title}
      onClose={close}
      closable={!busy}
      className="size-s"
      role="alertdialog"
      initialFocus="[data-cancel]"
      icon={tone === 'danger' ? <TrashIcon /> : <WarningIcon />}
      tone={tone === 'danger' ? 'danger' : 'warning'}
    >
      <div className="modal-body flow">
        {message && <p>{message}</p>}
        {details}
        {error && (
          <Callout tone="danger" role="alert">
            {error}
          </Callout>
        )}
      </div>
      <footer className="modal-foot">
        {secondary && (
          <button type="button" className="btn" disabled={busy} onClick={() => void run(secondary.onClick)}>
            {secondary.label}
          </button>
        )}
        <span className="spacer" />
        <button type="button" className="btn" data-cancel disabled={busy} onClick={close}>
          {cancelLabel}
        </button>
        <button type="button" className={`btn ${tone}`} disabled={busy} onClick={() => void run(onConfirm)}>
          {busy && busyLabel ? busyLabel : confirmLabel}
        </button>
      </footer>
    </Modal>
  );
}
