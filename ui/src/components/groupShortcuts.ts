import { useTabs, type EditorLayout } from '../store/tabs';
import { closeGroup } from './tabActions';

/** How long a `Ctrl+K` waits for its second key. */
const CHORD_MS = 1500;
const PRESETS: Record<string, EditorLayout> = { '1': 'single', '2': 'columns', '3': 'rows', '4': 'grid' };

let chordAt = 0;

/** Forgets a pending `Ctrl+K` (tests). */
export function resetChord() {
  chordAt = 0;
}

function consume(e: KeyboardEvent) {
  e.preventDefault();
  e.stopPropagation();
}

/** Splits the focused group's active tab off (an empty group has nothing to split). */
function splitActive(direction: 'right' | 'down') {
  const { activeId, splitTab } = useTabs.getState();
  if (activeId) splitTab(activeId, direction);
}

/**
 * Editor group keys (spec §4): `Ctrl+\` split right; `Ctrl+K` then `Ctrl+\` split down, `→ / ←`
 * next / previous group, `1…4` layout preset, `W` close the group; `Ctrl+Alt+1…4` focus a group,
 * `Ctrl+Alt+→ / ←` move the tab to the next / previous group. Returns true when it took the key.
 *
 * `Ctrl+K` itself is not consumed: the SQL editor has its own `Ctrl+K` chords, and a second key
 * that is not ours reaches it as usual.
 */
export function onGroupShortcut(e: KeyboardEvent): boolean {
  if (e.repeat) return false;
  const ctrl = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  const tabs = useTabs.getState();

  if (chordAt && Date.now() - chordAt <= CHORD_MS && !e.altKey && !e.shiftKey) {
    if (key === 'control' || key === 'meta') return false;
    chordAt = 0;
    const run = (action: () => void) => {
      consume(e);
      action();
      return true;
    };
    if (key === '\\') return run(() => splitActive('down'));
    if (key === 'arrowright') return run(() => tabs.focusNeighbour(1));
    if (key === 'arrowleft') return run(() => tabs.focusNeighbour(-1));
    if (PRESETS[key]) return run(() => tabs.setLayout(PRESETS[key]!));
    if (key === 'w') return run(() => void closeGroup());
    return false;
  }
  chordAt = 0;
  if (!ctrl) return false;

  if (e.altKey && !e.shiftKey) {
    const n = Number(key);
    if (n >= 1 && n <= 4) {
      const group = tabs.groups[n - 1];
      if (!group) return false;
      consume(e);
      tabs.focusGroup(group.id);
      return true;
    }
    if (key === 'arrowright' || key === 'arrowleft') {
      consume(e);
      tabs.moveToNeighbour(key === 'arrowright' ? 1 : -1);
      return true;
    }
    return false;
  }
  if (e.altKey || e.shiftKey) return false;
  if (key === 'k') {
    chordAt = Date.now();
    return false;
  }
  if (key === '\\') {
    consume(e);
    splitActive('right');
    return true;
  }
  return false;
}
