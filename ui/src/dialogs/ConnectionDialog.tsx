import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { open as pickFile } from '@tauri-apps/plugin-dialog';
import { api, isCancelled, isHostKeyMismatch, isHostKeyUnknown, stageLabel, toAppError } from '../api';
import type {
  AppError,
  ConnectionConfig,
  EnvTag,
  OpenResult,
  SecretChangeInput,
  SecretsInput,
  SecretsUpdateInput,
  SshAuth,
  SslMode,
  StageTiming,
} from '../bindings';
import { useConnections } from '../store/connections';
import { HostKeyDialog } from './HostKeyDialog';
import { Modal } from './Modal';

type TabId = 'general' | 'ssh' | 'ssl' | 'advanced';
type Engine = 'mysql' | 'mariadb' | 'postgres';

const NIL_UUID = '00000000-0000-0000-0000-000000000000';
const TABS: { id: TabId; label: string }[] = [
  { id: 'general', label: 'General' },
  { id: 'ssh', label: 'SSH' },
  { id: 'ssl', label: 'SSL' },
  { id: 'advanced', label: 'Advanced' },
];

/** Backend `config` error paths -> the input (and the tab holding it). */
const FIELDS: Record<string, { tab: TabId; id: string }> = {
  name: { tab: 'general', id: 'cd-name' },
  group_id: { tab: 'general', id: 'cd-group' },
  parent_id: { tab: 'general', id: 'cd-group' },
  host: { tab: 'general', id: 'cd-host' },
  port: { tab: 'general', id: 'cd-port' },
  'ssh.host': { tab: 'ssh', id: 'cd-ssh-host' },
  'ssh.user': { tab: 'ssh', id: 'cd-ssh-user' },
  'ssh.key_path': { tab: 'ssh', id: 'cd-ssh-key' },
  passphrase: { tab: 'ssh', id: 'cd-ssh-passphrase' },
  'ssl.ca_path': { tab: 'ssl', id: 'cd-ssl-ca' },
  'ssl.cert_path': { tab: 'ssl', id: 'cd-ssl-cert' },
  'ssl.key_path': { tab: 'ssl', id: 'cd-ssl-key' },
};

/** Backend field path -> the form value it comes from. */
const FIELD_KEYS: Record<string, keyof Form> = {
  name: 'name',
  group_id: 'groupId',
  parent_id: 'groupId',
  host: 'host',
  port: 'port',
  'ssh.host': 'sshHost',
  'ssh.user': 'sshUser',
  'ssh.key_path': 'sshKeyPath',
  passphrase: 'passphrase',
  'ssl.ca_path': 'sslCa',
  'ssl.cert_path': 'sslCert',
  'ssl.key_path': 'sslKey',
};

interface FieldError {
  field: string;
  message: string;
  /** Value of the input when the error arrived. */
  value: string;
}

const timingsText = (timings: StageTiming[]) => timings.map((t) => ` · ${stageLabel(t.stage)} ${t.ms} ms`).join('');

const defaultPort = (engine: Engine) => (engine === 'postgres' ? 5432 : 3306);

