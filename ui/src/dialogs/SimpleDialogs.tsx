import { useState, type FormEvent } from 'react';
import { toAppError } from '../api';
import { Modal } from './Modal';

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
    if (!name.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await onSubmit(name.trim());
    } catch (err) {
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  return (
    <Modal title={title} onClose={onClose} className="narrow">
      <form onSubmit={(e) => void submit(e)}>
        <div className="modal-body stack">
          <div className="field">
            <label htmlFor="nd-name">{label}</label>
            <input id="nd-name" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={onClose}>
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
  title: string;
  message: string;
  confirmLabel: string;
  onConfirm(): Promise<void>;
  onClose(): void;
}

/** Destructive confirmation (delete connection / group). */
export function ConfirmDialog({ title, message, confirmLabel, onConfirm, onClose }: ConfirmProps) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const confirm = async () => {
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
    } catch (err) {
      setError(toAppError(err).message);
      setBusy(false);
    }
  };

  return (
    <Modal title={title} onClose={onClose} className="narrow" role="alertdialog" initialFocus="[data-cancel]">
      <div className="modal-body stack">
        <p>{message}</p>
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel onClick={onClose}>
          Cancel
        </button>
        <button type="button" className="btn danger" disabled={busy} onClick={() => void confirm()}>
          {confirmLabel}
        </button>
      </footer>
    </Modal>
  );
}
