import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { HostKeyDialog } from './HostKeyDialog';
import { PasswordPrompt } from './PasswordPrompt';
import { Failure, appError, callsOf, mockBackend } from '../test/backend';

describe('PasswordPrompt', () => {
  it('password_prompt_submits_needed_fields', async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn();
    render(
      <PasswordPrompt
        connectionName="shop"
        needs={['db_password', 'ssh_key_passphrase']}
        onSubmit={onSubmit}
        onCancel={vi.fn()}
      />,
    );
    expect(screen.queryByLabelText('SSH password')).not.toBeInTheDocument();
    const connect = screen.getByRole('button', { name: 'Connect' });
    expect(connect).toBeDisabled();
    await user.type(screen.getByLabelText('Database password'), 'pw');
    await user.type(screen.getByLabelText('Key passphrase'), 'phrase');
    await user.click(connect);
    expect(onSubmit).toHaveBeenCalledWith({ db_password: 'pw', ssh_key_passphrase: 'phrase' });
  });

  it('cancel_and_escape_call_onCancel', async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    render(<PasswordPrompt connectionName="shop" needs={['db_password']} onSubmit={vi.fn()} onCancel={onCancel} />);
    await user.keyboard('{Escape}');
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledTimes(2);
  });

  it('focus_is_trapped_and_returns_to_opener', async () => {
    const user = userEvent.setup();
    const opener = document.createElement('button');
    document.body.append(opener);
    opener.focus();
    const { unmount } = render(
      <PasswordPrompt connectionName="shop" needs={['db_password']} onSubmit={vi.fn()} onCancel={vi.fn()} />,
    );
    expect(screen.getByLabelText('Database password')).toHaveFocus();
    await user.tab({ shift: true });
    expect(screen.getByRole('button', { name: 'Cancel' })).toHaveFocus();
    await user.tab();
    expect(screen.getByLabelText('Database password')).toHaveFocus();
    unmount();
    expect(opener).toHaveFocus();
    opener.remove();
  });
});

describe('HostKeyDialog', () => {
  it('trusts the key then reports done', async () => {
    mockBackend({ trust_host_key: () => null });
    const user = userEvent.setup();
    const onDone = vi.fn();
    render(<HostKeyDialog host="h" port={2222} fingerprint="SHA256:zz" keyOpenssh="ssh-ed25519 K" onDone={onDone} />);
    expect(screen.getByText('SHA256:zz')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Trust and connect' }));
    expect(onDone).toHaveBeenCalledWith(true);
    expect(callsOf('trust_host_key')[0]).toEqual({ host: 'h', port: 2222, fingerprint: 'SHA256:zz', keyOpenssh: 'ssh-ed25519 K' });
  });

  it('cancel reports not trusted and a failed trust stays open', async () => {
    mockBackend({
      trust_host_key: () => {
        throw new Failure(appError('io', 'known_hosts is read-only'));
      },
    });
    const user = userEvent.setup();
    const onDone = vi.fn();
    render(<HostKeyDialog host="h" port={22} fingerprint="f" keyOpenssh="k" onDone={onDone} />);
    await user.click(screen.getByRole('button', { name: 'Trust and connect' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('known_hosts is read-only');
    expect(onDone).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onDone).toHaveBeenCalledWith(false);
  });
});
