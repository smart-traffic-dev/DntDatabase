import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  Failure,
  appError,
  callsOf,
  config,
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
        throw new Failure(appError('config', 'Key file does not exist', { field: 'ssh.key_path' }));
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
    expect(alert).toHaveTextContent('Key file does not exist');
    expect(screen.getByLabelText('Key file')).toHaveAttribute('aria-invalid', 'true');
    await waitFor(() => expect(screen.getByLabelText('Key file')).toHaveFocus());
    // Editing the field clears its error.
    await user.type(screen.getByLabelText('Key file'), 'k');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByLabelText('Key file')).not.toHaveAttribute('aria-invalid');
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

  it('test_failure_shows_stage_hint_and_detail', async () => {
    mockBackend({
      test_connection: () => {
        throw new Failure(
          appError('connect', 'Connection refused', {
            stage: 'connect_db',
            hint: 'Check the host and port.',
            detail: 'os error 10061',
          }),
        );
      },
    });
    const user = userEvent.setup();
    render(<ConnectionDialog mode="create" onClose={vi.fn()} />);
    await user.click(screen.getByTestId('conn-test'));
    const bar = await screen.findByRole('alert');
    expect(bar).toHaveTextContent('Connect');
    expect(bar).toHaveTextContent('Connection refused');
    expect(bar).toHaveTextContent('Check the host and port.');
    expect(screen.queryByText('os error 10061')).not.toBeInTheDocument();
    await user.click(within(bar).getByRole('button', { name: 'Details' }));
    expect(screen.getByText('os error 10061')).toBeInTheDocument();
  });

  it('unknown_host_key_flow', async () => {
    let n = 0;
    mockBackend({
      test_connection: () => {
        if (n++ === 0) {
          throw new Failure(
            appError('ssh_host_key_unknown', 'Unknown host key', {
              host: 'bastion',
              port: 22,
              fingerprint: 'SHA256:abc123',
              key_openssh: 'ssh-ed25519 AAAA',
            }),
          );
        }
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
      keyOpenssh: 'ssh-ed25519 AAAA',
    });
    expect(callsOf('test_connection')).toHaveLength(2);
    expect(onClose).not.toHaveBeenCalled();
  });

  it('host_key_mismatch_warns_without_trust_button', async () => {
    mockBackend({
      test_connection: () => {
        throw new Failure(appError('ssh', 'Host key changed', { stage: 'host_key_mismatch' }));
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
