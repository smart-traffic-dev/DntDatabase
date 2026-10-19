import { useState } from 'react';
import { api, toAppError } from '../api';
import { Modal } from './Modal';

interface Props {
  host: string;
  port: number;
  fingerprint: string;
  keyOpenssh: string;
  /** The server's key CHANGED: show a warning with this message and no trust button. */
  mismatchMessage?: string;
  /** `true` once the key was stored in known_hosts; `false` when the user cancelled. */
  onDone(trusted: boolean): void;
}

/** Asks the user to trust an unknown SSH host key (trust on first use). */
export function HostKeyDialog({ host, port, fingerprint, keyOpenssh, mismatchMessage, onDone }: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trust = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.trustHostKey(host, port, fingerprint, keyOpenssh);
    } catch (e) {
      setError(toAppError(e).message);
      setBusy(false);
      return;
    }
    onDone(true);
    // The parent may keep this dialog mounted for a follow-up prompt.
    setBusy(false);
  };

  if (mismatchMessage !== undefined) {
    return (
      <Modal title="SSH host key changed" onClose={() => onDone(false)} className="narrow" role="alertdialog">
        <div className="modal-body">
          <div className="result bad" role="alert">
            <strong>Warning: the host key of {host ? `${host}:${port}` : 'the SSH server'} has CHANGED.</strong>
            <p>This could mean someone is intercepting the connection. {mismatchMessage}</p>
            <p>Remove the old key from known_hosts manually to continue.</p>
          </div>
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={() => onDone(false)}>
            Close
          </button>
        </footer>
      </Modal>
    );
  }

  return (
    <Modal title="Unknown SSH host key" onClose={() => !busy && onDone(false)} className="narrow" initialFocus="[data-trust]">
      <div className="modal-body">
        <p>
          The authenticity of <strong>{host}:{port}</strong> cannot be established. Compare this fingerprint with the
          one your server administrator gave you.
        </p>
        <dl className="kv">
          <dt>Host</dt>
          <dd className="mono">
            {host}:{port}
          </dd>
          <dt>Fingerprint</dt>
          <dd className="mono" data-testid="host-fingerprint">
            {fingerprint}
          </dd>
        </dl>
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <button type="button" className="btn" onClick={() => onDone(false)} disabled={busy}>
          Cancel
        </button>
        <button type="button" className="btn primary" data-trust onClick={() => void trust()} disabled={busy}>
          Trust and connect
        </button>
      </footer>
    </Modal>
  );
}
