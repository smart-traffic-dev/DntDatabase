import { act, fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useObjectData } from '../store/objectData';
import { usePalette } from '../store/palette';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { setActiveEditor } from '../tabs/query/activeEditor';
import { config, ids, mockBackend, resetStores } from '../test/backend';
import { seedTree } from '../test/treeSeed';
import { ThemeProvider } from '../theme/ThemeProvider';
import { CommandPalette } from './CommandPalette';
import { registerPaletteCommand } from './paletteCommands';
import { onQueryShortcut } from './queryShortcuts';

vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ close: async () => {} }) }));

const mount = () =>
  render(
    <ThemeProvider>
      <input aria-label="outside" />
      <CommandPalette />
    </ThemeProvider>,
  );

const press = (init: KeyboardEventInit & { key: string }, target: Element | Window = document.body) =>
  act(() => {
    target.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init }));
  });
const input = () => screen.getByRole('combobox');
/** Each option's name: its label, then the shortcut (or "coming soon"). */
const rows = () => screen.queryAllByRole('option').map((r) => r.getAttribute('aria-label') ?? r.textContent ?? '');
const selected = () => screen.getByRole('option', { selected: true }).getAttribute('aria-label');
/** Section headers in order, each with the labels of its rows. */
const sections = () =>
  screen.queryAllByRole('group').map((g) => ({
    title: g.getAttribute('aria-label'),
    rows: within(g)
      .queryAllByRole('option')
      .map((r) => r.querySelector('.palette-label')?.textContent ?? ''),
  }));
const modeChip = () => screen.getByRole('button', { name: /^Mode:/ });

beforeEach(() => {
  try {
    localStorage.clear();
  } catch {
    // No storage: the MRU starts empty anyway.
  }
  resetStores();
  useCompletion.reset();
  useObjectData.setState({ byTab: {} });
  usePalette.getState().hide();
  mockBackend({ tabs_save: () => null });
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  window.addEventListener('keydown', onQueryShortcut, true);
});
afterEach(() => window.removeEventListener('keydown', onQueryShortcut, true));

