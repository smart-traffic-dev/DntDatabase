import { useEffect, useState } from 'react';
import { api, toAppError } from '../api';
import type { LogLevel } from '../bindings';
import {
  DEFAULT_TABLE_PAGE_SIZE,
  MAX_TABLE_PAGE_SIZE,
  MIN_TABLE_PAGE_SIZE,
  isValidPageSize,
  useAppSettings,
} from '../store/appSettings';
import { useTheme, type Theme } from '../theme/ThemeProvider';
import { Modal } from './Modal';

const THEMES: { id: Theme; label: string }[] = [
  { id: 'light', label: 'Light' },
  { id: 'dark', label: 'Dark' },
  { id: 'system', label: 'System' },
];

/** "Table data page size": the default rows per page of a table's Data view (stored in app.db). */
function PageSizeField() {
  const stored = useAppSettings((s) => s.tablePageSize);
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    void useAppSettings.getState().loadTablePageSize();
  }, []);

  const value = text ?? (stored === null ? '' : String(stored));
  const commit = async () => {
    if (text === null) return;
    const n = Number(text.trim());
    if (!isValidPageSize(n)) {
      setError(`Enter a whole number from ${MIN_TABLE_PAGE_SIZE} to ${MAX_TABLE_PAGE_SIZE.toLocaleString('en-US')}.`);
      return;
    }
    setError(null);
    if (n === stored) {
      setText(null);
      return;
    }
    try {
      await useAppSettings.getState().setTablePageSize(n);
      setText(null);
      setSaved(true);
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  return (
    <div className="field">
      <label className="field-label" htmlFor="settings-page-size">
        Table data page size
      </label>
      <input
        id="settings-page-size"
        type="number"
        inputMode="numeric"
        min={MIN_TABLE_PAGE_SIZE}
        max={MAX_TABLE_PAGE_SIZE}
        step={1}
        value={value}
        disabled={stored === null}
        aria-invalid={error ? true : undefined}
        aria-describedby="settings-page-size-hint"
        onChange={(e) => {
          setSaved(false);
          setText(e.target.value);
        }}
        onBlur={() => void commit()}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            void commit();
          }
        }}
      />
      <p id="settings-page-size-hint" className="hint">
        Rows per page when a table or view opens on its Data ({DEFAULT_TABLE_PAGE_SIZE} by default). Open tabs keep their own size.
        {saved ? ' Saved.' : ''}
      </p>
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
    </div>
  );
}

/**
 * "Confirm UPDATE/DELETE without WHERE" (stored in app.db, on by default): asks before a
 * statement that changes every row of a table runs on a connection that is not Production.
 * Production connections always ask.
 */
function UnboundedWritesField() {
  const [on, setOn] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api
      .getConfirmUnboundedWrites()
      .then((v) => live && setOn(v))
      .catch((e: unknown) => {
        if (!live) return;
        setOn(true);
        setError(toAppError(e).message);
      });
    return () => {
      live = false;
    };
  }, []);

  const toggle = async (next: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.setConfirmUnboundedWrites(next);
      setOn(next);
    } catch (e) {
      setError(toAppError(e).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="field">
      <label className="check">
        <input
          type="checkbox"
          checked={on ?? true}
          disabled={on === null || busy}
          aria-describedby="settings-unbounded-hint"
          onChange={(e) => void toggle(e.target.checked)}
        />
        <span>Confirm UPDATE/DELETE without WHERE</span>
      </label>
      <p id="settings-unbounded-hint" className="hint">
        Asks before a statement that changes every row of a table (UPDATE or DELETE without WHERE, TRUNCATE) runs on a
        non-Production connection. Production connections always ask.
      </p>
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
    </div>
  );
}

/** App settings: appearance (local), debug logging, the table page size and the unbounded-write confirmation (stored in app.db). */
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
        <PageSizeField />
        <UnboundedWritesField />
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
