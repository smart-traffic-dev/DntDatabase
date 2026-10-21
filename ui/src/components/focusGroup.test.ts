import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
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

  it('gives up quietly when nothing shows up', () => {
    focusFocusedGroup();
    expect(() => vi.advanceTimersByTime(2000)).not.toThrow();
  });
});
