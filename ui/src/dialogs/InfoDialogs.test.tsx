import { act, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { mockBackend, resetStores } from '../test/backend';
import { DialogHost } from './DialogHost';

const app = vi.hoisted(() => ({ getVersion: vi.fn(async () => '1.2.3') }));
vi.mock('@tauri-apps/api/app', () => app);

beforeEach(() => {
  resetStores();
  app.getVersion.mockReset();
  app.getVersion.mockResolvedValue('1.2.3');
});

describe('Help dialogs', () => {
  it('F1 opens Settings on the Keyboard shortcuts tab, and Close closes it', () => {
    mockBackend({ get_log_level: () => 'Info' });
    render(<DialogHost />);
    act(() => useDialogs.getState().requestDialog({ kind: 'settings', tab: 'shortcuts' }));
    const dialog = screen.getByRole('dialog', { name: 'Settings' });
    expect(screen.getByRole('tab', { name: 'Keyboard shortcuts' })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tab', { name: 'Keyboard shortcuts' })).toHaveFocus();
    expect(dialog).toHaveTextContent('Ctrl+B');
    expect(dialog).toHaveTextContent('Ctrl+Shift+I');
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('About shows the name, the version and the description', async () => {
    render(<DialogHost />);
    act(() => useDialogs.getState().requestDialog({ kind: 'about' }));
    expect(await screen.findByText('version 1.2.3')).toBeInTheDocument();
    expect(screen.getByRole('dialog', { name: 'About DntDatabase' })).toHaveTextContent(
      'Database manager for MySQL, MariaDB and PostgreSQL',
    );
  });

  it('About falls back to "dev" when the version is unavailable', async () => {
    app.getVersion.mockRejectedValue(new Error('no runtime'));
    render(<DialogHost />);
    act(() => useDialogs.getState().requestDialog({ kind: 'about' }));
    expect(await screen.findByText('version dev')).toBeInTheDocument();
  });
});
