import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { DialogHost } from '../dialogs/DialogHost';
import { callsOf, mockBackend, resetStores } from '../test/backend';
import { installCloseGuard, setRunningSource, setTxStateSource, useQueryDocs, type CloseGuardWindow } from './queryTabs';

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
    await handler?.({ preventDefault: () => {} });
  };
  return { win, requestClose };
}

function backend(order: string[], active: () => number) {
  return mockBackend({
    job_active_count: () => {
      order.push('job_active_count');
      return active();
    },
    job_stop_all: () => {
      order.push('job_stop_all');
      return null;
    },
    tabs_save: () => {
      order.push('tabs_save');
      return null;
    },
    query_close_tab: () => null,
    query_tx: () => 'idle',
  });
}

beforeEach(() => {
  resetStores();
  setTxStateSource(null);
  setRunningSource(null);
});

describe('app close guard with background jobs (R8)', () => {
  it('asks when jobs are live; Cancel keeps the app and the jobs', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order, () => 2);
    render(<DialogHost />);
    await installCloseGuard(win);
    void requestClose();
    expect(await screen.findByText('2 background jobs are running. Quitting stops them.')).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull());
    expect(callsOf('job_stop_all')).toHaveLength(0);
    expect(win.destroy).not.toHaveBeenCalled();
    expect(win.order).toEqual(['job_active_count']);
  });

  it('Stop jobs and quit stops every job before the rest of the close flow', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order, () => 1);
    useQueryDocs.getState().newQuery(null, null);
    render(<DialogHost />);
    await installCloseGuard(win);
    void requestClose();
    expect(await screen.findByText('1 background job is running. Quitting stops it.')).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole('button', { name: 'Stop jobs and quit' }));
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(win.order).toEqual(['job_active_count', 'job_stop_all', 'tabs_save', 'destroy']);
  });

  it('while job_stop_all runs Cancel and Esc do nothing; the quit goes on', async () => {
    const { win, requestClose } = fakeWindow();
    let release: (() => void) | null = null;
    mockBackend({
      job_active_count: () => 1,
      job_stop_all: () => new Promise<null>((r) => (release = () => r(null))),
      tabs_save: () => null,
      query_close_tab: () => null,
      query_tx: () => 'idle',
    });
    render(<DialogHost />);
    await installCloseGuard(win);
    void requestClose();
    const user = userEvent.setup();
    await user.click(await screen.findByRole('button', { name: 'Stop jobs and quit' }));
    const cancel = screen.getByRole('button', { name: 'Cancel' });
    expect(cancel).toBeDisabled();
    await user.keyboard('{Escape}');
    expect(screen.getByRole('alertdialog')).toBeInTheDocument();
    release!();
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
  });

  it('no live job: no question, the app closes', async () => {
    const { win, requestClose } = fakeWindow();
    backend(win.order, () => 0);
    render(<DialogHost />);
    await installCloseGuard(win);
    await requestClose();
    await waitFor(() => expect(win.destroy).toHaveBeenCalled());
    expect(callsOf('job_stop_all')).toHaveLength(0);
    expect(screen.queryByRole('alertdialog')).toBeNull();
  });
});
