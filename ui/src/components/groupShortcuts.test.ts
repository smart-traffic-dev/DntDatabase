import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { groupTabs, useTabs, type QueryTabRef } from '../store/tabs';
import { onGroupShortcut, resetChord } from './groupShortcuts';

const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });
const s = () => useTabs.getState();
const key = (init: KeyboardEventInit & { key: string }) => {
  const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
  return { handled: onGroupShortcut(e), prevented: e.defaultPrevented };
};

beforeEach(() => {
  s().reset();
  resetChord();
  for (const id of ['a', 'b', 'c']) s().open(q(id));
});
afterEach(() => vi.useRealTimers());

describe('editor group shortcuts', () => {
  it('Ctrl+\\ splits right', () => {
    expect(key({ key: '\\', ctrlKey: true })).toEqual({ handled: true, prevented: true });
    expect(s().layout).toBe('columns');
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['c']);
  });

  it('Ctrl+K Ctrl+\\ splits down; Ctrl+K alone is left to the editor', () => {
    expect(key({ key: 'k', ctrlKey: true })).toEqual({ handled: false, prevented: false });
    expect(key({ key: '\\', ctrlKey: true }).handled).toBe(true);
    expect(s().layout).toBe('rows');
  });

  it('a chord expires; an unrelated second key passes through', () => {
    vi.useFakeTimers();
    key({ key: 'k', ctrlKey: true });
    vi.advanceTimersByTime(2000);
    key({ key: '\\', ctrlKey: true });
    expect(s().layout).toBe('columns'); // plain Ctrl+\ again
    s().setLayout('single');
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: 'c', ctrlKey: true }).handled).toBe(false);
  });

  it('Ctrl+K 1..4 picks a layout preset, Ctrl+K W closes the focused group', async () => {
    key({ key: 'k', ctrlKey: true });
    key({ key: '4' });
    expect(s().layout).toBe('grid');
    key({ key: 'k', ctrlKey: true });
    key({ key: '3' });
    expect(s().layout).toBe('rows');
    s().focusGroup(s().groups[1]!.id);
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: 'w' }).handled).toBe(true);
    await vi.waitFor(() => expect(s().groups).toHaveLength(1));
  });

  it('Ctrl+Alt+1..4 focuses a group, Ctrl+Alt+→ moves the tab, Ctrl+K → focuses the next group', () => {
    s().splitTab('c', 'right');
    key({ key: '1', ctrlKey: true, altKey: true });
    expect(s().focusedGroupId).toBe(s().groups[0]!.id);
    key({ key: 'ArrowRight', ctrlKey: true, altKey: true });
    expect(groupTabs(s(), s().groups[1]!.id).map((t) => t.id)).toEqual(['c', 'b']);
    key({ key: 'k', ctrlKey: true });
    key({ key: 'ArrowRight' });
    expect(s().focusedGroupId).toBe(s().groups[0]!.id); // wraps from the last group
  });
});
