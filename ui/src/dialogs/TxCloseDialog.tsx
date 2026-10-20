import { useState } from 'react';
import { toAppError } from '../api';
import type { TxAction } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { Modal } from './Modal';

interface Props {
  /** Title of the tab being closed. */
  title: string;
  /** Connection the transaction runs on (named in the message when known). */
  connectionName?: string;
  production?: boolean;
  /** Commits or rolls back; a rejection keeps the dialog open with the error. */
  onSettle(action: TxAction): Promise<void>;
  /** `true` after a successful Commit / Rollback, `false` on Cancel. */
  onDone(closed: boolean): void;
}

/** Closing a tab whose session has an open transaction (spec §7.2): Commit / Rollback / Cancel. */
export function TxCloseDialog({ title, connectionName, production = false, onSettle, onDone }: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const settle = async (action: TxAction) => {
    setBusy(true);
    setError(null);
    try {
      await onSettle(action);
    } catch (e) {
      setError(toAppError(e).message);
      setBusy(false);
      return;
    }
    onDone(true);
  };

  return (
    <Modal
      title={`Close ${title}`}
      badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}
      onClose={() => onDone(false)}
      className="narrow"
      role="alertdialog"
      initialFocus="[data-cancel]"
    >
      <div className="modal-body stack">
        <p>
          {connectionName ? (
            <>
              This tab has an open transaction on <strong>{connectionName}</strong>
              {production ? ' (PRODUCTION)' : ''}.
            </>
          ) : (
            'This tab has an open transaction.'
          )}
        </p>
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" data-cancel onClick={() => onDone(false)}>
          Cancel
        </button>
        <button type="button" className="btn" disabled={busy} onClick={() => void settle('Rollback')}>
          Rollback
        </button>
        <button type="button" className="btn primary" disabled={busy} onClick={() => void settle('Commit')}>
          Commit
        </button>
      </footer>
    </Modal>
  );
}
