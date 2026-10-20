import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { clearCursor, flushCursor, reportCursor, resetCursor, useCursor } from './cursor';

let frames: (() => void)[] = [];
const nextFrame = () => {
  const run = frames;
  frames = [];
  run.forEach((f) => f());
};

beforeEach(() => {
  frames = [];
  vi.stubGlobal('requestAnimationFrame', (cb: () => void) => frames.push(cb));
  resetCursor();
});
afterEach(() => vi.unstubAllGlobals());

describe('cursor store', () => {
  it('coalesces many moves in a frame into one update with the latest position', () => {
    let updates = 0;
    const off = useCursor.subscribe(() => void updates++);
    for (let i = 1; i <= 200; i++) reportCursor('t1', { line: 1, col: i, selected: 0 });
    // Nothing yet, and a single frame is queued.
    expect(updates).toBe(0);
    expect(frames).toHaveLength(1);
    nextFrame();
    expect(updates).toBe(1);
    expect(useCursor.getState().byTab.t1).toEqual({ line: 1, col: 200, selected: 0 });
    off();
  });

  it('keeps tabs apart and skips an update when nothing changed', () => {
    reportCursor('a', { line: 3, col: 4, selected: 0 });
    reportCursor('b', { line: 9, col: 1, selected: 12 });
    nextFrame();
    expect(useCursor.getState().byTab).toEqual({
      a: { line: 3, col: 4, selected: 0 },
      b: { line: 9, col: 1, selected: 12 },
    });
    const before = useCursor.getState();
    reportCursor('a', { line: 3, col: 4, selected: 0 });
    nextFrame();
    expect(useCursor.getState()).toBe(before);
  });

  it('schedules another frame after one ran', () => {
    reportCursor('a', { line: 1, col: 1, selected: 0 });
    nextFrame();
    reportCursor('a', { line: 2, col: 1, selected: 0 });
    expect(frames).toHaveLength(1);
    nextFrame();
    expect(useCursor.getState().byTab.a?.line).toBe(2);
  });

  it('forgets a tab whose editor was disposed', () => {
    reportCursor('a', { line: 1, col: 1, selected: 0 });
    nextFrame();
    clearCursor('a');
    nextFrame();
    expect(useCursor.getState().byTab).toEqual({});
    // Clearing an unknown tab changes nothing.
    const before = useCursor.getState();
    clearCursor('zzz');
    nextFrame();
    expect(useCursor.getState()).toBe(before);
  });

  it('flushCursor applies what is queued right away', () => {
    reportCursor('a', { line: 5, col: 5, selected: 1 });
    flushCursor();
    expect(useCursor.getState().byTab.a).toEqual({ line: 5, col: 5, selected: 1 });
  });
});
