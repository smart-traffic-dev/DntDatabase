// Derived data of the Explain panel (spec: docs/superpowers/specs/2026-10-21-explain-panel-redesign-design.md).
// Pure functions: the panel and its views only format what these return.
import type { PlanNode, ServerInfo } from '../../bindings';
import { selfCosts } from './explainTree';

/** Full table scan warning: at least this many estimated rows. */
export const SEQ_SCAN_ROWS = 10_000;
/** Estimate skew warning: actual / estimated at or above this, or at or below its inverse. */
export const SKEW_FACTOR = 5;
/** Loop-count warning. */
export const LOOP_COUNT = 1_000;

export interface FlatNode {
  node: PlanNode;
  depth: number;
  parent: PlanNode | null;
  /** True when the node is its parent's last child (its connector is `└─`). */
  last: boolean;
  /**
   * One entry per ancestor below the root: true when that ancestor has a later sibling, so its
   * vertical guide continues (`│`). Index 0 belongs to the depth-1 ancestor.
   */
  guides: boolean[];
}

export interface Insight {
  kind: 'full-scan' | 'skew' | 'loops';
  title: string;
  body: string;
}

export interface CostSegment {
  key: string;
  label: string;
  cost: number;
  share: number;
  /** null for the aggregate "other" segment. */
  node: PlanNode | null;
}

export type NodeIconKind = 'scan' | 'join' | 'sort' | 'hash' | 'limit' | 'other';

const formatRows = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });
const formatNumber = new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 });

/** Grouped number for costs and rows; an em dash when the field is missing. */
export function fmtNum(n: number | null | undefined): string {
  return n === null || n === undefined || !Number.isFinite(n) ? '—' : formatNumber.format(n);
}

/** Grouped integer (rows, loops). */
export function fmtInt(n: number | null | undefined): string {
  return n === null || n === undefined || !Number.isFinite(n) ? '—' : formatRows.format(n);
}

const formatMs = new Intl.NumberFormat('en-US', { maximumFractionDigits: 1 });
const formatSmallMs = new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 });

/** Milliseconds with one decimal (two under 1 ms, where per-loop times live). */
export function fmtMs(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '—';
  return `${(n !== 0 && Math.abs(n) < 1 ? formatSmallMs : formatMs).format(n)} ms`;
}

/**
 * Whether the server reports `actual_ms` / `actual_rows` per loop: MySQL (tree format) and
 * PostgreSQL do; MariaDB's `ANALYZE FORMAT=JSON` reports totals. Unknown → per loop.
 */
export function reportsPerLoop(info: ServerInfo | undefined): boolean {
  return info?.variant !== 'MariaDb';
}

/** A node's time and rows over all its loops. */
export function nodeTotals(node: PlanNode, perLoop: boolean): { ms: number | null; rows: number | null } {
  const loops = perLoop ? (node.loops ?? 1) : 1;
  return {
    ms: node.actual_ms === null ? null : node.actual_ms * loops,
    rows: node.actual_rows === null ? null : node.actual_rows * loops,
  };
}

export interface SlowHint {
  start: number;
  end: number;
  reason: string;
}

const SLOW_EXTRA: [RegExp, string][] = [
  [/using temporary/gi, 'Uses a temporary table'],
  [/using filesort/gi, 'Sorts without an index'],
];

/** Known-slow parts of a MySQL 5.7 table-plan cell (`type` = ALL, filesort / temporary in Extra). */
export function slowHints(column: string, value: string): SlowHint[] {
  const col = low(column);
  if (col === 'type') {
    return value.trim().toUpperCase() === 'ALL' ? [{ start: 0, end: value.length, reason: 'Full table scan' }] : [];
  }
  if (col !== 'extra') return [];
  const out: SlowHint[] = [];
  for (const [re, reason] of SLOW_EXTRA) {
    for (const m of value.matchAll(re)) out.push({ start: m.index, end: m.index + m[0].length, reason });
  }
  return out.sort((a, b) => a.start - b.start);
}

/** Self-cost bar fill: only the hottest node is amber. */
export function barFill(hot: boolean): string {
  return hot ? 'var(--dnt-status-warning-solid)' : 'var(--dnt-accent)';
}

/**
 * Cost-bar segment fill: the hottest is amber, the others the accent fading toward the
 * background by rank (100 % → 25 %), the "other" segment grey.
 */
export function segmentFill({ hot, rank, count, other }: { hot: boolean; rank: number; count: number; other: boolean }): string {
  if (other) return 'var(--dnt-border-strong)';
  if (hot) return barFill(true);
  const steps = Math.max(1, count - 2);
  const pct = Math.round(100 - (75 * Math.max(0, rank - 1)) / steps);
  return `color-mix(in srgb, var(--dnt-accent) ${pct}%, var(--dnt-bg-app))`;
}

