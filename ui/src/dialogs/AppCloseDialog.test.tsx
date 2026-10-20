import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { installCloseGuard, setTxStateSource, useQueryDocs, type CloseGuardWindow } from '../store/queryTabs';
import { callsOf, calls, fail, mockBackend, resetStores } from '../test/backend';
import { AppCloseDialog } from './AppCloseDialog';
import { DialogHost } from './DialogHost';

/** A window whose close request the test fires by hand. */
function fakeWindow() {
  let handler: ((e: { preventDefault(): void }) => unknown) | undefined;
  const order: string[] = [];
  const win: CloseGuardWindow & { order: string[] } = {
    order,
    onCloseRequested: vi.fn(async (h) => {
      handler = h;
      return () => {
        handler = undefined;
      };
    }),
    destroy: vi.fn(async () => {
      order.push('destroy');
    }),
  };
  const requestClose = async () => {
    const preventDefault = vi.fn(() => order.push('preventDefault'));
    await handler?.({ preventDefault });
    return preventDefault;
  };
  return { win, requestClose };
}

function backend(
  order: string[],
  over: { closeTab?: (id: string) => unknown; tabsSave?: () => unknown } = {},
) {
  return mockBackend({
    tabs_save: () => {
      order.push('tabs_save');
      return over.tabsSave ? over.tabsSave() : null;
    },
    query_close_tab: (a) => {
      order.push(`query_close_tab:${String(a.tabId)}:${String(a.force)}`);
      return over.closeTab ? over.closeTab(String(a.tabId)) : null;
    },
    query_tx: (a) => {
      order.push(`query_tx:${String(a.tabId)}:${String(a.action)}`);
      return 'idle';
    },
  });
}

beforeEach(() => {
  resetStores();
  setTxStateSource(null);
});

describe('AppCloseDialog', () => {
  it('lists the tabs and offers Rollback all & quit / Cancel', async () => {
    const onQuit = vi.fn(async () => {});
    const onCancel = vi.fn();
    render(<AppCloseDialog tabs={['Query 1', 'report.sql']} onQuit={onQuit} onCancel={onCancel} />);
    const items = screen.getAllByRole('listitem').map((li) => li.textContent);
    expect(items).toEqual(['Query 1', 'report.sql']);
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Rollback all & quit' }));
    expect(onQuit).toHaveBeenCalled();
  });
});

describe('app close guard', () => {
  it('app_close_lists_open_transactions', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order);
    const a = useQueryDocs.getState().newQuery(null, null);
    const b = useQueryDocs.getState().newQuery(null, null);
    const c = useQueryDocs.getState().newQuery(null, null);
    setTxStateSource((id) => (id === a ? 'active' : id === c ? 'aborted' : 'idle'));
    render(<DialogHost />);
    await installCloseGuard(win);
    const user = userEvent.setup();

    // Cancel keeps the app open.
    const prevented = await requestClose();
    expect(prevented).toHaveBeenCalled();
    const dialog = await screen.findByRole('alertdialog');
    expect(screen.getAllByRole('listitem').map((li) => li.textContent)).toEqual(['Query 1', 'Query 3']);
    expect(dialog).not.toHaveTextContent('Query 2');
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(win.destroy).not.toHaveBeenCalled();
    expect(callsOf('query_tx')).toHaveLength(0);

    // Rollback all & quit rolls back each open transaction, then quits.
    await requestClose();
    await user.click(await screen.findByRole('button', { name: 'Rollback all & quit' }));
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(win.order.filter((s) => s.startsWith('query_tx') || s === 'destroy')).toEqual([
      `query_tx:${a}:Rollback`,
      `query_tx:${c}:Rollback`,
      'destroy',
    ]);
    void b;
  });

  it('app_close_without_tx_quits_after_flush', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order);
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { sql: 'SELECT 1' });
    render(<DialogHost />);
    await installCloseGuard(win);
    await requestClose();
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(win.order).toEqual(['preventDefault', 'tabs_save', `query_close_tab:${id}:false`, 'destroy']);
    expect(callsOf('tabs_save')[0]!.tab).toMatchObject({ id, sql_text: 'SELECT 1' });
    expect(calls()).not.toContain('query_tx');
    expect(screen.queryByRole('alertdialog')).toBeNull();
  });

  it('without a tx source, the probe finds open transactions and running tabs', async () => {
    const { win, requestClose } = fakeWindow();
    const a = useQueryDocs.getState().newQuery(null, null);
    const b = useQueryDocs.getState().newQuery(null, null);
    const c = useQueryDocs.getState().newQuery(null, null);
    backend(win.order, {
      closeTab: (id) => {
        if (id === a) throw fail('transaction_open');
        if (id === c) throw fail('busy');
        return null;
      },
    });
    render(<DialogHost />);
    await installCloseGuard(win);
    await requestClose();
    await screen.findByRole('alertdialog');
    expect(screen.getAllByRole('listitem').map((li) => li.textContent)).toEqual(['Query 1', 'Query 3 (running)']);
    expect(callsOf('query_close_tab')).toEqual([a, b, c].map((tabId) => ({ tabId, force: false })));
    expect(win.destroy).not.toHaveBeenCalled();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Rollback all & quit' }));
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(callsOf('query_tx').map((x) => x.tabId)).toEqual([a, c]);
  });

  it('all idle tabs: the app closes', async () => {
    const { win, requestClose } = fakeWindow();
    useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().newQuery(null, null);
    backend(win.order);
    render(<DialogHost />);
    await installCloseGuard(win);
    await requestClose();
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(callsOf('query_close_tab')).toHaveLength(2);
    expect(screen.queryByRole('alertdialog')).toBeNull();
  });

  it('a failed save asks; Cancel keeps the app open', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order, { tabsSave: () => { throw fail('internal'); } });
    useQueryDocs.getState().newQuery(null, null);
    render(<DialogHost />);
    await installCloseGuard(win);
    // The handler waits for the answer, as Tauri awaits it.
    void requestClose();
    expect(await screen.findByText('Could not save 1 tab(s).')).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(win.destroy).not.toHaveBeenCalled();
    expect(callsOf('query_close_tab')).toHaveLength(0);
  });

  it('a failed save asks; Quit anyway goes on and quits', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order, { tabsSave: () => { throw fail('internal'); } });
    useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().newQuery(null, null);
    render(<DialogHost />);
    await installCloseGuard(win);
    void requestClose();
    expect(await screen.findByText('Could not save 2 tab(s).')).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Quit anyway' }));
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(callsOf('query_close_tab')).toHaveLength(2);
  });
});
