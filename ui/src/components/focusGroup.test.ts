import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useLayout } from '../store/layout';
import { useTabs, type QueryTabRef } from '../store/tabs';
import { setActiveEditor } from '../tabs/query/activeEditor';
import { focusFocusedGroup } from './focusGroup';
import { tabDomId } from './TabBar';

const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });

beforeEach(() => {
  vi.useFakeTimers();
  useTabs.getState().reset();
});
afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = '';
});

describe('focusFocusedGroup', () => {
  it("puts the caret in the focused group's editor, waiting for it to mount", () => {
    useTabs.getState().open(q('a'));
    focusFocusedGroup();
    const editor = { focus: vi.fn(), trigger: vi.fn() };
    vi.advanceTimersByTime(50);
    setActiveEditor('a', editor);
    vi.advanceTimersByTime(50);
    expect(editor.focus).toHaveBeenCalledOnce();
    setActiveEditor('a', null);
  });

  it('falls back to the active tab button (object tabs have no editor)', () => {
    useTabs.getState().open({ id: 'o', kind: 'object', connectionId: 'c', title: 'o', objectRef: { database: 'd', schema: null, name: 'o', kind: 'Table' } });
    const button = document.createElement('button');
    button.id = tabDomId('o');
    document.body.append(button);
    focusFocusedGroup();
    vi.advanceTimersByTime(1000);
    expect(document.activeElement).toBe(button);
  });

  it("focuses the tab button when the group's results are maximized (the editor is hidden)", () => {
    useTabs.getState().open(q('a'));
    useLayout.getState().toggleGroupMaximize(useTabs.getState().focusedGroupId);
    const editor = { focus: vi.fn(), trigger: vi.fn() };
    setActiveEditor('a', editor);
    const button = document.createElement('button');
    button.id = tabDomId('a');
    document.body.append(button);
    focusFocusedGroup();
    vi.advanceTimersByTime(1000);
    expect(editor.focus).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(button);
    setActiveEditor('a', null);
  });

  it('gives up quietly when nothing shows up', () => {
    focusFocusedGroup();
    expect(() => vi.advanceTimersByTime(2000)).not.toThrow();
  });
});
