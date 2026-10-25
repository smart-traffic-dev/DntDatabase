import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { JobSummary } from '../bindings';
import { useConnections } from '../store/connections';
import { useJobs } from '../store/jobs';
import { callsOf, config, ids, mockBackend, resetStores } from '../test/backend';
import { jobDetail, jobSummary } from '../test/jobs';
import { JobsPanel } from './JobsPanel';
import { progressText, stateLabel } from './jobFormat';
import { registerRerun } from './rerun';

vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => {}) }));

const running = jobSummary({
  id: 'run',
  title: 'Dump shop to shop.sql',
  kind: 'dump',
  state: 'running',
  connection_id: ids.prod,
  progress: { phase: 'Tables', done: 3, total: 12, unit: 'statements' },
});
const failed = jobSummary({
  id: 'fail',
  title: 'Restore shop from shop.sql',
  kind: 'restore',
  state: 'failed',
  rerunnable: false,
  summary: 'Stopped at statement 14',
  finished_at: '2026-10-25T10:05:00.000Z',
  created_at: '2026-10-25T09:00:00.000Z',
});
const done = jobSummary({
  id: 'done',
  title: 'Dump crm to crm.sql',
  kind: 'dump',
  state: 'done',
  rerunnable: true,
  result_path: 'C:\\dumps\\crm.sql',
  summary: '12 tables',
  finished_at: '2026-10-25T10:01:00.000Z',
  created_at: '2026-10-25T08:00:00.000Z',
});

function seed(jobs: JobSummary[]) {
  act(() => {
    for (const job of jobs) useJobs.getState().apply({ job, new_log: [] });
  });
}

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
});

describe('jobFormat', () => {
  it('names states and progress', () => {
    expect(stateLabel(jobSummary({ state: 'failed', outcome_unknown: true }))).toBe('Outcome unknown');
    expect(stateLabel(jobSummary({ state: 'cancelled' }))).toBe('Cancelled');
    expect(progressText({ phase: 'Rows', done: 1200, total: 5000, unit: 'rows' })).toBe('Rows · 1,200 / 5,000 rows');
    expect(progressText({ phase: '', done: 1536, total: null, unit: 'bytes' })).toBe('1.5 KB');
    expect(progressText({ phase: 'Starting', done: 0, total: null, unit: 'none' })).toBe('Starting');
  });
});

