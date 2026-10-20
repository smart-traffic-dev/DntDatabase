import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { emptyRun, useQueryRun, type TabRun } from '../store/queryRun';
import { installRunNotifier, LONG_RUN_MS, type NotifyWindow } from './runNotifier';

const INFORMATIONAL = 2;

let clock = 0;
let focused = true;
let focusListener: (() => void) | null = null;
const win = {
  requestUserAttention: vi.fn<NotifyWindow['requestUserAttention']>(async () => {}),
  setTitle: vi.fn<NotifyWindow['setTitle']>(async () => {}),
} satisfies NotifyWindow;
let uninstall: () => void;

const running = (startedAt: number): TabRun => ({ ...emptyRun, phase: { kind: 'running', index: 0, total: 1, startedAt }, startedAt });
const finish = (tabId: string, patch: Partial<TabRun> = {}) =>
  useQueryRun.setState((s) => ({ runs: new Map(s.runs).set(tabId, { ...emptyRun, lastRun: { rows: 1, elapsedMs: 1 }, stop: null, ...patch }) }));
const start = (tabId: string) => useQueryRun.setState((s) => ({ runs: new Map(s.runs).set(tabId, running(clock)) }));

beforeEach(() => {
  clock = 1_000_000;
  focused = false;
  focusListener = null;
  win.requestUserAttention.mockClear();
  win.setTitle.mockClear();
  useQueryRun.setState({ runs: new Map() });
  uninstall = installRunNotifier({
    win,
    attention: INFORMATIONAL,
    baseTitle: 'DntDatabase',
    titleOf: (id) => (id === 'q1' ? 'orders' : undefined),
    hasFocus: () => focused,
    now: () => clock,
    onFocus: (l) => {
      focusListener = l;
      return () => (focusListener = null);
    },
  });
});
afterEach(() => uninstall());

describe('long query notification', () => {
  it('flashes the taskbar and titles the window when a long run ends in the background', () => {
    start('q1');
    clock += LONG_RUN_MS + 1;
    finish('q1');
    expect(win.requestUserAttention).toHaveBeenCalledWith(INFORMATIONAL);
    expect(win.setTitle).toHaveBeenCalledWith('✓ Done · orders - DntDatabase');
  });

  it('says so when the run failed', () => {
    start('q1');
    clock += 60_000;
    finish('q1', { stop: { kind: 'error' } as never });
    expect(win.setTitle).toHaveBeenCalledWith('✗ Failed · orders - DntDatabase');
    expect(win.requestUserAttention).toHaveBeenCalledTimes(1);
  });

  it('is quiet for a short run', () => {
    start('q1');
    clock += LONG_RUN_MS;
    finish('q1');
    expect(win.requestUserAttention).not.toHaveBeenCalled();
    expect(win.setTitle).not.toHaveBeenCalled();
  });

  it('is quiet while the window has the focus', () => {
    focused = true;
    start('q1');
    clock += 60_000;
    finish('q1');
    expect(win.requestUserAttention).not.toHaveBeenCalled();
  });

  it('puts the title back when the focus returns, once', () => {
    start('q1');
    clock += 60_000;
    finish('q1');
    win.setTitle.mockClear();
    focusListener?.();
    expect(win.setTitle).toHaveBeenCalledWith('DntDatabase');
    focusListener?.();
    expect(win.setTitle).toHaveBeenCalledTimes(1);
  });

  it('does not touch the title on focus when nothing was announced', () => {
    focusListener?.();
    expect(win.setTitle).not.toHaveBeenCalled();
  });

  it('ignores updates of a run in progress and runs it never saw start', () => {
    start('q1');
    clock += 60_000;
    // A progress update while still running.
    useQueryRun.setState((s) => ({ runs: new Map(s.runs).set('q1', { ...s.runs.get('q1')!, current: 1 }) }));
    expect(win.requestUserAttention).not.toHaveBeenCalled();
    // A tab that appears already finished.
    finish('q2');
    expect(win.requestUserAttention).not.toHaveBeenCalled();
  });

  it('survives a window that rejects', async () => {
    win.setTitle.mockRejectedValueOnce(new Error('denied'));
    win.requestUserAttention.mockRejectedValueOnce(new Error('denied'));
    start('q1');
    clock += 60_000;
    expect(() => finish('q1')).not.toThrow();
    await Promise.resolve();
  });

  it('stops after the uninstaller ran', () => {
    uninstall();
    start('q1');
    clock += 60_000;
    finish('q1');
    expect(win.requestUserAttention).not.toHaveBeenCalled();
    expect(focusListener).toBeNull();
  });
});
