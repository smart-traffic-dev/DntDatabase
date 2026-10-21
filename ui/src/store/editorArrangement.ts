import { useTabs, type Arrangement, type EditorLayout } from './tabs';

export const ARRANGEMENT_KEY = 'dnt.editor.arrangement';
const LAYOUTS: readonly EditorLayout[] = ['single', 'columns', 'rows', 'grid'];

function isArrangement(v: unknown): v is Arrangement {
  const a = v as Arrangement;
  return (
    !!a &&
    LAYOUTS.includes(a.layout) &&
    typeof a.ratios?.x === 'number' &&
    typeof a.ratios?.y === 'number' &&
    Array.isArray(a.groups) &&
    a.groups.every((g) => Array.isArray(g?.tabIds)) &&
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
  if (isArrangement(saved)) useTabs.getState().applyArrangement(saved);
}

/**
 * Saves the arrangement whenever the groups, the layout or the ratios change; returns the
 * unsubscribe. Turned on after the restore, so the empty startup state never overwrites it.
 */
export function persistArrangement(): () => void {
  return useTabs.subscribe((s, prev) => {
    if (s.groups === prev.groups && s.groupOf === prev.groupOf && s.layout === prev.layout && s.ratios === prev.ratios && s.tabs === prev.tabs) return;
    try {
      localStorage.setItem(ARRANGEMENT_KEY, JSON.stringify(useTabs.getState().arrangement()));
    } catch {
      // Storage unavailable: the layout just starts single next time.
    }
  });
}
