import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { open as pickFile } from '@tauri-apps/plugin-dialog';
import { api, isCancelled, isHostKeyMismatch, isHostKeyUnknown, stageLabel, toAppError } from '../api';
import type {
  AppError,
  ConnectionConfig,
  DbKind,
  EnvTag,
  OpenResult,
  ParsedConnString,
  SecretChangeInput,
  SecretsInput,
  SecretsUpdateInput,
  ServerVariant,
  SshAuth,
  SslMode,
  StageTiming,
} from '../bindings';
import { EnvTag as EnvTagBadge } from '../components/EnvTag';
import { EngineLogo } from '../components/EngineLogo';
import { CheckIcon, ChevronDownIcon, ErrorIcon, LockIcon } from '../components/icons';
import { useConnections } from '../store/connections';
import { autoName, DEFAULT_HOST, defaultPort, defaultUser, ENGINES, engineInfo } from './connectionDefaults';
import { HostKeyDialog } from './HostKeyDialog';
import { Modal } from './Modal';
import { PasswordInput } from './PasswordInput';
import { ConfirmDialog } from './SimpleDialogs';

/**
 * One page (single-page redesign): name, engine, server and environment always show; the
 * connection URL, SSH tunnel, SSL / TLS and Advanced are folded rows that open in place.
 */
type Fold = 'url' | 'ssh' | 'ssl' | 'advanced';

/** Where a field lives: on the page itself, or inside a fold (which opens to show its error). */
type SectionId = 'main' | Fold;

const NIL_UUID = '00000000-0000-0000-0000-000000000000';

const PORT_MAX = 65_535;

const SSL_MODES: { id: SslMode; label: string }[] = [
  { id: 'disable', label: 'Disable' },
  { id: 'prefer', label: 'Prefer' },
  { id: 'require', label: 'Require' },
  { id: 'verify_ca', label: 'Verify CA' },
  { id: 'verify_full', label: 'Verify full' },
];

const ENVIRONMENTS: { id: '' | EnvTag; label: string; testid: string }[] = [
  { id: '', label: 'None', testid: 'conn-env-none' },
  { id: 'local', label: 'Local', testid: 'conn-env-local' },
  { id: 'staging', label: 'Staging', testid: 'conn-env-staging' },
  { id: 'production', label: 'Production', testid: 'conn-env-production' },
];

/** Field error paths (the backend's `config` errors and the dialog's own checks) -> the input, its section. */
const FIELDS: Record<string, { section: SectionId; id: string }> = {
  name: { section: 'main', id: 'cd-name' },
  group_id: { section: 'advanced', id: 'cd-group' },
  parent_id: { section: 'advanced', id: 'cd-group' },
  host: { section: 'main', id: 'cd-host' },
  port: { section: 'main', id: 'cd-port' },
  conn_string: { section: 'url', id: 'cd-conn-string' },
  ssh_host: { section: 'ssh', id: 'cd-ssh-host' },
  'ssh.host': { section: 'ssh', id: 'cd-ssh-host' },
  'ssh.port': { section: 'ssh', id: 'cd-ssh-port' },
  'ssh.user': { section: 'ssh', id: 'cd-ssh-user' },
  'ssh.key_path': { section: 'ssh', id: 'cd-ssh-key' },
  passphrase: { section: 'ssh', id: 'cd-ssh-passphrase' },
  'ssl.ca_path': { section: 'ssl', id: 'cd-ssl-ca' },
  'ssl.cert_path': { section: 'ssl', id: 'cd-ssl-cert' },
  'ssl.key_path': { section: 'ssl', id: 'cd-ssl-key' },
  'options.connect_timeout_s': { section: 'advanced', id: 'cd-timeout' },
  'options.ssh_timeout_s': { section: 'advanced', id: 'cd-ssh-timeout' },
};

/** Field path -> the form value it comes from. */
const FIELD_KEYS: Record<string, keyof Form> = {
  name: 'name',
  group_id: 'groupId',
  parent_id: 'groupId',
  host: 'host',
  port: 'port',
  'ssh.host': 'sshHost',
  'ssh.port': 'sshPort',
  'ssh.user': 'sshUser',
  'ssh.key_path': 'sshKeyPath',
  passphrase: 'passphrase',
  'ssl.ca_path': 'sslCa',
  'ssl.cert_path': 'sslCert',
  'ssl.key_path': 'sslKey',
  'options.connect_timeout_s': 'connectTimeout',
  'options.ssh_timeout_s': 'sshTimeout',
};

interface FieldError {
  field: string;
  message: string;
  /** Value of the input when the error arrived. */
  value: string;
}

/** Per-stage timings; stages that took 0 ms are noise and are left out. */
const timingsText = (timings: StageTiming[]) =>
  timings
    .filter((t) => t.ms > 0)
    .map((t) => ` · ${stageLabel(t.stage)} ${t.ms} ms`)
    .join('');

const ENGINE_NAME: Record<ServerVariant, string> = {
  MySql57: 'MySQL',
  MySql8: 'MySQL',
  MariaDb: 'MariaDB',
  Postgres: 'PostgreSQL',
};

interface Form {
  engine: DbKind;
  name: string;
  groupId: string;
  host: string;
  port: string;
  portTouched: boolean;
  username: string;
  password: string;
  passwordTouched: boolean;
  defaultDb: string;
  envTag: '' | EnvTag;
  readOnly: boolean;
  savePassword: boolean;
  sshOn: boolean;
  sshHost: string;
  sshPort: string;
  sshUser: string;
  sshAuth: SshAuth;
  sshKeyPath: string;
  sshPassword: string;
  sshPasswordTouched: boolean;
  passphrase: string;
  passphraseTouched: boolean;
  sslMode: SslMode;
  sslCa: string;
  sslCert: string;
  sslKey: string;
  connectTimeout: string;
  sshTimeout: string;
  appName: string;
  queryTimeout: string;
}

