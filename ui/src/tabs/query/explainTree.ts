// Helpers of the Explain panel (spec 8.2): self cost, the hottest node, Analyze availability.
import type { PlanNode, ServerInfo } from '../../bindings';

/**
 * The nearest descendants of `n` that have a cost, in document order: nodes without a cost
 * (MySQL `nested_loop`, …) are looked through. Those passed through are recorded in `uncosted`.
 */
function nearestCosted(n: PlanNode, uncosted: PlanNode[]): PlanNode[] {
  const out: PlanNode[] = [];
  const visit = (c: PlanNode) => {
    if (c.est_cost !== null) {
      out.push(c);
      return;
    }
    uncosted.push(c);
    c.children.forEach(visit);
  };
  n.children.forEach(visit);
  return out;
}

/**
 * Own ("self") cost of every node of the plan (spec 8.2): its cost minus that of its nearest
 * costed descendants. null for a node without a cost, or when its own share cannot be told
 * (its costed descendants sum above it, e.g. a PostgreSQL Limit cheaper than its child, or a
 * Merge Join whose inputs sum above it).
 *
 * Every engine's costs are read as per subtree: the backend (crates/query `explain.rs`)
 * already turns MySQL's cumulative `prefix_cost`s into per-subtree costs, so the UI must not
 * correct them a second time.
 */
export function selfCosts(root: PlanNode): Map<PlanNode, number | null> {
  const out = new Map<PlanNode, number | null>();
  const visit = (n: PlanNode) => {
    const uncosted: PlanNode[] = [];
    const group = nearestCosted(n, uncosted);
    for (const u of uncosted) out.set(u, null);
    let own: number | null = null;
    if (n.est_cost !== null) {
      const sum = group.reduce((a, d) => a + d.est_cost!, 0);
      const tol = 1e-9 * Math.max(1, Math.abs(n.est_cost));
      own = sum <= n.est_cost + tol ? Math.max(0, n.est_cost - sum) : null;
    }
    out.set(n, own);
    group.forEach(visit);
  };
  visit(root);
  return out;
}

/** Own cost of `n` within the plan rooted at `n` (see `selfCosts`). */
export function selfCost(n: PlanNode): number | null {
  return selfCosts(n).get(n) ?? null;
}

/**
 * The node with the highest own cost (the first in document order on a tie). null when fewer
 * than two nodes have a cost (a single-node plan is not "hot") or no node costs anything of
 * its own: a node is never marked by its subtree's cost alone.
 */
export function hottestNode(root: PlanNode, costs: Map<PlanNode, number | null> = selfCosts(root)): PlanNode | null {
  let best: PlanNode | null = null;
  let bestCost = 0;
  let costed = 0;
  const visit = (n: PlanNode) => {
    if (n.est_cost !== null) costed += 1;
    const cost = costs.get(n) ?? null;
    if (cost !== null && cost > bestCost) {
      best = n;
      bestCost = cost;
    }
    n.children.forEach(visit);
  };
  visit(root);
  return costed >= 2 ? best : null;
}

/**
 * Whether "Explain Analyze" can run on the server: not on MySQL 5.7, and on MySQL 8 only from 8.0.18.
 * Unknown (the connection is not open yet) counts as available; the run opens it.
 */
export function analyzeAvailable(info: ServerInfo | undefined): boolean {
  if (!info) return true;
  if (info.variant === 'MySql57') return false;
  if (info.variant === 'MySql8') {
    const [major, minor, patch] = info.version;
    return major > 8 || (major === 8 && (minor > 0 || patch >= 18));
  }
  return true;
}
