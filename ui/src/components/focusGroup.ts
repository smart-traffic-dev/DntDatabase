import { useTabs } from '../store/tabs';
import { editorOf } from '../tabs/query/activeEditor';
import { tabDomId } from './TabBar';

const STEP_MS = 16;
/** How long to wait for a moved / newly shown tab to mount (Monaco mounts asynchronously). */
const WAIT_MS = 600;

let pending: ReturnType<typeof setTimeout> | null = null;

/**
 * Moves the keyboard focus into the focused group: its query editor, else its active tab
 * button. Called after a keyboard group change (focus, split, move), which only changes the
 * store: without it the caret stays in the old group while every shortcut acts on the new one.
 */
export function focusFocusedGroup(): void {
  if (pending) clearTimeout(pending);
  const deadline = Date.now() + WAIT_MS;
  const attempt = () => {
    pending = null;
    const id = useTabs.getState().activeId;
    if (!id) return;
    const editor = editorOf(id);
    // The editor first: the tab button exists before the editor has mounted.
    if (editor) return editor.focus();
    const isQuery = useTabs.getState().tabs.find((t) => t.id === id)?.kind === 'query';
    const button = document.getElementById(tabDomId(id));
    if (button && (!isQuery || Date.now() >= deadline)) return button.focus();
    if (Date.now() < deadline) pending = setTimeout(attempt, STEP_MS);
  };
  pending = setTimeout(attempt, 0);
}
