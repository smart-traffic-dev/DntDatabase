import { getCurrentWindow } from '@tauri-apps/api/window';
import { commands } from '../bindings';
import { explainQuery, runFromEditor, stopQuery } from '../store/runQuery';
import { useDialogs } from '../store/dialogs';
import { useLayout } from '../store/layout';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs, type Tab } from '../store/tabs';
import type { Theme } from '../theme/ThemeProvider';
import { EDIT_COMMANDS, runEditorCommand } from '../tabs/query/activeEditor';
import type { MenuAction, MenuEntry } from './ActionMenu';
import { transferItems } from './ActionMenu';
import { addActiveQueryToFavorites, listFavoritesForMenu, openSnippetsManager } from './favoritesActions';
import { openNewQuery } from './newQuery';
import { showCommandPalette, showQuickOpen } from '../store/palette';
import { closeTab, reopenClosedTab } from './tabActions';
import { focusConnectionTree, toggleResults, zoomIn, zoomOut, zoomReset } from './windowActions';

/** What the menu model depends on: plain data, so the builder is a pure function. */
export interface MenuState {
  theme: Theme;
  setTheme(theme: Theme): void;
  sidebarVisible: boolean;
  /** The result panel of query tabs is shown. */
  resultsVisible: boolean;
  /** A tab is open. */
  hasTab: boolean;
  /** A closed tab can be reopened. */
  canReopen: boolean;
  /** The active tab is a query tab: its id. */
  queryTabId: string | null;
  /** The active query tab has a mounted editor (the Edit menu acts on it). */
  hasEditor: boolean;
  /** The active query tab can run (bound to a stored connection, idle, not starting). */
  runnable: boolean;
  /** A run of the active query tab can be stopped. */
  stoppable: boolean;
  /** The editor of the active query tab has selected text. */
  hasSelection: boolean;
}

export interface AppMenu {
  id: 'file' | 'edit' | 'view' | 'query' | 'favorites' | 'tools' | 'help';
  label: string;
  /** The Alt+<key> mnemonic: a letter of `label`, underlined in the bar. */
  mnemonic: string;
  items: MenuEntry[];
}

const sep: MenuEntry = { separator: true };

/** Developer tools: the backend opens the webview's DevTools. Quiet when there is no backend (browser preview). */
export async function openDevtools() {
  try {
    await commands.openDevtools();
  } catch {
    // Not inside Tauri, or the call failed: nothing to show.
  }
}

/** Exit goes through the window's close request, so the close guard (open transactions, unsaved tabs) runs. */
function exitApp() {
  try {
    void getCurrentWindow()
      .close()
      .catch(() => {});
  } catch {
    // Not inside Tauri (browser preview).
  }
}

const activeTab = (): Tab | undefined => {
  const { tabs, activeId } = useTabs.getState();
  return tabs.find((t) => t.id === activeId);
};

/** The saved favorites (first 20) after a separator, as Navicat's Favorites menu lists them. */
function favoriteItems(): MenuEntry[] {
  const favorites = listFavoritesForMenu();
  return favorites.length ? [sep, ...favorites.map((f) => ({ label: f.label, run: f.open }))] : [];
}