interface Form {
  engine: Engine;
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
  const engine: Engine = initial?.kind === 'postgres' ? 'postgres' : 'mysql';
  return {
    engine,
    name: initial?.name ?? '',
    groupId: initial?.group_id ?? '',
    host: initial?.host ?? '',
    port: String(initial?.port ?? defaultPort(engine)),
    portTouched: !!initial && initial.port !== defaultPort(engine),
    username: initial?.username ?? '',
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

type TestState =
  | { state: 'idle' }
  | { state: 'running' }
  | { state: 'ok'; result: OpenResult }
  | { state: 'failed'; error: AppError; mismatch: boolean };

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
  const [tab, setTab] = useState<TabId>('general');
  const [rawFieldError, setFieldError] = useState<FieldError | null>(null);
  const [saveError, setSaveError] = useState<AppError | null>(null);
  const [saving, setSaving] = useState(false);
  const [test, setTest] = useState<TestState>({ state: 'idle' });
  const [hostKey, setHostKey] = useState<HostKeyRequest | null>(null);
  const [showDetail, setShowDetail] = useState(false);
  const testSeq = useRef(0);

  // A field error goes away as soon as its input is edited.
  const fieldError =
    rawFieldError && String(form[FIELD_KEYS[rawFieldError.field] ?? 'name']) === rawFieldError.value
      ? rawFieldError
      : null;

  const set = <K extends keyof Form>(key: K, value: Form[K]) => setForm((f) => ({ ...f, [key]: value }));
  const mariadb = form.engine === 'mariadb';

  // Move the cursor to the input a backend error points at.
  useEffect(() => {
    if (!fieldError) return;
    const target = FIELDS[fieldError.field];
    if (target) document.getElementById(target.id)?.focus();
  }, [fieldError]);

  const chooseEngine = (engine: Engine) =>
    setForm((f) => ({
      ...f,
      engine,
      port: f.portTouched ? f.port : String(defaultPort(engine)),
    }));

  const buildConfig = (): ConnectionConfig => ({
    id: initial?.id ?? NIL_UUID,
    group_id: form.groupId || null,
    name: form.name.trim(),
    sort_order: initial?.sort_order ?? 0,
    kind: form.engine === 'postgres' ? 'postgres' : 'mysql',
    host: form.host.trim(),
    port: num(form.port, defaultPort(form.engine)),
    username: form.username,
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
      // 0 = no timeout; never negative or fractional.
      query_timeout_s: Math.max(0, Math.floor(num(form.queryTimeout, 0))),
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

  /** Shows a `config` error under its input; false when it is not one. */
  const showFieldError = (error: AppError): boolean => {
    const target = error.field ? FIELDS[error.field] : undefined;
    if (error.kind !== 'config' || !error.field || !target) return false;
    setTab(target.tab);
    setFieldError({
      field: error.field,
      message: error.message,
      value: String(form[FIELD_KEYS[error.field] ?? 'name']),
    });
    return true;
  };

  const save = async () => {
    setSaving(true);
    setFieldError(null);
    setSaveError(null);
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
    const seq = ++testSeq.current;
    setTest({ state: 'running' });
    setFieldError(null);
    setShowDetail(false);
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
  /** Tabs holding an error get a marker. */
  const errorTab = fieldError ? FIELDS[fieldError.field]?.tab : undefined;

  const onTabKey = (e: KeyboardEvent<HTMLButtonElement>) => {
    const dir = e.key === 'ArrowRight' ? 1 : e.key === 'ArrowLeft' ? -1 : 0;
    if (!dir) return;
    e.preventDefault();
    const index = TABS.findIndex((t) => t.id === tab);
    const next = TABS[(index + dir + TABS.length) % TABS.length];
    if (next) {
      setTab(next.id);
      document.getElementById(`cd-tab-${next.id}`)?.focus();
    }
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

  const panel: Record<TabId, ReactNode> = {
    general: (
      <div className="grid">
        <div className="field span2">
          <label htmlFor="cd-name">Name</label>
          <input
            id="cd-name"
            data-testid="conn-name"
            value={form.name}
            onChange={(e) => set('name', e.target.value)}
            {...aria('name')}
          />
          {errorFor('name')}
        </div>
        <div className="field span2">
          <label htmlFor="cd-group">Group</label>
          <select id="cd-group" value={form.groupId} onChange={(e) => set('groupId', e.target.value)} {...aria('group_id')}>
            <option value="">No group</option>
            {groups.map((g) => (
              <option key={g.id} value={g.id}>
                {g.name}
              </option>
            ))}
          </select>
          {errorFor('group_id')}
          {errorFor('parent_id')}
        </div>
        <div className="field">
          <label htmlFor="cd-host">Host</label>
          <input
            id="cd-host"
            data-testid="conn-host"
            value={form.host}
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
            value={form.port}
            onChange={(e) => setForm((f) => ({ ...f, port: e.target.value, portTouched: true }))}
            {...aria('port')}
          />
          {errorFor('port')}
        </div>
        <div className="field">
          <label htmlFor="cd-user">User</label>
          <input id="cd-user" data-testid="conn-user" value={form.username} onChange={(e) => set('username', e.target.value)} />
        </div>
        <div className="field">
          <label htmlFor="cd-password">Password</label>
          <input
            id="cd-password"
            data-testid="conn-password"
            type="password"
            autoComplete="new-password"
            value={form.password}
            placeholder={mode === 'edit' && hasDbPassword && !form.passwordTouched ? '••••••••' : undefined}
            onChange={(e) => setForm((f) => ({ ...f, password: e.target.value, passwordTouched: true }))}
          />
        </div>
        <div className="field span2">
          <label className="check">
            <input
              type="checkbox"
              checked={form.savePassword}
              onChange={(e) => set('savePassword', e.target.checked)}
            />
            <span>Save password in Windows Credential Manager</span>
          </label>
          {!form.savePassword && <p className="hint">You will be asked for the password each time. Test uses what you typed above.</p>}
        </div>
        <div className="field">
          <label htmlFor="cd-db">Default database</label>
          <input id="cd-db" value={form.defaultDb} onChange={(e) => set('defaultDb', e.target.value)} />
        </div>
        <div className="field">
          <label htmlFor="cd-env">Environment tag</label>
          <select id="cd-env" value={form.envTag} onChange={(e) => set('envTag', e.target.value as '' | EnvTag)}>
            <option value="">None</option>
            <option value="production">Production</option>
            <option value="staging">Staging</option>
            <option value="local">Local</option>
          </select>
        </div>
        <div className="field span2">
          <label className="check">
            <input type="checkbox" checked={form.readOnly} onChange={(e) => set('readOnly', e.target.checked)} />
            <span>Read-only connection</span>
          </label>
          {form.readOnly && <p className="hint">Sessions are opened read-only on the server.</p>}
        </div>
      </div>
    ),
    ssh: (
      <div className="grid">
        <div className="field span2">
          <label className="check">
            <input type="checkbox" checked={form.sshOn} onChange={(e) => set('sshOn', e.target.checked)} />
            <span>Connect through an SSH tunnel</span>
          </label>
        </div>
        <fieldset className="contents" disabled={!form.sshOn}>
          <div className="field">
            <label htmlFor="cd-ssh-host">SSH host</label>
            <input id="cd-ssh-host" value={form.sshHost} onChange={(e) => set('sshHost', e.target.value)} {...aria('ssh.host')} />
            {errorFor('ssh.host')}
          </div>
          <div className="field">
            <label htmlFor="cd-ssh-port">SSH port</label>
            <input id="cd-ssh-port" type="number" value={form.sshPort} onChange={(e) => set('sshPort', e.target.value)} />
          </div>
          <div className="field">
            <label htmlFor="cd-ssh-user">SSH user</label>
            <input id="cd-ssh-user" value={form.sshUser} onChange={(e) => set('sshUser', e.target.value)} {...aria('ssh.user')} />
            {errorFor('ssh.user')}
          </div>
          <div className="field">
            <label htmlFor="cd-ssh-auth">Authentication</label>
            <select id="cd-ssh-auth" value={form.sshAuth} onChange={(e) => set('sshAuth', e.target.value as SshAuth)}>
              <option value="key">Private key</option>
              <option value="password">Password</option>
            </select>
          </div>
          {form.sshAuth === 'password' ? (
            <div className="field span2">
              <label htmlFor="cd-ssh-password">SSH password</label>
              <input
                id="cd-ssh-password"
                type="password"
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
                <input
                  id="cd-ssh-passphrase"
                  type="password"
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
        </fieldset>
      </div>
    ),
    ssl: (
      <div className="grid">
        <div className="field span2">
          <label htmlFor="cd-ssl-mode">SSL mode</label>
          <select id="cd-ssl-mode" value={form.sslMode} onChange={(e) => set('sslMode', e.target.value as SslMode)}>
            <option value="disable">Disable</option>
            <option value="prefer">Prefer</option>
            <option value="require">Require</option>
            <option value="verify_ca">Verify CA</option>
            <option value="verify_full">Verify full</option>
          </select>
        </div>
        <div className="span2">{pathInput('cd-ssl-ca', 'CA certificate', 'sslCa', 'ssl.ca_path')}</div>
        <div className="span2">{pathInput('cd-ssl-cert', 'Client certificate', 'sslCert', 'ssl.cert_path')}</div>
        <div className="span2">{pathInput('cd-ssl-key', 'Client key', 'sslKey', 'ssl.key_path')}</div>
      </div>
    ),
    advanced: (
      <div className="grid">
        <div className="field">
          <label htmlFor="cd-timeout">Connect timeout (s)</label>
          <input id="cd-timeout" type="number" min={1} value={form.connectTimeout} onChange={(e) => set('connectTimeout', e.target.value)} />
        </div>
        <div className="field">
          <label htmlFor="cd-ssh-timeout">SSH timeout (s)</label>
          <input id="cd-ssh-timeout" type="number" min={1} value={form.sshTimeout} onChange={(e) => set('sshTimeout', e.target.value)} />
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
          <p className="hint">0 means no timeout. A query that runs longer is stopped.</p>
        </div>
        <div className="field span2">
          <label htmlFor="cd-app">Application name</label>
          <input id="cd-app" value={form.appName} onChange={(e) => set('appName', e.target.value)} />
        </div>
      </div>
    ),
  };

  const engines: { id: Engine; label: string; testid?: string }[] = [
    { id: 'mysql', label: 'MySQL', testid: 'conn-engine-mysql' },
    { id: 'mariadb', label: 'MariaDB', testid: 'conn-engine-mariadb' },
    { id: 'postgres', label: 'PostgreSQL', testid: 'conn-engine-postgres' },
  ];

  return (
    <>
      <Modal
        title={mode === 'edit' ? 'Edit connection' : 'New connection'}
        onClose={() => !saving && onClose()}
        className="wide"
        initialFocus="#cd-name"
        badge={
          form.envTag === 'production' ? (
            <span className="chip env-production" data-testid="env-chip">
              Production
            </span>
          ) : undefined
        }
      >
        <form
          className="conn-form"
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <div className="conn-body">
            <fieldset className="engines">
              <legend>Engine</legend>
              {engines.map((en) => (
                <label key={en.id} className={`engine${form.engine === en.id ? ' on' : ''}`}>
                  <input
                    type="radio"
                    name="engine"
                    data-testid={en.testid}
                    checked={form.engine === en.id}
                    onChange={() => chooseEngine(en.id)}
                  />
                  <span>{en.label}</span>
                </label>
              ))}
              {mariadb && <span className="chip chip-ma">MariaDB</span>}
            </fieldset>
            <div className="conn-main">
              <div role="tablist" aria-label="Connection settings" className="tabs">
                {TABS.map((t) => (
                  <button
                    key={t.id}
                    type="button"
                    role="tab"
                    id={`cd-tab-${t.id}`}
                    aria-selected={tab === t.id}
                    aria-controls={`cd-panel-${t.id}`}
                    tabIndex={tab === t.id ? 0 : -1}
                    className={`tab${tab === t.id ? ' on' : ''}${errorTab === t.id ? ' has-error' : ''}`}
                    onClick={() => setTab(t.id)}
                    onKeyDown={onTabKey}
                  >
                    {t.label}
                  </button>
                ))}
              </div>
              <div role="tabpanel" id={`cd-panel-${tab}`} aria-labelledby={`cd-tab-${tab}`} className="panel">
                {panel[tab]}
              </div>
            </div>
          </div>

          {test.state === 'running' && (
            <div className="result" role="status">
              Testing connection…
            </div>
          )}
          {test.state === 'ok' && (
            <div className="result ok" role="status" data-testid="test-result">
              Connected · {test.result.info.version_string}
              {timingsText(test.result.timings)}
            </div>
          )}
          {test.state === 'failed' && (
            <div className="result bad" role="alert" data-testid="test-result">
              {test.mismatch ? (
                <>
                  <strong>Warning: the SSH host key has CHANGED.</strong>
                  <p>
                    This could mean someone is intercepting the connection. {test.error.message} Remove the old key
                    from known_hosts manually to continue.
                  </p>
                </>
              ) : (
                <>
                  <div>
                    {test.error.stage && <strong data-testid="test-stage">{stageLabel(test.error.stage)}: </strong>}
                    <span>{test.error.message}</span>
                  </div>
                  {test.error.hint && <p className="hint-line">{test.error.hint}</p>}
                  {test.error.timings && test.error.timings.length > 0 && (
                    <p className="hint-line">Completed{timingsText(test.error.timings)}</p>
                  )}
                </>
              )}
              {test.error.detail && (
                <>
                  <button
                    type="button"
                    className="link"
                    aria-expanded={showDetail}
                    onClick={() => setShowDetail((v) => !v)}
                  >
                    Details
                  </button>
                  {showDetail && <pre className="detail">{test.error.detail}</pre>}
                </>
              )}
            </div>
          )}
          {saveError && (
            <div className="result bad" role="alert">
              {saveError.message}
            </div>
          )}

          <footer className="modal-foot">
            <button
              type="button"
              className="btn"
              data-testid="conn-test"
              onClick={() => void runTest()}
              disabled={saving || test.state === 'running'}
            >
              Test
            </button>
            <span className="spacer" />
            <button type="button" className="btn" onClick={() => onClose()} disabled={saving}>
              Cancel
            </button>
            <button type="submit" className="btn primary" data-testid="conn-save" disabled={saving}>
              Save
            </button>
          </footer>
        </form>
      </Modal>
      {hostKey && (
        <HostKeyDialog
          {...hostKey}
          onDone={(trusted) => {
            setHostKey(null);
            if (trusted) void runTest();
          }}
        />
      )}
    </>
  );
}
