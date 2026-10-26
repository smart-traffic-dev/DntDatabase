import type { LayoutPreset } from '../store/layoutTree';
import { useTabs } from '../store/tabs';
import { focusFocusedGroup } from './focusGroup';
import { closeAllTabs, closeGroup, closeOtherTabs, togglePin } from './tabActions';

/** How long a `Ctrl+K` waits for its second key. */
const CHORD_MS = 1500;
const PRESETS: Record<string, LayoutPreset> = { '1': 'single', '2': 'columns', '3': 'rows', '4': 'grid' };

let chordAt = 0;

/** Whether a `Ctrl+K` waits for its second key (which may be a plain key). */
export const chordPending = () => chordAt !== 0 && Date.now() - chordAt <= CHORD_MS;

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
  focusFocusedGroup();
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

  const target = e.target instanceof Element ? e.target : null;
  const inEditor = !!target?.closest('.sql-editor');
  // `Ctrl+K Shift+Enter` pins; every other second key is unshifted.
  const pinChord = e.shiftKey && !ctrl && key === 'enter';
  const chordLive = chordPending();
  // A modifier pressed on the way to the second key (the Shift of Shift+Enter) keeps the chord.
  if (chordLive && (key === 'shift' || key === 'control' || key === 'meta' || key === 'alt')) return false;
  if (chordLive && !e.altKey && (!e.shiftKey || pinChord)) {
    chordAt = 0;
    const run = (action: () => void) => {
      // In the SQL editor Monaco saw the Ctrl+K too: let the key reach it (prevented, so it is
      // not typed) so it leaves its own chord mode instead of waiting for another key.
      if (inEditor) e.preventDefault();
      else consume(e);
      action();
      return true;
    };
    // Exact modifiers: Ctrl+K Ctrl+1..7 are Monaco's fold levels.
    if (key === '\\' && ctrl) return run(() => splitActive('down'));
    if (key === 'm' && ctrl) return run(() => tabs.toggleMaximize());
    // Ctrl+K Ctrl+W closes the group's unpinned tabs, Ctrl+K Ctrl+O all but the active one.
    if (key === 'w' && ctrl) return run(() => void closeAllTabs());
    if (key === 'o' && ctrl) {
      const active = tabs.tabs.find((t) => t.id === tabs.activeId);
      return active ? run(() => void closeOtherTabs(active)) : false;
    }
    if (ctrl) return false;
    if (pinChord) {
      const active = tabs.tabs.find((t) => t.id === tabs.activeId);
      return active ? run(() => togglePin(active)) : false;
    }
    if (key === 'arrowright') return run(() => {
      tabs.focusNeighbour(1);
      focusFocusedGroup();
    });
    if (key === 'arrowleft') return run(() => {
      tabs.focusNeighbour(-1);
      focusFocusedGroup();
    });
    if (PRESETS[key]) return run(() => tabs.setLayout(PRESETS[key]!));
    if (key === 'enter' && tabs.activeId) return run(() => tabs.keepTab(tabs.activeId!));
    // With one group, "close the group" would close every tab: as in the View menu, it needs two.
    if (key === 'w' && tabs.groups.length > 1) return run(() => void closeGroup());
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
      focusFocusedGroup();
      return true;
    }
    if (key === 'arrowright' || key === 'arrowleft') {
      consume(e);
      tabs.moveToNeighbour(key === 'arrowright' ? 1 : -1);
      focusFocusedGroup();
      return true;
    }
    return false;
  }
  // Ctrl+Shift+PgUp / PgDn: move the active tab left / right in its strip.
  if (e.shiftKey && !e.altKey && (key === 'pageup' || key === 'pagedown')) {
    consume(e);
    tabs.shiftTab(key === 'pagedown' ? 1 : -1);
    return true;
  }
  if (e.altKey || e.shiftKey) return false;
  if (key === 'k') {
    // Fields other than the SQL editor (the sidebar filter, the palette input) keep their keys.
    const inField = !!target?.closest('input, textarea, select, [contenteditable="true"]') && !inEditor;
    if (!inField) chordAt = Date.now();
    return false;
  }
  if (key === '\\') {
    consume(e);
    splitActive('right');
    return true;
  }
  return false;
}
