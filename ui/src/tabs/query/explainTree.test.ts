import { describe, expect, it } from 'vitest';
import type { PlanNode } from '../../bindings';
import { serverInfo } from '../../test/backend';
import { analyzeAvailable, hottestNode, selfCost } from './explainTree';

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

  it('self_cost_is_null_without_a_cost_and_never_negative', () => {
    expect(selfCost(node('x', null))).toBeNull();
    expect(selfCost(node('x', 3, [node('y', 9)]))).toBe(0);
    // A child without a cost counts as 0.
    expect(selfCost(node('x', 7, [node('y', null)]))).toBe(7);
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
