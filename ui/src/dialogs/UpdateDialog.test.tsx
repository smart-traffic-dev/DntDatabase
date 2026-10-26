import { act, fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import type { UpdateInfo, UpdateProgress } from '../bindings';
import { useDialogs } from '../store/dialogs';
import { mockBackend, resetStores } from '../test/backend';
import { appError, Failure } from '../test/fixtures/data';
import { DialogHost } from './DialogHost';

const INFO: UpdateInfo = { version: '0.2.0', current_version: '0.1.0', notes: 'Edit cells in place.', date: null };

beforeEach(() => resetStores());

function open(found?: UpdateInfo) {
  render(<DialogHost />);
  act(() => useDialogs.getState().requestDialog({ kind: 'update', found }));
}

describe('UpdateDialog', () => {
  it('says the app is up to date when no newer release exists', async () => {
    mockBackend({ update_check: () => null });
    open();
    expect(await screen.findByTestId('update-status')).toHaveTextContent('DntDatabase is up to date.');
  });

  it('shows a newer release with its notes', async () => {
    mockBackend({ update_check: () => INFO });
    open();
    expect(await screen.findByTestId('update-status')).toHaveTextContent('DntDatabase 0.2.0 is available (you have 0.1.0).');
    expect(screen.getByTestId('update-notes')).toHaveTextContent('Edit cells in place.');
    expect(screen.getByRole('button', { name: 'Install and restart' })).toBeEnabled();
  });

  it('shows a failed check with Try again', async () => {
    let calls = 0;
    mockBackend({
      update_check: () => {
        calls += 1;
        if (calls === 1) throw new Failure(appError('connect', 'Could not reach the update server: offline'));
        return null;
      },
    });
    open();
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not reach the update server: offline');
    fireEvent.click(screen.getByRole('button', { name: 'Try again' }));
    expect(await screen.findByTestId('update-status')).toHaveTextContent('up to date');
  });

  it('install goes through the quit checks, then installs with progress', async () => {
    const order: string[] = [];
    let release: () => void = () => {};
    mockBackend({
      job_active_count: () => {
        order.push('job_active_count');
        return 0;
      },
      update_install: ({ channel }) =>
        new Promise<null>((resolve) => {
          order.push('update_install');
          (channel as { onmessage(p: UpdateProgress): void }).onmessage({ downloaded: 5 * 1024 * 1024, total: 10 * 1024 * 1024 });
          release = () => resolve(null);
        }),
    });
    open(INFO); // found at startup: no check
    fireEvent.click(screen.getByRole('button', { name: 'Install and restart' }));
    expect(await screen.findByText('Downloading… 5.0 MB of 10.0 MB')).toBeInTheDocument();
    expect(screen.getByRole('progressbar', { name: 'Download progress' })).toHaveAttribute('aria-valuenow', '50');
    expect(order).toEqual(['job_active_count', 'update_install']);
    // The installer closes the app: the dialog cannot be closed meanwhile.
    expect(screen.getByRole('button', { name: 'Close' })).toBeDisabled();
    await act(async () => release());
  });

  it('install is not started when the user keeps running jobs', async () => {
    const order: string[] = [];
    mockBackend({
      job_active_count: () => 2,
      update_install: () => {
        order.push('update_install');
        return null;
      },
    });
    open(INFO);
    fireEvent.click(screen.getByRole('button', { name: 'Install and restart' }));
    // The quit flow asks about the jobs; Cancel keeps them and the app.
    const cancel = await screen.findByRole('button', { name: 'Cancel' });
    fireEvent.click(cancel);
    expect(await screen.findByRole('button', { name: 'Install and restart' })).toBeEnabled();
    expect(order).toEqual([]);
  });
});
