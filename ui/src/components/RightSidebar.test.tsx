import { fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { RIGHT_DEFAULT, RIGHT_MAX, RIGHT_MIN, useLayout } from '../store/layout';
import { mockBackend, resetStores } from '../test/backend';
import { RightSidebar } from './RightSidebar';

beforeEach(() => {
  resetStores();
  localStorage.clear();
  useLayout.getState().reset();
  mockBackend({ history_list: () => [] });
});

const bar = () => screen.getByRole('navigation', { name: 'Right sidebar' });

describe('RightSidebar', () => {
  it('has an activity bar with History and Jobs only (no disabled placeholders for planned panels)', () => {
    render(<RightSidebar />);
    const buttons = within(bar()).getAllByRole('button');
    expect(buttons.map((b) => b.getAttribute('aria-label'))).toEqual(['History', 'Jobs']);
    for (const b of buttons) expect(b).toBeEnabled();
    expect(buttons[0]).toHaveAttribute('title', expect.stringContaining('Ctrl+Shift+H'));
  });

  it('is collapsed at first: the panel opens from the History icon and the same icon collapses it again', async () => {
    const user = userEvent.setup();
    render(<RightSidebar />);
    const history = within(bar()).getByRole('button', { name: 'History' });
    expect(screen.queryByTestId('history-view')).toBeNull();
    expect(history).toHaveAttribute('aria-pressed', 'false');
    await user.click(history);
    expect(await screen.findByTestId('history-view')).toBeInTheDocument();
    expect(history).toHaveAttribute('aria-pressed', 'true');
    expect(useLayout.getState().rightVisible).toBe(true);
    expect(JSON.parse(localStorage.getItem('dnt.layout') ?? '{}').rightVisible).toBe(true);
    await user.click(history);
    expect(screen.queryByTestId('history-view')).toBeNull();
    expect(useLayout.getState().rightVisible).toBe(false);
  });

  it('the Jobs icon shows the Jobs panel, History switches back, and the choice persists', async () => {
    const user = userEvent.setup();
    render(<RightSidebar />);
    const history = within(bar()).getByRole('button', { name: 'History' });
    const jobs = within(bar()).getByRole('button', { name: 'Jobs' });
    await user.click(jobs);
    expect(await screen.findByTestId('jobs-view')).toBeInTheDocument();
    expect(screen.queryByTestId('history-view')).toBeNull();
    expect(jobs).toHaveAttribute('aria-pressed', 'true');
    expect(history).toHaveAttribute('aria-pressed', 'false');
    expect(JSON.parse(localStorage.getItem('dnt.layout') ?? '{}').rightActivity).toBe('jobs');

    await user.click(history);
    expect(await screen.findByTestId('history-view')).toBeInTheDocument();
    expect(screen.queryByTestId('jobs-view')).toBeNull();
    expect(useLayout.getState().rightVisible).toBe(true);

    // The active icon collapses the panel.
    await user.click(history);
    expect(useLayout.getState().rightVisible).toBe(false);
  });

  it('the panel header has a collapse button', async () => {
    useLayout.getState().setRightVisible(true);
    render(<RightSidebar />);
    await userEvent.setup().click(await screen.findByRole('button', { name: 'Collapse right sidebar' }));
    expect(screen.queryByTestId('history-view')).toBeNull();
  });

  it('the panel is as wide as the store says and resizes between 260 and 560 px', async () => {
    useLayout.getState().setRightVisible(true);
    render(<RightSidebar />);
    const panel = await screen.findByTestId('right-sidebar-panel');
    expect(panel).toHaveStyle({ width: `${RIGHT_DEFAULT}px` });
    const splitter = screen.getByRole('separator', { name: 'Resize right sidebar' });
    expect(splitter).toHaveAttribute('aria-valuemin', String(RIGHT_MIN));
    expect(splitter).toHaveAttribute('aria-valuemax', String(RIGHT_MAX));
    // Dragging the line left makes the panel wider.
    fireEvent.keyDown(splitter, { key: 'ArrowLeft' });
    expect(useLayout.getState().rightWidth).toBe(RIGHT_DEFAULT + 16);
    expect(panel).toHaveStyle({ width: `${RIGHT_DEFAULT + 16}px` });
    fireEvent.keyDown(splitter, { key: 'End' });
    expect(useLayout.getState().rightWidth).toBe(RIGHT_MAX);
    fireEvent.keyDown(splitter, { key: 'Home' });
    expect(useLayout.getState().rightWidth).toBe(RIGHT_MIN);
    fireEvent.doubleClick(splitter);
    expect(useLayout.getState().rightWidth).toBe(RIGHT_DEFAULT);
  });
});
