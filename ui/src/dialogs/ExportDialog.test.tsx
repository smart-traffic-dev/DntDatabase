import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { save } from '@tauri-apps/plugin-dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useTree } from '../store/tree';
import { callsOf, ids, mockBackend, resetStores, sampleTree } from '../test/backend';
import { ExportDialog } from './ExportDialog';

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn(), open: vi.fn() }));

beforeEach(async () => {
  resetStores();
  vi.mocked(save).mockReset();
  mockBackend({ list_tree: () => sampleTree(), export_connections: () => null });
  await useTree.getState().reloadStored();
});

describe('ExportDialog', () => {
  it('export_requires_matching_passphrase', async () => {
    const user = userEvent.setup();
    render(<ExportDialog preselected={[{ type: 'connection', id: ids.legacy }]} onClose={() => {}} />);
    const exportBtn = screen.getByRole('button', { name: 'Export' });
    expect(exportBtn).toBeEnabled();
    expect(screen.getByText('SSH private keys are exported as file paths only.')).toBeInTheDocument();

    await user.click(screen.getByRole('checkbox', { name: 'Include passwords' }));
    expect(exportBtn).toBeDisabled();

    await user.type(screen.getByLabelText('Passphrase'), 'short');
    await user.type(screen.getByLabelText('Confirm passphrase'), 'short');
    expect(screen.getByText('Passphrase must be at least 8 characters.')).toBeInTheDocument();
    expect(exportBtn).toBeDisabled();

    await user.type(screen.getByLabelText('Passphrase'), 'er-long');
    expect(screen.getByText('Passphrases do not match.')).toBeInTheDocument();
    expect(exportBtn).toBeDisabled();

    await user.type(screen.getByLabelText('Confirm passphrase'), 'er-long');
    expect(screen.queryByText('Passphrases do not match.')).toBeNull();
    expect(exportBtn).toBeEnabled();
  });

  it('export_calls_api_with_selected_nodes', async () => {
    const user = userEvent.setup();
    vi.mocked(save).mockResolvedValue('C:\\out\\x.dntconn');
    const onClose = vi.fn();
    // Group A (shop-prod, analytics) is preselected as a whole.
    render(<ExportDialog preselected={[{ type: 'group', id: ids.groupA }]} onClose={onClose} />);
    expect(screen.getByRole('checkbox', { name: /shop-prod/ })).toBeChecked();
    expect(screen.getByRole('checkbox', { name: /legacy-crm/ })).not.toBeChecked();

    await user.click(screen.getByRole('checkbox', { name: /legacy-crm/ }));
    await user.click(screen.getByRole('checkbox', { name: 'Include passwords' }));
    await user.type(screen.getByLabelText('Passphrase'), 'correct horse');
    await user.type(screen.getByLabelText('Confirm passphrase'), 'correct horse');
    await user.click(screen.getByRole('button', { name: 'Export' }));

    await waitFor(() => expect(onClose).toHaveBeenCalled());
    const opts = vi.mocked(save).mock.calls[0]![0]!;
    expect(opts.defaultPath).toMatch(/^connections-\d{4}-\d{2}-\d{2}\.dntconn$/);
    expect(opts.filters).toEqual([{ name: 'DntDatabase connections', extensions: ['dntconn'] }]);
    expect(callsOf('export_connections')).toEqual([
      {
        ids: [
          { type: 'group', id: ids.groupA },
          // legacy-crm is the only connection of group B, so the whole group is checked.
          { type: 'group', id: ids.groupB },
        ],
        passphrase: 'correct horse',
        path: 'C:\\out\\x.dntconn',
      },
    ]);
  });

  it('does nothing when the file dialog is cancelled', async () => {
    const user = userEvent.setup();
    vi.mocked(save).mockResolvedValue(null);
    const onClose = vi.fn();
    render(<ExportDialog preselected={[{ type: 'connection', id: ids.prod }]} onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    expect(callsOf('export_connections')).toEqual([]);
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
