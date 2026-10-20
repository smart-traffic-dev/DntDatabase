// The one rule for "this cell is not whole" (spec §2 rule 8: never write from cut data). A cell is
// cut when the page says so (`truncated`: long text cut by the backend, binary sent as a preview) or
// when its value is visibly incomplete (a binary preview shorter than its length, also inside arrays).
import type { TableDataPage, Value } from '../../bindings';

/** Whether `v` holds a binary preview shorter than the value (searched inside arrays). */
export function isCutValue(v: Value): boolean {
  if (v.t === 'Bytes') return v.v.preview.length < v.v.len;
  if (v.t === 'Array') return v.v.some(isCutValue);
  return false;
}

const cutSets = new WeakMap<TableDataPage['truncated'], Set<string>>();

function cutSet(page: TableDataPage): Set<string> {
  let set = cutSets.get(page.truncated);
  if (!set) cutSets.set(page.truncated, (set = new Set(page.truncated.map(([r, c]) => `${r},${c}`))));
  return set;
}

/** Whether cell (`row`, `col`) of the loaded page is not the whole value. */
export function isCutCell(page: TableDataPage, row: number, col: number): boolean {
  if (cutSet(page).has(`${row},${col}`)) return true;
  const v = page.rows[row]?.[col];
  return v !== undefined && isCutValue(v);
}
