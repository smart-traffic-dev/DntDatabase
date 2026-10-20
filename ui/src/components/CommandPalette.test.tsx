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
const rows = () => screen.queryAllByRole('option').map((r) => r.textContent ?? '');
const selected = () => screen.getByRole('option', { selected: true }).textContent;

beforeEach(() => {
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
  it('Ctrl+Shift+P opens it with a > prompt listing the enabled menu actions with their hints', () => {
    mount();
    expect(screen.queryByRole('dialog', { name: 'Command palette' })).toBeNull();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    const dialog = screen.getByRole('dialog', { name: 'Command palette' });
    // Modal-less: the window shortcuts must keep working around it.
    expect(dialog).not.toHaveAttribute('aria-modal');
    expect(input()).toHaveValue('>');
    expect(input()).toHaveFocus();
    const all = rows();
    expect(all.some((r) => r.startsWith('File: New query') && r.includes('Ctrl+T'))).toBe(true);
    expect(all.some((r) => r.startsWith('View: Command palette') && r.includes('Ctrl+Shift+P'))).toBe(true);
    expect(all.some((r) => r.startsWith('Help: Keyboard shortcuts') && r.includes('F1'))).toBe(true);
    // Disabled items are left out (no tab open).
    expect(all.some((r) => r.startsWith('File: Save'))).toBe(false);
    expect(all.some((r) => r.startsWith('Query: Run all'))).toBe(false);
  });

  it('typing filters fuzzily; Enter runs the best match and closes', async () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    await userEvent.setup().type(input(), 'nwqry');
    expect(rows()[0]).toMatch(/^File: New query/);
    expect(selected()).toMatch(/^File: New query/);
    press({ key: 'Enter' }, input());
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useTabs.getState().tabs[0]?.kind).toBe('query');
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
    expect(selected()).toBe(rows().at(-1));
    press({ key: 'Escape' }, input());
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(getByLabelText('outside')).toHaveFocus();
  });

  it('a click on a row runs it; a click elsewhere closes the palette', () => {
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    fireEvent.click(screen.getByRole('option', { name: /^Help: Keyboard shortcuts/ }));
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
    expect(rows().some((r) => r.startsWith('Test: Extra thing'))).toBe(false);
    on = true;
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    expect(rows().some((r) => r.startsWith('Test: Extra thing'))).toBe(true);
    off();
  });

  it('offers Monaco\'s own palette as "Editor: Command palette" when an editor is mounted', () => {
    const trigger = vi.fn();
    const id = useQueryDocs.getState().newQuery(null, null);
    setActiveEditor(id, { focus: () => {}, trigger });
    mount();
    press({ key: 'P', ctrlKey: true, shiftKey: true });
    fireEvent.click(screen.getByRole('option', { name: /^Editor: Command palette/ }));
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
    expect(rows()).toContain('shop-prod › shop ›orders');
    expect(rows()).toHaveLength(4);
  });

  it('typing narrows the list; Enter opens the table (Data first)', async () => {
    mount();
    press({ key: 'p', ctrlKey: true });
    await userEvent.setup().type(input(), 'ordr');
    expect(rows()[0]).toBe('shop-prod › shop ›orders');
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

  it('typing > switches to commands, deleting it switches back', async () => {
    mount();
    press({ key: 'p', ctrlKey: true });
    const user = userEvent.setup();
    await user.type(input(), '>');
    expect(screen.getByRole('dialog', { name: 'Command palette' })).toBeInTheDocument();
    expect(rows().some((r) => r.startsWith('File: New query'))).toBe(true);
    await user.type(input(), '{Backspace}');
    expect(screen.getByRole('dialog', { name: 'Go to object' })).toBeInTheDocument();
    expect(rows()).toHaveLength(4);
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
