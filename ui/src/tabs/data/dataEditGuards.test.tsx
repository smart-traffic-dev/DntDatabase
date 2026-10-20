import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { EditableInfo, ObjectRef } from '../../bindings';
import { onQueryShortcut } from '../../components/queryShortcuts';
import { DialogHost } from '../../dialogs/DialogHost';
import { useDataEdits } from '../../store/dataEdits';
import { useDialogs } from '../../store/dialogs';
import { installCloseGuard, type CloseGuardWindow } from '../../store/queryTabs';
import { objectTabId, useTabs } from '../../store/tabs';
import { callsOf, ids, mockBackend, resetStores, sampleTree } from '../../test/backend';

const objectRef: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const info: EditableInfo = {
  editable: true,
  reason: null,
  key: ['id'],
  hidden_key: [],
  editable_columns: ['name'],
  atomic: 'yes',
  protection: 'row_version',
};
const tabId = objectTabId(ids.prod, objectRef);

/** An object tab with one staged update. */
function stagedTab() {
  useTabs.getState().open({ id: tabId, kind: 'object', connectionId: ids.prod, objectRef, title: 'orders' });
  const edits = useDataEdits.getState();
  edits.setInfo(tabId, objectRef, info);
  edits.stage(tabId, {
    kind: 'update',
    row: '[["id",{"t":"Int","v":1}]]',
    ref: { key: [['id', { t: 'Int', v: 1 }]], original: [], xmin: null },
    column: 'name',
    input: { kind: 'null' },
  });
}

const question = () => {
  const q = useDialogs.getState().question;
  if (q?.kind !== 'discardEdits') throw new Error('expected the discardEdits question');
  return q;
};

beforeEach(() => resetStores());

describe('staged data edits are not lost silently', () => {
  it('quitting asks first and stays open when the user keeps the edits', async () => {
    mockBackend({ tabs_save: () => null });
    stagedTab();
    let handler: ((e: { preventDefault(): void }) => unknown) | undefined;
    const win: CloseGuardWindow = {
      onCloseRequested: vi.fn(async (h) => {
        handler = h;
        return () => undefined;
      }),
      destroy: vi.fn(async () => undefined),
    };
    await installCloseGuard(win);

    const first = handler!({ preventDefault: () => undefined });
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    expect(question().count).toBe(1);
    question().done(false);
    await first;
    expect(win.destroy).not.toHaveBeenCalled();
    expect(useDataEdits.getState().hasStaged(tabId)).toBe(true);

    const second = handler!({ preventDefault: () => undefined });
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    await second;
    expect(win.destroy).toHaveBeenCalled();
  });

  it('deleting a connection asks before anything is deleted', async () => {
    const user = userEvent.setup();
    mockBackend({ delete_connection: () => null, list_tree: () => sampleTree() });
    stagedTab();
    useDialogs.setState({ request: { kind: 'deleteNode', node: { type: 'connection', id: ids.prod }, label: 'shop-prod' } });
    render(<DialogHost />);
    await user.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(false);
    expect(callsOf('delete_connection')).toEqual([]);
    expect(useTabs.getState().tabs).toHaveLength(1);
    // Keeping the edits cancels the delete and closes its dialog.
    await waitFor(() => expect(useDialogs.getState().request).toBeNull());
    expect(useDataEdits.getState().hasStaged(tabId)).toBe(true);

    act(() => useDialogs.setState({ request: { kind: 'deleteNode', node: { type: 'connection', id: ids.prod }, label: 'shop-prod' } }));
    await user.click(await screen.findByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(useDialogs.getState().question?.kind).toBe('discardEdits'));
    question().done(true);
    await waitFor(() => expect(callsOf('delete_connection')).toHaveLength(1));
    await waitFor(() => expect(useTabs.getState().tabs).toHaveLength(0));
  });
});

describe('Ctrl+0 with the window shortcuts installed', () => {
  beforeEach(() => window.addEventListener('keydown', onQueryShortcut, true));
  afterEach(() => window.removeEventListener('keydown', onQueryShortcut, true));

  function press(target: Element) {
    const reached = vi.fn();
    target.addEventListener('keydown', reached);
    const event = new KeyboardEvent('keydown', { key: '0', ctrlKey: true, bubbles: true, cancelable: true });
    target.dispatchEvent(event);
    return { reached: reached.mock.calls.length > 0, prevented: event.defaultPrevented };
  }

  it('reaches an editable data grid (set NULL) but still resets the zoom elsewhere', () => {
    document.body.innerHTML = '<div data-editing-grid="true"><div class="gt-canvas" id="in-grid" tabindex="0"></div></div><div id="elsewhere"></div>';
    expect(press(document.getElementById('in-grid')!)).toEqual({ reached: true, prevented: false });
    // Elsewhere the window shortcut consumes it as before.
    expect(press(document.getElementById('elsewhere')!)).toEqual({ reached: false, prevented: true });
    document.body.innerHTML = '';
  });
});
