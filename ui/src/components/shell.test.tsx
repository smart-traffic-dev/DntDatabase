import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { EmptyWorkspace } from '../App';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { config, ids, resetStores } from '../test/backend';
import { cycleTab } from './tabActions';
import { onQueryShortcut } from './queryShortcuts';
import { Sidebar } from './Sidebar';

beforeEach(resetStores);

const key = (init: KeyboardEventInit & { key: string }, target: EventTarget = document.body) =>
  target.dispatchEvent(new KeyboardEvent('keydown', { ctrlKey: true, bubbles: true, cancelable: true, ...init }));

describe('tab shortcuts', () => {
  const open = () =>
    act(() => {
      for (const id of ['a', 'b', 'c']) useTabs.getState().open({ id, kind: 'query', connectionId: null, title: id });
    });

  it('cycles tabs with wrap-around', () => {
    open();
    expect(useTabs.getState().activeId).toBe('c');
    cycleTab(1);
    expect(useTabs.getState().activeId).toBe('a');
    cycleTab(-1);
    expect(useTabs.getState().activeId).toBe('c');
  });

  it('Ctrl+Tab, Ctrl+Shift+Tab and Ctrl+PgUp/PgDn switch tabs', () => {
    open();
    window.addEventListener('keydown', onQueryShortcut, true);
    try {
      act(() => void key({ key: 'Tab' }));
      expect(useTabs.getState().activeId).toBe('a');
      act(() => void key({ key: 'Tab', shiftKey: true }));
      expect(useTabs.getState().activeId).toBe('c');
      act(() => void key({ key: 'PageUp' }));
      expect(useTabs.getState().activeId).toBe('b');
      act(() => void key({ key: 'PageDown' }));
      expect(useTabs.getState().activeId).toBe('c');
    } finally {
      window.removeEventListener('keydown', onQueryShortcut, true);
    }
  });

  it('Ctrl+W closes the active object tab', () => {
    const o = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };
    act(() => useTabs.getState().open({ id: 'o1', kind: 'object', connectionId: ids.prod, objectRef: o, title: 'orders' }));
    window.addEventListener('keydown', onQueryShortcut, true);
    try {
      act(() => void key({ key: 'w' }));
      expect(useTabs.getState().tabs).toEqual([]);
    } finally {
      window.removeEventListener('keydown', onQueryShortcut, true);
    }
  });
});

describe('empty states', () => {
  it('sidebar offers New connection / Import when nothing is stored', async () => {
    render(<Sidebar />);
    // Not loaded yet: no call to action.
    expect(screen.queryByTestId('sidebar-empty')).toBeNull();
    act(() => useTree.setState({ childIds: new Map([['', []]]) }));
    expect(screen.getByTestId('sidebar-empty')).toHaveTextContent('No connections yet');
    await userEvent.setup().click(screen.getByRole('button', { name: 'Import…' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'import' });
  });

  it('workspace points at New connection without connections, else at the shortcuts', async () => {
    render(<EmptyWorkspace />);
    expect(screen.getByTestId('workspace-empty')).toHaveTextContent('No connections yet');
    act(() => useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]));
    expect(screen.getByTestId('workspace-empty')).toHaveTextContent('Ctrl+T');
    expect(useQueryDocs.getState().docs.size).toBe(0);
  });
});
