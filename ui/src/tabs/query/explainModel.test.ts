import { describe, expect, it } from 'vitest';
import type { PlanNode, ServerInfo } from '../../bindings';
import { sqlCaps } from '../../test/fixtures/caps';
import {
  allWarnings,
  ancestorsOf,
  costSegments,
  filterTree,
  flattenPlan,
  fmtInt,
  fmtMs,
  fmtNum,
  fmtPct,
  hotNodes,
  iconOf,
  maxSelfCost,
  nodeCount,
  nodeTotals,
  planWarnings,
  reportsPerLoop,
  rowsRatio,
  segmentFill,
  selfCostTotal,
  selfShare,
  slowHints,
  totalCost,
} from './explainModel';
import { selfCosts } from './explainTree';

const node = (label: string, patch: Partial<PlanNode> = {}): PlanNode => ({
  label,
  detail: null,
  est_cost: null,
  est_rows: null,
  actual_rows: null,
  actual_ms: null,
  loops: null,
  children: [],
  ...patch,
});

describe('flattenPlan', () => {
  it('records depth, the last-child flag and the guides of every row', () => {
    const d = node('D');
    const b = node('B', { children: [d] });
    const c = node('C');
    const a = node('A', { children: [b, c] });

    const rows = flattenPlan(a);
    expect(rows.map((r) => [r.node.label, r.depth, r.last])).toEqual([
      ['A', 0, true],
      ['B', 1, false],
      ['D', 2, true],
      ['C', 1, true],
    ]);
    expect(rows[0]!.guides).toEqual([]);
    expect(rows[1]!.guides).toEqual([]);
    // B has a later sibling, so D's vertical guide continues.
    expect(rows[2]!.guides).toEqual([true]);
    expect(rows[2]!.parent).toBe(b);
  });

  it('has no continuing guides down a single chain', () => {
    const deep = node('D');
    const c = node('C', { children: [deep] });
    const b = node('B', { children: [c] });
    const a = node('A', { children: [b] });
    const rows = flattenPlan(a);
    expect(rows[3]!.guides).toEqual([false, false]);
  });
});

describe('formatting', () => {
  it('groups numbers and marks missing values', () => {
    expect(fmtNum(1234.567)).toBe('1,234.57');
    expect(fmtNum(null)).toBe('—');
    expect(fmtInt(1234.9)).toBe('1,235');
    expect(fmtInt(null)).toBe('—');
    expect(fmtMs(12.34)).toBe('12.3 ms');
    expect(fmtMs(null)).toBe('—');
    expect(fmtPct(0.1234)).toBe('12.3%');
    expect(fmtPct(null)).toBe('—');
  });
});

describe('costs', () => {
  const b = node('B', { est_cost: 60 });
  const c = node('C', { est_cost: 30 });
  const root = node('A', { est_cost: 100, children: [b, c] });
  const costs = selfCosts(root);

  it('computes the total, the shares and the self-cost scale', () => {
    expect(totalCost(root, costs)).toBe(100);
    expect(selfCostTotal(costs)).toBe(100);
    expect(selfShare(root, costs, 100)).toBe(0.1);
    expect(selfShare(b, costs, 100)).toBe(0.6);
    expect(maxSelfCost(costs)).toBe(60);
  });

  it('falls back to the self costs when the root has no cost', () => {
    const uncosted = node('A', { children: [node('B', { est_cost: 5 }), node('C', { est_cost: 7 })] });
    expect(totalCost(uncosted)).toBe(12);
  });

  it('keeps the cost bar whole by folding the tail into one segment', () => {
    const segments = costSegments(root, costs, 2);
    expect(segments.map((s) => [s.label, s.cost])).toEqual([
      ['B', 60],
      ['C', 30],
      ['1 more', 10],
    ]);
    expect(segments.reduce((sum, s) => sum + s.share, 0)).toBeCloseTo(1);
    expect(segments[2]?.node).toBeNull();
  });

  it('returns the hottest nodes in cost order', () => {
    expect(hotNodes(root, costs).map((n) => n.label)).toEqual(['B', 'C', 'A']);
  });

  it('has no segments when every cost is zero', () => {
    const flat = node('A', { est_cost: 0, children: [node('B', { est_cost: 0 })] });
    expect(costSegments(flat)).toEqual([]);
  });
});

