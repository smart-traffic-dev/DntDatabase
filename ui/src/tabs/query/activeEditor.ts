import { useTabs } from '../../store/tabs';

/** The part of a Monaco editor the Edit menu needs. */
export interface MenuEditor {
  focus(): void;
  trigger(source: string, handlerId: string, payload: unknown): void;
}

const editors = new Map<string, MenuEditor>();

/** Remembers (or, with `null`, forgets) the mounted editor of a query tab. */
export function setActiveEditor(tabId: string, editor: MenuEditor | null) {
  if (editor) editors.set(tabId, editor);
  else editors.delete(tabId);
}

/** The editor registered for `tabId`, if any. */
export const editorOf = (tabId: string): MenuEditor | null => editors.get(tabId) ?? null;

/** The editor of the active tab, or null when the active tab is not a query tab with a mounted editor. */
export function getActiveEditor(): MenuEditor | null {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  return tab?.kind === 'query' ? editorOf(tab.id) : null;
}

/** Monaco command ids behind the Edit menu items. */
export const EDIT_COMMANDS = {
  undo: 'undo',
  redo: 'redo',
  cut: 'editor.action.clipboardCutAction',
  copy: 'editor.action.clipboardCopyAction',
  paste: 'editor.action.clipboardPasteAction',
  selectAll: 'editor.action.selectAll',
  find: 'actions.find',
  replace: 'editor.action.startFindReplaceAction',
  // Actions registered by editorCommands.ts (triggered by id like Monaco's own).
  formatSql: 'dnt.formatSql',
  upperCase: 'dnt.toUpperCase',
  lowerCase: 'dnt.toLowerCase',
  duplicate: 'dnt.duplicate',
  toggleComment: 'editor.action.commentLine',
} as const;

/** Runs a Monaco command on the active editor; false when there is none. */
export function runEditorCommand(id: string): boolean {
  const editor = getActiveEditor();
  if (!editor) return false;
  editor.focus();
  editor.trigger('menu', id, null);
  return true;
}
