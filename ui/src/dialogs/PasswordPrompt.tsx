import { useState, type FormEvent } from 'react';
import type { SecretsInput } from '../bindings';
import { Modal } from './Modal';
import { PasswordInput } from './PasswordInput';

export type SecretNeed = 'db_password' | 'ssh_password' | 'ssh_key_passphrase';

interface Props {
  connectionName: string;
  needs: SecretNeed[];
  /** Message of the previous failed attempt. */
  error?: string | null;
  /** Backend hint for that failure; a generic one is shown when absent. */
  errorHint?: string | null;
  onSubmit(secrets: SecretsInput): void;
  onCancel(): void;
}

const LABELS: Record<SecretNeed, string> = {
  db_password: 'Database password',
  ssh_password: 'SSH password',
  ssh_key_passphrase: 'Key passphrase',
};

/** Asks only for the secrets a connection needs. Values live in this component's state only. */
export function PasswordPrompt({ connectionName, needs, error, errorHint, onSubmit, onCancel }: Props) {
  const [values, setValues] = useState<Partial<Record<SecretNeed, string>>>({});

  const onlyPassphrase = needs.length === 1 && needs[0] === 'ssh_key_passphrase';

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
    <Modal
      title={onlyPassphrase ? `Unlock key for ${connectionName}` : `Password for ${connectionName}`}
      onClose={onCancel}
      className="narrow"
    >
      <form onSubmit={submit}>
        <div className="modal-body stack">
          {needs.map((n) => (
            <div className="field" key={n}>
              <label htmlFor={`pp-${n}`}>{LABELS[n]}</label>
              <PasswordInput
                id={`pp-${n}`}
                label={LABELS[n]}
                autoComplete="off"
                value={values[n] ?? ''}
                onChange={(e) => setValues((v) => ({ ...v, [n]: e.target.value }))}
              />
            </div>
          ))}
          {error && (
            <div role="alert" className="field-error">
              <p>{error}</p>
              <p className="hint">
                {errorHint ?? (onlyPassphrase ? 'Check the passphrase and try again.' : 'Check the password and try again.')}
              </p>
            </div>
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
