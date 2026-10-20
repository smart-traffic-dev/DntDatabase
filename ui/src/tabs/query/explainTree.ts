// Helpers of the Explain panel (spec 8.2): self cost, the hottest node, Analyze availability.
import type { Dialect, PlanNode, ServerInfo } from '../../bindings';

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

/** MySQL / MariaDB plans carry cumulative `prefix_cost`s; PostgreSQL costs are per subtree. */
export function costsAreCumulative(dialect: Dialect | undefined): boolean {
  return dialect?.engine === 'mysql';
}

/**
 * Splits `parent` (the cost of a node) over its nearest costed descendants `costs`:
 * - additive (they sum to at most `parent`): kept, the node's own share is the rest;
 * - with `cumulative` (MySQL `prefix_cost`s: summing above `parent`, non-decreasing along the
 *   join order): each one's share is its increase over the previous one; the node keeps
 *   `parent` minus the last (a sort or temporary table on top), or null when `query_cost` is
 *   below the last prefix cost;
 * - anything else: kept as they are, and the node's own share is unknown (null), e.g. a
 *   PostgreSQL Limit cheaper than its child, or a Merge Join whose inputs sum above it.
 *
 * Known limit (until the backend normalises MySQL costs): when a MySQL `query_cost` is at
 * least the SUM of its tables' prefix costs (a large sort on top), the prefix costs are read
 * as additive, so later tables are credited their running total. The block itself is then
 * usually the costliest node anyway.
 */
function split(
  costs: number[],
  parent: number | null,
  cumulative: boolean,
): { shares: number[]; own: number | null } {
  if (parent === null) return { shares: costs, own: null };
  const tol = 1e-9 * Math.max(1, Math.abs(parent));
  const sum = costs.reduce((a, b) => a + b, 0);
  if (sum <= parent + tol) return { shares: costs, own: Math.max(0, parent - sum) };
  const ordered = costs.every((c, i) => i === 0 || c >= costs[i - 1]! - tol);
  if (!cumulative || !ordered) return { shares: costs, own: null };
  const last = costs.at(-1)!;
  return {
    shares: costs.map((c, i) => (i === 0 ? c : Math.max(0, c - costs[i - 1]!))),
    own: last <= parent + tol ? Math.max(0, parent - last) : null,
  };
}

/**
 * Own ("self") cost of every node of the plan (spec 8.2): its cost minus that of its nearest
 * costed descendants. With `cumulative` (MySQL, see `costsAreCumulative`), running-total
 * descendant costs are turned into per-node costs first. null for a node without a cost, or
 * when its own share cannot be told.
 */
export function selfCosts(root: PlanNode, opts: { cumulative?: boolean } = {}): Map<PlanNode, number | null> {
  const cumulative = opts.cumulative ?? false;
  const out = new Map<PlanNode, number | null>();
  const visit = (n: PlanNode, cost: number | null) => {
    const uncosted: PlanNode[] = [];
    const group = nearestCosted(n, uncosted);
    for (const u of uncosted) out.set(u, null);
    const { shares, own } = split(
      group.map((d) => d.est_cost!),
      cost,
      cumulative,
    );
    out.set(n, n.est_cost === null ? null : own);
    group.forEach((d, i) => visit(d, shares[i]!));
  };
  visit(root, root.est_cost);
  return out;
}

/** Own cost of `n` within the plan rooted at `n` (see `selfCosts`). */
export function selfCost(n: PlanNode, opts: { cumulative?: boolean } = {}): number | null {
  return selfCosts(n, opts).get(n) ?? null;
}

/**
 * The node with the highest own cost (the first in document order on a tie). null when fewer
 * than two nodes have a cost (a single-node plan is not "hot") or no node costs anything of
 * its own: a node is never marked by a cumulative cost alone.
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
