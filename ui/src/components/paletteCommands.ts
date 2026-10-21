import { getActiveEditor, runEditorCommand } from '../tabs/query/activeEditor';
import { groupTabs, useTabs } from '../store/tabs';
import { closeOtherTabs, closeTabsToRight, togglePin } from './tabActions';

/** A command the palette lists besides the menu actions. */
export interface PaletteCommand {
  /** Unique name; registering the same id again replaces the command. */
  id: string;
  /** "Group: Action": the palette lists the action under a "Group" section header. */
  label: string;
  hint?: string;
  /** Hidden from the palette while this returns false. */
  enabled?(): boolean;
  run(): void;
}

const commands = new Map<string, PaletteCommand>();

/** Registers an extra palette command; returns a function that removes it. */
export function registerPaletteCommand(command: PaletteCommand): () => void {
  commands.set(command.id, command);
  return () => {
    if (commands.get(command.id) === command) commands.delete(command.id);
  };
}

/** The registered commands that are enabled now, in registration order. */
export function paletteCommands(): PaletteCommand[] {
  return [...commands.values()].filter((c) => c.enabled?.() ?? true);
}

/** The tabs of the focused group: the bulk closes act inside it. */
const focusedTabs = () => {
  const s = useTabs.getState();
  return groupTabs(s, s.focusedGroupId);
};

const activeTab = () => {
  const { tabs, activeId } = useTabs.getState();
  return tabs.find((t) => t.id === activeId);
};

// Monaco binds F1 to its own palette; the app's F1 wins everywhere, so it is offered here instead.
registerPaletteCommand({
  id: 'editor.commandPalette',
  label: 'Editor: Command palette',
  enabled: () => getActiveEditor() !== null,
  run: () => void runEditorCommand('editor.action.quickCommand'),
});
registerPaletteCommand({
  id: 'tab.togglePin',
  label: 'Tab: Pin or unpin the active tab',
  enabled: () => activeTab() !== undefined,
  run: () => {
    const tab = activeTab();
    if (tab) togglePin(tab);
  },
});
registerPaletteCommand({
  id: 'tab.closeOthers',
  label: 'Tab: Close other tabs',
  enabled: () => {
    const { activeId } = useTabs.getState();
    return focusedTabs().some((t) => t.id !== activeId && !t.pinned);
  },
  run: () => {
    const tab = activeTab();
    if (tab) void closeOtherTabs(tab);
  },
});
registerPaletteCommand({
  id: 'tab.closeRight',
  label: 'Tab: Close tabs to the right',
  enabled: () => {
    const { activeId } = useTabs.getState();
    const tabs = focusedTabs();
    const at = tabs.findIndex((t) => t.id === activeId);
    return at >= 0 && tabs.slice(at + 1).some((t) => !t.pinned);
  },
  run: () => {
    const tab = activeTab();
    if (tab) void closeTabsToRight(tab);
  },
});
