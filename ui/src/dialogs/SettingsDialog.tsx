import { useEffect, useState } from 'react';
import { api, toAppError } from '../api';
import type { LogLevel } from '../bindings';
import { useTheme, type Theme } from '../theme/ThemeProvider';
import { Modal } from './Modal';

const THEMES: { id: Theme; label: string }[] = [
  { id: 'light', label: 'Light' },
  { id: 'dark', label: 'Dark' },
  { id: 'system', label: 'System' },
];

/** App settings: appearance (local), and debug logging (stored in app.db, applied at start-up too). */
export function SettingsDialog({ onClose }: { onClose(): void }) {
  const { theme, setTheme } = useTheme();
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
        <div className="field">
          <span className="field-label">Appearance</span>
          <div className="theme-options" role="radiogroup" aria-label="Appearance">
            {THEMES.map((t) => (
              <button
                key={t.id}
                type="button"
                role="radio"
                aria-checked={theme === t.id}
                className="theme-option"
                onClick={() => setTheme(t.id)}
              >
                {t.label}
              </button>
            ))}
          </div>
        </div>
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
