import { isLayoutNode } from './layoutTree';
import { useTabs, type Arrangement, type ArrangementV1 } from './tabs';

export const ARRANGEMENT_KEY = 'dnt.editor.arrangement';
const PRESETS: readonly string[] = ['single', 'columns', 'rows', 'grid'];

const groupsOk = (groups: unknown) =>
  Array.isArray(groups) && groups.every((g) => Array.isArray((g as { tabIds?: unknown })?.tabIds));

/** The tree shape saved since editor v2. */
function isArrangement(v: unknown): v is Arrangement {
  const a = v as Arrangement;
  return !!a && a.v === 2 && isLayoutNode(a.tree) && groupsOk(a.groups) && a.groups.every((g) => typeof g.id === 'string') && typeof a.focused === 'string';
}

/** The first saved shape (presets + shared ratios): still restored, converted by the store. */
function isArrangementV1(v: unknown): v is ArrangementV1 {
  const a = v as ArrangementV1;
  return (
    !!a &&
    PRESETS.includes(a.layout) &&
    typeof a.ratios?.x === 'number' &&
    typeof a.ratios?.y === 'number' &&
    groupsOk(a.groups) &&
    typeof a.focused === 'number'
  );
}

/** Puts the reopened tabs back into the saved groups. Call after the tabs are restored. */
export function restoreArrangement(): void {
  let saved: unknown;
  try {
    saved = JSON.parse(localStorage.getItem(ARRANGEMENT_KEY) ?? 'null');
  } catch {
    return;
  }
  if (isArrangement(saved) || isArrangementV1(saved)) useTabs.getState().applyArrangement(saved);
}

/**
 * Saves the arrangement whenever the groups, the tree or the tabs change; returns the
 * unsubscribe. Turned on after the restore, so the empty startup state never overwrites it.
 */
export function persistArrangement(): () => void {
  return useTabs.subscribe((s, prev) => {
    if (s.groups === prev.groups && s.groupOf === prev.groupOf && s.tree === prev.tree && s.tabs === prev.tabs) return;
    try {
      localStorage.setItem(ARRANGEMENT_KEY, JSON.stringify(useTabs.getState().arrangement()));
    } catch {
      // Storage unavailable: the layout just starts single next time.
    }
  });
}
