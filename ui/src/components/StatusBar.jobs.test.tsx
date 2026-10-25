import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { useJobs } from '../store/jobs';
import { useLayout } from '../store/layout';
import { resetStores } from '../test/backend';
import { jobSummary } from '../test/jobs';
import { StatusBar } from './StatusBar';

beforeEach(() => {
  resetStores();
  localStorage.clear();
  useLayout.getState().reset();
  useLayout.getState().setRightVisible(false);
});

describe('StatusBar jobs counter', () => {
  it('shows "n jobs" while jobs are live and opens the Jobs panel', async () => {
    render(<StatusBar />);
    expect(screen.queryByTestId('status-jobs')).toBeNull();
    act(() => {
      useJobs.getState().apply({ job: jobSummary({ id: 'a', state: 'running' }), new_log: [] });
      useJobs.getState().apply({ job: jobSummary({ id: 'b', state: 'queued' }), new_log: [] });
      useJobs.getState().apply({ job: jobSummary({ id: 'c', state: 'done' }), new_log: [] });
    });
    const counter = screen.getByTestId('status-jobs');
    expect(counter).toHaveTextContent('2 jobs');
    await userEvent.setup().click(counter);
    expect(useLayout.getState().rightVisible).toBe(true);
    expect(useLayout.getState().rightActivity).toBe('jobs');

    act(() => useJobs.getState().apply({ job: jobSummary({ id: 'a', state: 'done' }), new_log: [] }));
    expect(screen.getByTestId('status-jobs')).toHaveTextContent('1 job');
    act(() => useJobs.getState().apply({ job: jobSummary({ id: 'b', state: 'cancelled' }), new_log: [] }));
    expect(screen.queryByTestId('status-jobs')).toBeNull();
  });
});
