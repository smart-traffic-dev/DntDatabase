import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { useLayout } from '../store/layout';
import { usePalette } from '../store/palette';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs, type ObjectTabRef } from '../store/tabs';
import { mockBackend, resetStores } from '../test/backend';
import { installBrowserGuard } from '../desktop/browserGuard';
import { onQueryShortcut } from './queryShortcuts';

vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ close: async () => {} }) }));

const objectTab = (name: string): ObjectTabRef => {
  const objectRef = { database: 'shop', schema: null, name, kind: 'Table' as const };
  return { id: objectTabId('c1', objectRef), kind: 'object', connectionId: 'c1', objectRef, title: name };
};

/** Dispatches a key at `target`; returns whether anything called preventDefault. */
function press(init: KeyboardEventInit & { key: string }, target: Element = document.body) {
  const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(e);
  return e.defaultPrevented;
}
const ctrl = (key: string, extra: KeyboardEventInit = {}, target?: Element) => press({ key, ctrlKey: true, ...extra }, target);

let uninstallGuard: () => void;
beforeEach(() => {
  resetStores();
  localStorage.clear();
  useLayout.getState().reset();
  useLayout.getState().setFontScale(1);
  usePalette.getState().hide();
  mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null, open_devtools: () => null });
  window.addEventListener('keydown', onQueryShortcut, true);
  uninstallGuard = installBrowserGuard(window, { allowReload: false });
});
afterEach(() => {
  window.removeEventListener('keydown', onQueryShortcut, true);
  uninstallGuard();
  document.body.innerHTML = '';
});

describe('typing', () => {
  it('plain keys never query the document; the second key of a Ctrl+K chord still works', () => {
    useQueryDocs.getState().newQuery(null, null);
    const lookup = vi.spyOn(document, 'querySelector');
    try {
      for (const key of ['s', 'E', ' ', '1', 'Enter', 'Backspace']) press({ key });
      press({ key: 'S', shiftKey: true });
      expect(lookup).not.toHaveBeenCalled();
      // Ctrl+K then a plain 2: the "columns" layout preset.
      ctrl('k');
      press({ key: '2' });
      expect(useTabs.getState().layout).toBe('columns');
    } finally {
      lookup.mockRestore();
    }
  });
});

describe('tab shortcuts', () => {
  beforeEach(() => {
    for (const n of ['a', 'b', 'c']) useTabs.getState().open(objectTab(n));
  });

  it('Ctrl+1..8 go to tab N, Ctrl+9 to the last tab', () => {
    ctrl('1');
    expect(useTabs.getState().activeId).toBe(useTabs.getState().tabs[0]!.id);
    ctrl('2');
    expect(useTabs.getState().activeId).toBe(useTabs.getState().tabs[1]!.id);
    ctrl('8');
    expect(useTabs.getState().activeId).toBe(useTabs.getState().tabs[1]!.id);
    ctrl('9');
    expect(useTabs.getState().activeId).toBe(useTabs.getState().tabs[2]!.id);
  });

  it('Ctrl+Shift+T reopens the last closed tab and does not open a new query', async () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    useQueryDocs.getState().update(id, { sql: 'select 1' });
    await useQueryDocs.getState().closeTab(id);
    expect(useQueryDocs.getState().docs.has(id)).toBe(false);
    expect(ctrl('T', { shiftKey: true })).toBe(true);
    expect(useQueryDocs.getState().docs.get(id)?.sql).toBe('select 1');
    expect(useTabs.getState().tabs).toHaveLength(4);
  });
});

