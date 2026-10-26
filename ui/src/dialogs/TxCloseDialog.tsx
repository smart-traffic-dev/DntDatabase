import { useState } from 'react';
import { toAppError } from '../api';
import type { TxAction } from '../bindings';
import { EnvTag } from '../components/EnvTag';
import { HistoryIcon } from '../components/icons';
import { Modal } from './Modal';
import { Callout } from './parts';

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

  const cancel = () => !busy && onDone(false);
  // On Production the safe answer (Rollback) is the primary one; Commit moves to the left.
  const commit = (
    <button type="button" className={`btn${production ? '' : ' primary'}`} disabled={busy} onClick={() => void settle('Commit')}>
      Commit
    </button>
  );
  const rollback = (
    <button type="button" className={`btn${production ? ' primary' : ''}`} disabled={busy} onClick={() => void settle('Rollback')}>
      Rollback
    </button>
  );

  return (
    <Modal
      title={`Close ${title}`}
      subtitle="The transaction is still open"
      icon={<HistoryIcon />}
      tone="warning"
      badge={production ? <EnvTag env="production" variant="chip" testId="env-chip" /> : undefined}
      onClose={cancel}
      closable={!busy}
      className="size-s"
      role="alertdialog"
      initialFocus="[data-cancel]"
    >
      <div className="modal-body flow">
        <p>
          {connectionName ? (
            <>
              This tab has an open transaction on <strong>{connectionName}</strong>. Commit or roll it back before the tab closes.
            </>
          ) : (
            'This tab has an open transaction. Commit or roll it back before the tab closes.'
          )}
        </p>
        {error && (
          <Callout tone="danger" role="alert">
            {error}
          </Callout>
        )}
      </div>
      <footer className="modal-foot">
        {production && commit}
        <span className="spacer" />
        <button type="button" className="btn" data-cancel disabled={busy} onClick={cancel}>
          Cancel
        </button>
        {production ? rollback : <>{rollback}{commit}</>}
      </footer>
    </Modal>
  );
}
