import { useState } from 'react';
import { toAppError } from '../api';
import { WarningIcon } from '../components/icons';
import { Modal } from './Modal';
import { Callout } from './parts';

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

  const cancel = () => !busy && onCancel();

  return (
    <Modal
      title="Quit with open work?"
      subtitle="Running queries stop and every open transaction is rolled back"
      icon={<WarningIcon />}
      tone="warning"
      onClose={cancel}
      closable={!busy}
      className="size-s"
      role="alertdialog"
      initialFocus="[data-cancel]"
    >
      <div className="modal-body flow">
        <ul className="confirm-details">
          {tabs.map((t, i) => (
            <li key={i}>{t}</li>
          ))}
        </ul>
        {error && (
          <Callout tone="danger" role="alert">
            {error}
          </Callout>
        )}
      </div>
      <footer className="modal-foot">
        <span className="spacer" />
        <button type="button" className="btn" data-cancel disabled={busy} onClick={cancel}>
          Cancel
        </button>
        <button type="button" className="btn danger" disabled={busy} onClick={() => void quit()}>
          {busy ? 'Quitting…' : 'Rollback all & quit'}
        </button>
      </footer>
    </Modal>
  );
}
