import { describe, expect, it } from 'vitest';
import type { Dialect, PlanNode } from '../../bindings';
import { serverInfo } from '../../test/backend';
import { analyzeAvailable, costsAreCumulative, hottestNode, selfCost, selfCosts } from './explainTree';

const MYSQL = { cumulative: true };

const node = (label: string, est_cost: number | null, children: PlanNode[] = []): PlanNode => ({
  label,
  detail: null,
  est_cost,
  est_rows: null,
  actual_rows: null,
  actual_ms: null,
  loops: null,
  children,
});

describe('explainTree', () => {
  it('hottest_node_by_self_cost', () => {
    // Cumulative costs: the root is the largest, but its self cost is only 5.
    const slow = node('Seq Scan', 80);
    const fast = node('Index Scan', 10);
    const root = node('Hash Join', 95, [slow, fast]);
    expect(selfCost(root)).toBe(5);
    expect(selfCost(slow)).toBe(80);
    expect(hottestNode(root)).toBe(slow);
  });

  it('self_cost_is_null_without_a_cost_or_when_it_cannot_be_told', () => {
    expect(selfCost(node('x', null))).toBeNull();
    // A child costing more than its parent (PG Limit): the parent's own cost is unknown.
    expect(selfCost(node('x', 3, [node('y', 9)]))).toBeNull();
    // A child without a cost (and no costed descendant) counts as 0.
    expect(selfCost(node('x', 7, [node('y', null)]))).toBe(7);
  });

  // F04: an intermediate node without a cost (MySQL nested_loop) is looked through.
  it('self_cost_subtracts_the_nearest_costed_descendants', () => {
    const a = node('Seq Scan a', 30);
    const b = node('Seq Scan b', 50);
    const root = node('Aggregate', 100, [node('Join', null, [a, b])]);
    expect(selfCost(root)).toBe(20);
    expect(hottestNode(root)).toBe(b);
  });

  // F04: MySQL `prefix_cost`s are cumulative along the join order.
  it('mysql_prefix_costs_are_turned_into_per_node_costs', () => {
    const o = node('index o', 0.55);
    const c = node('eq_ref c', 0.9);
    const root = node('Query block #1', 0.9, [node('Nested loop', null, [o, c])]);
    const costs = selfCosts(root, MYSQL);
    expect(costs.get(root)).toBe(0);
    expect(costs.get(o)).toBeCloseTo(0.55);
    expect(costs.get(c)).toBeCloseTo(0.35);
    // Never the root by its cumulative cost.
    expect(hottestNode(root, costs)).toBe(o);
    // Already normalised (backend fix): the same answer.
    const o2 = node('index o', 0.55);
    const c2 = node('eq_ref c', 0.35);
    const normalised = node('Query block #1', 0.9, [node('Nested loop', null, [o2, c2])]);
    expect(hottestNode(normalised, selfCosts(normalised, MYSQL))).toBe(o2);
  });

  it('cumulative_costs_are_only_read_for_mysql', () => {
    expect(costsAreCumulative({ engine: 'mysql', ansi_quotes: false, no_backslash_escapes: false } as Dialect)).toBe(true);
    expect(costsAreCumulative({ engine: 'postgres' })).toBe(false);
    expect(costsAreCumulative(undefined)).toBe(false);
  });

  // PostgreSQL costs are per subtree: a Merge Join's inputs can sum above it and still not
  // be cumulative. Read that way, the Index Scan would be credited only 20 and Sort marked.
  it('pg_merge_join_is_not_read_as_cumulative', () => {
    const sort = node('Sort', 20);
    const index = node('Index Scan', 40);
    const join = node('Merge Join', 50, [sort, index]);
    const costs = selfCosts(join);
    expect(costs.get(join)).toBeNull();
    expect(costs.get(sort)).toBe(20);
    expect(costs.get(index)).toBe(40);
    expect(hottestNode(join, costs)).toBe(index);
  });

  // MySQL `query_cost` vs the last `prefix_cost` of its tables.
  it('mysql_query_cost_above_or_below_the_last_prefix_cost', () => {
    // Above (a sort or temporary table on top): the block keeps the difference.
    const o = node('index o', 0.55);
    const c = node('eq_ref c', 0.9);
    const sorted = node('Query block #1', 1.2, [node('Nested loop', null, [o, c])]);
    const costs = selfCosts(sorted, MYSQL);
    expect(costs.get(sorted)).toBeCloseTo(0.3);
    expect(costs.get(c)).toBeCloseTo(0.35);
    expect(hottestNode(sorted, costs)).toBe(o);
    // Below the last prefix cost: the tables still get per-node shares; the block's own is unknown.
    const o2 = node('index o', 0.55);
    const c2 = node('eq_ref c', 0.9);
    const below = node('Query block #1', 0.8, [node('Nested loop', null, [o2, c2])]);
    const costs2 = selfCosts(below, MYSQL);
    expect(costs2.get(below)).toBeNull();
    expect(costs2.get(c2)).toBeCloseTo(0.35);
    expect(hottestNode(below, costs2)).toBe(o2);
  });

  it('costs_that_are_neither_additive_nor_cumulative_mark_nothing_by_them', () => {
    // Children out of order and above the parent: the parent's own cost is unknown.
    const x = node('x', 9);
    const y = node('y', 4);
    const root = node('root', 5, [x, y]);
    expect(selfCost(root)).toBeNull();
    expect(selfCosts(root, MYSQL).get(root)).toBeNull();
    expect(hottestNode(root)).toBe(x);
  });

  it('no_highlight_with_fewer_than_two_costed_nodes', () => {
    expect(hottestNode(node('Seq Scan', 35))).toBeNull();
    expect(hottestNode(node('Query block', 4, [node('Nested loop', null, [node('t', null)])]))).toBeNull();
    // Nothing costs anything of its own.
    expect(hottestNode(node('a', 0, [node('b', 0)]))).toBeNull();
  });

  it('hottest_node_is_null_when_nothing_has_a_cost', () => {
    expect(hottestNode(node('a', null, [node('b', null)]))).toBeNull();
  });

  it('hottest_node_prefers_the_first_on_a_tie', () => {
    const a = node('a', 10);
    const b = node('b', 10);
    expect(hottestNode(node('root', 20, [a, b]))).toBe(a);
  });

  it('analyze_availability_by_server', () => {
    expect(analyzeAvailable(serverInfo('MySql57', [5, 7, 44]))).toBe(false);
    expect(analyzeAvailable(serverInfo('MySql8', [8, 0, 17]))).toBe(false);
    expect(analyzeAvailable(serverInfo('MySql8', [8, 0, 18]))).toBe(true);
    expect(analyzeAvailable(serverInfo('MySql8', [8, 4, 0]))).toBe(true);
    expect(analyzeAvailable(serverInfo('MariaDb', [10, 6, 0]))).toBe(true);
    expect(analyzeAvailable(serverInfo('Postgres', [16, 0, 0]))).toBe(true);
    // Not connected yet: the run opens the connection, so the menu stays usable.
    expect(analyzeAvailable(undefined)).toBe(true);
  });
});