function initialForm(initial: ConnectionConfig | undefined): Form {
  const engine: DbKind = initial?.kind ?? 'mysql';
  return {
    engine,
    name: initial?.name ?? '',
    groupId: initial?.group_id ?? '',
    // A new connection starts filled in with a local server's defaults (localhost, the engine's
    // port and admin user); switching the engine swaps the ones still untouched.
    host: initial?.host ?? DEFAULT_HOST,
    port: String(initial?.port ?? defaultPort(engine)),
    portTouched: !!initial && initial.port !== defaultPort(engine),
    username: initial?.username ?? defaultUser(engine),
    password: '',
    passwordTouched: false,
    defaultDb: initial?.default_db ?? '',
    envTag: initial?.env_tag ?? '',
    readOnly: initial?.read_only ?? false,
    savePassword: (initial?.secret_mode ?? 'saved') === 'saved',
    sshOn: !!initial?.ssh,
    sshHost: initial?.ssh?.host ?? '',
    sshPort: String(initial?.ssh?.port ?? 22),
    sshUser: initial?.ssh?.user ?? '',
    sshAuth: initial?.ssh?.auth ?? 'key',
    sshKeyPath: initial?.ssh?.key_path ?? '',
    sshPassword: '',
    sshPasswordTouched: false,
    passphrase: '',
    passphraseTouched: false,
    sslMode: initial?.ssl?.mode ?? 'prefer',
    sslCa: initial?.ssl?.ca_path ?? '',
    sslCert: initial?.ssl?.cert_path ?? '',
    sslKey: initial?.ssl?.key_path ?? '',
    connectTimeout: String(initial?.options.connect_timeout_s ?? 10),
    sshTimeout: String(initial?.options.ssh_timeout_s ?? 10),
    appName: initial?.options.application_name ?? '',
    queryTimeout: String(initial?.options.query_timeout_s ?? 0),
  };
}

const num = (s: string, fallback: number) => {
  const n = Number(s);
  return s.trim() !== '' && Number.isFinite(n) ? n : fallback;
};
const orNull = (s: string) => (s.trim() === '' ? null : s.trim());

/** Upper bound of the query timeout: one day (the backend field is a `u32` of seconds). */
export const MAX_QUERY_TIMEOUT_S = 86_400;

/** The query timeout field as whole seconds in 0..=MAX_QUERY_TIMEOUT_S; empty or invalid is 0 (off). */
export function queryTimeoutSeconds(s: string): number {
  return Math.min(MAX_QUERY_TIMEOUT_S, Math.max(0, Math.floor(num(s, 0))));
}

/** Why a whole-number field is out of `min..=max`; empty is fine (it means the default). */
function rangeError(s: string, min: number, max: number, what: string): string | null {
  if (s.trim() === '') return null;
  const n = Number(s);
  return Number.isInteger(n) && n >= min && n <= max ? null : `Enter ${what} from ${min} to ${max}.`;
}

/** The dialog's own checks, in form order: the first problem as a field error path and message. */
export function formProblem(f: Pick<Form, 'port' | 'sshOn' | 'sshPort' | 'connectTimeout' | 'sshTimeout'>) {
  const checks: [string, string | null][] = [
    ['port', rangeError(f.port, 1, PORT_MAX, 'a port')],
    ['ssh.port', f.sshOn ? rangeError(f.sshPort, 1, PORT_MAX, 'a port') : null],
    ['options.connect_timeout_s', rangeError(f.connectTimeout, 1, MAX_QUERY_TIMEOUT_S, 'whole seconds')],
    ['options.ssh_timeout_s', rangeError(f.sshTimeout, 1, MAX_QUERY_TIMEOUT_S, 'whole seconds')],
  ];
  const hit = checks.find(([, message]) => message !== null);
  return hit ? { field: hit[0], message: hit[1]! } : null;
}

/** CA / client certificate files matter once the server is verified, or when some are already set. */
const showsSslFiles = (f: Form) =>
  f.sslMode === 'verify_ca' || f.sslMode === 'verify_full' || (f.sslMode !== 'disable' && !!(f.sslCa || f.sslCert || f.sslKey));

const CLOSED: Record<Fold, boolean> = { url: false, ssh: false, ssl: false, advanced: false };

/** The form on another engine: a port and user still at the old engine's default (or empty) follow the new one. */
export function withEngine<F extends Pick<Form, 'engine' | 'port' | 'portTouched' | 'username'>>(f: F, engine: DbKind): F {
  if (engine === f.engine) return f;
  const user = f.username.trim();
  return {
    ...f,
    engine,
    port: f.portTouched && f.port.trim() !== '' ? f.port : String(defaultPort(engine)),
    portTouched: f.portTouched && f.port.trim() !== '',
    username: user === '' || user === defaultUser(f.engine) ? defaultUser(engine) : f.username,
  };
}

type TestState =
  | { state: 'idle' }
  | { state: 'running' }
  | { state: 'ok'; result: OpenResult }
  | { state: 'failed'; error: AppError; mismatch: boolean };

type ParseState =
  | { state: 'idle' }
  | { state: 'parsing' }
  | { state: 'ok'; parsed: ParsedConnString }
  | { state: 'failed'; message: string };

interface HostKeyRequest {
  host: string;
  port: number;
  fingerprint: string;
  keyOpenssh: string;
}

