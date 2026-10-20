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

  // F04: MySQL `prefix_cost`s are cumulative along the join order.
  it('mysql_prefix_costs_are_turned_into_per_node_costs', () => {
    const o = node('index o', 0.55);
    const c = node('eq_ref c', 0.9);
    const root = node('Query block #1', 0.9, [node('Nested loop', null, [o, c])]);
    const costs = selfCosts(root);
    expect(costs.get(root)).toBe(0);
    expect(costs.get(o)).toBeCloseTo(0.55);
    expect(costs.get(c)).toBeCloseTo(0.35);
    // Never the root by its cumulative cost.
    expect(hottestNode(root)).toBe(o);
    // Already normalised (backend fix): the same answer.
    const o2 = node('index o', 0.55);
    const c2 = node('eq_ref c', 0.35);
    expect(hottestNode(node('Query block #1', 0.9, [node('Nested loop', null, [o2, c2])]))).toBe(o2);
  });

  it('costs_that_are_neither_additive_nor_cumulative_mark_nothing_by_them', () => {
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