describe('command palette', () => {
  it('Ctrl+Shift+P opens it in Commands mode, rows grouped under their menu without a prefix', () => {
    mount();
    expect(screen.queryByRole('dialog', { name: 'Command palette' })).toBeNull();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    const dialog = screen.getByRole('dialog', { name: 'Command palette' });
    // Modal-less: the window shortcuts must keep working around it.
    expect(dialog).not.toHaveAttribute('aria-modal');
    // The mode is a chip, not a typed ">".
    expect(modeChip()).toHaveTextContent('Commands');
    expect(input()).toHaveValue('');
    expect(input()).toHaveFocus();
    const all = rows();
    expect(all).toContain('New query, Ctrl+T');
    expect(all).toContain('Command palette, Ctrl+Shift+P');
    expect(all).toContain('Keyboard shortcuts, F1');
    expect(all.some((r) => r.startsWith('File:'))).toBe(false);
    const titles = sections().map((s) => s.title);
    // Menu order; Edit has nothing to run without an editor.
    expect(titles.slice(0, 2)).toEqual(['File', 'View']);
    expect(sections().find((s) => s.title === 'File')?.rows).toContain('New query');
    // Disabled items are left out (no tab open)...
    expect(all.some((r) => r.startsWith('Save,'))).toBe(false);
    expect(all.some((r) => r.startsWith('Run all'))).toBe(false);
    // ...except the planned features, listed last.
    expect(titles.at(-1)).toBe('Coming soon');
    expect(sections().at(-1)?.rows).toEqual(
      expect.arrayContaining(['Tables list', 'ER diagram', 'Compare schemas…']),
    );
  });

  it('shortcuts render as keycaps', () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    const row = screen.getByRole('option', { name: 'Command palette, Ctrl+Shift+P' });
    expect([...row.querySelectorAll('kbd.keycap')].map((k) => k.textContent)).toEqual(['Ctrl', 'Shift', 'P']);
  });

  it('"Coming soon" rows are not runnable and the arrows skip them', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    await userEvent.setup().type(input(), 'er diagram');
    const soon = screen.getByRole('option', { name: 'ER diagram, coming soon' });
    expect(soon).toHaveAttribute('aria-disabled', 'true');
    expect(soon).toHaveTextContent('Coming soon');
    fireEvent.click(soon);
    expect(screen.getByRole('dialog', { name: 'Command palette' })).toBeInTheDocument();
    // Never the selected row, so Enter does not pick it either.
    expect(soon).toHaveAttribute('aria-selected', 'false');
  });

  it('typing filters fuzzily and highlights the matched characters; Enter runs the best match', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    await userEvent.setup().type(input(), 'nwqry');
    expect(rows()[0]).toBe('New query, Ctrl+T');
    expect(selected()).toBe('New query, Ctrl+T');
    const best = screen.getByRole('option', { selected: true });
    expect([...best.querySelectorAll('.palette-match')].map((m) => m.textContent).join('')).toBe('Nwqry');
    press({ key: 'Enter' }, input());
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useTabs.getState().tabs[0]?.kind).toBe('query');
  });

  it('the menu name still matches ("file new")', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    await userEvent.setup().type(input(), 'file new con');
    expect(rows()[0]).toBe('New connection…, Ctrl+Shift+N');
  });

  it('a command that ran is listed first under Recent next time', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    fireEvent.click(screen.getByRole('option', { name: /^Keyboard shortcuts/ }));
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    const [first] = sections();
    expect(first).toEqual({ title: 'Recent', rows: ['Keyboard shortcuts'] });
    // Listed once: not again under Help.
    expect(sections().find((s) => s.title === 'Help')?.rows ?? []).not.toContain('Keyboard shortcuts');
    expect(selected()).toBe('Keyboard shortcuts, F1');
    // Recent rows name their menu.
    expect(screen.getByRole('option', { selected: true }).querySelector('.palette-sub')).toHaveTextContent('Help');
  });

  it('an unusable store leaves Recent empty and the palette working', () => {
    const get = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('denied');
    });
    const set = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('denied');
    });
    try {
      mount();
      press({ key: 'P', ctrlKey: true, shiftKey: true });
      expect(sections()[0]?.title).toBe('File');
      fireEvent.click(screen.getByRole('option', { name: /^Keyboard shortcuts/ }));
      expect(useDialogs.getState().request).toEqual({ kind: 'shortcuts' });
    } finally {
      get.mockRestore();
      set.mockRestore();
    }
  });

  it('the mode chip and Tab switch between Commands and Objects', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    const user = userEvent.setup();
    await user.click(modeChip());
    expect(screen.getByRole('dialog', { name: 'Go to object' })).toBeInTheDocument();
    expect(modeChip()).toHaveTextContent('Objects');
    expect(input()).toHaveFocus();
    press({ key: 'Tab' }, input());
    expect(screen.getByRole('dialog', { name: 'Command palette' })).toBeInTheDocument();
    expect(input()).toHaveFocus();
  });

  it('arrows move the selection (wrapping), Esc closes and gives the focus back', async () => {
    const { getByLabelText } = mount();
    getByLabelText('outside').focus();
    press({ key: 'P', ctrlKey: true, shiftKey: true }, getByLabelText('outside'));
    expect(input()).toHaveFocus();
    const first = selected();
    press({ key: 'ArrowDown' }, input());
    expect(selected()).not.toBe(first);
    press({ key: 'ArrowUp' }, input());
    expect(selected()).toBe(first);
    press({ key: 'ArrowUp' }, input());
    // Wraps to the last runnable row ("Coming soon" rows below it are skipped).
    const runnable = screen.getAllByRole('option').filter((o) => o.getAttribute('aria-disabled') !== 'true');
    expect(selected()).toBe(runnable.at(-1)?.getAttribute('aria-label'));
    press({ key: 'Escape' }, input());
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(getByLabelText('outside')).toHaveFocus();
  });

  it('a click on a row runs it; a click elsewhere closes the palette', () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    fireEvent.click(screen.getByRole('option', { name: /^Keyboard shortcuts/ }));
    expect(useDialogs.getState().request).toEqual({ kind: 'shortcuts' });
    expect(screen.queryByRole('dialog')).toBeNull();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('lists registered extra commands only while they are enabled', () => {
    let on = false;
    const off = registerPaletteCommand({ id: 'test.extra', label: 'Test: Extra thing', enabled: () => on, run: () => {} });
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    expect(rows()).not.toContain('Extra thing');
    on = true;
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    // "Group: Action" labels are grouped under the group.
    expect(sections().find((s) => s.title === 'Test')?.rows).toEqual(['Extra thing']);
    off();
  });

  it('offers Monaco\'s own palette as "Editor: Command palette" when an editor is mounted', () => {
    const trigger = vi.fn();
    const id = useQueryDocs.getState().newQuery(null, null);
    setActiveEditor(id, { focus: () => {}, trigger });
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    const editor = screen.getAllByRole('group').find((g) => g.getAttribute('aria-label') === 'Editor')!;
    fireEvent.click(within(editor).getByRole('option', { name: 'Command palette' }));
    expect(trigger).toHaveBeenCalledWith('menu', 'editor.action.quickCommand', null);
    setActiveEditor(id, null);
  });

  it('shows an empty state when nothing matches', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    await userEvent.setup().type(input(), 'qqqqzzzz');
    expect(screen.getByRole('status')).toHaveTextContent('No matching commands');
  });
});

