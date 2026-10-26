import { useState, type FormEvent, type KeyboardEvent } from 'react';
import type { SecretsInput } from '../bindings';
import { LockIcon, WarningIcon } from '../components/icons';
import { Modal } from './Modal';
import { Callout } from './parts';
import { PasswordInput } from './PasswordInput';

export type SecretNeed = 'db_password' | 'ssh_password' | 'ssh_key_passphrase';

interface Props {
  connectionName: string;
  needs: SecretNeed[];
  /** Who asks: "app@db.internal:3306 · via bastion.example.com". */
  target?: string | null;
  /** The connection attempt is running: the form is locked and Connect says so. */
  busy?: boolean;
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
export function PasswordPrompt({ connectionName, needs, target, busy = false, error, errorHint, onSubmit, onCancel }: Props) {
  const [values, setValues] = useState<Partial<Record<SecretNeed, string>>>({});
  const [capsLock, setCapsLock] = useState(false);

  const onlyPassphrase = needs.length === 1 && needs[0] === 'ssh_key_passphrase';

  // A key passphrase may legitimately be empty (unencrypted key); the passwords may not.
  const complete = needs.every((n) => n === 'ssh_key_passphrase' || (values[n] ?? '') !== '');

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!complete || busy) return;
    const out: SecretsInput = {};
    for (const n of needs) out[n] = values[n] ? values[n] : null;
    onSubmit(out);
  };

  const watchCaps = (e: KeyboardEvent<HTMLInputElement>) => setCapsLock(e.getModifierState?.('CapsLock') ?? false);

  return (
    <Modal
      title={onlyPassphrase ? `Unlock key for ${connectionName}` : `Password for ${connectionName}`}
      subtitle={target ?? undefined}
      icon={<LockIcon />}
      tone="accent"
      onClose={onCancel}
      className="size-s"
    >
      <form onSubmit={submit}>
        <div className="modal-body flow">
          {error && (
            <Callout tone="danger" role="alert" title={error}>
              <p>{errorHint ?? (onlyPassphrase ? 'Check the passphrase and try again.' : 'Check the password and try again.')}</p>
            </Callout>
          )}
          {needs.map((n) => (
            <div className="field" key={n}>
              <label htmlFor={`pp-${n}`}>{LABELS[n]}</label>
              <PasswordInput
                id={`pp-${n}`}
                label={LABELS[n]}
                autoComplete="off"
                // Read-only, not disabled: a disabled field drops focus out of the dialog.
                readOnly={busy}
                value={values[n] ?? ''}
                onKeyDown={watchCaps}
                onKeyUp={watchCaps}
                onChange={(e) => setValues((v) => ({ ...v, [n]: e.target.value }))}
              />
            </div>
          ))}
          {capsLock && (
            <p className="hint caps-hint" role="status">
              <WarningIcon /> Caps Lock is on
            </p>
          )}
        </div>
        <footer className="modal-foot">
          <span className="spacer" />
          <button type="button" className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={!complete || busy}>
            {busy ? 'Connecting…' : 'Connect'}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
