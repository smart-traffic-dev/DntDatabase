import type { JobSummary } from '../../ui/src/bindings';
import { appError } from '../../ui/src/test/fixtures/data';
import { createBackend } from '../src/backend';
import { jobDetail, jobEvent, jobProgress, jobSummary, logLine } from '../src/jobs';
import { emitEvent, installBackend } from '../src/shim';
import { expect, test } from '../src/test';

const JOB_EVENT = 'job://progress';

// Closing the window while jobs run (the close guard) needs the Tauri window API: not testable here.
test.describe('jobs panel (browser, fake IPC backend)', () => {
  test('a running job shows its progress from events; Stop cancels it and the final event shows Cancelled', async ({ page }) => {
    let job: JobSummary = jobSummary({
      id: 'job-run',
      title: 'Import orders.csv into orders',
      progress: jobProgress({ phase: 'Rows', done: 100, total: 1000, unit: 'rows' }),
    });
    const backend = createBackend({
      job_list: () => [job],
      job_active_count: () => (job.state === 'running' ? 1 : 0),
      job_detail: () => jobDetail(job),
      job_cancel: () => true,
    });
    await installBackend(page, backend.handlers);
    // Page timers only run when the test advances them: the 2 s `job_list` poll never fires, so
    // what the row shows after the first load comes from the emitted events.
    await page.clock.install();
    await page.goto('/');

    // The status bar counts the live job and opens the Jobs panel.
    const counter = page.getByTestId('status-jobs');
    await expect(counter).toHaveText('1 job');
    await counter.click();
    await expect(page.getByTestId('jobs-view')).toBeVisible();
    const row = page.getByTestId('job-row').filter({ hasText: 'Import orders.csv into orders' });
    await expect(row).toContainText('Rows · 100 / 1,000 rows');
    const lists = () => backend.calls.filter((c) => c.cmd === 'job_list').length;
    const listsBeforeEvents = lists();

    // A progress event advances the row.
    job = { ...job, progress: jobProgress({ phase: 'Rows', done: 500, total: 1000, unit: 'rows' }) };
    await emitEvent(page, JOB_EVENT, jobEvent(job));
    await expect(row).toContainText('Rows · 500 / 1,000 rows');
    await expect(row.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '50');

    // Stop asks the backend to cancel this job.
    await row.getByRole('button', { name: 'Stop' }).click();
    await expect.poll(() => backend.calls.filter((c) => c.cmd === 'job_cancel').map((c) => c.args.id)).toEqual(['job-run']);

    // The backend's final event: the row shows Cancelled, the counter goes away.
    job = { ...job, state: 'cancelled', finished_at: new Date().toISOString(), summary: 'Stopped after 500 rows' };
    await emitEvent(page, JOB_EVENT, jobEvent(job, [logLine('Stopped by the user', 'warn')]));
    await expect(row.getByTestId('job-state')).toHaveText('Cancelled');
    await expect(row.getByRole('button', { name: 'Stop' })).toHaveCount(0);
    await expect(counter).toHaveCount(0);
    // Nothing after the first load came from polling.
    expect(lists()).toBe(listsBeforeEvents);
  });

  test('a failed job shows its log in the detail; a non-rerunnable job has no Run again', async ({ page }) => {
    const job = jobSummary({
      id: 'job-failed',
      kind: 'export',
      title: 'Export orders to orders.csv',
      state: 'failed',
      finished_at: new Date().toISOString(),
      error: appError('query', 'disk full'),
      rerunnable: false,
    });
    const backend = createBackend({
      job_list: () => [job],
      job_detail: ({ id }) => {
        expect(id).toBe('job-failed');
        return jobDetail(job, [logLine('Writing orders.csv'), logLine('disk full', 'error')], {
          dropped_lines: 3,
          params: { kind: 'import', request: {} },
        });
      },
    });
    await installBackend(page, backend.handlers);
    await page.goto('/');

    // No live job: no status bar counter; the right sidebar's Jobs icon opens the panel.
    await expect(page.getByTestId('status-jobs')).toHaveCount(0);
    await page.getByRole('navigation', { name: 'Right sidebar' }).getByRole('button', { name: 'Jobs' }).click();
    const row = page.getByTestId('job-row').filter({ hasText: 'Export orders to orders.csv' });
    await expect(row.getByTestId('job-state')).toHaveText('Failed');
    await expect(row).toContainText('disk full');

    await row.click();
    const detail = page.getByTestId('job-detail');
    await expect(detail).toBeVisible();
    await expect(detail).toContainText('Export');
    await expect(detail.getByTestId('job-log')).toContainText('Writing orders.csv');
    await expect(detail.getByTestId('job-log')).toContainText('ERROR  disk full');
    await expect(detail).toContainText('3 lines not shown');
    await expect(detail.getByRole('button', { name: 'Copy log' })).toBeEnabled();
    await expect(detail.getByRole('button', { name: 'Run again' })).toHaveCount(0);
    await expect(detail.getByRole('button', { name: 'Stop' })).toHaveCount(0);

    await detail.getByRole('button', { name: 'Back to jobs' }).click();
    await expect(page.getByTestId('job-detail')).toHaveCount(0);
    await expect(row).toBeVisible();
  });
});
