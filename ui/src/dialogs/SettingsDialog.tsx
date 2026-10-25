import { useEffect, useState, type KeyboardEvent, type ReactNode } from 'react';
import { api, toAppError } from '../api';
import { commands, type LogLevel } from '../bindings';
import { toggleResults, toggleRightSidebar, zoomIn, zoomOut, zoomReset } from '../components/windowActions';
import { open as pickFolder } from '@tauri-apps/plugin-dialog';
import {
  DEFAULT_IDLE_TX_WARN_MINUTES,
  DEFAULT_JOBS_PER_CONNECTION,
  DEFAULT_TABLE_PAGE_SIZE,
  MAX_IDLE_TX_WARN_MINUTES,
  MAX_JOBS_PER_CONNECTION,
  MAX_TABLE_PAGE_SIZE,
  MIN_IDLE_TX_WARN_MINUTES,
  MIN_JOBS_PER_CONNECTION,
  MIN_TABLE_PAGE_SIZE,
  isValidIdleTxMinutes,
  isValidJobsPerConnection,
  isValidPageSize,
  useAppSettings,
} from '../store/appSettings';
import { FONT_SCALE_DEFAULT, FONT_SCALE_MAX, FONT_SCALE_MIN, resultsOf, useLayout } from '../store/layout';
import type { LayoutPreset } from '../store/layoutTree';
import { useTabs } from '../store/tabs';
import { useTheme, type Density, type Theme } from '../theme/ThemeProvider';
import { Modal } from './Modal';
import { SHORTCUTS } from './shortcutsList';

const THEMES: { id: Theme; label: string }[] = [
  { id: 'light', label: 'Light' },
  { id: 'dark', label: 'Dark' },
  { id: 'system', label: 'System' },
];