describe('JobsPanel', () => {
  it('lists live jobs first with progress and Stop, then finished ones with their state', async () => {
    mockBackend({ job_list: () => [running, failed, done], job_cancel: () => true });
    seed([running, failed, done]);
    render(<JobsPanel />);
    const rows = screen.getAllByTestId('job-row');
    expect(rows.map((r) => r.getAttribute('data-job-id'))).toEqual(['run', 'fail', 'done']);

    const live = rows[0]!;
    expect(live).toHaveTextContent('Dump shop to shop.sql');
    expect(live).toHaveTextContent('shop-prod');
    expect(live).toHaveTextContent('Tables · 3 / 12 statements');
    expect(within(live).getByRole('progressbar')).toHaveAttribute('aria-valuenow', '25');

    expect(within(rows[2]!).getByTestId('job-state')).toHaveTextContent('Done');
    expect(rows[2]).toHaveTextContent('12 tables');
    expect(within(rows[1]!).getByTestId('job-state')).toHaveTextContent('Failed');
    expect(rows[1]).toHaveTextContent('Stopped at statement 14');
    expect(within(rows[1]!).queryByRole('button', { name: 'Stop' })).toBeNull();

    await userEvent.setup().click(within(live).getByRole('button', { name: 'Stop' }));
    expect(callsOf('job_cancel')).toEqual([{ id: 'run' }]);
  });

  it('says so when there are no jobs', () => {
    render(<JobsPanel />);
    expect(screen.getByText('No background jobs.')).toBeInTheDocument();
  });

  it('a row opens its detail: the log with the dropped count, Copy log, Open folder and Run again', async () => {
    const rerun = vi.fn();
    const unregister = registerRerun('dump', rerun);
    const params = { kind: 'dump' as const, request: { database: 'crm' } };
    mockBackend({
      job_list: () => [done],
      job_detail: () =>
        jobDetail(done, {
          log: [
            { at: '2026-10-25T10:00:00.000Z', level: 'info', message: 'Dumping crm' },
            { at: '2026-10-25T10:01:00.000Z', level: 'warn', message: 'Skipped 2 triggers' },
          ],
          dropped_lines: 1000,
          params,
        }),
      job_reveal_result: () => null,
    });
    seed([done]);
    const user = userEvent.setup();
    render(<JobsPanel />);
    await user.click(screen.getByTestId('job-row'));
    const detail = await screen.findByTestId('job-detail');
    const log = await within(detail).findByTestId('job-log');
    expect(log).toHaveTextContent('Dumping crm');
    expect(log).toHaveTextContent('Skipped 2 triggers');
    expect(within(detail).getByText('1,000 lines not shown')).toBeInTheDocument();

    await user.click(within(detail).getByRole('button', { name: 'Open folder' }));
    expect(callsOf('job_reveal_result')).toEqual([{ id: 'done' }]);

    await user.click(within(detail).getByRole('button', { name: 'Run again' }));
    expect(rerun).toHaveBeenCalledWith(params);
    expect(within(detail).getByRole('button', { name: 'Copy log' })).toBeInTheDocument();

    await user.click(within(detail).getByRole('button', { name: 'Back to jobs' }));
    expect(screen.queryByTestId('job-detail')).toBeNull();
    unregister();
  });

  it('focus goes to Back on open and back to the row on return; openJob opens a detail from outside', async () => {
    mockBackend({ job_list: () => [done, failed], job_detail: () => jobDetail(done) });
    seed([done, failed]);
    const user = userEvent.setup();
    render(<JobsPanel />);
    const row = screen.getAllByTestId('job-row').find((r) => r.getAttribute('data-job-id') === 'done')!;
    await user.click(within(row).getByRole('button', { name: /Show details/ }));
    const back = await screen.findByRole('button', { name: 'Back to jobs' });
    expect(back).toHaveFocus();
    await user.click(back);
    const again = screen.getAllByTestId('job-row').find((r) => r.getAttribute('data-job-id') === 'done')!;
    expect(within(again).getByRole('button', { name: /Show details/ })).toHaveFocus();

    act(() => useJobs.getState().openJob('fail'));
    expect(await screen.findByTestId('job-detail')).toHaveTextContent('Restore shop from shop.sql');
  });

  it('Run again is absent for a job that cannot run again', async () => {
    mockBackend({
      job_list: () => [failed],
      job_detail: () =>
        jobDetail(failed, { log: [{ at: '2026-10-25T10:00:00.000Z', level: 'error', message: 'Server error 1064' }] }),
    });
    seed([failed]);
    const user = userEvent.setup();
    render(<JobsPanel />);
    await user.click(screen.getByTestId('job-row'));
    const detail = await screen.findByTestId('job-detail');
    expect(await within(detail).findByText(/Server error 1064/)).toBeInTheDocument();
    expect(within(detail).queryByRole('button', { name: 'Run again' })).toBeNull();
    expect(within(detail).queryByRole('button', { name: 'Open folder' })).toBeNull();
  });

  it('Clear finished clears the history and keeps live jobs', async () => {
    let list = [running, failed, done];
    mockBackend({
      job_list: () => list,
      job_clear_history: () => {
        list = [running];
        return null;
      },
    });
    seed([running, failed, done]);
    render(<JobsPanel />);
    await userEvent.setup().click(screen.getByRole('button', { name: 'Clear finished' }));
    expect(callsOf('job_clear_history')).toHaveLength(1);
    expect(screen.getAllByTestId('job-row').map((r) => r.getAttribute('data-job-id'))).toEqual(['run']);
  });
});
