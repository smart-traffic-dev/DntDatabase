import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  backendError,
  callsOf,
  config,
  fail,
  ids,
  mockBackend,
  resetStores,
  serverInfo,
} from '../test/backend';
import { ConnectionDialog } from './ConnectionDialog';

beforeEach(resetStores);

const okResult = {
  info: { ...serverInfo('MySql8'), version_string: '8.0.36' },
  timings: [
    { stage: 'connect_db', ms: 12 },
    { stage: 'detect_server', ms: 3 },
  ],
};

describe('ConnectionDialog', () => {
  it('switching_engine_updates_default_port', async () => {
    mockBackend({});
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    const port = screen.getByTestId('conn-port');
    expect(port).toHaveValue(3306);
    await user.click(screen.getByTestId('conn-engine-postgres'));
    expect(port).toHaveValue(5432);
    await user.click(screen.getByLabelText('MariaDB'));
    expect(port).toHaveValue(3306);
    expect(screen.getByText('MariaDB', { selector: '.chip' })).toBeInTheDocument();
    // An edited port is kept.
    await user.clear(port);
    await user.type(port, '3307');
    await user.click(screen.getByTestId('conn-engine-postgres'));
    expect(port).toHaveValue(3307);
  });

  it('field_error_focuses_tab', async () => {
    mockBackend({
      create_connection: () => {
        throw fail('field_ssh_key_path');
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByRole('tab', { name: 'SSH' }));
    await user.click(screen.getByLabelText('Connect through an SSH tunnel'));
    await user.click(screen.getByRole('tab', { name: 'General' }));
    await user.type(screen.getByTestId('conn-name'), 'x');
    await user.click(screen.getByTestId('conn-save'));
    expect(await screen.findByRole('tab', { name: 'SSH', selected: true })).toBeInTheDocument();
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
    expect(await screen.findByRole('tab', { name: 'SSL', selected: true })).toBeInTheDocument();
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
    expect(bar).toHaveTextContent('Connected · 8.0.36 · Connect 12 ms · Detect server 3 ms');
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
    expect(within(bar).getByTestId('test-stage')).toHaveTextContent('Connect:');
    expect(bar).toHaveTextContent('Could not connect to the database server.');
    expect(bar).toHaveTextContent('Connection refused: check host, port, firewall');
    // Stages that completed before the failure keep their timings.
    expect(bar).toHaveTextContent('Secrets 1 ms');
    expect(screen.queryByText(/os error 10061/)).not.toBeInTheDocument();
    await user.click(within(bar).getByRole('button', { name: 'Details' }));
    expect(screen.getByText(/os error 10061/)).toBeInTheDocument();
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
    await user.click(within(dialog).getByRole('button', { name: 'Trust and connect' }));
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
    await user.type(screen.getByTestId('conn-name'), 'local');
    await user.type(screen.getByTestId('conn-host'), '127.0.0.1');
    await user.type(screen.getByTestId('conn-user'), 'root');
    await user.type(screen.getByTestId('conn-password'), 'secret');
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
        ssl: null,
      },
      secrets: { db_password: 'secret' },
    });
  });

  it('labels_are_associated', () => {
    mockBackend({});
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    for (const label of ['Name', 'Group', 'Host', 'Port', 'User', 'Password', 'Default database', 'Environment tag']) {
      expect(screen.getByLabelText(label)).toBeInTheDocument();
    }
    expect(screen.getByRole('dialog')).toHaveAttribute('aria-modal', 'true');
    expect(screen.getByRole('tablist')).toBeInTheDocument();
  });

  it('ask_mode_clears_stored_secrets', async () => {
    mockBackend({ update_connection: (a) => a.cfg });
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<ConnectionDialog mode="edit" initial={config(ids.prod, 'x').config} hasDbPassword onClose={onClose} />);
    await user.click(screen.getByLabelText('Save password in Windows Credential Manager'));
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
});
