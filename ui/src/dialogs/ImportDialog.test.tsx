import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { ImportPreview } from '../bindings';
import { useDataEdits } from '../store/dataEdits';
import { useDialogs } from '../store/dialogs';
import { useConnections } from '../store/connections';
import { objectTabId, useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { callsOf, calls, fail, ids, mockBackend, resetStores, sampleTree, serverInfo } from '../test/backend';
import { ImportDialog } from './ImportDialog';

/** What the user picks in the backend's open dialog (`file_pick_open`). */
const open = vi.fn<(args: Record<string, unknown>) => Promise<string | null>>();

/** `mockBackend` with the backend's open dialog answering `open`. */
const backend = (handlers: Parameters<typeof mockBackend>[0]) =>
  mockBackend({ file_pick_open: (a) => open(a), ...handlers });

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
  open.mockReset();
  open.mockResolvedValue(FILE);
});

describe('ImportDialog', () => {
  it('import_wrong_passphrase_inline_error', async () => {
    const user = userEvent.setup();
    backend({
      import_needs_passphrase: () => true,
      preview_import: (a) => {
        if (a.passphrase === 'wrong-pass') {
          throw fail('passphrase_incorrect');
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
    backend({ import_needs_passphrase: () => false, preview_import: () => preview });
    render(<ImportDialog onClose={() => {}} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByText('shop');

    // The backend opens the dialog (.dntconn / .ncx filters); the UI names the kind of file.
    expect(callsOf('file_pick_open')).toEqual([{ kind: 'connections' }]);
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
    backend({
      import_needs_passphrase: () => false,
      preview_import: () => preview,
      apply_import: () => ({ imported: 2, overwritten: 1, skipped: 3, overwritten_ids: [] }),
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

  it('overwrite closes the replaced connections and their tabs before reloading', async () => {
    const user = userEvent.setup();
    backend({
      import_needs_passphrase: () => false,
      preview_import: () => preview,
      apply_import: () => ({ imported: 0, overwritten: 1, skipped: 0, overwritten_ids: [ids.prod] }),
      list_tree: () => sampleTree(),
    });
    await useTree.getState().reloadStored();
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    useConnections.getState().setInfo(ids.analytics, serverInfo('Postgres'));
    const orders = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
    const tabOf = (connectionId: string) => ({
      id: objectTabId(connectionId, orders),
      kind: 'object' as const,
      connectionId,
      objectRef: orders,
      title: 'orders',
    });
    useTabs.getState().open(tabOf(ids.prod));
    useTabs.getState().open(tabOf(ids.analytics));

    const onClose = vi.fn();
    render(<ImportDialog onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByText('shop');
    await user.click(screen.getByRole('radio', { name: /Overwrite/ }));
    await user.click(screen.getByRole('button', { name: 'Import' }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());

    expect(useConnections.getState().info.has(ids.prod)).toBe(false);
    expect(useConnections.getState().info.has(ids.analytics)).toBe(true);
    expect(useTabs.getState().tabs.map((t) => t.connectionId)).toEqual([ids.analytics]);
  });

  it('keeps the dialog open while applying and still reloads and notifies when apply resolves', async () => {
    const user = userEvent.setup();
    let finish!: () => void;
    const gate = new Promise<void>((r) => (finish = r));
    backend({
      import_needs_passphrase: () => false,
      preview_import: () => preview,
      apply_import: async () => {
        await gate;
        return { imported: 1, overwritten: 0, skipped: 0, overwritten_ids: [] };
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
    open.mockResolvedValue(null);
    backend({});
    render(<ImportDialog onClose={() => {}} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await waitFor(() => expect(open).toHaveBeenCalled());
    expect(calls()).toEqual(['file_pick_open']);
    expect(screen.queryByRole('alert')).toBeNull();
  });
});

describe('ImportDialog with staged data edits', () => {
  /** A tab of connection `a` (the one the preview's first row overwrites) with a staged change. */
  function stagedTabOfA() {
    const obj = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
    const id = objectTabId('a', obj);
    useTabs.getState().open({ id, kind: 'object', connectionId: 'a', objectRef: obj, title: 'orders' });
    useDataEdits.getState().setInfo(id, obj, {
      editable: true, reason: null, key: ['id'], hidden_key: [], editable_columns: ['name'], atomic: 'yes', protection: 'row_version',
    });
    useDataEdits.getState().stage(id, {
      kind: 'update',
      row: 'r1',
      ref: { key: [['id', { t: 'Int', v: 1 }]], original: [], xmin: null },
      column: 'name',
      input: { kind: 'null' },
    });
    return id;
  }

  async function overwriteImport() {
    const user = userEvent.setup();
    backend({
      import_needs_passphrase: () => false,
      preview_import: () => preview,
      apply_import: () => ({ imported: 0, overwritten: 1, skipped: 0, overwritten_ids: ['a'] }),
      list_tree: () => sampleTree(),
    });
    const onClose = vi.fn();
    render(<ImportDialog onClose={onClose} />);
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    await screen.findByText('shop');
    await user.click(screen.getByRole('radio', { name: /Overwrite/ }));
    await user.click(screen.getByRole('button', { name: 'Import' }));
    return { onClose };
  }

  const asked = () => {
    const q = useDialogs.getState().question;
    if (q?.kind !== 'discardForImport') throw new Error('expected the discardForImport question');
    return q;
  };

  it('offers only Discard and import / Cancel import; Cancel performs no import', async () => {
    const id = stagedTabOfA();
    const { onClose } = await overwriteImport();
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardForImport'));
    expect(asked().count).toBe(1);
    asked().done(false);
    await waitFor(() => expect(screen.getByRole('button', { name: 'Import' })).toBeEnabled());
    expect(callsOf('apply_import')).toEqual([]);
    expect(onClose).not.toHaveBeenCalled();
    expect(useDataEdits.getState().hasStaged(id)).toBe(true);
    expect(useTabs.getState().tabs).toHaveLength(1);
  });

  it('Discard and import discards the edits, closes the tabs and imports', async () => {
    const id = stagedTabOfA();
    const { onClose } = await overwriteImport();
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardForImport'));
    asked().done(true);
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(callsOf('apply_import')).toEqual([{ path: FILE, passphrase: null, policy: 'overwrite' }]);
    expect(useDataEdits.getState().hasStaged(id)).toBe(false);
    expect(useTabs.getState().tabs).toHaveLength(0);
  });

  it('the question has no keep option: its buttons are Discard and import / Cancel import', async () => {
    stagedTabOfA();
    await overwriteImport();
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardForImport'));
    const { DialogHost } = await import('./DialogHost');
    render(<DialogHost />);
    const names = (await screen.findAllByRole('button')).map((b) => b.textContent);
    expect(names).toEqual(expect.arrayContaining(['Discard and import', 'Cancel import']));
    expect(names).not.toContain('Keep');
  });
});