describe('rows', () => {
  it('computes the actual / estimated ratio', () => {
    expect(rowsRatio(node('A', { est_rows: 100, actual_rows: 10 }))).toBe(0.1);
    expect(rowsRatio(node('A', { est_rows: 0, actual_rows: 10 }))).toBeNull();
    expect(rowsRatio(node('A', { est_rows: null, actual_rows: 10 }))).toBeNull();
  });
});

describe('warnings', () => {
  it('flags a large sequential scan, but not a small one', () => {
    const big = planWarnings(node('Seq Scan on orders', { est_rows: 10_000 }));
    expect(allWarnings(big).map((w) => w.kind)).toEqual(['full-scan']);
    expect(planWarnings(node('Seq Scan on orders', { est_rows: 9_999 })).size).toBe(0);
  });

  it('flags an estimate skew in Analyze plans only', () => {
    const high = planWarnings(node('Index Scan', { est_rows: 100, actual_rows: 500 }));
    expect(high.values().next().value?.map((w) => w.title)).toEqual(['Estimate skew 5×']);
    const low = planWarnings(node('Index Scan', { est_rows: 100, actual_rows: 20 }));
    expect(allWarnings(low)[0]?.title).toBe('Estimate skew 5×');
    expect(planWarnings(node('Index Scan', { est_rows: 100, actual_rows: 50 })).size).toBe(0);
    expect(planWarnings(node('Index Scan', { est_rows: 100, actual_rows: null })).size).toBe(0);
  });

  it('flags a high loop count', () => {
    expect(planWarnings(node('Index Scan', { loops: 999 })).size).toBe(0);
    const loops = planWarnings(node('Index Scan', { loops: 1_000 }));
    expect(allWarnings(loops).map((w) => w.kind)).toEqual(['loops']);
  });

  it('collects every warning of the plan', () => {
    const root = node('Nested Loop', {
      children: [
        node('Seq Scan on a', { est_rows: 50_000 }),
        node('Index Scan on b', { est_rows: 100, actual_rows: 1_000 }),
      ],
    });
    expect(allWarnings(planWarnings(root))).toHaveLength(2);
  });
});

describe('filter', () => {
  const d = node('Filter', { detail: 'condition: x > 1' });
  const b = node('Seq Scan on orders', { detail: 'Filter: paid', children: [d] });
  const c = node('Index Scan on users', { detail: 'Index: users_pkey' });
  const root = node('Hash Join', { children: [b, c] });

  it('keeps the match, its ancestors and its subtree', () => {
    const visible = filterTree(root, 'orders');
    expect(visible?.has(b)).toBe(true);
    expect(visible?.has(root)).toBe(true);
    expect(visible?.has(d)).toBe(true);
    expect(visible?.has(c)).toBe(false);
  });

  it('matches the detail and shows the subtree of a matching parent', () => {
    expect(filterTree(root, 'users_pkey')?.has(c)).toBe(true);
    const parent = filterTree(root, 'hash');
    expect(parent?.has(root)).toBe(true);
    expect(parent?.has(b)).toBe(true);
    expect(parent?.has(c)).toBe(true);
  });

  it('is null without a filter', () => {
    expect(filterTree(root, '   ')).toBeNull();
  });

  it('lists the ancestors nearest first', () => {
    expect(ancestorsOf(root, d).map((n) => n.label)).toEqual(['Seq Scan on orders', 'Hash Join']);
  });
});

