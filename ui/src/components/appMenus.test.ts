import { describe, expect, it, vi } from 'vitest';
import type { MenuAction, MenuEntry } from './ActionMenu';
import { buildMenus, type MenuState } from './appMenus';

const base: MenuState = {
  theme: 'system',
  setTheme: () => {},
  sidebarVisible: true,
  hasTab: false,
  queryTabId: null,
  hasEditor: false,
  runnable: false,
  stoppable: false,
  hasSelection: false,
};

const items = (menu: string, state: Partial<MenuState> = {}) =>
  (buildMenus({ ...base, ...state }).find((m) => m.label === menu)?.items ?? []).filter((i): i is MenuAction => 'label' in i);
const item = (menu: string, label: string, state: Partial<MenuState> = {}) => items(menu, state).find((i) => i.label === label);
const isSep = (e: MenuEntry) => 'separator' in e;

describe('buildMenus', () => {
  it('has the six menus in order with unique mnemonics that are letters of the label', () => {
    const menus = buildMenus(base);
    expect(menus.map((m) => m.label)).toEqual(['File', 'Edit', 'View', 'Query', 'Tools', 'Help']);
    expect(menus.map((m) => m.mnemonic)).toEqual(['F', 'E', 'V', 'Q', 'T', 'H']);
    for (const m of menus) expect(m.label).toContain(m.mnemonic);
  });

  it('File lists its items in order with separators', () => {
    const file = buildMenus(base)[0]?.items ?? [];
    expect(file.map((e) => (isSep(e) ? '-' : (e as MenuAction).label))).toEqual([
      'New connection…',
      'New query',
      'Open SQL file…',
      'Save',
      'Save as…',
      '-',
      'Import connections…',
      'Export connections…',
      '-',
      'Close tab',
      'Exit',
    ]);
    expect(item('File', 'New query')?.hint).toBe('Ctrl+T');
    expect(item('File', 'Exit')?.hint).toBe('Alt+F4');
  });

  it('Save, Save as and Close tab are disabled without a tab; New and Open stay enabled', () => {
    expect(item('File', 'Save')?.disabled).toBe(true);
    expect(item('File', 'Save as…')?.disabled).toBe(true);
    expect(item('File', 'Close tab')?.disabled).toBe(true);
    expect(item('File', 'New query')?.disabled).toBeFalsy();
    expect(item('File', 'Open SQL file…')?.disabled).toBeFalsy();
  });

  it('Save is enabled on a query tab; Close tab on any tab', () => {
    expect(item('File', 'Save', { hasTab: true, queryTabId: 'q1' })?.disabled).toBe(false);
    expect(item('File', 'Save as…', { hasTab: true, queryTabId: 'q1' })?.disabled).toBe(false);
    // An object tab: closable, nothing to save.
    expect(item('File', 'Save', { hasTab: true })?.disabled).toBe(true);
    expect(item('File', 'Close tab', { hasTab: true })?.disabled).toBe(false);
  });

  it('Edit items need an editor', () => {
    for (const i of items('Edit')) expect(i.disabled, i.label).toBe(true);
    for (const i of items('Edit', { hasEditor: true })) expect(i.disabled, i.label).toBe(false);
    expect(items('Edit').map((i) => i.label)).toEqual(['Undo', 'Redo', 'Cut', 'Copy', 'Paste', 'Select all', 'Find', 'Replace', 'Format SQL', 'Toggle comment', 'Duplicate line', 'Upper case', 'Lower case']);
  });

  it('View ticks the current theme and the sidebar toggle', () => {
    const checked = (state: Partial<MenuState>) => items('View', state).filter((i) => i.checked).map((i) => i.label);
    expect(checked({ theme: 'light', sidebarVisible: false })).toEqual(['Light']);
    expect(checked({ theme: 'dark' })).toEqual(['Dark', 'Show sidebar']);
    expect(checked({ theme: 'system' })).toEqual(['System', 'Show sidebar']);
    expect(item('View', 'Light')?.checked).toBe(false);
    expect(item('View', 'Reset layout')?.checked).toBeUndefined();
  });

  it('choosing a theme calls setTheme', () => {
    const setTheme = vi.fn();
    item('View', 'Dark', { setTheme })?.run();
    expect(setTheme).toHaveBeenCalledWith('dark');
  });

  it('Query items are disabled without a runnable query tab', () => {
    for (const i of items('Query')) expect(i.disabled, i.label).toBe(true);
    // A query tab that cannot run (no connection / running): still disabled.
    for (const i of items('Query', { queryTabId: 'q1', runnable: false })) expect(i.disabled, i.label).toBe(true);
  });

  it('Query items follow the toolbar rules', () => {
    const run = { queryTabId: 'q1', runnable: true };
    expect(item('Query', 'Run statement', run)?.disabled).toBe(false);
    expect(item('Query', 'Run all', run)?.disabled).toBe(false);
    expect(item('Query', 'Explain', run)?.disabled).toBe(false);
    // Run selection needs selected text; Stop needs a running statement.
    expect(item('Query', 'Run selection', run)?.disabled).toBe(true);
    expect(item('Query', 'Run selection', { ...run, hasSelection: true })?.disabled).toBe(false);
    expect(item('Query', 'Stop', run)?.disabled).toBe(true);
    expect(item('Query', 'Stop', { queryTabId: 'q1', runnable: false, stoppable: true })?.disabled).toBe(false);
  });

  it('Tools and Help entries', () => {
    expect(items('Tools').map((i) => i.label)).toEqual(['Settings…', 'Developer tools']);
    expect(item('Tools', 'Developer tools')?.hint).toBe('Ctrl+Shift+I');
    expect(items('Help').map((i) => i.label)).toEqual(['Keyboard shortcuts', 'About DntDatabase']);
  });
});