/** 0.1234 → "12.3%". */
export function fmtPct(share: number | null | undefined): string {
  return share === null || share === undefined || !Number.isFinite(share) ? '—' : `${Math.round(share * 1000) / 10}%`;
}

/** Depth-first rows in document order, with the guides the tree and grid draw. */
export function flattenPlan(root: PlanNode): FlatNode[] {
  const out: FlatNode[] = [];
  const visit = (node: PlanNode, depth: number, parent: PlanNode | null, last: boolean, guides: boolean[]) => {
    out.push({ node, depth, parent, last, guides });
    const end = node.children.length - 1;
    node.children.forEach((child, i) => {
      // A node's own sibling state belongs to the level its children start at.
      const below = depth === 0 ? [] : [...guides, !last];
      visit(child, depth + 1, node, i === end, below);
    });
  };
  visit(root, 0, null, true, []);
  return out;
}

/** Number of nodes, the root included. */
export function nodeCount(root: PlanNode): number {
  let n = 0;
  const visit = (node: PlanNode) => {
    n += 1;
    node.children.forEach(visit);
  };
  visit(root);
  return n;
}

/** The plan's total cost: the root's, or the self costs when the root has none. */
export function totalCost(root: PlanNode, costs: Map<PlanNode, number | null> = selfCosts(root)): number | null {
  if (root.est_cost !== null) return root.est_cost;
  let sum = 0;
  for (const cost of costs.values()) if (cost !== null) sum += cost;
  return sum > 0 ? sum : null;
}

/** A node's share of the plan's total cost, null when either side is unknown. */
export function selfShare(node: PlanNode, costs: Map<PlanNode, number | null>, total: number | null): number | null {
  const own = costs.get(node) ?? null;
  if (own === null || total === null || total <= 0) return null;
  return own / total;
}

/** The largest self cost of the plan: the scale of the tree's self-cost bars. */
export function maxSelfCost(costs: Map<PlanNode, number | null>): number {
  let max = 0;
  for (const cost of costs.values()) if (cost !== null && cost > max) max = cost;
  return max;
}

/**
 * Sum of the positive self costs: the denominator of the distribution bar and the shares. In a
 * well-formed plan it equals the root's total cost; a node cheaper than its child leaves a null
 * self cost and makes it differ, and sharing one denominator keeps every percentage consistent.
 */
export function selfCostTotal(costs: Map<PlanNode, number | null>): number | null {
  let sum = 0;
  for (const cost of costs.values()) if (cost !== null && cost > 0) sum += cost;
  return sum > 0 ? sum : null;
}

/** Nodes with the highest self cost, in plan order on a tie. */
export function hotNodes(root: PlanNode, costs: Map<PlanNode, number | null> = selfCosts(root), limit = 3): PlanNode[] {
  const costed: { node: PlanNode; cost: number; index: number }[] = [];
  flattenPlan(root).forEach(({ node }, index) => {
    const cost = costs.get(node) ?? null;
    if (cost !== null && cost > 0) costed.push({ node, cost, index });
  });
  costed.sort((a, b) => b.cost - a.cost || a.index - b.index);
  return costed.slice(0, limit).map((c) => c.node);
}

/**
 * Top segments of the cost bar. Nodes are ordered by self cost; anything past `limit` is
 * folded into one "other" segment so the bar always adds up to the whole.
 */
export function costSegments(
  root: PlanNode,
  costs: Map<PlanNode, number | null> = selfCosts(root),
  limit = 8,
): CostSegment[] {
  const flat = flattenPlan(root);
  const entries: { node: PlanNode; cost: number; index: number }[] = [];
  flat.forEach(({ node }, index) => {
    const cost = costs.get(node) ?? null;
    if (cost !== null && cost > 0) entries.push({ node, cost, index });
  });
  const total = entries.reduce((sum, e) => sum + e.cost, 0);
  if (total <= 0) return [];
  entries.sort((a, b) => b.cost - a.cost || a.index - b.index);
  const top = entries.slice(0, limit);
  const rest = entries.slice(limit);
  const segments: CostSegment[] = top.map((e) => ({
    key: `n${e.index}`,
    label: e.node.label,
    cost: e.cost,
    share: e.cost / total,
    node: e.node,
  }));
  if (rest.length > 0) {
    const cost = rest.reduce((sum, e) => sum + e.cost, 0);
    segments.push({ key: 'other', label: `${rest.length} more`, cost, share: cost / total, node: null });
  }
  return segments;
}