describe('icons', () => {
  it('maps node families to glyphs', () => {
    expect(iconOf('Seq Scan on orders').kind).toBe('scan');
    expect(iconOf('Hash Join').kind).toBe('join');
    expect(iconOf('Sort').kind).toBe('sort');
    expect(iconOf('HashAggregate').kind).toBe('hash');
    expect(iconOf('Limit').kind).toBe('limit');
    expect(iconOf('Result').kind).toBe('other');
  });
});

describe('counts', () => {
  it('counts the root too', () => {
    expect(nodeCount(node('A', { children: [node('B'), node('C')] }))).toBe(3);
  });
});

describe('per-loop figures', () => {
  const lookup = node('Index lookup on r', { actual_ms: 0.03, actual_rows: 42, loops: 4980 });

  it('nodeTotals multiplies time and rows by the loops when the engine reports per loop', () => {
    const t = nodeTotals(lookup, true);
    expect(t.ms).toBeCloseTo(149.4);
    expect(t.rows).toBe(209_160);
  });

  it('nodeTotals keeps the values as given when the engine reports totals', () => {
    expect(nodeTotals(lookup, false)).toEqual({ ms: 0.03, rows: 42 });
  });

  it('nodeTotals: no loops counts as one; missing values stay null', () => {
    expect(nodeTotals(node('a', { actual_ms: 2, actual_rows: 5 }), true)).toEqual({ ms: 2, rows: 5 });
    expect(nodeTotals(node('a', { loops: 10 }), true)).toEqual({ ms: null, rows: null });
  });

  it('reportsPerLoop: MariaDB reports totals, every other server (or none known) per loop', () => {
    const info = (variant: 'MySql8' | 'MariaDb' | 'Postgres'): ServerInfo =>
      variant === 'Postgres'
        ? { kind: 'postgres', variant, version: [8, 0, 36], version_string: '', capabilities: sqlCaps('db_schema') }
        : { kind: 'mysql', variant, version: [8, 0, 36], version_string: '', capabilities: sqlCaps('db') };
    expect(reportsPerLoop(info('MariaDb'))).toBe(false);
    expect(reportsPerLoop(info('MySql8'))).toBe(true);
    expect(reportsPerLoop(info('Postgres'))).toBe(true);
    expect(reportsPerLoop(undefined)).toBe(true);
  });
});

describe('slowHints (MySQL 5.7 table plans)', () => {
  it('type ALL is a full table scan, the whole value', () => {
    expect(slowHints('type', 'ALL')).toEqual([{ start: 0, end: 3, reason: 'Full table scan' }]);
    expect(slowHints('TYPE', 'all')).toEqual([{ start: 0, end: 3, reason: 'Full table scan' }]);
    expect(slowHints('type', 'ref')).toEqual([]);
  });

  it('Extra: only the filesort and temporary phrases, in text order', () => {
    const extra = 'Using where; Using temporary; Using filesort';
    expect(slowHints('Extra', extra)).toEqual([
      { start: 13, end: 28, reason: 'Uses a temporary table' },
      { start: 30, end: 44, reason: 'Sorts without an index' },
    ]);
    expect(slowHints('Extra', 'Using where')).toEqual([]);
  });

  it('other columns get no hint', () => {
    expect(slowHints('table', 'ALL')).toEqual([]);
    expect(slowHints('key', 'Using filesort')).toEqual([]);
  });
});

describe('segmentFill', () => {
  it('the hottest segment is the warning colour, the others accent fading by rank, other is grey', () => {
    expect(segmentFill({ hot: true, rank: 0, count: 4, other: false })).toBe('var(--dnt-status-warning-solid)');
    expect(segmentFill({ hot: false, rank: 1, count: 4, other: false })).toBe(
      'color-mix(in srgb, var(--dnt-accent) 100%, var(--dnt-bg-app))',
    );
    expect(segmentFill({ hot: false, rank: 3, count: 4, other: false })).toBe(
      'color-mix(in srgb, var(--dnt-accent) 25%, var(--dnt-bg-app))',
    );
    expect(segmentFill({ hot: false, rank: 4, count: 5, other: true })).toBe('var(--dnt-border-strong)');
  });
});