describe('quick open', () => {
  beforeEach(() => {
    seedTree(ids.prod, [
      { db: 'shop', kind: 'Table', names: ['orders', 'customers'] },
      { db: 'shop', kind: 'View', names: ['recent_orders'] },
      { db: 'shop', kind: 'Function', names: ['order_total'] },
    ]);
  });

  it('Ctrl+P lists the loaded objects as "connection › database › name"', () => {
    mount();
    press({ key: 'p', ctrlKey: true });
    expect(screen.getByRole('dialog', { name: 'Go to object' })).toBeInTheDocument();
    expect(input()).toHaveValue('');
    expect(rows()).toContain('shop-prod › shop › orders');
    expect(rows()).toHaveLength(4);
  });

  it('typing narrows the list; Enter opens the table (Data first)', async () => {
    mount();
    press({ key: 'p', ctrlKey: true });
    await userEvent.setup().type(input(), 'ordr');
    expect(rows()[0]).toBe('shop-prod › shop › orders');
    press({ key: 'Enter' }, input());
    const tab = useTabs.getState().tabs[0];
    expect(tab).toMatchObject({ kind: 'object', title: 'orders', connectionId: ids.prod });
    expect(useObjectData.getState().byTab[tab!.id]?.sub).toBeUndefined();
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('Ctrl+Enter opens the DDL', async () => {
    mount();
    press({ key: 'p', ctrlKey: true });
    await userEvent.setup().type(input(), 'customers');
    press({ key: 'Enter', ctrlKey: true }, input());
    const tab = useTabs.getState().tabs[0]!;
    expect(tab.title).toBe('customers');
    expect(useObjectData.getState().byTab[tab.id]?.sub).toBe('ddl');
  });

  it('typing > switches to commands, Backspace on the empty input switches back', async () => {
    mount();
    press({ key: 'p', ctrlKey: true });
    const user = userEvent.setup();
    expect(modeChip()).toHaveTextContent('Objects');
    await user.type(input(), '>');
    expect(screen.getByRole('dialog', { name: 'Command palette' })).toBeInTheDocument();
    // The ">" became the chip; the input is empty.
    expect(modeChip()).toHaveTextContent('Commands');
    expect(input()).toHaveValue('');
    expect(rows()).toContain('New query, Ctrl+T');
    await user.type(input(), '{Backspace}');
    expect(screen.getByRole('dialog', { name: 'Go to object' })).toBeInTheDocument();
    expect(rows()).toHaveLength(4);
  });

  it('Esc closes the palette and gives the focus back to its opener', async () => {
    const { getByLabelText } = mount();
    getByLabelText('outside').focus();
    press({ key: 'p', ctrlKey: true }, getByLabelText('outside'));
    await userEvent.setup().keyboard('{Escape}');
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(getByLabelText('outside')).toHaveFocus();
  });

  it('opens from inside a text field, and points at the tree when nothing is loaded', () => {
    useTree.getState().reset();
    const { getByLabelText } = mount();
    getByLabelText('outside').focus();
    press({ key: 'p', ctrlKey: true }, getByLabelText('outside'));
    expect(within(screen.getByRole('dialog')).getByRole('status')).toHaveTextContent('No objects loaded yet');
  });

  it('never asks the server while typing', async () => {
    const mock = mockBackend({});
    mount();
    press({ key: 'p', ctrlKey: true });
    await userEvent.setup().type(input(), 'order');
    expect(mock).not.toHaveBeenCalled();
  });
});
