import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { groupTabs, useTabs, type QueryTabRef } from '../store/tabs';
const focus = vi.hoisted(() => ({ focusFocusedGroup: vi.fn() }));
vi.mock('./focusGroup', () => focus);

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

  it('keyboard group changes move the caret into the focused group', () => {
    focus.focusFocusedGroup.mockClear();
    key({ key: '\\', ctrlKey: true });
    key({ key: '1', ctrlKey: true, altKey: true });
    key({ key: 'ArrowRight', ctrlKey: true, altKey: true });
    key({ key: 'k', ctrlKey: true });
    key({ key: 'ArrowLeft' });
    expect(focus.focusFocusedGroup).toHaveBeenCalledTimes(4);
  });

  it('Ctrl+K Ctrl+M maximizes the focused group and restores it', () => {
    s().splitTab('c', 'right');
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: 'm', ctrlKey: true }).handled).toBe(true);
    expect(s().maximizedGroupId).toBe(s().focusedGroupId);
    key({ key: 'k', ctrlKey: true });
    key({ key: 'm', ctrlKey: true });
    expect(s().maximizedGroupId).toBeUndefined();
  });

  it('Ctrl+K Enter keeps a preview tab', () => {
    s().openPreview({ id: 'p', kind: 'object', connectionId: 'c', title: 'p', objectRef: { database: 'd', schema: null, name: 'p', kind: 'Table' } });
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: 'Enter' }).handled).toBe(true);
    expect(s().tabs.find((t) => t.id === 'p')?.preview).toBe(false);
  });

  it('Ctrl+Shift+PageUp / PageDown move the active tab in its strip', () => {
    s().activate('a');
    expect(key({ key: 'PageDown', ctrlKey: true, shiftKey: true }).handled).toBe(true);
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['b', 'a', 'c']);
    key({ key: 'PageUp', ctrlKey: true, shiftKey: true });
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['a', 'b', 'c']);
  });

  it("leaves Monaco's own Ctrl+K Ctrl+<digit> chords (fold level) alone", () => {
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: '2', ctrlKey: true }).handled).toBe(false);
    expect(s().layout).toBe('single');
  });

  it('a Ctrl+K typed in a text field does not arm the chord', () => {
    const input = document.createElement('input');
    document.body.append(input);
    try {
      const at = (init: KeyboardEventInit & { key: string }) => {
        const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
        input.dispatchEvent(e);
        return onGroupShortcut(e);
      };
      // dispatchEvent sets e.target; the handler reads it.
      at({ key: 'k', ctrlKey: true });
      expect(at({ key: '1' })).toBe(false);
      expect(at({ key: 'w' })).toBe(false);
    } finally {
      input.remove();
    }
  });

  it('inside the SQL editor the second key is prevented but still reaches Monaco (ends its chord)', () => {
    const host = document.createElement('div');
    host.className = 'sql-editor';
    const area = document.createElement('textarea');
    host.append(area);
    document.body.append(host);
    try {
      const fire = (init: KeyboardEventInit & { key: string }) => {
        const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
        const stop = vi.spyOn(e, 'stopPropagation');
        area.dispatchEvent(e);
        return { handled: onGroupShortcut(e), prevented: e.defaultPrevented, stopped: stop.mock.calls.length > 0 };
      };
      fire({ key: 'k', ctrlKey: true });
      expect(fire({ key: '4' })).toEqual({ handled: true, prevented: true, stopped: false });
      expect(s().layout).toBe('grid');
    } finally {
      host.remove();
    }
  });

  it('Ctrl+K W does nothing with a single group (it would close every tab)', () => {
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: 'w' }).handled).toBe(false);
    expect(s().tabs).toHaveLength(3);
    // Ctrl still held is another chord: Ctrl+K Ctrl+W closes all tabs of the group, not the group.
    s().splitTab('c', 'right');
    key({ key: 'k', ctrlKey: true });
    expect(key({ key: 'w', ctrlKey: true }).handled).toBe(true);
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
