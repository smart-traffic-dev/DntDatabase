import { describe, expect, it } from 'vitest';
import type { PlanNode } from '../../bindings';
import { serverInfo } from '../../test/backend';
import { analyzeAvailable, hottestNode, selfCost, selfCosts } from './explainTree';

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

  // F04: the backend (crates/query explain.rs) normalises MySQL `prefix_cost`s into
  // per-subtree costs; these are the values it yields for the mysql80/84 fixtures
  // (crates/query/tests/explain.rs `mysql_json_costs_are_per_subtree`).
  it('mysql_fixture_costs_mark_a_table_access_not_the_query_block', () => {
    for (const [total, oCost, cCost] of [
      [2.4, 1.3, 1.1],
      [1.55, 1.2, 0.35],
    ] as const) {
      const o = node('index o', oCost);
      const c = node('eq_ref c', cCost);
      const loop = node('Nested loop', null, [o, c]);
      const root = node('Query block #1', total, [loop]);
      const costs = selfCosts(root);
      expect(costs.get(root)).toBeCloseTo(0);
      expect(costs.get(loop)).toBeNull();
      expect(costs.get(o)).toBeCloseTo(oCost);
      expect(costs.get(c)).toBeCloseTo(cCost);
      expect(hottestNode(root, costs)).toBe(o);
    }
  });

  // The costs are already per subtree: they must never be corrected a second time as if
  // they were running totals (that would credit `c` only 4 − 2 = 2 here).
  it('ordered_costs_above_the_parent_are_not_read_as_running_totals', () => {
    const b = node('ALL b', 2);
    const c = node('ALL c', 4);
    const root = node('Query block #1', 5, [node('Nested loop', null, [b, c])]);
    const costs = selfCosts(root);
    expect(costs.get(root)).toBeNull();
    expect(costs.get(b)).toBe(2);
    expect(costs.get(c)).toBe(4);
    expect(hottestNode(root, costs)).toBe(c);
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

  // A MySQL block with a sort on top (backend: subquery under a table, block raised to its parts).
  it('mysql_block_keeps_the_rest_of_its_cost', () => {
    const a = node('ALL a', 1.5);
    const b = node('ALL b', 3);
    const d = node('ALL d', 5.5, [node('Query block #2', 3, [b])]);
    const root = node('Query block #1', 8, [node('Nested loop', null, [a, d])]);
    const costs = selfCosts(root);
    expect(costs.get(root)).toBeCloseTo(1);
    expect(costs.get(d)).toBeCloseTo(2.5);
    expect(costs.get(b)).toBeCloseTo(3);
    expect(hottestNode(root, costs)).toBe(b);
  });

  it('costs_that_are_not_additive_mark_nothing_by_them', () => {
    // Children out of order and above the parent: the parent's own cost is unknown.
    const x = node('x', 9);
    const y = node('y', 4);
    const root = node('root', 5, [x, y]);
    expect(selfCost(root)).toBeNull();
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