describe('window shortcuts', () => {
  it('Ctrl+J toggles the result panel flag', () => {
    expect(ctrl('j')).toBe(true);
    expect(useLayout.getState().resultsVisible).toBe(false);
    ctrl('j');
    expect(useLayout.getState().resultsVisible).toBe(true);
  });

  it('Ctrl+Shift+N, Ctrl+, and F1 open the new connection dialog, settings, and settings on its Shortcuts tab', () => {
    ctrl('N', { shiftKey: true });
    expect(useDialogs.getState().request).toEqual({ kind: 'newConnection' });
    ctrl(',');
    expect(useDialogs.getState().request).toEqual({ kind: 'settings' });
    expect(press({ key: 'F1' })).toBe(true);
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'shortcuts' });
  });

  it('F1 wins inside the editor: it is stopped before Monaco sees it', () => {
    const editor = document.createElement('div');
    editor.className = 'sql-editor';
    const textarea = document.createElement('textarea');
    editor.append(textarea);
    document.body.append(editor);
    const monaco = vi.fn();
    editor.addEventListener('keydown', monaco);
    press({ key: 'F1' }, textarea);
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'shortcuts' });
    expect(monaco).not.toHaveBeenCalled();
    // Ctrl+E (Monaco binds it too) is ours as well.
    ctrl('e', {}, textarea);
    expect(monaco).not.toHaveBeenCalled();
  });

  it('Ctrl+Shift+P / Ctrl+P open the palette in commands / objects mode', () => {
    ctrl('P', { shiftKey: true });
    expect(usePalette.getState()).toMatchObject({ open: true, mode: 'commands' });
    ctrl('p');
    expect(usePalette.getState()).toMatchObject({ open: true, mode: 'objects' });
  });

  it('zoom keys change the font scale and are still prevented from zooming the browser', () => {
    expect(ctrl('=')).toBe(true);
    expect(useLayout.getState().fontScale).toBe(1.1);
    ctrl('+', { shiftKey: true });
    expect(useLayout.getState().fontScale).toBe(1.2);
    ctrl('-');
    expect(useLayout.getState().fontScale).toBe(1.1);
    press({ key: '+', code: 'NumpadAdd', ctrlKey: true });
    expect(useLayout.getState().fontScale).toBe(1.2);
    press({ key: '-', code: 'NumpadSubtract', ctrlKey: true });
    expect(useLayout.getState().fontScale).toBe(1.1);
    expect(ctrl('0')).toBe(true);
    expect(useLayout.getState().fontScale).toBe(1);
    // Clamped.
    for (let i = 0; i < 30; i++) ctrl('=');
    expect(useLayout.getState().fontScale).toBe(1.6);
    for (let i = 0; i < 30; i++) ctrl('-');
    expect(useLayout.getState().fontScale).toBe(0.8);
  });

  it('Ctrl+E selects the first tree node and focuses the tree', async () => {
    const { useTree } = await import('../store/tree');
    const { seedTree } = await import('../test/treeSeed');
    seedTree('c1', [{ db: 'shop', kind: 'Table', names: ['orders'] }]);
    useTree.setState({ childIds: new Map([['', ['conn:c1']], ...useTree.getState().childIds]) });
    const tree = document.createElement('div');
    tree.setAttribute('role', 'tree');
    tree.setAttribute('aria-label', 'Connections');
    tree.tabIndex = 0;
    document.body.append(tree);
    ctrl('e');
    expect(useTree.getState().selectedId).toBe('conn:c1');
    expect(tree).toHaveFocus();
    // A selection is kept.
    useTree.getState().select('conn:c1/db:shop');
    document.body.focus();
    ctrl('e');
    expect(useTree.getState().selectedId).toBe('conn:c1/db:shop');
  });

  it('Ctrl+E shows a hidden sidebar first', () => {
    useLayout.setState({ sidebarVisible: false });
    ctrl('e');
    expect(useLayout.getState().sidebarVisible).toBe(true);
  });
});

describe('right sidebar and per-group results shortcuts', () => {
  it('Ctrl+Alt+B toggles the right sidebar, also from a field', () => {
    expect(ctrl('b', { altKey: true })).toBe(true);
    expect(useLayout.getState().rightVisible).toBe(true);
    const field = document.createElement('input');
    document.body.append(field);
    ctrl('b', { altKey: true }, field);
    expect(useLayout.getState().rightVisible).toBe(false);
    // Plain Ctrl+B is still the left sidebar.
    ctrl('b');
    expect(useLayout.getState().sidebarVisible).toBe(false);
    expect(useLayout.getState().rightVisible).toBe(false);
  });

  it('Ctrl+Shift+H opens History and focuses its search, also from a field', async () => {
    const search = document.createElement('input');
    search.setAttribute('data-testid', 'history-search');
    const other = document.createElement('input');
    document.body.append(other);
    other.focus();
    expect(ctrl('H', { shiftKey: true }, other)).toBe(true);
    expect(useLayout.getState().rightVisible).toBe(true);
    // The panel mounts on the next render: the search is focused as soon as it exists.
    document.body.append(search);
    await new Promise((r) => setTimeout(r, 20));
    expect(search).toHaveFocus();
  });

  it('Ctrl+J and Ctrl+Shift+J act on the focused editor group only', () => {
    useLayout.setState({ groupResults: {} });
    const focused = useTabs.getState().focusedGroupId;
    ctrl('j');
    expect(useLayout.getState().groupResults[focused]?.visible).toBe(false);
    ctrl('j');
    expect(useLayout.getState().groupResults[focused]?.visible).toBe(true);
    expect(ctrl('J', { shiftKey: true })).toBe(true);
    expect(useLayout.getState().groupResults[focused]).toMatchObject({ maximized: true, visible: true });
    ctrl('J', { shiftKey: true });
    expect(useLayout.getState().groupResults[focused]?.maximized).toBe(false);
  });
});

describe('guards', () => {
  it('are ignored while a modal dialog is open', () => {
    const modal = document.createElement('div');
    modal.setAttribute('aria-modal', 'true');
    document.body.append(modal);
    ctrl('j');
    ctrl('P', { shiftKey: true });
    ctrl('=');
    press({ key: 'F1' });
    expect(useLayout.getState().resultsVisible).toBe(true);
    expect(usePalette.getState().open).toBe(false);
    expect(useLayout.getState().fontScale).toBe(1);
    expect(useDialogs.getState().request).toBeNull();
  });

  it('fields outside the editor keep their keys, except the palette keys', () => {
    const field = document.createElement('input');
    document.body.append(field);
    ctrl('j', {}, field);
    ctrl('=', {}, field);
    ctrl('1', {}, field);
    expect(useLayout.getState().resultsVisible).toBe(true);
    expect(useLayout.getState().fontScale).toBe(1);
    ctrl('p', {}, field);
    expect(usePalette.getState().open).toBe(true);
  });

  it('Ctrl+Alt combinations and repeats are not shortcuts', () => {
    ctrl('j', { altKey: true });
    ctrl('j', { repeat: true });
    expect(useLayout.getState().resultsVisible).toBe(true);
  });

  it('plain keys without Ctrl do nothing', () => {
    press({ key: 'j' });
    press({ key: '=' });
    press({ key: 'p' });
    expect(useLayout.getState().resultsVisible).toBe(true);
    expect(usePalette.getState().open).toBe(false);
  });
});
