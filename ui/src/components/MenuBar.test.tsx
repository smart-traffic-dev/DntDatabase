import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useDialogs } from '../store/dialogs';
import { EDITOR_RATIO_DEFAULT, useLayout } from '../store/layout';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
import { setActiveEditor } from '../tabs/query/activeEditor';
import { ThemeProvider } from '../theme/ThemeProvider';
import { callsOf, mockBackend, resetStores } from '../test/backend';
import { onQueryShortcut } from './queryShortcuts';
import { MenuBar } from './MenuBar';

const win = vi.hoisted(() => ({ close: vi.fn(async () => {}) }));
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => win }));

const mount = () =>
  render(
    <ThemeProvider>
      <MenuBar />
    </ThemeProvider>,
  );

const top = (name: string) => screen.getByRole('menuitem', { name });

beforeEach(() => {
  resetStores();
  useLayout.getState().reset();
  localStorage.clear();
  win.close.mockClear();
  mockBackend({ open_devtools: () => null });
});

describe('MenuBar', () => {
  it('renders the menubar with the seven menus, collapsed', () => {
    mount();
    const bar = screen.getByRole('menubar');
    const labels = within(bar)
      .getAllByRole('menuitem')
      .map((b) => b.textContent);
    expect(labels).toEqual(['File', 'Edit', 'View', 'Query', 'Favorites', 'Tools', 'Help']);
    for (const b of within(bar).getAllByRole('menuitem')) {
      expect(b).toHaveAttribute('aria-haspopup', 'menu');
      expect(b).toHaveAttribute('aria-expanded', 'false');
    }
    expect(screen.queryByRole('menu')).toBeNull();
  });

  it('underlines the mnemonic letter', () => {
    mount();
    expect(top('File').querySelector('u')?.textContent).toBe('F');
  });

  it('the dropdown opens under its label, never in the window corner', () => {
    mount();
    const rect = (left: number) => ({ left, right: left + 40, top: 0, bottom: 32, width: 40, height: 32, x: left, y: 0, toJSON: () => ({}) });
    vi.spyOn(top('Query'), 'getBoundingClientRect').mockReturnValue(rect(160) as DOMRect);
    vi.spyOn(top('View'), 'getBoundingClientRect').mockReturnValue(rect(110) as DOMRect);
    fireEvent.click(top('Query'));
    const menu = screen.getByRole('menu', { name: 'Query' });
    expect(menu.style.left).toBe('160px');
    expect(menu.style.top).toBe('32px');
    expect(menu.style.visibility).toBe('');
    // Hovering another label moves the dropdown under it.
    fireEvent.mouseEnter(top('View'));
    expect(screen.getByRole('menu', { name: 'View' }).style.left).toBe('110px');
  });

  it('click opens the dropdown, a second click closes it', () => {
    mount();
    fireEvent.click(top('File'));
    expect(top('File')).toHaveAttribute('aria-expanded', 'true');
    expect(screen.getByRole('menu', { name: 'File' })).toBeInTheDocument();
    expect(screen.getByRole('menuitem', { name: /New query/ })).toBeInTheDocument();
    fireEvent.mouseDown(top('File'));
    fireEvent.click(top('File'));
    expect(screen.queryByRole('menu')).toBeNull();
    expect(top('File')).toHaveAttribute('aria-expanded', 'false');
  });

  it('hovering another label switches the open menu; hovering while closed does nothing', () => {
    mount();
    fireEvent.mouseEnter(top('Edit'));
    expect(screen.queryByRole('menu')).toBeNull();
    fireEvent.click(top('File'));
    fireEvent.mouseEnter(top('Tools'));
    expect(screen.queryByRole('menu', { name: 'File' })).toBeNull();
    expect(screen.getByRole('menu', { name: 'Tools' })).toBeInTheDocument();
  });

  it('menu buttons do not take focus on mousedown and a pointer-opened menu leaves the focus alone', () => {
    const outside = document.createElement('input');
    document.body.appendChild(outside);
    outside.focus();
    mount();
    const down = new MouseEvent('mousedown', { bubbles: true, cancelable: true, button: 0 });
    top('File').dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
    fireEvent.click(top('File'));
    expect(document.activeElement).toBe(outside);
    outside.remove();
  });

  it('ArrowRight / ArrowLeft move to the next / previous menu (wrapping)', () => {
    mount();
    fireEvent.click(top('File'));
    fireEvent.keyDown(document.activeElement ?? document.body, { key: 'ArrowRight' });
    expect(screen.getByRole('menu', { name: 'Edit' })).toBeInTheDocument();
    fireEvent.keyDown(document.activeElement ?? document.body, { key: 'ArrowLeft' });
    fireEvent.keyDown(document.activeElement ?? document.body, { key: 'ArrowLeft' });
    expect(screen.getByRole('menu', { name: 'Help' })).toBeInTheDocument();
    // Opened with the keyboard: the first item has the focus.
    expect(document.activeElement).toHaveAttribute('role', 'menuitem');
    expect(screen.getByRole('menu', { name: 'Help' }).contains(document.activeElement)).toBe(true);
  });

  it('Esc closes the menu', () => {
    mount();
    fireEvent.click(top('View'));
    fireEvent.keyDown(document.body, { key: 'Escape' });
    expect(screen.queryByRole('menu')).toBeNull();
  });

  it('Esc returns the focus to where it was', () => {
    const outside = document.createElement('input');
    document.body.appendChild(outside);
    outside.focus();
    mount();
    fireEvent.keyDown(window, { key: 'f', altKey: true });
    expect(screen.getByRole('menu', { name: 'File' }).contains(document.activeElement)).toBe(true);
    fireEvent.keyDown(document.activeElement ?? document.body, { key: 'Escape' });
    expect(screen.queryByRole('menu')).toBeNull();
    expect(document.activeElement).toBe(outside);
    outside.remove();
  });

  it('Alt+F / E / V / Q / T / H open the matching menu', () => {
    mount();
    const cases: [string, string][] = [
      ['f', 'File'],
      ['e', 'Edit'],
      ['v', 'View'],
      ['q', 'Query'],
      ['t', 'Tools'],
      ['h', 'Help'],
    ];
    for (const [key, name] of cases) {
      fireEvent.keyDown(window, { key, altKey: true });
      expect(screen.getByRole('menu', { name })).toBeInTheDocument();
    }
  });

  it('Alt alone and Alt+<other letter> do nothing; Alt+letter is ignored while a modal dialog is open', () => {
    mount();
    fireEvent.keyDown(window, { key: 'Alt', altKey: true });
    fireEvent.keyDown(window, { key: 'x', altKey: true });
    expect(screen.queryByRole('menu')).toBeNull();
    const modal = document.createElement('div');
    modal.setAttribute('aria-modal', 'true');
    document.body.appendChild(modal);
    fireEvent.keyDown(window, { key: 'f', altKey: true });
    expect(screen.queryByRole('menu')).toBeNull();
    modal.remove();
  });

  it('disabled items are present but do nothing and are skipped by the arrow keys', () => {
    mount();
    fireEvent.click(top('File'));
    const save = screen.getByRole('menuitem', { name: /^Save$|Save\s*Ctrl\+S/ });
    expect(save).toBeDisabled();
    fireEvent.click(save);
    // The menu stays open and nothing was asked of the store.
    expect(screen.getByRole('menu', { name: 'File' })).toBeInTheDocument();
    // Arrow navigation never lands on a disabled item.
    fireEvent.keyDown(window, { key: 'ArrowDown' });
    for (let i = 0; i < 12; i++) {
      expect(document.activeElement).not.toBeDisabled();
      fireEvent.keyDown(document.activeElement ?? document.body, { key: 'ArrowDown' });
    }
  });

  it('File > New connection asks for the dialog and closes the menu', () => {
    mount();
    fireEvent.click(top('File'));
    fireEvent.click(screen.getByRole('menuitem', { name: /New connection/ }));
    expect(useDialogs.getState().request).toEqual({ kind: 'newConnection' });
    expect(screen.queryByRole('menu')).toBeNull();
  });

  it('File > New query opens a query tab', () => {
    mount();
    fireEvent.click(top('File'));
    fireEvent.click(screen.getByRole('menuitem', { name: /New query/ }));
    expect(useTabs.getState().tabs.some((t) => t.kind === 'query')).toBe(true);
  });

  it('File > Exit asks the window to close', () => {
    mount();
    fireEvent.click(top('File'));
    fireEvent.click(screen.getByRole('menuitem', { name: /Exit/ }));
    expect(win.close).toHaveBeenCalledOnce();
  });

  it('Help items request their dialogs', () => {
    mount();
    fireEvent.click(top('Help'));
    fireEvent.click(screen.getByRole('menuitem', { name: /^Keyboard shortcuts/ }));
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'shortcuts' });
    fireEvent.click(top('Help'));
    fireEvent.click(screen.getByRole('menuitem', { name: 'About DntDatabase' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'about' });
  });

  it('Tools > Developer tools calls the backend command', async () => {
    mount();
    fireEvent.click(top('Tools'));
    await act(async () => {
      fireEvent.click(screen.getByRole('menuitem', { name: /Developer tools/ }));
    });
    expect(callsOf('open_devtools')).toHaveLength(1);
  });

  it('Developer tools stays quiet when the command fails (no backend)', async () => {
    mockBackend({});
    mount();
    fireEvent.click(top('Tools'));
    await act(async () => {
      fireEvent.click(screen.getByRole('menuitem', { name: /Developer tools/ }));
    });
    expect(callsOf('open_devtools')).toHaveLength(1);
  });

  it('View > Appearance… opens Settings on its Appearance tab (the theme moved there)', () => {
    mount();
    fireEvent.click(top('View'));
    expect(screen.queryByRole('menuitemcheckbox', { name: /Dark/ })).toBeNull();
    fireEvent.click(screen.getByRole('menuitem', { name: 'Appearance…' }));
    expect(useDialogs.getState().request).toEqual({ kind: 'settings', tab: 'appearance' });
  });

  it('View > Show sidebar toggles the layout and Reset layout restores the defaults', () => {
    mount();
    fireEvent.click(top('View'));
    fireEvent.click(screen.getByRole('menuitemcheckbox', { name: /Show sidebar/ }));
    expect(useLayout.getState().sidebarVisible).toBe(false);
    useLayout.getState().setSidebarWidth(400);
    useLayout.getState().setEditorRatio(0.3);
    fireEvent.click(top('View'));
    expect(screen.getByRole('menuitemcheckbox', { name: /Show sidebar/ })).toHaveAttribute('aria-checked', 'false');
    fireEvent.click(screen.getByRole('menuitem', { name: 'Reset layout' }));
    const s = useLayout.getState();
    expect([s.sidebarWidth, s.sidebarVisible, s.editorRatio]).toEqual([280, true, EDITOR_RATIO_DEFAULT]);
  });

  it('Edit items run on the active query tab editor', () => {
    const id = useQueryDocs.getState().newQuery(null, null);
    const editor = { focus: vi.fn(), trigger: vi.fn() };
    setActiveEditor(id, editor);
    mount();
    fireEvent.click(top('Edit'));
    fireEvent.click(screen.getByRole('menuitem', { name: /Undo/ }));
    expect(editor.focus).toHaveBeenCalled();
    expect(editor.trigger).toHaveBeenCalledWith('menu', 'undo', null);
    fireEvent.click(top('Edit'));
    fireEvent.click(screen.getByRole('menuitem', { name: /Replace/ }));
    expect(editor.trigger).toHaveBeenLastCalledWith('menu', 'editor.action.startFindReplaceAction', null);
    setActiveEditor(id, null);
  });

  it('Edit items are disabled without an editor', () => {
    mount();
    fireEvent.click(top('Edit'));
    expect(screen.getByRole('menuitem', { name: /Undo/ })).toBeDisabled();
  });

  it('Query items are disabled without a runnable query tab', () => {
    mount();
    fireEvent.click(top('Query'));
    for (const name of [/Run statement/, /Run all/, /Run selection/, /Stop/, /Explain/]) {
      expect(screen.getByRole('menuitem', { name })).toBeDisabled();
    }
  });

  it('Ctrl+B toggles the sidebar and Ctrl+Shift+I opens the developer tools', async () => {
    onQueryShortcut(new KeyboardEvent('keydown', { key: 'b', ctrlKey: true }));
    expect(useLayout.getState().sidebarVisible).toBe(false);
    onQueryShortcut(new KeyboardEvent('keydown', { key: 'b', ctrlKey: true }));
    expect(useLayout.getState().sidebarVisible).toBe(true);
    await act(async () => {
      onQueryShortcut(new KeyboardEvent('keydown', { key: 'I', ctrlKey: true, shiftKey: true }));
    });
    expect(callsOf('open_devtools')).toHaveLength(1);
  });
});
