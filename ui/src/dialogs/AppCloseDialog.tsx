import { useState } from 'react';
import { toAppError } from '../api';
import { Modal } from './Modal';

interface Props {
  /** Titles of the tabs with an open transaction or a running query ("Title (running)"). */
  tabs: string[];
  /** Stops the runs, rolls every transaction back and quits; a rejection keeps the dialog open with the error. */
  onQuit(): Promise<void>;
  onCancel(): void;
}

/** Quitting with open transactions (spec §7.2): Rollback all & quit / Cancel. */
export function AppCloseDialog({ tabs, onQuit, onCancel }: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const quit = async () => {
    setBusy(true);
    setError(null);
    try {
      await onQuit();
    } catch (e) {
      setError(toAppError(e).message);
      setBusy(false);
    }
  };

  return (
    <Modal title="Open transactions" onClose={onCancel} className="narrow" role="alertdialog" initialFocus="[data-cancel]">
      <div className="modal-body stack">
        <p>
          These tabs have an open transaction or a running query. Quitting stops the running queries and rolls
          every transaction back:
        </p>
        <ul>
          {tabs.map((t, i) => (
            <li key={i}>{t}</li>
          ))}
        </ul>
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="btn danger" disabled={busy} onClick={() => void quit()}>
          Rollback all &amp; quit
        </button>
      </footer>
    </Modal>
  );
}
