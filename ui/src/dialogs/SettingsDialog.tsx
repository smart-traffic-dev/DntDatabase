import { useEffect, useState } from 'react';
import { api, toAppError } from '../api';
import type { LogLevel } from '../bindings';
import { Modal } from './Modal';

/** App settings. P1 has one: debug logging (stored in app.db, applied at start-up too). */
export function SettingsDialog({ onClose }: { onClose(): void }) {
  const [level, setLevel] = useState<LogLevel | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api
      .getLogLevel()
      .then((l) => live && setLevel(l))
      .catch((e: unknown) => {
        if (!live) return;
        setLevel('Info');
        setError(toAppError(e).message);
      });
    return () => {
      live = false;
    };
  }, []);

  const toggle = async (debug: boolean) => {
    const next: LogLevel = debug ? 'Debug' : 'Info';
    setBusy(true);
    setError(null);
    try {
      await api.setLogLevel(next);
      setLevel(next);
    } catch (e) {
      setError(toAppError(e).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title="Settings" onClose={onClose} className="narrow">
      <div className="modal-body stack">
        <label className="check">
          <input
            type="checkbox"
            checked={level === 'Debug'}
            disabled={level === null || busy}
            onChange={(e) => void toggle(e.target.checked)}
          />
          <span>Debug logging</span>
        </label>
        <p className="hint">Writes more detail to the log files. Passwords are never logged.</p>
        {error && (
          <p role="alert" className="field-error">
            {error}
          </p>
        )}
      </div>
      <footer className="modal-foot">
        <span className="spacer" />
        <button type="button" className="btn" onClick={onClose}>
          Close
        </button>
      </footer>
    </Modal>
  );
}