/** Actual / estimated rows; null unless both are known and the estimate is positive. */
export function rowsRatio(node: PlanNode): number | null {
  if (node.actual_rows === null || node.est_rows === null || node.est_rows <= 0) return null;
  return node.actual_rows / node.est_rows;
}

const low = (s: string) => s.toLowerCase();
const text = (node: PlanNode) => `${node.label} ${node.detail ?? ''}`;

/**
 * Warnings the normalized plan can prove: a full scan, a badly wrong row estimate when Analyze
 * ran, and a hot loop. Every other quality signal needs fields `PlanNode` does not carry.
 */
export function planWarnings(root: PlanNode): Map<PlanNode, Insight[]> {
  const out = new Map<PlanNode, Insight[]>();
  const visit = (node: PlanNode) => {
    const found: Insight[] = [];
    const label = low(node.label);
    if (label.includes('seq scan') && (node.est_rows ?? 0) >= SEQ_SCAN_ROWS) {
      found.push({
        kind: 'full-scan',
        title: 'Full table scan',
        body: `Sequential scan over ${fmtInt(node.est_rows)} rows; an index may help if the filter is selective.`,
      });
    }
    if (node.actual_rows !== null && node.est_rows !== null && node.est_rows > 0) {
      const ratio = node.actual_rows / node.est_rows;
      if (ratio >= SKEW_FACTOR || ratio <= 1 / SKEW_FACTOR) {
        const factor = ratio >= 1 ? ratio : 1 / ratio;
        found.push({
          kind: 'skew',
          title: `Estimate skew ${fmtInt(Math.round(factor))}×`,
          body: `The planner expected ${fmtInt(node.est_rows)} rows; ${fmtInt(node.actual_rows)} came back.`,
        });
      }
    }
    if (node.loops !== null && node.loops >= LOOP_COUNT) {
      found.push({
        kind: 'loops',
        title: 'High loop count',
        body: `This node ran ${fmtInt(node.loops)} times; per-loop overhead may dominate.`,
      });
    }
    if (found.length > 0) out.set(node, found);
    node.children.forEach(visit);
  };
  visit(root);
  return out;
}

/** Every warning of the plan, in document order. */
export function allWarnings(byNode: Map<PlanNode, Insight[]>): Insight[] {
  return [...byNode.values()].flat();
}

/** The icon family of a node label. */
export function iconOf(label: string): { kind: NodeIconKind; glyph: string } {
  const l = low(label);
  if (l.includes('scan')) return { kind: 'scan', glyph: '▤' };
  if (l.includes('join') || l.includes('loop')) return { kind: 'join', glyph: '⋈' };
  if (l.includes('sort') || l.includes('order')) return { kind: 'sort', glyph: '↕' };
  if (l.includes('hash') || l.includes('aggregate') || l.includes('group') || l.includes('window')) {
    return { kind: 'hash', glyph: '#' };
  }
  if (l.includes('limit')) return { kind: 'limit', glyph: '◫' };
  return { kind: 'other', glyph: '▢' };
}

/** Case-insensitive match over the label and detail. */
export function matchesFilter(node: PlanNode, filter: string): boolean {
  const q = low(filter.trim());
  return q.length > 0 && low(text(node)).includes(q);
}

/**
 * Nodes the tree shows for `filter`: the matches, their ancestors (for the shape) and the
 * descendants of a match (a matching parent keeps its context). null when no filter is set.
 */
export function filterTree(root: PlanNode, filter: string): Set<PlanNode> | null {
  if (filter.trim().length === 0) return null;
  const flat = flattenPlan(root);
  const parents = new Map<PlanNode, PlanNode | null>();
  for (const f of flat) parents.set(f.node, f.parent);
  const visible = new Set<PlanNode>();
  const addBelow = (node: PlanNode) => {
    visible.add(node);
    node.children.forEach(addBelow);
  };
  for (const f of flat) {
    if (!matchesFilter(f.node, filter)) continue;
    let node: PlanNode | null = f.node;
    while (node !== null) {
      visible.add(node);
      node = parents.get(node) ?? null;
    }
    addBelow(f.node);
  }
  return visible;
}

/** Ancestors of `node` below `root`, nearest first. */
export function ancestorsOf(root: PlanNode, node: PlanNode): PlanNode[] {
  const flat = flattenPlan(root);
  const parents = new Map<PlanNode, PlanNode | null>();
  for (const f of flat) parents.set(f.node, f.parent);
  const out: PlanNode[] = [];
  let parent = parents.get(node) ?? null;
  while (parent !== null) {
    out.push(parent);
    parent = parents.get(parent) ?? null;
  }
  return out;
}
