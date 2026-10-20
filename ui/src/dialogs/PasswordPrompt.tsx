import { useState, type FormEvent } from 'react';
import type { SecretsInput } from '../bindings';
import { Modal } from './Modal';

export type SecretNeed = 'db_password' | 'ssh_password' | 'ssh_key_passphrase';

interface Props {
  connectionName: string;
  needs: SecretNeed[];
  /** Message of the previous failed attempt. */
  error?: string | null;
  onSubmit(secrets: SecretsInput): void;
  onCancel(): void;
}

const LABELS: Record<SecretNeed, string> = {
  db_password: 'Database password',
  ssh_password: 'SSH password',
  ssh_key_passphrase: 'Key passphrase',
};

/** Asks only for the secrets a connection needs. Values live in this component's state only. */
export function PasswordPrompt({ connectionName, needs, error, onSubmit, onCancel }: Props) {
  const [values, setValues] = useState<Partial<Record<SecretNeed, string>>>({});

  // A key passphrase may legitimately be empty (unencrypted key); the passwords may not.
  const complete = needs.every((n) => n === 'ssh_key_passphrase' || (values[n] ?? '') !== '');

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!complete) return;
    const out: SecretsInput = {};
    for (const n of needs) out[n] = values[n] ? values[n] : null;
    onSubmit(out);
  };

  return (
    <Modal title={`Password for ${connectionName}`} onClose={onCancel} className="narrow">
      <form onSubmit={submit}>
        <div className="modal-body stack">
          {needs.map((n) => (
            <div className="field" key={n}>
              <label htmlFor={`pp-${n}`}>{LABELS[n]}</label>
              <input
                id={`pp-${n}`}
                type="password"
                autoComplete="off"
                value={values[n] ?? ''}
                onChange={(e) => setValues((v) => ({ ...v, [n]: e.target.value }))}
              />
            </div>
          ))}
          {error && (
            <p role="alert" className="field-error">
              {error}
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <button type="button" className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={!complete}>
            Connect
          </button>
        </footer>
      </form>
    </Modal>
  );
}