/** The seven top-level menus, left to right. Items that cannot run now are present but disabled. */
export function buildMenus(s: MenuState): AppMenu[] {
  const dialog = useDialogs.getState().requestDialog;
  const q = s.queryTabId;
  const edit = (label: string, hint: string | undefined, id: string): MenuAction => ({
    label,
    hint,
    disabled: !s.hasEditor,
    run: () => void runEditorCommand(id),
  });
  const theme = (label: string, value: Theme): MenuAction => ({
    label,
    checked: s.theme === value,
    run: () => s.setTheme(value),
  });

  return [
    {
      id: 'file',
      label: 'File',
      mnemonic: 'F',
      items: [
        { hint: 'Ctrl+Shift+N', label: 'New connection…', run: () => dialog({ kind: 'newConnection' }) },
        { label: 'New query', hint: 'Ctrl+T', run: () => openNewQuery() },
        { label: 'Open SQL file…', hint: 'Ctrl+O', run: () => void useQueryDocs.getState().openFile() },
        { label: 'Save', hint: 'Ctrl+S', disabled: !q, run: () => q && void useQueryDocs.getState().saveFile(q, false) },
        { label: 'Save as…', hint: 'Ctrl+Shift+S', disabled: !q, run: () => q && void useQueryDocs.getState().saveFile(q, true) },
        sep,
        ...transferItems(),
        sep,
        {
          label: 'Close tab',
          hint: 'Ctrl+W',
          disabled: !s.hasTab,
          run: () => {
            const tab = activeTab();
            if (tab) void closeTab(tab);
          },
        },
        { label: 'Reopen closed tab', hint: 'Ctrl+Shift+T', disabled: !s.canReopen, run: () => void reopenClosedTab() },
        { label: 'Exit', hint: 'Alt+F4', run: exitApp },
      ],
    },
    {
      id: 'edit',
      label: 'Edit',
      mnemonic: 'E',
      items: [
        edit('Undo', 'Ctrl+Z', EDIT_COMMANDS.undo),
        edit('Redo', 'Ctrl+Y', EDIT_COMMANDS.redo),
        sep,
        edit('Cut', 'Ctrl+X', EDIT_COMMANDS.cut),
        edit('Copy', 'Ctrl+C', EDIT_COMMANDS.copy),
        edit('Paste', 'Ctrl+V', EDIT_COMMANDS.paste),
        edit('Select all', 'Ctrl+A', EDIT_COMMANDS.selectAll),
        sep,
        edit('Find', 'Ctrl+F', EDIT_COMMANDS.find),
        edit('Replace', 'Ctrl+H', EDIT_COMMANDS.replace),
        sep,
        edit('Format SQL', 'Ctrl+Shift+F', EDIT_COMMANDS.formatSql),
        edit('Toggle comment', 'Ctrl+/', EDIT_COMMANDS.toggleComment),
        edit('Duplicate line', 'Ctrl+D', EDIT_COMMANDS.duplicate),
        edit('Upper case', 'Ctrl+Shift+U', EDIT_COMMANDS.upperCase),
        edit('Lower case', 'Ctrl+U', EDIT_COMMANDS.lowerCase),
      ],
    },
    {
      id: 'view',
      label: 'View',
      mnemonic: 'V',
      items: [
        theme('Light', 'light'),
        theme('Dark', 'dark'),
        theme('System', 'system'),
        sep,
        { label: 'Show sidebar', hint: 'Ctrl+B', checked: s.sidebarVisible, run: () => useLayout.getState().toggleSidebar() },
        { label: 'Show results', hint: 'Ctrl+J', checked: s.resultsVisible, run: toggleResults },
        { label: 'Focus connection tree', hint: 'Ctrl+E', run: focusConnectionTree },
        sep,
        { label: 'Command palette', hint: 'Ctrl+Shift+P', run: showCommandPalette },
        { label: 'Go to object', hint: 'Ctrl+P', run: showQuickOpen },
        sep,
        { label: 'Zoom in', hint: 'Ctrl+=', run: zoomIn },
        { label: 'Zoom out', hint: 'Ctrl+-', run: zoomOut },
        { label: 'Reset zoom', hint: 'Ctrl+0', run: zoomReset },
        sep,
        { label: 'Reset layout', run: () => useLayout.getState().reset() },
      ],
    },
    {
      id: 'query',
      label: 'Query',
      mnemonic: 'Q',
      items: [
        { label: 'Run statement', hint: 'Ctrl+Enter', disabled: !q || !s.runnable, run: () => q && void runFromEditor(q, 'cursor') },
        { label: 'Run all', hint: 'Ctrl+Shift+Enter', disabled: !q || !s.runnable, run: () => q && void runFromEditor(q, 'all') },
        {
          label: 'Run selection',
          hint: 'Ctrl+Shift+R',
          disabled: !q || !s.runnable || !s.hasSelection,
          run: () => q && void runFromEditor(q, 'selection'),
        },
        { label: 'Stop', hint: 'Esc', disabled: !q || !s.stoppable, run: () => q && void stopQuery(q) },
        { label: 'Explain', hint: 'Ctrl+Shift+E', disabled: !q || !s.runnable, run: () => q && void explainQuery(q, false) },
      ],
    },
    {
      id: 'favorites',
      label: 'Favorites',
      mnemonic: 'a',
      items: [
        { label: 'Add to favorites…', hint: 'Ctrl+Shift+D', disabled: !q, run: () => void addActiveQueryToFavorites() },
        { label: 'Manage snippets…', run: openSnippetsManager },
        ...favoriteItems(),
      ],
    },
    {
      id: 'tools',
      label: 'Tools',
      mnemonic: 'T',
      items: [
        { hint: 'Ctrl+,', label: 'Settings…', run: () => dialog({ kind: 'settings' }) },
        sep,
        { label: 'Developer tools', hint: 'Ctrl+Shift+I', run: () => void openDevtools() },
      ],
    },
    {
      id: 'help',
      label: 'Help',
      mnemonic: 'H',
      items: [
        { label: 'Keyboard shortcuts', hint: 'F1', run: () => dialog({ kind: 'shortcuts' }) },
        { label: 'About DntDatabase', run: () => dialog({ kind: 'about' }) },
      ],
    },
  ];
}
