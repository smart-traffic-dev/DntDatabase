import type { PlanStmtView, PlanView } from '../bindings';

/** Statements of each plan by index (a plan never changes, so its map is built once). */
const byIndex = new WeakMap<PlanView, Map<number, PlanStmtView>>();

/** The statements of `plan` keyed by their index: O(1) lookups on scripts of any size. */
export function stmtsByIndex(plan: PlanView): Map<number, PlanStmtView> {
  let map = byIndex.get(plan);
  if (!map) {
    map = new Map(plan.stmts.map((s) => [s.index, s]));
    byIndex.set(plan, map);
  }
  return map;
}
