import { act, render, screen } from '@testing-library/react';
import { Profiler } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { flushCursor, reportCursor, resetCursor } from '../store/cursor';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { mockBackend, resetStores } from '../test/backend';
import { StatusBar } from './StatusBar';

let frames: (() => void)[] = [];
beforeEach(() => {
  resetStores();
  resetCursor();
  frames = [];
  vi.stubGlobal('requestAnimationFrame', (cb: () => void) => frames.push(cb));
  mockBackend({ tabs_save: () => null });
});
afterEach(() => vi.unstubAllGlobals());

const frame = () => act(() => void frames.splice(0).forEach((f) => f()));

describe('status bar cursor position', () => {
  it('shows Ln / Col and the selection size of the active query tab', () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    render(<StatusBar />);
    expect(screen.queryByTestId('status-cursor')).toBeNull();
    act(() => reportCursor(id, { line: 12, col: 8, selected: 0 }));
    frame();
    expect(screen.getByTestId('status-cursor')).toHaveTextContent('Ln 12, Col 8');
    expect(screen.getByTestId('status-cursor')).not.toHaveTextContent('selected');
    act(() => reportCursor(id, { line: 12, col: 42, selected: 34 }));
    frame();
    expect(screen.getByTestId('status-cursor')).toHaveTextContent('Ln 12, Col 42 (34 selected)');
  });

  it('follows the active tab and disappears on a tab without an editor', () => {
    const a = useQueryDocs.getState().newQuery(null, null);
    const b = useQueryDocs.getState().newQuery(null, null);
    act(() => {
      reportCursor(a, { line: 1, col: 1, selected: 0 });
      reportCursor(b, { line: 7, col: 3, selected: 0 });
      flushCursor();
    });
    render(<StatusBar />);
    expect(screen.getByTestId('status-cursor')).toHaveTextContent('Ln 7, Col 3');
    act(() => useTabs.getState().activate(a));
    expect(screen.getByTestId('status-cursor')).toHaveTextContent('Ln 1, Col 1');
    act(() =>
      useTabs.getState().open({
        id: 'o1',
        kind: 'object',
        connectionId: 'c',
        objectRef: { database: 'd', schema: null, name: 'n', kind: 'Table' },
        title: 'n',
      }),
    );
    expect(screen.queryByTestId('status-cursor')).toBeNull();
  });

  it('a hundred caret moves in one frame re-render only the cursor widget, once', () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    let commits = 0;
    render(
      <Profiler id="bar" onRender={() => void commits++}>
        <StatusBar />
      </Profiler>,
    );
    const before = commits;
    act(() => {
      for (let col = 1; col <= 100; col++) reportCursor(id, { line: 1, col, selected: 0 });
    });
    expect(commits - before).toBe(0);
    frame();
    expect(commits - before).toBe(1);
    expect(screen.getByTestId('status-cursor')).toHaveTextContent('Col 100');
    // Typing replaces the docs map but does not touch the status bar at all.
    const afterFrame = commits;
    for (let i = 1; i <= 20; i++) act(() => useQueryDocs.getState().update(id, { sql: 'x'.repeat(i) }));
    expect(commits - afterFrame).toBe(0);
  });
});
