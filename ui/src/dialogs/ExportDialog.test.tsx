import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useTree } from '../store/tree';
import { callsOf, ids, mockBackend, resetStores, sampleTree } from '../test/backend';
import { ExportDialog } from './ExportDialog';

/** What the user picks in the backend's save dialog (`file_pick_save`). */
const save = vi.fn<(args: Record<string, unknown>) => Promise<string | null>>();

beforeEach(async () => {
  resetStores();
  save.mockReset();
  mockBackend({ list_tree: () => sampleTree(), export_connections: () => null, file_pick_save: (a) => save(a) });
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

  it('select_all_and_none_with_plural_safe_count', async () => {
    const user = userEvent.setup();
    render(<ExportDialog onClose={() => {}} />);
    expect(screen.getByText('No connections selected')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Export' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Select all' }));
    expect(screen.getByText(/^\d+ connections selected$/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Export' })).toBeEnabled();
    await user.click(screen.getByRole('button', { name: 'Select none' }));
    expect(screen.getByText('No connections selected')).toBeInTheDocument();
  });

  it('one_selected_connection_is_singular', () => {
    render(<ExportDialog preselected={[{ type: 'connection', id: ids.legacy }]} onClose={() => {}} />);
    expect(screen.getByText('1 connection selected')).toBeInTheDocument();
  });

  it('export_calls_api_with_selected_nodes', async () => {
    const user = userEvent.setup();
    save.mockResolvedValue('C:\\out\\x.dntconn');
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
    // The backend opens the save dialog (with the .dntconn filter); the UI names the kind and file.
    const [picked] = callsOf('file_pick_save');
    expect(picked!.kind).toBe('connections');
    expect(picked!.defaultName).toMatch(/^connections-\d{4}-\d{2}-\d{2}\.dntconn$/);
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
    save.mockResolvedValue(null);
    const onClose = vi.fn();
    render(<ExportDialog preselected={[{ type: 'connection', id: ids.prod }]} onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Export' }));
    await waitFor(() => expect(save).toHaveBeenCalled());
    expect(callsOf('export_connections')).toEqual([]);
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
