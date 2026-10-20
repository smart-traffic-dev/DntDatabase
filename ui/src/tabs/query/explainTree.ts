// Helpers of the Explain panel (spec 8.2): self cost, the hottest node, Analyze availability.
import type { PlanNode, ServerInfo } from '../../bindings';

/**
 * Cost of the node itself: its estimated cost minus the cost of its children. null without an
 * estimate. A child without an estimate counts as 0; the result is never negative (some engines
 * report non-cumulative costs).
 */
export function selfCost(n: PlanNode): number | null {
  if (n.est_cost === null) return null;
  const children = n.children.reduce((sum, c) => sum + (c.est_cost ?? 0), 0);
  return Math.max(0, n.est_cost - children);
}

/** The node with the highest self cost (the first in document order on a tie); null if none has a cost. */
export function hottestNode(root: PlanNode): PlanNode | null {
  let best: PlanNode | null = null;
  let bestCost = -1;
  const visit = (n: PlanNode) => {
    const cost = selfCost(n);
    if (cost !== null && cost > bestCost) {
      best = n;
      bestCost = cost;
    }
    n.children.forEach(visit);
  };
  visit(root);
  return best;
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
