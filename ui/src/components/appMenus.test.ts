import { describe, expect, it } from 'vitest';
import type { MenuAction, MenuEntry } from './ActionMenu';
import { buildMenus, type MenuState } from './appMenus';

const base: MenuState = {
  sidebarVisible: true,
  rightSidebarVisible: false,
  resultsVisible: true,
  hasTab: false,
  canReopen: false,
  queryTabId: null,
  hasEditor: false,
  runnable: false,
  stoppable: false,
  hasSelection: false,
  layout: 'single',
  groupCount: 1,
  maximized: false,
};

const items = (menu: string, state: Partial<MenuState> = {}) =>
  (buildMenus({ ...base, ...state }).find((m) => m.label === menu)?.items ?? []).filter((i): i is MenuAction => 'label' in i);
const item = (menu: string, label: string, state: Partial<MenuState> = {}) => items(menu, state).find((i) => i.label === label);
const isSep = (e: MenuEntry) => 'separator' in e;

describe('buildMenus', () => {
  it('has the seven menus in order with unique mnemonics that are letters of the label', () => {
    const menus = buildMenus(base);
    expect(menus.map((m) => m.label)).toEqual(['File', 'Edit', 'View', 'Query', 'Favorites', 'Tools', 'Help']);
    expect(menus.map((m) => m.mnemonic)).toEqual(['F', 'E', 'V', 'Q', 'a', 'T', 'H']);
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
      'Export results…',
      '-',
      'Import connections…',
      'Export connections…',
      '-',
      'Close tab',
      'Reopen closed tab',
      'Exit',
    ]);
    expect(item('File', 'New query')?.hint).toBe('Ctrl+T');
    expect(item('File', 'New connection…')?.hint).toBe('Ctrl+Shift+N');
    expect(item('File', 'Reopen closed tab')?.hint).toBe('Ctrl+Shift+T');
    expect(item('File', 'Reopen closed tab')?.disabled).toBe(true);
    expect(item('File', 'Reopen closed tab', { canReopen: true })?.disabled).toBe(false);
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

  it('View ticks the panel toggles; the theme lives in Settings > Appearance', () => {
    const checked = (state: Partial<MenuState>) =>
      items('View', state)
        .filter((i) => i.checked && !i.label.startsWith('Layout:'))
        .map((i) => i.label);
    expect(checked({ sidebarVisible: false, resultsVisible: false })).toEqual([]);
    expect(checked({})).toEqual(['Show sidebar', 'Show results']);
    expect(checked({ rightSidebarVisible: true })).toEqual(['Show sidebar', 'Show results', 'Right sidebar']);
    expect(item('View', 'Light')).toBeUndefined();
    expect(item('View', 'Reset layout')?.checked).toBeUndefined();
  });

  it('View offers the editor layouts (current one ticked) and the splits', () => {
    const layouts = (state: Partial<MenuState>) =>
      items('View', state).filter((i) => i.label.startsWith('Layout:')).map((i) => [i.label, i.hint, i.checked]);
    expect(layouts({ layout: 'rows', groupCount: 2 })).toEqual([
      ['Layout: Single', 'Ctrl+K 1', false],
      ['Layout: Two columns', 'Ctrl+K 2', false],
      ['Layout: Two rows', 'Ctrl+K 3', true],
      ['Layout: Grid 2 × 2', 'Ctrl+K 4', false],
    ]);
    expect(item('View', 'Split right', { hasTab: true })?.hint).toBe('Ctrl+\\');
    expect(item('View', 'Split right', { hasTab: true, groupCount: 4 })?.disabled).toBe(true);
    expect(item('View', 'Split down')?.disabled).toBe(true);
    expect(item('View', 'Close group', { groupCount: 2 })?.disabled).toBe(false);
    expect(item('View', 'Maximize group', { groupCount: 2 })).toMatchObject({ hint: 'Ctrl+K Ctrl+M', disabled: false, checked: false });
    expect(item('View', 'Maximize group', { groupCount: 2, maximized: true })?.checked).toBe(true);
    expect(item('View', 'Maximize group')?.disabled).toBe(true);
  });

  it('View > Appearance… and Help > Keyboard shortcuts open Settings on their tab', async () => {
    const { useDialogs } = await import('../store/dialogs');
    item('View', 'Appearance…')?.run();
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'appearance' });
    item('Help', 'Keyboard shortcuts')?.run();
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'shortcuts' });
    useDialogs.getState().closeRequest();
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

  it('the old toolbar placeholders are disabled "Coming soon" items in the menus that will own them', () => {
    const soon = (menu: string) => items(menu).filter((i) => i.hint === 'Coming soon');
    expect(soon('View').map((i) => i.label)).toEqual(['Tables list', 'Views list', 'Functions list']);
    expect(soon('Tools').map((i) => i.label)).toEqual(['Import / Export data…', 'Backup…', 'ER diagram', 'Compare schemas…']);
    for (const i of [...soon('View'), ...soon('Tools')]) expect(i.disabled, i.label).toBe(true);
  });

  it('Tools and Help entries', () => {
    expect(items('Tools').filter((i) => !i.disabled).map((i) => i.label)).toEqual(['Settings…', 'Developer tools']);
    expect(item('Tools', 'Developer tools')?.hint).toBe('Ctrl+Shift+I');
    expect(item('Tools', 'Settings…')?.hint).toBe('Ctrl+,');
    expect(item('Help', 'Keyboard shortcuts')?.hint).toBe('F1');
    const group = /^(Layout:|Split|Close group|Maximize group)/;
    expect(items('View').filter((i) => i.hint && i.hint !== 'Coming soon' && !group.test(i.label)).map((i) => [i.label, i.hint])).toEqual([
      ['Show sidebar', 'Ctrl+B'],
      ['Show results', 'Ctrl+J'],
      ['Maximize results', 'Ctrl+Shift+J'],
      ['Right sidebar', 'Ctrl+Alt+B'],
      ['History', 'Ctrl+Shift+H'],
      ['Filter rows (Data tab)', 'F3'],
      ['Focus connection tree', 'Ctrl+E'],
      ['Command palette', 'Ctrl+Shift+P'],
      ['Go to object', 'Ctrl+P'],
      ['Zoom in', 'Ctrl+='],
      ['Zoom out', 'Ctrl+-'],
      ['Reset zoom', 'Ctrl+0'],
    ]);
    expect(items('Help').map((i) => i.label)).toEqual(['Keyboard shortcuts', 'About DntDatabase']);
  });
});
