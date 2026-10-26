import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  appError,
  backendError,
  callsOf,
  config,
  fail,
  ids,
  mockBackend,
  resetStores,
  serverInfo,
} from '../test/backend';
const sheet = import.meta.glob('./dialogs.css', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;
const css = Object.values(sheet)[0] ?? '';
import { ConnectionDialog, formProblem, MAX_QUERY_TIMEOUT_S, queryTimeoutSeconds, withEngine } from './ConnectionDialog';

beforeEach(resetStores);

type User = ReturnType<typeof userEvent.setup>;
type FoldName = 'ssh' | 'ssl' | 'advanced';

const foldRow = (fold: FoldName) => screen.getByTestId(`conn-section-${fold}`);

/** Opens a folded row (SSH tunnel, SSL / TLS, Advanced); its fields show right under it. */
async function openFold(user: User, fold: FoldName) {
  if (foldRow(fold).getAttribute('aria-expanded') !== 'true') await user.click(foldRow(fold));
  expect(foldRow(fold)).toHaveAttribute('aria-expanded', 'true');
}

/** Picks the engine card. */
const chooseEngine = (user: User, engine: 'mysql' | 'postgres') => user.click(screen.getByTestId(`conn-engine-${engine}`));

/** Shows the connection URL field under Name / Engine. */
const openUrl = (user: User) => user.click(screen.getByTestId('conn-url-toggle'));

const okResult = {
  info: { ...serverInfo('MySql8'), version_string: '8.0.36' },
  timings: [
    { stage: 'connect_db', ms: 12 },
    { stage: 'detect_server', ms: 3 },
  ],
};

describe('ConnectionDialog', () => {
  it('a new connection starts with local defaults; switching engine swaps the untouched ones', async () => {
    mockBackend({});
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    const port = () => screen.getByTestId('conn-port');
    const login = () => screen.getByTestId('conn-user');
    expect(screen.getByTestId('conn-host')).toHaveValue('localhost');
    expect(port()).toHaveValue(3306);
    expect(login()).toHaveValue('root');
    // At its default, the port needs no note.
    expect(screen.queryByText(/^Default:/)).toBeNull();

    await chooseEngine(user, 'postgres');
    expect(port()).toHaveValue(5432);
    expect(login()).toHaveValue('postgres');

    // An edited port and user are kept, and a short note names the default the port differs from.
    await chooseEngine(user, 'mysql');
    expect(port()).toHaveValue(3306);
    await user.clear(port());
    await user.type(port(), '3307');
    await user.clear(login());
    await user.type(login(), 'app');
    await chooseEngine(user, 'postgres');
    expect(port()).toHaveValue(3307);
    expect(login()).toHaveValue('app');
    expect(screen.getByText('Default: 5432')).toHaveAttribute('title', 'PostgreSQL default: 5432');
  });

  it('withEngine swaps only the values still at the old default', () => {
    const f = { engine: 'mysql' as const, port: '3306', portTouched: false, username: 'root' };
    expect(withEngine(f, 'postgres')).toMatchObject({ engine: 'postgres', port: '5432', username: 'postgres' });
    expect(withEngine({ ...f, port: '3307', portTouched: true, username: 'app' }, 'postgres')).toMatchObject({ port: '3307', username: 'app' });
    // A cleared port or user takes the new engine's default.
    expect(withEngine({ ...f, port: '', portTouched: true, username: '' }, 'postgres')).toMatchObject({ port: '5432', username: 'postgres' });
    expect(withEngine(f, 'mysql')).toBe(f);
  });

  it('one page: Engine cards first with their marks, then Name; a new connection starts on Engine', () => {
    mockBackend({});
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    // No rail and no steps: every main field is on the page.
    expect(screen.queryByRole('tablist')).toBeNull();
    for (const id of ['conn-name', 'conn-host', 'conn-port', 'conn-user', 'conn-password']) {
      expect(screen.getByTestId(id)).toBeVisible();
    }
    const engine = screen.getByRole('radiogroup', { name: 'Engine' });
    expect(within(engine).getAllByRole('radio')).toHaveLength(2);
    // MySQL and MariaDB share one driver, so one card carries both marks, visible beside the name.
    const mysql = screen.getByTestId('conn-engine-mysql').closest('label')!;
    expect(mysql).toHaveTextContent('MySQL / MariaDBPort 3306');
    expect(mysql.querySelectorAll('.engine-logo svg path')).toHaveLength(2);
    expect(screen.getByTestId('conn-engine-postgres').closest('label')).toHaveTextContent('PostgreSQLPort 5432');
    expect(screen.getByTestId('conn-engine-mysql')).toBeChecked();
    // The engine comes before the name in the page order.
    expect(engine.compareDocumentPosition(screen.getByTestId('conn-name')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.getByTestId('conn-engine-mysql')).toHaveFocus();
  });

  it('folded rows show their state, open in place, and Save is always there', async () => {
    mockBackend({});
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    for (const f of ['ssh', 'ssl', 'advanced'] as const) expect(foldRow(f)).toHaveAttribute('aria-expanded', 'false');
    expect(foldRow('ssh')).toHaveTextContent('SSH tunnelOff');
    expect(foldRow('ssl')).toHaveTextContent('SSL / TLSPrefer');
    expect(foldRow('advanced')).toHaveTextContent('AdvancedNo group · Connect 10 s · Query off');
    expect(screen.getByTestId('conn-save')).toHaveTextContent('Save');

    await openFold(user, 'ssh');
    await user.click(screen.getByLabelText('Connect through an SSH tunnel'));
    expect(foldRow('ssh')).toHaveTextContent('On');
    await user.type(screen.getByLabelText('SSH host'), 'bastion');
    await user.type(screen.getByLabelText('SSH user'), 'ops');
    expect(foldRow('ssh')).toHaveTextContent('ops@bastion:22');
    await openFold(user, 'ssl');
    await user.click(screen.getByTestId('conn-ssl-require'));
    expect(foldRow('ssl')).toHaveTextContent('Require');
    // A second click folds the row again.
    await user.click(foldRow('ssh'));
    expect(foldRow('ssh')).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByLabelText('SSH host')).toBeNull();
  });

  it('Edit opens with the host focused and the name on the page', () => {
    const stored = config(ids.prod, 'shop');
    mockBackend({});
    render(<ConnectionDialog mode="edit" initial={stored.config} onClose={vi.fn()} />);
    expect(screen.getByTestId('conn-host')).toHaveFocus();
    expect(screen.getByTestId('conn-name')).toHaveValue(stored.config.name);
  });

  it('a quick local connection saves straight from Connection', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.type(screen.getByTestId('conn-password'), 'pw');
    // Enter in a field submits the form: that is Save.
    await user.keyboard('{Enter}');
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('create_connection')[0]).toMatchObject({
      cfg: { kind: 'mysql', host: 'localhost', port: 3306, name: 'localhost' },
      secrets: { db_password: 'pw' },
    });
  });

  it('an out-of-range port is refused before anything is sent', async () => {
    mockBackend({ create_connection: (a) => a.cfg, test_connection: () => okResult });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.type(screen.getByTestId('conn-port'), '70000');
    await user.click(screen.getByTestId('conn-save'));
    // Save goes back to the field holding the problem.
    expect(await screen.findByRole('alert')).toHaveTextContent('Enter a port from 1 to 65535.');
    await waitFor(() => expect(screen.getByTestId('conn-port')).toHaveFocus());
    await user.click(screen.getByTestId('conn-test'));
    expect(callsOf('create_connection')).toHaveLength(0);
    expect(callsOf('test_connection')).toHaveLength(0);
  });

  it('formProblem checks ports and timeouts in form order', () => {
    const ok = { port: '', sshOn: false, sshPort: '22', connectTimeout: '10', sshTimeout: '10' };
    expect(formProblem(ok)).toBeNull();
    expect(formProblem({ ...ok, port: '5432' })).toBeNull();
    for (const port of ['0', '-1', '65536', '3306.5', '1e9']) {
      expect(formProblem({ ...ok, port })).toMatchObject({ field: 'port' });
    }
    // The SSH port only counts while the tunnel is on.
    expect(formProblem({ ...ok, sshPort: '0' })).toBeNull();
    expect(formProblem({ ...ok, sshOn: true, sshPort: '0' })).toMatchObject({ field: 'ssh.port' });
    expect(formProblem({ ...ok, connectTimeout: '0' })).toMatchObject({ field: 'options.connect_timeout_s' });
    expect(formProblem({ ...ok, port: '0', connectTimeout: '0' })).toMatchObject({ field: 'port' });
  });

  it('a bad timeout opens Advanced again, marks the row and points at the field', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await openFold(user, 'advanced');
    await user.clear(screen.getByLabelText('Connect timeout (s)'));
    await user.type(screen.getByLabelText('Connect timeout (s)'), '0');
    await user.click(foldRow('advanced'));
    expect(foldRow('advanced')).toHaveAttribute('aria-expanded', 'false');
    await user.click(screen.getByTestId('conn-save'));
    expect(await screen.findByRole('alert')).toHaveTextContent('Enter whole seconds from 1 to 86400.');
    expect(foldRow('advanced')).toHaveAttribute('aria-expanded', 'true');
    expect(foldRow('advanced')).toHaveTextContent('AdvancedError');
    await waitFor(() => expect(screen.getByLabelText('Connect timeout (s)')).toHaveFocus());
    expect(callsOf('create_connection')).toHaveLength(0);
  });

  it('SSH fields appear only with the tunnel on; certificate files only when they matter', async () => {
    mockBackend({});
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await openFold(user, 'ssh');
    expect(screen.queryByLabelText('SSH host')).toBeNull();
    await user.click(screen.getByLabelText('Connect through an SSH tunnel'));
    expect(screen.getByLabelText('SSH host')).toBeEnabled();

    await openFold(user, 'ssl');
    expect(screen.queryByLabelText('CA certificate')).toBeNull();
    await user.click(screen.getByTestId('conn-ssl-require'));
    expect(screen.queryByLabelText('CA certificate')).toBeNull();
    await user.click(screen.getByTestId('conn-ssl-verify_ca'));
    expect(screen.getByLabelText('CA certificate')).toBeInTheDocument();
    expect(screen.getByLabelText('Client key')).toBeInTheDocument();
  });

  it('a stored certificate stays visible under Require', async () => {
    const stored = config(ids.prod, 'shop').config;
    mockBackend({});
    const user = userEvent.setup();
    render(
      <ConnectionDialog
        mode="edit"
        initial={{ ...stored, ssl: { version: 1, mode: 'require', ca_path: null, cert_path: 'c.pem', key_path: 'k.pem' } }}
        onClose={vi.fn()}
      />,
    );
    await openFold(user, 'ssl');
    expect(screen.getByLabelText('Client certificate')).toHaveValue('c.pem');
  });

  it('environment is a radio chip group; Production explains what it changes and is saved', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.type(screen.getByTestId('conn-host'), 'h');
    const group = screen.getByRole('radiogroup', { name: 'Environment' });
    expect(within(group).getAllByRole('radio').map((r) => r.closest('label')?.textContent)).toEqual([
      'None',
      'Local',
      'Staging',
      'Production',
    ]);
    expect(screen.getByTestId('conn-env-none')).toBeChecked();
    expect(screen.queryByTestId('env-chip')).toBeNull();
    // The explanation only shows once it applies.
    expect(group).not.toHaveAttribute('aria-describedby');
    await user.click(screen.getByTestId('conn-env-production'));
    expect(group).toHaveAccessibleDescription(/Asks before statements that change data or structure/);
    expect(screen.getByTestId('env-chip')).toHaveTextContent('Production');
    await user.type(screen.getByTestId('conn-name'), 'p');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('create_connection')[0]).toMatchObject({ cfg: { env_tag: 'production' } });
  });

  it('a running test shows Testing with Cancel; a cancelled test drops its late result', async () => {
    let finish: (v: unknown) => void = () => {};
    mockBackend({ test_connection: () => new Promise((resolve) => (finish = resolve)) });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByTestId('conn-test'));
    const foot = screen.getByRole('dialog').querySelector('.modal-foot') as HTMLElement;
    expect(within(foot).getByRole('status')).toHaveTextContent('Testing connection…');
    expect(screen.getByTestId('conn-test')).toBeDisabled();
    await user.click(within(foot).getByRole('button', { name: 'Cancel test' }));
    expect(within(foot).queryByRole('status')).toBeNull();
    expect(screen.getByTestId('conn-test')).toBeEnabled();
    finish(okResult);
    await new Promise((r) => setTimeout(r, 0));
    expect(screen.queryByTestId('test-result')).toBeNull();
  });

  it('field_error_focuses_step', async () => {
    mockBackend({
      create_connection: () => {
        throw fail('field_ssh_key_path');
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await openFold(user, 'ssh');
    await user.click(screen.getByLabelText('Connect through an SSH tunnel'));
    await user.click(foldRow('ssh'));
    await user.type(screen.getByTestId('conn-name'), 'x');
    await user.click(screen.getByTestId('conn-save'));
    // The row holding the error opens again and says so instead of its state.
    await waitFor(() => expect(foldRow('ssh')).toHaveAttribute('aria-expanded', 'true'));
    expect(foldRow('ssh')).toHaveTextContent('SSH tunnelError');
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('SSH key authentication requires a private key file.');
    expect(screen.getByLabelText('Key file')).toHaveAttribute('aria-invalid', 'true');
    await waitFor(() => expect(screen.getByLabelText('Key file')).toHaveFocus());
    // Editing the field clears its error.
    await user.type(screen.getByLabelText('Key file'), 'k');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByLabelText('Key file')).not.toHaveAttribute('aria-invalid');
  });

  it('unreadable_client_key_error_goes_to_its_field', async () => {
    mockBackend({
      test_connection: () => {
        // The drivers report unreadable CA / certificate / key files as config errors.
        throw fail('field_ssl_ca_path', {
          field: 'ssl.key_path',
          message: 'Cannot read the client key file key.pem: access denied.',
        });
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByTestId('conn-test'));
    await waitFor(() => expect(foldRow('ssl')).toHaveAttribute('aria-expanded', 'true'));
    expect(await screen.findByRole('alert')).toHaveTextContent('Cannot read the client key file');
    expect(screen.getByLabelText('Client key')).toHaveAttribute('aria-invalid', 'true');
  });

  it('test_success_shows_version_and_timings', async () => {
    mockBackend({ test_connection: () => okResult });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.type(screen.getByTestId('conn-password'), 'pw');
    await user.click(screen.getByTestId('conn-test'));
    const bar = await screen.findByTestId('test-result');
    expect(bar).toHaveTextContent('Connected to MySQL 8.0.36 · Connect 12 ms · Detect server 3 ms');
    expect(callsOf('test_connection')[0]).toMatchObject({ secrets: { db_password: 'pw' } });
  });

  it('test_failure_shows_stage_hint_detail_and_completed_timings', async () => {
    mockBackend({
      test_connection: () => {
        throw fail('connect_refused');
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByTestId('conn-test'));
    const bar = await screen.findByRole('alert');
    // Failed, on the footer's left: the step, the message and the hint.
    expect(screen.getByRole('dialog').querySelector('.modal-foot')).toContainElement(bar);
    expect(within(bar).getByTestId('test-stage')).toHaveTextContent('Connect:');
    expect(bar).toHaveTextContent('Could not connect to the database server.');
    expect(bar).toHaveTextContent('Connection refused: check host, port, firewall');
    expect(screen.queryByText(/os error 10061/)).not.toBeInTheDocument();
    const details = within(bar).getByRole('button', { name: 'Details' });
    await user.click(details);
    expect(details).toHaveAttribute('aria-expanded', 'true');
    // Stages that completed before the failure keep their timings; the raw detail follows.
    expect(bar).toHaveTextContent('Steps completed: Secrets 1 ms');
    expect(screen.getByText(/os error 10061/)).toBeInTheDocument();
    // Esc closes the details first, not the dialog.
    await user.keyboard('{Escape}');
    expect(screen.queryByText(/os error 10061/)).not.toBeInTheDocument();
    expect(screen.getByRole('dialog', { name: 'New connection' })).toBeInTheDocument();
    expect(details).toHaveFocus();
  });

  it.each([
    ['auth_mysql', 'Connect:', 'Wrong user/password'],
    ['tls', 'Connect:', 'TLS handshake failed.'],
    ['server_db_missing', 'Connect:', 'Database does not exist.'],
    ['timeout_connect', 'Connect:', 'The operation timed out.'],
    ['ssh_connect', 'SSH connect:', 'SSH connection failed.'],
    ['ssh_auth', 'SSH authentication:', 'If the key has a passphrase, enter it.'],
    ['ssh_channel', 'SSH channel:', 'The SSH server could not reach db.internal:3306.'],
  ] as const)('test_failure_%s_names_its_step', async (name, step, text) => {
    mockBackend({
      test_connection: () => {
        throw fail(name);
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByTestId('conn-test'));
    const bar = await screen.findByRole('alert');
    expect(within(bar).getByTestId('test-stage')).toHaveTextContent(step);
    expect(bar).toHaveTextContent(text);
  });

  it('unknown_host_key_flow', async () => {
    let n = 0;
    mockBackend({
      test_connection: () => {
        if (n++ === 0) throw fail('ssh_host_key_unknown');
        return okResult;
      },
      trust_host_key: () => null,
    });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.click(screen.getByTestId('conn-test'));
    const dialog = await screen.findByRole('dialog', { name: 'Unknown SSH host key' });
    expect(dialog).toHaveTextContent('bastion:22');
    expect(dialog).toHaveTextContent('SHA256:abc123');
    // Unverified key: Cancel has the focus, and the test context says "test", not "connect".
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toHaveFocus();
    await user.click(within(dialog).getByRole('button', { name: 'Trust and test' }));
    expect(await screen.findByTestId('test-result')).toHaveTextContent('Connected');
    expect(callsOf('trust_host_key')[0]).toMatchObject({
      host: 'bastion',
      port: 22,
      fingerprint: 'SHA256:abc123',
      keyOpenssh: backendError('ssh_host_key_unknown').key_openssh,
    });
    expect(callsOf('test_connection')).toHaveLength(2);
    expect(onClose).not.toHaveBeenCalled();
  });

  it('untouched_form_closes_without_asking', async () => {
    mockBackend({});
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Close dialog' }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('dirty_form_asks_before_discarding_on_cancel_close_and_escape', async () => {
    mockBackend({});
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.type(screen.getByTestId('conn-name'), 'x');
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    const ask = await screen.findByRole('alertdialog', { name: 'Discard changes?' });
    expect(onClose).not.toHaveBeenCalled();
    // The safe action has the focus: Enter keeps editing.
    expect(within(ask).getByRole('button', { name: 'Cancel' })).toHaveFocus();
    await user.keyboard('{Enter}');
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
    expect(screen.getByTestId('conn-name')).toHaveValue('x');
    await user.click(screen.getByRole('button', { name: 'Close dialog' }));
    await user.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Discard' }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('discard_prompt_traps_focus_and_escape_closes_only_the_top_modal', async () => {
    mockBackend({});
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.type(screen.getByTestId('conn-name'), 'x');
    await user.click(screen.getByRole('button', { name: 'Close dialog' }));
    const ask = await screen.findByRole('alertdialog', { name: 'Discard changes?' });
    const cancel = within(ask).getByRole('button', { name: 'Cancel' });
    const discard = within(ask).getByRole('button', { name: 'Discard' });
    expect(cancel).toHaveFocus();
    await user.tab();
    expect(discard).toHaveFocus();
    await user.tab();
    expect(cancel).toHaveFocus();
    await user.tab({ shift: true });
    expect(discard).toHaveFocus();
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
    expect(screen.getByRole('dialog', { name: 'New connection' })).toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
  });

  it('the test result sits on the footer left; a long page scrolls inside the frame', async () => {
    mockBackend({ test_connection: () => okResult });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    const foot = screen.getByRole('dialog').querySelector('.modal-foot') as HTMLElement;
    const slot = foot.querySelector('.test-slot');
    expect(slot).not.toBeNull();
    // Test and its result first, then Cancel and Save on the right; no Back / Next.
    expect(foot.firstElementChild).toContainElement(screen.getByTestId('conn-test'));
    expect(foot.lastElementChild).toBe(screen.getByTestId('conn-save'));
    expect(screen.getByTestId('conn-save')).toHaveTextContent('Save');
    expect(screen.queryByTestId('conn-back')).toBeNull();
    // Open folds can make the page long: it scrolls between the fixed header and footer.
    const page = /\.conn-page\s*\{([^}]*)\}/.exec(css)?.[1] ?? '';
    expect(page).toMatch(/overflow:\s*auto/);
    expect(screen.getByTestId('conn-panel')).toHaveClass('conn-page');
    await user.click(screen.getByTestId('conn-test'));
    const result = await screen.findByTestId('test-result');
    expect(slot).toContainElement(result);
    expect(result).toHaveClass('ok');
  });

  it('dark_dialogs_get_a_visible_line_colour', () => {
    expect(css).toMatch(/\.dark \.modal\s*\{[^}]*--dlg-line:/);
    expect(css).toMatch(/\.modal\s*\{[^}]*--dlg-line:/);
  });

  it('host_key_mismatch_warns_without_trust_button', async () => {
    mockBackend({
      test_connection: () => {
        throw fail('ssh_host_key_mismatch');
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByTestId('conn-test'));
    const bar = await screen.findByRole('alert');
    expect(bar).toHaveTextContent('Remove the old key from known_hosts manually to continue');
    expect(screen.queryByRole('button', { name: /trust/i })).not.toBeInTheDocument();
  });

  it('edit_keeps_password_when_untouched', async () => {
    const stored = config(ids.prod, 'shop', { env_tag: 'production' });
    mockBackend({ update_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="edit" initial={stored.config} hasDbPassword onClose={onClose} />);
    expect(screen.getByTestId('conn-password')).toHaveAttribute('placeholder', '••••••••');
    expect(screen.getByTestId('env-chip')).toHaveTextContent('Production');
    expect(screen.getByTestId('conn-save')).toHaveTextContent('Save');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    const call = callsOf('update_connection')[0];
    expect(call).toMatchObject({ cfg: { id: ids.prod }, secrets: { db_password: 'keep' } });
  });

  it('edit_clears_or_sets_password_when_changed', async () => {
    const stored = config(ids.prod, 'shop');
    mockBackend({ update_connection: (a) => a.cfg });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="edit" initial={stored.config} hasDbPassword onClose={vi.fn()} />);
    const pw = screen.getByTestId('conn-password');
    await user.type(pw, 'a');
    await user.clear(pw);
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('update_connection')).toHaveLength(1));
    expect(callsOf('update_connection')[0]).toMatchObject({ secrets: { db_password: 'clear' } });
  });

  it('create_sends_nil_id_and_typed_password', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.clear(screen.getByTestId('conn-host'));
    await user.type(screen.getByTestId('conn-host'), '127.0.0.1');
    await user.clear(screen.getByTestId('conn-user'));
    await user.type(screen.getByTestId('conn-user'), 'root');
    await user.type(screen.getByTestId('conn-password'), 'secret');
    await user.type(screen.getByTestId('conn-name'), 'local');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('create_connection')[0]).toMatchObject({
      cfg: {
        id: '00000000-0000-0000-0000-000000000000',
        name: 'local',
        host: '127.0.0.1',
        port: 3306,
        kind: 'mysql',
        secret_mode: 'saved',
        ssh: null,
        // The default Prefer is saved as such: a missing SSL config means plaintext.
        ssl: { version: 1, mode: 'prefer', ca_path: null, cert_path: null, key_path: null },
      },
      secrets: { db_password: 'secret' },
    });
  });

  it('a stored connection without an SSL config shows Disable and saves it explicitly', async () => {
    mockBackend({ update_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    const stored = config(ids.prod, 'shop', { ssl: null });
    render(<ConnectionDialog mode="edit" initial={stored.config} hasDbPassword onClose={onClose} />);
    expect(foldRow('ssl')).toHaveTextContent('SSL / TLSDisable');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('update_connection')[0]).toMatchObject({ cfg: { ssl: { mode: 'disable' } } });
  });

  it('read_only_flag_round_trips', async () => {
    mockBackend({ update_connection: (a) => a.cfg, create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const stored = config(ids.prod, 'shop', { read_only: true });
    const onClose = vi.fn();
    const { unmount } = render(<ConnectionDialog mode="edit" initial={stored.config} hasDbPassword onClose={onClose} />);
    const box = screen.getByTestId('conn-read-only');
    expect(box).toBeChecked();
    await user.click(box);
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('update_connection')[0]).toMatchObject({ cfg: { read_only: false } });
    unmount();

    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.type(screen.getByTestId('conn-host'), 'h');
    expect(screen.getByTestId('conn-read-only')).not.toBeChecked();
    await user.type(screen.getByTestId('conn-name'), 'ro');
    await user.click(screen.getByTestId('conn-read-only'));
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('create_connection')).toHaveLength(1));
    expect(callsOf('create_connection')[0]).toMatchObject({ cfg: { read_only: true } });
  });

  it('labels_are_associated', async () => {
    mockBackend({});
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    expect(screen.getByRole('radiogroup', { name: 'Engine' })).toBeInTheDocument();
    expect(screen.getByRole('dialog')).toHaveAttribute('aria-modal', 'true');
    for (const label of ['Name', 'Host', 'Port', 'User', 'Password', 'Database', 'Save password']) {
      expect(screen.getByLabelText(label)).toBeInTheDocument();
    }
    expect(screen.getByRole('radiogroup', { name: 'Environment' })).toBeInTheDocument();
    expect(screen.getByRole('checkbox', { name: 'Read-only' })).toBeInTheDocument();
    await openFold(user, 'advanced');
    for (const label of ['Group', 'Application name']) expect(screen.getByLabelText(label)).toBeInTheDocument();
  });

  it('ask_mode_clears_stored_secrets', async () => {
    mockBackend({ update_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="edit" initial={config(ids.prod, 'x').config} hasDbPassword onClose={onClose} />);
    await user.click(screen.getByLabelText('Save password'));
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('update_connection')[0]).toMatchObject({
      cfg: { secret_mode: 'ask' },
      secrets: { db_password: 'clear' },
    });
  });

  it('escape_closes_without_saving', async () => {
    mockBackend({});
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await user.keyboard('{Escape}');
    expect(onClose).toHaveBeenCalledWith();
    expect(callsOf('create_connection')).toHaveLength(0);
  });

  it('query_timeout_field_saves_options', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.type(screen.getByTestId('conn-host'), 'h');
    await user.type(screen.getByTestId('conn-name'), 'q');
    await openFold(user, 'advanced');
    const field = screen.getByTestId('conn-query-timeout');
    expect(field).toHaveValue(0);
    expect(screen.getByLabelText('Query timeout (s)')).toBe(field);
    await user.clear(field);
    await user.type(field, '30');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('create_connection')).toHaveLength(1));
    expect(callsOf('create_connection')[0]).toMatchObject({ cfg: { options: { query_timeout_s: 30 } } });
  });

  it('query_timeout_shows_the_stored_value_and_empty_means_off', async () => {
    mockBackend({ update_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const stored = config(ids.prod, 'x').config;
    render(
      <ConnectionDialog
        mode="edit"
        initial={{ ...stored, options: { ...stored.options, query_timeout_s: 45 } }}
        onClose={vi.fn()}
      />,
    );
    await openFold(user, 'advanced');
    const field = screen.getByTestId('conn-query-timeout');
    expect(field).toHaveValue(45);
    // Emptied: no timeout. (The browser itself refuses negative and fractional input.)
    await user.clear(field);
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('update_connection')).toHaveLength(1));
    expect(callsOf('update_connection')[0]).toMatchObject({ cfg: { options: { query_timeout_s: 0 } } });
  });

  it('query_timeout_is_clamped_to_whole_seconds_within_a_day', () => {
    expect(MAX_QUERY_TIMEOUT_S).toBe(86_400);
    expect(queryTimeoutSeconds('')).toBe(0);
    expect(queryTimeoutSeconds('   ')).toBe(0);
    expect(queryTimeoutSeconds('abc')).toBe(0);
    expect(queryTimeoutSeconds('-5')).toBe(0);
    expect(queryTimeoutSeconds('-0.5')).toBe(0);
    expect(queryTimeoutSeconds('2.9')).toBe(2);
    expect(queryTimeoutSeconds('30')).toBe(30);
    expect(queryTimeoutSeconds('86400')).toBe(86_400);
    expect(queryTimeoutSeconds('86401')).toBe(86_400);
    expect(queryTimeoutSeconds('1e12')).toBe(86_400);
    expect(queryTimeoutSeconds('Infinity')).toBe(0);
  });

  it('a huge query timeout is saved clamped', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.type(screen.getByTestId('conn-host'), 'h');
    await user.type(screen.getByTestId('conn-name'), 'q');
    await openFold(user, 'advanced');
    const field = screen.getByTestId('conn-query-timeout');
    await user.clear(field);
    await user.type(field, '99999999999');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('create_connection')).toHaveLength(1));
    expect(callsOf('create_connection')[0]).toMatchObject({ cfg: { options: { query_timeout_s: 86_400 } } });
  });

  it('an empty host, user and name fall back to the engine defaults', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="create" onClose={onClose} />);
    await chooseEngine(user, 'postgres');
    // The name follows the defaults: `database @ host`, or the host.
    expect(screen.getByTestId('conn-name')).toHaveAttribute('placeholder', 'localhost');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('create_connection')[0]).toMatchObject({
      cfg: { name: 'localhost', host: 'localhost', port: 5432, username: 'postgres', default_db: null },
    });
  });

  it('the name is suggested from the database and host', async () => {
    mockBackend({ create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.clear(screen.getByTestId('conn-host'));
    await user.type(screen.getByTestId('conn-host'), 'db.internal');
    await user.type(screen.getByLabelText('Database'), 'sales');
    expect(screen.getByTestId('conn-name')).toHaveAttribute('placeholder', 'sales @ db.internal');
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('create_connection')).toHaveLength(1));
    expect(callsOf('create_connection')[0]).toMatchObject({ cfg: { name: 'sales @ db.internal' } });
  });

  it('a pasted connection string fills the fields and can still be edited', async () => {
    mockBackend({
      parse_connection_string: (a) => {
        expect(a.input).toBe('postgresql://app:s3cret@db.internal:6432/sales?sslmode=require&charset=utf8');
        return {
          engine: 'postgres',
          host: 'db.internal',
          port: 6432,
          user: 'app',
          password: 's3cret',
          database: 'sales',
          ssl_mode: 'require',
          application_name: null,
          connect_timeout_s: null,
          ignored: ['charset=utf8'],
          warnings: ['Several hosts are listed; only the first (db.internal) is used.'],
        };
      },
      create_connection: (a) => a.cfg,
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await openUrl(user);
    await user.type(screen.getByTestId('conn-string'), 'postgresql://app:s3cret@db.internal:6432/sales?sslmode=require&charset=utf8');
    const parsed = await screen.findByTestId('parse-result', {}, { timeout: 3_000 });
    expect(parsed).toHaveTextContent('Parsed · PostgreSQL · db.internal:6432');
    expect(parsed).toHaveTextContent('Ignored: charset=utf8');
    expect(parsed).toHaveTextContent('only the first');
    // The engine follows the scheme, the fields right below follow the values.
    expect(screen.getByTestId('conn-engine-postgres')).toBeChecked();
    expect(screen.getByTestId('conn-host')).toHaveValue('db.internal');
    expect(screen.getByTestId('conn-port')).toHaveValue(6432);
    expect(screen.getByTestId('conn-user')).toHaveValue('app');
    expect(screen.getByTestId('conn-password')).toHaveValue('s3cret');
    expect(screen.getByLabelText('Database')).toHaveValue('sales');
    await openFold(user, 'ssl');
    expect(screen.getByTestId('conn-ssl-require')).toBeChecked();
  });

  it('an unreadable connection string says why and changes nothing', async () => {
    mockBackend({
      parse_connection_string: () => {
        throw appError('config', 'unsupported scheme "redis://"; use postgresql://, mysql:// or mariadb://', {
          field: 'conn_string',
        });
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await openUrl(user);
    await user.type(screen.getByTestId('conn-string'), 'redis://localhost');
    const alert = await screen.findByRole('alert', {}, { timeout: 3_000 });
    expect(alert).toHaveTextContent('unsupported scheme "redis://"');
    expect(screen.getByTestId('conn-string')).toHaveAttribute('aria-invalid', 'true');
    expect(screen.getByTestId('conn-engine-mysql')).toBeChecked();
  });

  it('a string without a scheme keeps the chosen engine; mariadb:// picks MySQL / MariaDB', async () => {
    const parsed = (engine: 'mariadb' | null) => ({
      engine,
      host: 'db.internal',
      port: null,
      user: 'app',
      password: null,
      database: 'sales',
      ssl_mode: null,
      application_name: null,
      connect_timeout_s: null,
      ignored: [],
      warnings: [],
    });
    let next: 'mariadb' | null = null;
    mockBackend({ parse_connection_string: () => parsed(next), create_connection: (a) => a.cfg });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await chooseEngine(user, 'postgres');
    await openUrl(user);
    await user.type(screen.getByTestId('conn-string'), 'app@db.internal/sales');
    await screen.findByTestId('parse-result', {}, { timeout: 3_000 });
    expect(screen.getByTestId('conn-engine-postgres')).toBeChecked();
    // The port was not named: it stays the engine default.
    expect(screen.getByTestId('conn-port')).toHaveValue(5432);

    next = 'mariadb';
    await user.clear(screen.getByTestId('conn-string'));
    await user.type(screen.getByTestId('conn-string'), 'mariadb://app@db.internal/sales');
    await waitFor(() => expect(screen.getByTestId('conn-engine-mysql')).toBeChecked(), { timeout: 3_000 });
    await user.click(screen.getByTestId('conn-save'));
    await waitFor(() => expect(callsOf('create_connection')).toHaveLength(1));
    expect(callsOf('create_connection')[0]).toMatchObject({ cfg: { kind: 'mysql', port: 3306 } });
  });
});