const DENSITIES: { id: Density; label: string }[] = [
  { id: 'compact', label: 'Compact' },
  { id: 'comfortable', label: 'Comfortable' },
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

/** "Warn when a transaction stays open for (minutes)": after this long the status bar warns (Manual commit). */
function IdleTxField() {
  const stored = useAppSettings((s) => s.idleTxWarnMinutes);
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    void useAppSettings.getState().loadIdleTxWarnMinutes();
  }, []);

  const value = text ?? String(stored);
  const commit = async () => {
    if (text === null) return;
    const n = Number(text.trim());
    if (text.trim() === '' || !isValidIdleTxMinutes(n)) {
      setError(`Enter a whole number from ${MIN_IDLE_TX_WARN_MINUTES} to ${MAX_IDLE_TX_WARN_MINUTES}.`);
      return;
    }
    setError(null);
    if (n === stored) {
      setText(null);
      return;
    }
    try {
      await useAppSettings.getState().setIdleTxWarnMinutes(n);
      setText(null);
      setSaved(true);
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  return (
    <div className="field">
      <label className="field-label" htmlFor="settings-idle-tx">
        Warn when a transaction stays open for (minutes)
      </label>
      <input
        id="settings-idle-tx"
        type="number"
        inputMode="numeric"
        min={MIN_IDLE_TX_WARN_MINUTES}
        max={MAX_IDLE_TX_WARN_MINUTES}
        step={1}
        value={value}
        aria-invalid={error ? true : undefined}
        aria-describedby="settings-idle-tx-hint"
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
      <p id="settings-idle-tx-hint" className="hint">
        The status bar turns red, and a notice appears once, when a Manual-commit transaction stays open this long (
        {DEFAULT_IDLE_TX_WARN_MINUTES} by default).{saved ? ' Saved.' : ''}
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
 * statement that changes every row of a table runs on a Local or untagged connection.
 * Production and Staging connections always ask.
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
        Asks before a statement that changes every row of a table (UPDATE or DELETE without WHERE, TRUNCATE) runs on
        Local and untagged connections. Production and Staging connections always ask.
      </p>
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
    </div>
  );
}

/** A row of radio chips (theme, density, editor layout). */
function ChoiceGroup<T extends string>({
  label,
  options,
  value,
  onChange,
  columns = 3,
}: {
  label: string;
  options: { id: T; label: string }[];
  value: T;
  onChange(value: T): void;
  columns?: number;
}) {
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      <div className="theme-options" role="radiogroup" aria-label={label} style={{ gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))` }}>
        {options.map((o) => (
          <button key={o.id} type="button" role="radio" aria-checked={value === o.id} className="theme-option" onClick={() => onChange(o.id)}>
            {o.label}
          </button>
        ))}
      </div>
    </div>
  );
}

/** Code font size: the same scale as Ctrl+= / Ctrl+- / Ctrl+0. */
function FontScaleField() {
  const scale = useLayout((s) => s.fontScale);
  const percent = Math.round(scale * 100);
  return (
    <div className="field">
      <span className="field-label" id="settings-font-scale-label">
        Code font size
      </span>
      <div className="settings-stepper" role="group" aria-labelledby="settings-font-scale-label">
        <button type="button" className="btn" aria-label="Smaller" title="Smaller (Ctrl+-)" disabled={scale <= FONT_SCALE_MIN} onClick={zoomOut}>
          −
        </button>
        <output className="settings-stepper-value" aria-live="polite">
          {percent}%
        </output>
        <button type="button" className="btn" aria-label="Bigger" title="Bigger (Ctrl+=)" disabled={scale >= FONT_SCALE_MAX} onClick={zoomIn}>
          +
        </button>
        <button type="button" className="btn" disabled={scale === FONT_SCALE_DEFAULT} onClick={zoomReset}>
          Reset
        </button>
      </div>
      <p className="hint">Editor and result grid text. Ctrl+= / Ctrl+- / Ctrl+0 do the same.</p>
    </div>
  );
}

function AppearancePanel() {
  const { theme, setTheme, density, setDensity } = useTheme();
  return (
    <>
      <ChoiceGroup label="Theme" options={THEMES} value={theme} onChange={setTheme} />
      <ChoiceGroup label="Density" options={DENSITIES} value={density} onChange={setDensity} columns={2} />
      <FontScaleField />
    </>
  );
}

const LAYOUTS: { id: LayoutPreset; label: string }[] = [
  { id: 'single', label: 'Single' },
  { id: 'columns', label: 'Two columns' },
  { id: 'rows', label: 'Two rows' },
  { id: 'grid', label: 'Grid 2 × 2' },
];

function LayoutPanel() {
  const layout = useTabs((s) => s.layout);
  const sidebarVisible = useLayout((s) => s.sidebarVisible);
  const rightVisible = useLayout((s) => s.rightVisible);
  const focusedGroup = useTabs((s) => s.focusedGroupId);
  const resultsVisible = useLayout((s) => resultsOf(s, focusedGroup).visible);
  return (
    <>
      <ChoiceGroup
        label="Editor layout"
        options={LAYOUTS}
        // A custom split (dragged or split by hand) matches no preset: none is checked.
        value={layout as LayoutPreset}
        onChange={(l) => useTabs.getState().setLayout(l)}
        columns={4}
      />
      <p className="hint">Ctrl+K 1 … 4 switch layouts from the keyboard.</p>
      <label className="check">
        <input type="checkbox" checked={sidebarVisible} onChange={() => useLayout.getState().toggleSidebar()} />
        <span>Show sidebar (Ctrl+B)</span>
      </label>
      <label className="check">
        <input type="checkbox" checked={rightVisible} onChange={toggleRightSidebar} />
        <span>Show right sidebar (Ctrl+Alt+B)</span>
      </label>
      <label className="check">
        <input type="checkbox" checked={resultsVisible} onChange={toggleResults} />
        <span>Show results of the focused group (Ctrl+J)</span>
      </label>
      <div className="field">
        <div>
          <button type="button" className="btn" onClick={() => useLayout.getState().reset()}>
            Reset layout
          </button>
        </div>
        <p className="hint">Restores the default sidebar width and editor / result split.</p>
      </div>
    </>
  );
}

/** "Background jobs per connection" (P5 R2): imports, dumps and restores that may run at once on one connection. */
function JobsPerConnectionField() {
  const stored = useAppSettings((s) => s.jobsPerConnection);
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    void useAppSettings.getState().loadJobsPerConnection();
  }, []);

  const value = text ?? String(stored);
  const commit = async () => {
    if (text === null) return;
    const n = Number(text.trim());
    if (text.trim() === '' || !isValidJobsPerConnection(n)) {
      setError(`Enter a whole number from ${MIN_JOBS_PER_CONNECTION} to ${MAX_JOBS_PER_CONNECTION}.`);
      return;
    }
    setError(null);
    if (n === stored) {
      setText(null);
      return;
    }
    try {
      await useAppSettings.getState().setJobsPerConnection(n);
      setText(null);
      setSaved(true);
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  return (
    <div className="field">
      <label className="field-label" htmlFor="settings-jobs-per-connection">
        Background jobs per connection
      </label>
      <input
        id="settings-jobs-per-connection"
        type="number"
        inputMode="numeric"
        min={MIN_JOBS_PER_CONNECTION}
        max={MAX_JOBS_PER_CONNECTION}
        step={1}
        value={value}
        aria-invalid={error ? true : undefined}
        aria-describedby="settings-jobs-per-connection-hint"
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
      <p id="settings-jobs-per-connection-hint" className="hint">
        Imports, dumps and restores that run at the same time on one connection ({DEFAULT_JOBS_PER_CONNECTION} by default);
        more wait in the queue.{saved ? ' Saved.' : ''}
      </p>
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
    </div>
  );
}

/** "External tools folder" (P5 R25): searched first for mysqldump, pg_dump and the other tools; empty = PATH only. */
function ExternalToolsDirField() {
  const stored = useAppSettings((s) => s.externalToolsDir);
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    void useAppSettings.getState().loadExternalToolsDir();
  }, []);

  const value = text ?? stored ?? '';
  const save = async (raw: string) => {
    const dir = raw.trim() === '' ? null : raw.trim();
    setError(null);
    if (dir === stored) {
      setText(null);
      return;
    }
    try {
      await useAppSettings.getState().setExternalToolsDir(dir);
      setText(null);
      setSaved(true);
    } catch (e) {
      setText(raw);
      setError(toAppError(e).message);
    }
  };
  const browse = async () => {
    try {
      const picked = await pickFolder({ directory: true, multiple: false, defaultPath: stored ?? undefined });
      if (typeof picked === 'string') {
        setSaved(false);
        await save(picked);
      }
    } catch {
      // The picker is unavailable (or cancelled): the path can still be typed.
    }
  };

  return (
    <div className="field">
      <label className="field-label" htmlFor="settings-external-tools">
        External tools folder
      </label>
      <div className="settings-path-row">
        <input
          id="settings-external-tools"
          type="text"
          spellCheck={false}
          placeholder="PATH only"
          value={value}
          aria-invalid={error ? true : undefined}
          aria-describedby="settings-external-tools-hint"
          onChange={(e) => {
            setSaved(false);
            setText(e.target.value);
          }}
          onBlur={() => {
            if (text !== null) void save(text);
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && text !== null) {
              e.preventDefault();
              void save(text);
            }
          }}
        />
        <button type="button" className="btn" onClick={() => void browse()}>
          Browse…
        </button>
      </div>
      <p id="settings-external-tools-hint" className="hint">
        Searched first for mysqldump, mariadb-dump, pg_dump, pg_restore and psql; then every folder on PATH. Leave empty to
        use PATH only.{saved ? ' Saved.' : ''}
      </p>
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
    </div>
  );
}

function QueryDataPanel() {
  return (
    <>
      <PageSizeField />
      <UnboundedWritesField />
      <IdleTxField />
      <JobsPerConnectionField />
      <ExternalToolsDirField />
    </>
  );
}

function AdvancedPanel() {
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

  const openLogs = async () => {
    setError(null);
    try {
      await api.openLogFolder();
    } catch (e) {
      setError(toAppError(e).message);
    }
  };

  return (
    <>
      <div className="field">
        <label className="check">
          <input
            type="checkbox"
            checked={level === 'Debug'}
            disabled={level === null || busy}
            aria-describedby="settings-debug-hint"
            onChange={(e) => void toggle(e.target.checked)}
          />
          <span>Debug logging</span>
        </label>
        <p id="settings-debug-hint" className="hint">
          Writes more detail to the log files. Passwords are never logged.
        </p>
      </div>
      <div className="field settings-actions">
        <button type="button" className="btn" onClick={() => void openLogs()}>
          Open log folder
        </button>
        <button type="button" className="btn" onClick={() => void commands.openDevtools().catch(() => {})}>
          Developer tools
        </button>
      </div>
      {error && (
        <p role="alert" className="field-error">
          {error}
        </p>
      )}
    </>
  );
}

function ShortcutsPanel() {
  return (
    <>
      {SHORTCUTS.map((g) => (
        <section key={g.group} className="settings-section">
          <h3 className="field-label">{g.group}</h3>
          <dl className="kv">
            {g.items.map(([action, keys]) => (
              <div key={action} style={{ display: 'contents' }}>
                <dt>{action}</dt>
                <dd>
                  <kbd className="mono">{keys}</kbd>
                </dd>
              </div>
            ))}
          </dl>
        </section>
      ))}
    </>
  );
}

export type SettingsTab = 'appearance' | 'layout' | 'queryData' | 'advanced' | 'shortcuts';

const TABS: { id: SettingsTab; label: string; render(): ReactNode }[] = [
  { id: 'appearance', label: 'Appearance', render: () => <AppearancePanel /> },
  { id: 'layout', label: 'Layout', render: () => <LayoutPanel /> },
  { id: 'queryData', label: 'Query & data', render: () => <QueryDataPanel /> },
  { id: 'advanced', label: 'Advanced', render: () => <AdvancedPanel /> },
  { id: 'shortcuts', label: 'Keyboard shortcuts', render: () => <ShortcutsPanel /> },
];

/**
 * Every app setting in one place, one tab per area: Appearance (theme, density, code font),
 * Layout (editor layout, panels), Query & data (stored in app.db), Advanced (logging, tools)
 * and the keyboard shortcuts reference. Each change applies at once; Close just closes.
 */
export function SettingsDialog({ onClose, initialTab = 'appearance' }: { onClose(): void; initialTab?: SettingsTab }) {
  const [tab, setTab] = useState<SettingsTab>(initialTab);
  const current = TABS.find((t) => t.id === tab) ?? TABS[0]!;

  const onTabKey = (e: KeyboardEvent<HTMLButtonElement>) => {
    const index = TABS.findIndex((t) => t.id === tab);
    let next: number | null = null;
    if (e.key === 'ArrowDown' || e.key === 'ArrowRight') next = (index + 1) % TABS.length;
    else if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') next = (index - 1 + TABS.length) % TABS.length;
    else if (e.key === 'Home') next = 0;
    else if (e.key === 'End') next = TABS.length - 1;
    const target = next === null ? undefined : TABS[next];
    if (!target) return;
    e.preventDefault();
    setTab(target.id);
    document.getElementById(`settings-tab-${target.id}`)?.focus();
  };

  return (
    <Modal title="Settings" onClose={onClose} className="wide settings-dialog" initialFocus={`#settings-tab-${initialTab}`} testId="settings-dialog">
      <div className="conn-body settings-body">
        <div role="tablist" aria-label="Settings sections" aria-orientation="vertical" className="conn-nav">
          {TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              id={`settings-tab-${t.id}`}
              aria-selected={tab === t.id}
              aria-controls="settings-panel"
              tabIndex={tab === t.id ? 0 : -1}
              className={`conn-nav-item${tab === t.id ? ' on' : ''}`}
              onClick={() => setTab(t.id)}
              onKeyDown={onTabKey}
            >
              {t.label}
            </button>
          ))}
        </div>
        <div role="tabpanel" id="settings-panel" aria-labelledby={`settings-tab-${tab}`} className="conn-main panel stack" key={tab}>
          {current.render()}
        </div>
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
