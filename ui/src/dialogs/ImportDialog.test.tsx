import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { open } from '@tauri-apps/plugin-dialog';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ImportPreview } from '../bindings';
import { useDialogs } from '../store/dialogs';
import { Failure, appError, callsOf, calls, mockBackend, resetStores, sampleTree } from '../test/backend';
import { ImportDialog } from './ImportDialog';

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn(), open: vi.fn() }));

const FILE = 'C:\\in\\conns.dntconn';

const preview: ImportPreview = {
  rows: [
    { id: 'a', name: 'shop', kind: 'mysql', host: 'db1', group_path: 'Prod / EU', conflict: 'same_id', has_password: true, warnings: [] },
    { id: 'b', name: 'stats', kind: 'postgres', host: 'db2', group_path: '', conflict: 'same_name', has_password: false, warnings: ['SSH key file not found'] },
    { id: 'c', name: 'plain', kind: 'mysql', host: 'db3', group_path: '', conflict: null, has_password: false, warnings: [] },
  ],
  skipped: [{ name: 'weird', reason: 'Unsupported connection type' }],
};

beforeEach(() => {
  resetStores();
  vi.mocked(open).mockReset();
  vi.mocked(open).mockResolvedValue(FILE);
});

describe('ImportDialog', () => {
  it('import_wrong_passphrase_inline_error', async () => {
    const user = userEvent.setup();
    mockBackend({
      import_needs_passphrase: () => true,
      preview_import: (a) => {
        if (a.passphrase === 'wrong-pass') {
          throw new Failure(appError('config', 'Passphrase is incorrect.', { field: 'passphrase' }));
        }
        return preview;
      },
    });
    const onClose = vi.fn();
    render(<ImportDialog onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    const field = await screen.findByLabelText('Passphrase');
    await user.type(field, 'wrong-pass');
    await user.click(screen.getByRole('button', { name: 'Preview' }));

    const alert = await screen.findByText('Passphrase is incorrect.');
    expect(field.closest('.field')).toContainElement(alert);
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Passphrase')).toBeInTheDocument();

    // "Import without passwords" previews with a null passphrase.
    await user.click(screen.getByRole('button', { name: 'Import without passwords' }));
    await screen.findByText('shop');
    expect(callsOf('preview_import').at(-1)).toEqual({ path: FILE, passphrase: null });
  });

  it('import_preview_shows_conflicts_warnings_skipped', async () => {
    const user = userEvent.setup();
    mockBackend({ import_needs_passphrase: () => false, preview_import: () => preview });
    render(<ImportDialog onClose={() => {}} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByText('shop');

    expect(vi.mocked(open).mock.calls[0]![0]!.filters).toEqual([
      { name: 'DntDatabase connections', extensions: ['dntconn'] },
      { name: 'Navicat connections', extensions: ['ncx'] },
      { name: 'All files', extensions: ['*'] },
    ]);
    const headers = screen.getAllByRole('columnheader').map((h) => h.textContent);
    expect(headers).toEqual(['Name', 'Engine', 'Host', 'Group', 'Conflict', 'Warnings']);

    const rows = screen.getAllByRole('row').slice(1);
    expect(within(rows[0]!).getByText('Same id')).toBeInTheDocument();
    expect(within(rows[0]!).getByText('MY')).toHaveClass('chip-MY');
    expect(within(rows[0]!).getByText('Prod / EU')).toBeInTheDocument();
    expect(within(rows[1]!).getByText('Same name')).toBeInTheDocument();
    expect(within(rows[1]!).getByText('PG')).toHaveClass('chip-PG');
    expect(within(rows[1]!).getByText('SSH key file not found')).toBeInTheDocument();
    const skipped = screen.getByRole('region', { name: 'Skipped entries' });
    expect(within(skipped).getByText('weird')).toBeInTheDocument();
    expect(within(skipped).getByText(/Unsupported connection type/)).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: /Skip/ })).toBeChecked();
  });

  it('import_apply_uses_selected_policy_and_reloads_tree', async () => {
    const user = userEvent.setup();
    mockBackend({
      import_needs_passphrase: () => false,
      preview_import: () => preview,
      apply_import: () => ({ imported: 2, overwritten: 1, skipped: 3 }),
      list_tree: () => sampleTree(),
    });
    const onClose = vi.fn();
    render(<ImportDialog onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByText('shop');
    await user.click(screen.getByRole('radio', { name: /Overwrite/ }));
    await user.click(screen.getByRole('button', { name: 'Import' }));

    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('apply_import')).toEqual([{ path: FILE, passphrase: null, policy: 'overwrite' }]);
    expect(calls()).toContain('list_tree');
    expect(useDialogs.getState().notice).toBe('Imported 2 · Overwritten 1 · Skipped 3');
  });

  it('keeps the dialog open while applying and still reloads and notifies when apply resolves', async () => {
    const user = userEvent.setup();
    let finish!: () => void;
    const gate = new Promise<void>((r) => (finish = r));
    mockBackend({
      import_needs_passphrase: () => false,
      preview_import: () => preview,
      apply_import: async () => {
        await gate;
        return { imported: 1, overwritten: 0, skipped: 0 };
      },
      list_tree: () => sampleTree(),
    });
    const onClose = vi.fn();
    render(<ImportDialog onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByText('shop');
    await user.click(screen.getByRole('button', { name: 'Import' }));

    const cancel = screen.getByRole('button', { name: 'Cancel' });
    expect(cancel).toBeDisabled();
    await user.keyboard('{Escape}');
    expect(onClose).not.toHaveBeenCalled();

    finish();
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(calls()).toContain('list_tree');
    expect(useDialogs.getState().notice).toBe('Imported 1 · Overwritten 0 · Skipped 0');
  });

  it('does nothing when the file dialog is cancelled', async () => {
    const user = userEvent.setup();
    vi.mocked(open).mockResolvedValue(null);
    mockBackend({});
    render(<ImportDialog onClose={() => {}} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await waitFor(() => expect(open).toHaveBeenCalled());
    expect(calls()).toEqual([]);
    expect(screen.queryByRole('alert')).toBeNull();
  });
});