interface Props {
  mode: 'create' | 'edit';
  initial?: ConnectionConfig;
  /** Whether a database / SSH secret is stored (edit mode: shows the `••••••••` placeholder). */
  hasDbPassword?: boolean;
  hasSshSecret?: boolean;
  onClose(saved?: ConnectionConfig): void;
}

export function ConnectionDialog({ mode, initial, hasDbPassword = false, hasSshSecret = false, onClose }: Props) {
  const groups = useConnections((s) => s.groups);
  const [form, setForm] = useState<Form>(() => initialForm(initial));
  const [baseline] = useState(() => JSON.stringify(initialForm(initial)));
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const dirty = JSON.stringify(form) !== baseline;
  const [open, setOpen] = useState<Record<Fold, boolean>>(CLOSED);
  const [rawFieldError, setFieldError] = useState<FieldError | null>(null);
  const [saveError, setSaveError] = useState<AppError | null>(null);
  const [saving, setSaving] = useState(false);
  const [test, setTest] = useState<TestState>({ state: 'idle' });
  const [hostKey, setHostKey] = useState<HostKeyRequest | null>(null);
  const [showDetail, setShowDetail] = useState(false);
  const [connString, setConnString] = useState('');
  const [parse, setParse] = useState<ParseState>({ state: 'idle' });
  const testSeq = useRef(0);
  const parseSeq = useRef(0);
  const testButton = useRef<HTMLButtonElement>(null);
  const detailsButton = useRef<HTMLButtonElement>(null);

  // A field error goes away as soon as its input is edited.
  const fieldError =
    rawFieldError && String(form[FIELD_KEYS[rawFieldError.field] ?? 'name']) === rawFieldError.value
      ? rawFieldError
      : null;

  const set = <K extends keyof Form>(key: K, value: Form[K]) => setForm((f) => ({ ...f, [key]: value }));

  const toggleFold = (id: Fold) => setOpen((o) => ({ ...o, [id]: !o[id] }));

  // Move the cursor to the input an error points at (its fold is opened first).
  useEffect(() => {
    if (!fieldError) return;
    const target = FIELDS[fieldError.field];
    if (target) document.getElementById(target.id)?.focus();
  }, [fieldError]);

  const chooseEngine = (engine: DbKind) => setForm((f) => withEngine(f, engine));

  /** Fills the form from a parsed connection string (values it did not name are left alone). */
  const applyParsed = (p: ParsedConnString) => {
    setForm((prev) => {
      // No scheme (`user@host/db`) keeps the chosen engine; MariaDB shares the MySQL driver.
      const f = p.engine === null ? prev : withEngine(prev, p.engine === 'postgres' ? 'postgres' : 'mysql');
      return {
        ...f,
        host: p.host ?? f.host,
        port: p.port ? String(p.port) : f.port,
        portTouched: p.port ? true : f.portTouched,
        username: p.user ?? f.username,
        password: p.password ?? f.password,
        passwordTouched: p.password !== null ? true : f.passwordTouched,
        defaultDb: p.database ?? f.defaultDb,
        sslMode: p.ssl_mode ?? f.sslMode,
        appName: p.application_name ?? f.appName,
        connectTimeout: p.connect_timeout_s ? String(p.connect_timeout_s) : f.connectTimeout,
      };
    });
  };

  // Parse the pasted string once the user pauses; a stale answer is dropped.
  useEffect(() => {
    const text = connString.trim();
    const seq = ++parseSeq.current;
    if (text === '') {
      setParse({ state: 'idle' });
      return;
    }
    setParse({ state: 'parsing' });
    const timer = setTimeout(() => {
      api
        .parseConnectionString(text)
        .then((parsed) => {
          if (seq !== parseSeq.current) return;
          applyParsed(parsed);
          setParse({ state: 'ok', parsed });
        })
        .catch((e: unknown) => {
          if (seq !== parseSeq.current) return;
          setParse({ state: 'failed', message: toAppError(e).message });
        });
    }, 300);
    return () => clearTimeout(timer);
    // Deliberately keyed on the text only; `applyParsed` fills the other fields.
  }, [connString]);

  /** The values an empty field stands for: host, port, login, and the name derived from them. */
  const host = () => form.host.trim() || DEFAULT_HOST;
  const port = () => num(form.port, defaultPort(form.engine));
  const username = () => form.username.trim() || defaultUser(form.engine);
  const name = () => form.name.trim() || autoName(form.host, form.defaultDb);

  const buildConfig = (): ConnectionConfig => ({
    id: initial?.id ?? NIL_UUID,
    group_id: form.groupId || null,
    name: name(),
    sort_order: initial?.sort_order ?? 0,
    kind: form.engine,
    host: host(),
    port: port(),
    username: username(),
    default_db: orNull(form.defaultDb),
    env_tag: form.envTag === '' ? null : form.envTag,
    read_only: form.readOnly,
    secret_mode: form.savePassword ? 'saved' : 'ask',
    ssh: form.sshOn
      ? {
          version: initial?.ssh?.version ?? 1,
          host: form.sshHost.trim(),
          port: num(form.sshPort, 22),
          user: form.sshUser.trim(),
          auth: form.sshAuth,
          key_path: form.sshAuth === 'key' ? orNull(form.sshKeyPath) : null,
        }
      : null,
    ssl:
      !initial?.ssl && form.sslMode === 'prefer' && !form.sslCa && !form.sslCert && !form.sslKey
        ? null
        : {
            version: initial?.ssl?.version ?? 1,
            mode: form.sslMode,
            ca_path: orNull(form.sslCa),
            cert_path: orNull(form.sslCert),
            key_path: orNull(form.sslKey),
          },
    options: {
      version: initial?.options.version ?? 1,
      connect_timeout_s: num(form.connectTimeout, 10),
      ssh_timeout_s: num(form.sshTimeout, 10),
      charset: initial?.options.charset ?? null,
      application_name: orNull(form.appName),
      // 0 = no timeout; never negative, fractional or beyond MAX_QUERY_TIMEOUT_S.
      query_timeout_s: queryTimeoutSeconds(form.queryTimeout),
    },
  });

  /** Secrets as typed (used by Test, and by Save when creating). */
  const typedSecrets = (): SecretsInput => ({
    db_password: form.password || null,
    ssh_password: form.sshOn && form.sshAuth === 'password' ? form.sshPassword || null : null,
    ssh_key_passphrase: form.sshOn && form.sshAuth === 'key' ? form.passphrase || null : null,
  });

  const createSecrets = (): SecretsInput =>
    form.savePassword ? typedSecrets() : { db_password: null, ssh_password: null, ssh_key_passphrase: null };

  /** Untouched secrets stay as stored; a blanked field clears it. */
  const change = (touched: boolean, value: string): SecretChangeInput => {
    if (touched) return value ? { set: value } : 'clear';
    return 'keep';
  };

  const updateSecrets = (): SecretsUpdateInput => {
    if (!form.savePassword) return { db_password: 'clear', ssh_password: 'clear', ssh_key_passphrase: 'clear' };
    const usesSshPassword = form.sshOn && form.sshAuth === 'password';
    const usesPassphrase = form.sshOn && form.sshAuth === 'key';
    return {
      db_password: change(form.passwordTouched, form.password),
      ssh_password: usesSshPassword ? change(form.sshPasswordTouched, form.sshPassword) : 'clear',
      ssh_key_passphrase: usesPassphrase ? change(form.passphraseTouched, form.passphrase) : 'clear',
    };
  };

  /** Shows `message` under the input of `field` (opening its fold); false for an unknown field. */
  const pointAt = (field: string, message: string): boolean => {
    const target = FIELDS[field];
    if (!target) return false;
    const { section } = target;
    if (section !== 'main') setOpen((o) => ({ ...o, [section]: true }));
    setFieldError({ field, message, value: String(form[FIELD_KEYS[field] ?? 'name']) });
    return true;
  };

  /** A backend `config` error goes under its input; false when it is not one. */
  const showFieldError = (error: AppError): boolean =>
    error.kind === 'config' && !!error.field && pointAt(error.field, error.message);

  /** The dialog's own checks before anything is sent; false (and the field marked) on a problem. */
  const checkForm = (): boolean => {
    const problem = formProblem(form);
    if (problem) pointAt(problem.field, problem.message);
    return !problem;
  };

  const save = async () => {
    setFieldError(null);
    setSaveError(null);
    if (!checkForm()) return;
    setSaving(true);
    try {
      const cfg = buildConfig();
      const saved =
        mode === 'edit'
          ? await api.updateConnection(cfg, updateSecrets())
          : await api.createConnection(cfg, createSecrets());
      onClose(saved);
    } catch (e) {
      const error = toAppError(e);
      if (!showFieldError(error)) setSaveError(error);
      setSaving(false);
    }
  };

  const runTest = async () => {
    setFieldError(null);
    setShowDetail(false);
    if (!checkForm()) return;
    const seq = ++testSeq.current;
    setTest({ state: 'running' });
    try {
      const result = await api.testConnection(buildConfig(), typedSecrets());
      if (seq === testSeq.current) setTest({ state: 'ok', result });
    } catch (e) {
      if (seq !== testSeq.current) return;
      const error = toAppError(e);
      if (isCancelled(error)) {
        setTest({ state: 'idle' });
      } else if (isHostKeyUnknown(error) && error.fingerprint && error.key_openssh) {
        setTest({ state: 'idle' });
        setHostKey({
          host: error.host ?? form.sshHost.trim(),
          port: error.port ?? num(form.sshPort, 22),
          fingerprint: error.fingerprint,
          keyOpenssh: error.key_openssh,
        });
      } else if (showFieldError(error)) {
        setTest({ state: 'idle' });
      } else {
        setTest({ state: 'failed', error, mismatch: isHostKeyMismatch(error) });
      }
    }
  };

  /**
   * Cancel of a running test: the dialog stops waiting and drops its late answer. (There is no
   * backend cancel for a test; the attempt ends at its own connect timeout.)
   */
  const cancelTest = () => {
    testSeq.current++;
    setTest({ state: 'idle' });
    testButton.current?.focus();
  };

  /** Esc, the × and Cancel: ask before throwing away a filled form. */
  const requestClose = () => {
    if (saving) return;
    if (dirty) setConfirmDiscard(true);
    else onClose();
  };

  const browse = async (field: 'sshKeyPath' | 'sslCa' | 'sslCert' | 'sslKey') => {
    try {
      const picked = await pickFile({ multiple: false, directory: false });
      if (typeof picked === 'string') set(field, picked);
    } catch {
      // The picker is unavailable (or cancelled): the path can still be typed.
    }
  };

  const err = (field: string) => (fieldError?.field === field ? fieldError.message : null);
  const aria = (field: string) => ({
    'aria-invalid': err(field) ? true : undefined,
    'aria-describedby': err(field) ? `${FIELDS[field]?.id}-err` : undefined,
  });
  const errorFor = (field: string) => {
    const message = err(field);
    return message ? (
      <p role="alert" id={`${FIELDS[field]?.id}-err`} className="field-error">
        {message}
      </p>
    ) : null;
  };
  /** The fold holding an error says so in its row. */
  const errorSection = fieldError ? FIELDS[fieldError.field]?.section : undefined;
  const info = engineInfo(form.engine);

  /** The one-line state of a folded row, shown at its right end. */
  const summary = (id: Exclude<Fold, 'url'>): string => {
    if (id === 'ssh') {
      if (!form.sshOn) return 'Off';
      const sshHost = form.sshHost.trim();
      if (!sshHost) return 'On';
      const user = form.sshUser.trim();
      return `${user ? `${user}@` : ''}${sshHost}:${num(form.sshPort, 22)}`;
    }
    if (id === 'ssl') return SSL_MODES.find((m) => m.id === form.sslMode)?.label ?? '';
    const query = queryTimeoutSeconds(form.queryTimeout);
    return [
      groups.find((g) => g.id === form.groupId)?.name ?? 'No group',
      `Connect ${num(form.connectTimeout, 10)} s`,
      `Query ${query === 0 ? 'off' : `${query} s`}`,
    ].join(' · ');
  };

  /** A folded row: its name and state; open, it shows its fields right under it. */
  const fold = (id: Exclude<Fold, 'url'>, label: string, body: ReactNode) => {
    const hasError = errorSection === id;
    return (
      <section className={`conn-fold${open[id] ? ' open' : ''}${hasError ? ' has-error' : ''}`}>
        <button
          type="button"
          className="conn-fold-head"
          id={`cd-fold-${id}`}
          data-testid={`conn-section-${id}`}
          aria-expanded={open[id]}
          aria-controls={`cd-fold-${id}-body`}
          onClick={() => toggleFold(id)}
        >
          <ChevronDownIcon />
          <span className="conn-fold-title">{label}</span>
          <span className="conn-fold-hint">{hasError ? 'Error' : summary(id)}</span>
        </button>
        {open[id] && (
          <div id={`cd-fold-${id}-body`} className="conn-fold-body" role="region" aria-labelledby={`cd-fold-${id}`}>
            {body}
          </div>
        )}
      </section>
    );
  };

  const pathInput = (id: string, label: string, field: 'sshKeyPath' | 'sslCa' | 'sslCert' | 'sslKey', errField?: string) => (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <div className="row">
        <input id={id} value={form[field]} onChange={(e) => set(field, e.target.value)} {...(errField ? aria(errField) : {})} />
        <button type="button" className="btn" aria-label={`Browse for ${label.toLowerCase()}`} onClick={() => void browse(field)}>
          Browse…
        </button>
      </div>
      {errField && errorFor(errField)}
    </div>
  );

  /** A checkbox with the project's switch look; the native box stays the control (and the focus ring). */
  const toggle = (checked: boolean, onChange: (v: boolean) => void, text: ReactNode, hint?: ReactNode) => (
    <div className="switch-field">
      <label className="switch-label">
        <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
        <span className="switch-track" aria-hidden="true" />
        <span>{text}</span>
      </label>
      {hint && <p className="hint">{hint}</p>}
    </div>
  );

  const urlField = (
    <div className="field conn-string" id="cd-url">
      <label htmlFor="cd-conn-string">Connection URL</label>
      <input
        id="cd-conn-string"
        data-testid="conn-string"
        value={connString}
        placeholder={form.engine === 'postgres' ? 'postgresql://user:password@host:5432/database' : 'mysql://user:password@host:3306/database'}
        spellCheck={false}
        autoComplete="off"
        // Opened from its link: the cursor goes straight into it.
        autoFocus={!fieldError}
        onChange={(e) => setConnString(e.target.value)}
        // Enter right after a paste would save before the string is parsed.
        onKeyDown={(e) => {
          if (e.key === 'Enter') e.preventDefault();
        }}
        {...aria('conn_string')}
        aria-invalid={parse.state === 'failed' || !!err('conn_string') || undefined}
        aria-describedby={parse.state === 'failed' ? 'cd-conn-string-parse-err' : aria('conn_string')['aria-describedby']}
      />
      {errorFor('conn_string')}
      {parse.state === 'ok' && (
        <div className="parse-ok" role="status" data-testid="parse-result">
          <CheckIcon />
          <span>
            Parsed · {info.label} · {form.host || DEFAULT_HOST}:{port()}
          </span>
          {parse.parsed.ignored.length > 0 && <span className="parse-note">Ignored: {parse.parsed.ignored.join(', ')}</span>}
          {parse.parsed.warnings.map((w) => (
            <span key={w} className="parse-note">
              {w}
            </span>
          ))}
        </div>
      )}
      {parse.state === 'failed' && (
        <p className="field-error" id="cd-conn-string-parse-err" role="alert">
          {parse.message}
        </p>
      )}
    </div>
  );

  const envHelp = [
    form.envTag === 'production' ? 'Asks before statements that change data or structure, and is marked PROD in tabs and the status bar.' : null,
    form.readOnly ? 'Sessions are opened read-only on the server.' : null,
  ]
    .filter(Boolean)
    .join(' ');

  const main = (
    <>
      <div className="field">
        <span className="field-label" id="cd-engine-title">
          Engine
        </span>
        <div className="engine-cards" role="radiogroup" aria-labelledby="cd-engine-title">
          {ENGINES.map((en) => (
            <label key={en.id} className={`engine-card${form.engine === en.id ? ' on' : ''}`}>
              <input
                type="radio"
                name="engine"
                id={`cd-engine-${en.id}`}
                data-testid={`conn-engine-${en.id}`}
                checked={form.engine === en.id}
                onChange={() => chooseEngine(en.id)}
              />
              {/* The marks are decoration: the card is named by its text. */}
              <span className="engine-card-logos">
                {en.logos.map((logo) => (
                  <EngineLogo key={logo} engine={logo} size={28} />
                ))}
              </span>
              <span className="engine-card-text">
                <span className="engine-card-name">{en.label}</span>
                <span className="engine-card-port">Port {en.port}</span>
              </span>
            </label>
          ))}
        </div>
      </div>

      <div className="field">
        <label htmlFor="cd-name">Name</label>
        <input
          id="cd-name"
          data-testid="conn-name"
          value={form.name}
          placeholder={autoName(form.host, form.defaultDb)}
          onChange={(e) => set('name', e.target.value)}
          {...aria('name')}
        />
        {errorFor('name')}
      </div>

      {open.url && urlField}

      <div className="conn-row conn-row-host">
        <div className="field">
          <div className="field-label-row">
            <label htmlFor="cd-host">Host</label>
            <button
              type="button"
              className="link-btn"
              data-testid="conn-url-toggle"
              aria-expanded={open.url}
              aria-controls="cd-url"
              onClick={() => toggleFold('url')}
            >
              {open.url ? 'Hide connection URL' : 'Paste connection URL'}
            </button>
          </div>
          <input
            id="cd-host"
            data-testid="conn-host"
            value={form.host}
            placeholder={DEFAULT_HOST}
            onChange={(e) => set('host', e.target.value)}
            {...aria('host')}
          />
          {errorFor('host')}
        </div>
        <div className="field">
          <label htmlFor="cd-port">Port</label>
          <input
            id="cd-port"
            data-testid="conn-port"
            type="number"
            min={1}
            max={PORT_MAX}
            step={1}
            value={form.port}
            placeholder={String(info.port)}
            onChange={(e) => setForm((f) => ({ ...f, port: e.target.value, portTouched: true }))}
            {...aria('port')}
          />
          {errorFor('port') ??
            (form.port.trim() !== '' && num(form.port, -1) !== info.port ? (
              <p className="hint conn-port-hint" title={`${info.label} default: ${info.port}`}>
                Default: {info.port}
              </p>
            ) : null)}
        </div>
      </div>

      <div className="conn-row">
        <div className="field">
          <label htmlFor="cd-user">User</label>
          <input
            id="cd-user"
            data-testid="conn-user"
            value={form.username}
            placeholder={defaultUser(form.engine)}
            onChange={(e) => set('username', e.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="cd-password">Password</label>
          <PasswordInput
            id="cd-password"
            label="Password"
            data-testid="conn-password"
            autoComplete="new-password"
            value={form.password}
            placeholder={mode === 'edit' && hasDbPassword && !form.passwordTouched ? '••••••••' : undefined}
            onChange={(e) => setForm((f) => ({ ...f, password: e.target.value, passwordTouched: true }))}
          />
        </div>
      </div>

      <div className="conn-row conn-row-db">
        <div className="field">
          <label htmlFor="cd-db">Database</label>
          <input
            id="cd-db"
            value={form.defaultDb}
            placeholder={form.engine === 'postgres' ? defaultUser(form.engine) : 'All databases'}
            onChange={(e) => set('defaultDb', e.target.value)}
          />
        </div>
        <label className="check conn-save-pw" title="Saved in Windows Credential Manager">
          <input
            type="checkbox"
            data-testid="conn-save-password"
            checked={form.savePassword}
            onChange={(e) => set('savePassword', e.target.checked)}
          />
          Save password
        </label>
      </div>
      {!form.savePassword && (
        <p className="hint conn-hint">You'll be asked for the password each time. Test uses what you typed above.</p>
      )}

      <div className="field conn-env">
        <span className="field-label" id="cd-env-title">
          Environment
        </span>
        <div className="conn-env-row">
          <div
            role="radiogroup"
            aria-labelledby="cd-env-title"
            aria-describedby={envHelp ? 'cd-env-help' : undefined}
            className="chip-group"
          >
            {ENVIRONMENTS.map((env) => (
              <label
                key={env.id || 'none'}
                className={`chip-opt${form.envTag === env.id ? ' on' : ''}${env.id ? ` env-${env.id}` : ''}`}
              >
                <input
                  type="radio"
                  name="env"
                  data-testid={env.testid}
                  checked={form.envTag === env.id}
                  onChange={() => set('envTag', env.id)}
                />
                {env.id && <span className="env-dot" aria-hidden="true" />}
                <span>{env.label}</span>
              </label>
            ))}
          </div>
          <label className={`chip-opt conn-readonly${form.readOnly ? ' on' : ''}`} title="Open sessions read-only on the server">
            <input
              type="checkbox"
              data-testid="conn-read-only"
              checked={form.readOnly}
              onChange={(e) => set('readOnly', e.target.checked)}
            />
            <LockIcon />
            <span>Read-only</span>
          </label>
        </div>
        {envHelp && (
          <p id="cd-env-help" className="hint">
            {envHelp}
          </p>
        )}
      </div>
    </>
  );

  const sshBody = (
    <>
      {toggle(form.sshOn, (v) => set('sshOn', v), 'Connect through an SSH tunnel')}
      {/* The tunnel's fields only take room once the tunnel is on. */}
      {form.sshOn && (
        <div className="grid">
          <div className="field">
            <label htmlFor="cd-ssh-host">SSH host</label>
            <input id="cd-ssh-host" value={form.sshHost} onChange={(e) => set('sshHost', e.target.value)} {...aria('ssh.host')} />
            {errorFor('ssh.host')}
          </div>
          <div className="field">
            <label htmlFor="cd-ssh-port">SSH port</label>
            <input
              id="cd-ssh-port"
              type="number"
              min={1}
              max={PORT_MAX}
              step={1}
              value={form.sshPort}
              placeholder="22"
              onChange={(e) => set('sshPort', e.target.value)}
              {...aria('ssh.port')}
            />
            {errorFor('ssh.port')}
          </div>
          <div className="field">
            <label htmlFor="cd-ssh-user">SSH user</label>
            <input id="cd-ssh-user" value={form.sshUser} onChange={(e) => set('sshUser', e.target.value)} {...aria('ssh.user')} />
            {errorFor('ssh.user')}
          </div>
          <div className="field">
            <label htmlFor="cd-ssh-auth">Authentication</label>
            <div className="select-wrap">
              <select id="cd-ssh-auth" value={form.sshAuth} onChange={(e) => set('sshAuth', e.target.value as SshAuth)}>
                <option value="key">Private key</option>
                <option value="password">Password</option>
              </select>
              <ChevronDownIcon />
            </div>
          </div>
          {form.sshAuth === 'password' ? (
            <div className="field span2">
              <label htmlFor="cd-ssh-password">SSH password</label>
              <PasswordInput
                id="cd-ssh-password"
                label="SSH password"
                autoComplete="new-password"
                value={form.sshPassword}
                placeholder={mode === 'edit' && hasSshSecret && !form.sshPasswordTouched ? '••••••••' : undefined}
                onChange={(e) => setForm((f) => ({ ...f, sshPassword: e.target.value, sshPasswordTouched: true }))}
              />
            </div>
          ) : (
            <>
              <div className="span2">{pathInput('cd-ssh-key', 'Key file', 'sshKeyPath', 'ssh.key_path')}</div>
              <div className="field span2">
                <label htmlFor="cd-ssh-passphrase">Key passphrase</label>
                <PasswordInput
                  id="cd-ssh-passphrase"
                  label="Key passphrase"
                  autoComplete="new-password"
                  value={form.passphrase}
                  placeholder={mode === 'edit' && hasSshSecret && !form.passphraseTouched ? '••••••••' : undefined}
                  onChange={(e) => setForm((f) => ({ ...f, passphrase: e.target.value, passphraseTouched: true }))}
                  {...aria('passphrase')}
                />
                {errorFor('passphrase')}
              </div>
            </>
          )}
        </div>
      )}
    </>
  );

  const sslBody = (
    <>
      <div className="field">
        <span className="field-label" id="cd-ssl-title">
          Mode
        </span>
        <div className="seg-group" role="radiogroup" aria-labelledby="cd-ssl-title">
          {SSL_MODES.map((m) => (
            <label key={m.id} className={`segment${form.sslMode === m.id ? ' on' : ''}`}>
              <input
                type="radio"
                name="ssl-mode"
                aria-label={m.label}
                data-testid={`conn-ssl-${m.id}`}
                checked={form.sslMode === m.id}
                onChange={() => set('sslMode', m.id)}
              />
              <span>{m.label}</span>
            </label>
          ))}
        </div>
        {form.sslMode !== 'prefer' && form.sslMode !== 'disable' && (
          <p className="hint">
            {form.sslMode === 'require'
              ? 'Encrypted, but the server certificate is not verified.'
              : 'Needs a CA certificate; a client certificate is optional.'}
          </p>
        )}
      </div>
      {/* An error on one of the files (e.g. an unreadable key) shows them too. */}
      {(showsSslFiles(form) || !!fieldError?.field.startsWith('ssl.')) && (
        <div className="grid">
          <div className="span2">{pathInput('cd-ssl-ca', 'CA certificate', 'sslCa', 'ssl.ca_path')}</div>
          <div className="span2">{pathInput('cd-ssl-cert', 'Client certificate', 'sslCert', 'ssl.cert_path')}</div>
          <div className="span2">{pathInput('cd-ssl-key', 'Client key', 'sslKey', 'ssl.key_path')}</div>
        </div>
      )}
    </>
  );

  const advancedBody = (
    <div className="grid">
      <div className="field">
        <label htmlFor="cd-group">Group</label>
        <div className="select-wrap">
          <select id="cd-group" value={form.groupId} onChange={(e) => set('groupId', e.target.value)} {...aria('group_id')}>
            <option value="">No group</option>
            {groups.map((g) => (
              <option key={g.id} value={g.id}>
                {g.name}
              </option>
            ))}
          </select>
          <ChevronDownIcon />
        </div>
        {errorFor('group_id')}
        {errorFor('parent_id')}
      </div>
      <div className="field">
        <label htmlFor="cd-app">Application name</label>
        <input id="cd-app" value={form.appName} onChange={(e) => set('appName', e.target.value)} />
      </div>
      <div className="field">
        <label htmlFor="cd-timeout">Connect timeout (s)</label>
        <input
          id="cd-timeout"
          type="number"
          min={1}
          step={1}
          value={form.connectTimeout}
          onChange={(e) => set('connectTimeout', e.target.value)}
          {...aria('options.connect_timeout_s')}
        />
        {errorFor('options.connect_timeout_s')}
      </div>
      <div className="field">
        <label htmlFor="cd-ssh-timeout">SSH timeout (s)</label>
        <input
          id="cd-ssh-timeout"
          type="number"
          min={1}
          step={1}
          value={form.sshTimeout}
          onChange={(e) => set('sshTimeout', e.target.value)}
          {...aria('options.ssh_timeout_s')}
        />
        {errorFor('options.ssh_timeout_s')}
      </div>
      <div className="field span2">
        <label htmlFor="cd-query-timeout">Query timeout (s)</label>
        <input
          id="cd-query-timeout"
          data-testid="conn-query-timeout"
          type="number"
          min={0}
          step={1}
          value={form.queryTimeout}
          onChange={(e) => set('queryTimeout', e.target.value)}
        />
        <p className="hint">0 means no timeout. A query that runs longer is stopped. At most {MAX_QUERY_TIMEOUT_S} (one day).</p>
      </div>
    </div>
  );

  const steps = test.state === 'failed' && test.error.timings ? timingsText(test.error.timings).replace(/^ · /, '') : '';
  const hasDetails = test.state === 'failed' && (test.mismatch || !!test.error.detail || steps !== '');

  /** Esc while the details are open closes them (the top-most layer), not the dialog. */
  const onResultKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'Escape' || !showDetail) return;
    e.stopPropagation();
    setShowDetail(false);
    detailsButton.current?.focus();
  };

  // The footer's left side (spec §6): idle (nothing), testing + Cancel, connected, failed + Details; a save error.
  let status: ReactNode = null;
  if (saveError) {
    status = (
      <div className="test-result bad" role="alert">
        <ErrorIcon />
        <div className="test-text">
          <span className="test-line">{saveError.message}</span>
          {saveError.hint && <span className="test-sub">{saveError.hint}</span>}
        </div>
      </div>
    );
  } else if (test.state === 'running') {
    status = (
      <>
        <span className="test-result running" role="status">
          <span className="spinner inline" aria-hidden="true" />
          Testing connection…
        </span>
        <button type="button" className="link" aria-label="Cancel test" onClick={cancelTest}>
          Cancel
        </button>
      </>
    );
  } else if (test.state === 'ok') {
    const text = `Connected to ${ENGINE_NAME[test.result.info.variant]} ${test.result.info.version_string}${timingsText(test.result.timings)}`;
    status = (
      <div className="test-result ok" role="status" data-testid="test-result" title={text}>
        <CheckIcon />
        <span className="test-line">{text}</span>
      </div>
    );
  } else if (test.state === 'failed') {
    status = (
      <div className="test-result bad" role="alert" data-testid="test-result" onKeyDown={onResultKey}>
        <ErrorIcon />
        <div className="test-text">
          {test.mismatch ? (
            <>
              <strong className="test-line">Warning: the SSH host key has CHANGED.</strong>
              <span className="test-sub">Remove the old key from known_hosts manually to continue.</span>
            </>
          ) : (
            <>
              <span className="test-line" title={test.error.message}>
                {test.error.stage && <strong data-testid="test-stage">{stageLabel(test.error.stage)}: </strong>}
                <span>{test.error.message}</span>
              </span>
              {test.error.hint && (
                <span className="test-sub" title={test.error.hint}>
                  {test.error.hint}
                </span>
              )}
            </>
          )}
        </div>
        {hasDetails && (
          <button
            ref={detailsButton}
            type="button"
            className="link"
            aria-expanded={showDetail}
            aria-controls="cd-test-details"
            onClick={() => setShowDetail((v) => !v)}
          >
            Details
          </button>
        )}
        {showDetail && (
          <div id="cd-test-details" className="test-details" role="region" aria-label="Test details">
            {test.mismatch && (
              <p>
                This could mean someone is intercepting the connection. {test.error.message} Remove the old key from
                known_hosts manually to continue.
              </p>
            )}
            {steps !== '' && <p>Steps completed: {steps}</p>}
            {test.error.detail && <pre className="detail">{test.error.detail}</pre>}
          </div>
        )}
      </div>
    );
  }

  return (
    <>
      <Modal
        title={mode === 'edit' ? 'Edit connection' : 'New connection'}
        onClose={requestClose}
        className="conn-dialog"
        initialFocus={mode === 'edit' ? '#cd-host' : `#cd-engine-${form.engine}`}
        badge={form.envTag === 'production' ? <EnvTagBadge env="production" variant="chip" testId="env-chip" /> : undefined}
      >
        <form
          className="conn-form"
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <div className="conn-page" data-testid="conn-panel">
            {main}
            <div className="conn-folds">
              {fold('ssh', 'SSH tunnel', sshBody)}
              {fold('ssl', 'SSL / TLS', sslBody)}
              {fold('advanced', 'Advanced', advancedBody)}
            </div>
          </div>

          <footer className="modal-foot">
            <div className="test-slot">
              <button
                ref={testButton}
                type="button"
                className="btn"
                data-testid="conn-test"
                onClick={() => void runTest()}
                disabled={saving || test.state === 'running'}
              >
                Test
              </button>
              {status}
            </div>
            <button type="button" className="btn" onClick={requestClose} disabled={saving}>
              Cancel
            </button>
            <button type="submit" className="btn primary" data-testid="conn-save" disabled={saving}>
              Save
            </button>
          </footer>
        </form>
      </Modal>
      {confirmDiscard && (
        <ConfirmDialog
          title="Discard changes?"
          message="This connection has unsaved changes. Close it and discard them?"
          confirmLabel="Discard"
          onClose={() => setConfirmDiscard(false)}
          onConfirm={async () => {
            setConfirmDiscard(false);
            onClose();
          }}
        />
      )}
      {hostKey && (
        <HostKeyDialog
          {...hostKey}
          context="test"
          onDone={(trusted) => {
            setHostKey(null);
            if (trusted) void runTest();
          }}
        />
      )}
    </>
  );
}
