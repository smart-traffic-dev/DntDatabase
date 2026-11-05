// Navicat-style arrow keys in the designer grids: ↑ / ↓ go to the same field of the row above /
// below, ← / → go to the field beside (from a text field only when the caret is at its edge).
import type { KeyboardEvent } from 'react';

const FIELD = 'input:not([type="hidden"]), select, button:not(.dz-combo-chevron):not(.dz-link)';

const bodyRows = (grid: Element) => [...grid.querySelectorAll<HTMLElement>(':scope > [role="row"]:not(.dz-head)')];
const cellsOf = (row: Element) => [...row.querySelectorAll<HTMLElement>(':scope > [role="cell"]')];
const fieldOf = (cell: Element | undefined) =>
  [...(cell?.querySelectorAll<HTMLElement>(FIELD) ?? [])].find((f) => !(f as HTMLInputElement).disabled) ?? null;

const isText = (el: Element): el is HTMLInputElement =>
  el instanceof HTMLInputElement && !['checkbox', 'radio', 'button'].includes(el.type);

/** Focuses `el`; a text field gets its whole text selected (as Navicat does on entering a cell). */
export function focusField(el: HTMLElement) {
  el.focus();
  if (isText(el)) el.select();
}

/** The field of `row` in the cell at `at` (or the row itself when that cell has none). */
function fieldAt(row: HTMLElement, at: number): HTMLElement {
  return fieldOf(cellsOf(row)[at]) ?? row;
}

/** The caret is at the start (dir -1) / end (dir 1) of a text field with nothing selected. */
function atEdge(el: HTMLInputElement, dir: -1 | 1): boolean {
  try {
    const { selectionStart: s, selectionEnd: e, value } = el;
    if (s === null || e === null) return true;
    if (s === 0 && e === value.length) return true; // the whole text selected: the cell, not the text
    if (s !== e) return false;
    return dir < 0 ? s === 0 : s === value.length;
  } catch {
    return true; // number inputs have no caret API
  }
}

/**
 * The grid's key handler (bubbling, after the field's own: an open list has prevented its keys).
 * Returns true when it moved the focus.
 */
export function onGridNavKey(e: KeyboardEvent<HTMLElement>): boolean {
  if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return false;
  const dir = e.key === 'ArrowUp' || e.key === 'ArrowLeft' ? -1 : e.key === 'ArrowDown' || e.key === 'ArrowRight' ? 1 : 0;
  if (dir === 0) return false;
  const vertical = e.key === 'ArrowUp' || e.key === 'ArrowDown';
  const target = e.target as HTMLElement;
  if (target.getAttribute('aria-expanded') === 'true') return false;
  const grid = e.currentTarget;
  const row = target.closest<HTMLElement>('[role="row"]');
  if (!row || row.classList.contains('dz-head') || !grid.contains(row)) return false;
  const cells = cellsOf(row);
  const onRow = target === row;
  const at = onRow ? -1 : cells.findIndex((c) => c.contains(target));

  let next: HTMLElement | null = null;
  if (vertical) {
    const rows = bodyRows(grid);
    const to = rows[rows.indexOf(row) + dir];
    if (to) next = onRow ? to : fieldAt(to, at);
  } else {
    if (!onRow && isText(target) && !atEdge(target, dir)) return false;
    if (onRow) next = cells.map((c) => fieldOf(c)).find((f) => f) ?? null;
    else {
      for (let i = at + dir; i >= 0 && i < cells.length && !next; i += dir) next = fieldOf(cells[i]);
    }
  }
  if (!next) return false;
  e.preventDefault();
  focusField(next);
  return true;
}
