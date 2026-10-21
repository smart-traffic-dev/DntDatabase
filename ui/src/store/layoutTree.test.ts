import { describe, expect, it } from 'vitest';
import {
  isLayoutNode,
  leaves,
  presetOf,
  presetTree,
  removeLeaf,
  setRatio,
  splitLeaf,
  splitsOf,
  type LayoutNode,
} from './layoutTree';

const g = (id: string): LayoutNode => ({ kind: 'group', id });
let seq = 0;
const sid = () => `s${++seq}`;

describe('layout tree', () => {
  it('splitting a leaf on each side puts the new group on that side', () => {
    expect(splitLeaf(g('a'), 'a', 'right', 'b', 'x')).toEqual({ kind: 'split', id: 'x', dir: 'row', ratio: 0.5, a: g('a'), b: g('b') });
    expect(splitLeaf(g('a'), 'a', 'left', 'b', 'x')).toEqual({ kind: 'split', id: 'x', dir: 'row', ratio: 0.5, a: g('b'), b: g('a') });
    expect(splitLeaf(g('a'), 'a', 'down', 'b', 'x')).toMatchObject({ dir: 'col', a: g('a'), b: g('b') });
    expect(splitLeaf(g('a'), 'a', 'up', 'b', 'x')).toMatchObject({ dir: 'col', a: g('b'), b: g('a') });
  });

  it('only the target leaf changes; other splits keep their ratios', () => {
    let t: LayoutNode = splitLeaf(g('a'), 'a', 'right', 'b', 'x');
    t = setRatio(t, 'x', 0.3);
    t = splitLeaf(t, 'b', 'down', 'c', 'y');
    expect(t).toEqual({
      kind: 'split',
      id: 'x',
      dir: 'row',
      ratio: 0.3,
      a: g('a'),
      b: { kind: 'split', id: 'y', dir: 'col', ratio: 0.5, a: g('b'), b: g('c') },
    });
    expect(leaves(t)).toEqual(['a', 'b', 'c']);
  });

  it('removing a leaf lets its sibling take the parent place', () => {
    let t: LayoutNode = splitLeaf(g('a'), 'a', 'right', 'b', 'x');
    t = splitLeaf(t, 'b', 'down', 'c', 'y');
    expect(removeLeaf(t, 'b')).toEqual({ kind: 'split', id: 'x', dir: 'row', ratio: 0.5, a: g('a'), b: g('c') });
    expect(removeLeaf(removeLeaf(t, 'b')!, 'a')).toEqual(g('c'));
    expect(removeLeaf(g('a'), 'a')).toBeNull();
    expect(removeLeaf(t, 'zz')).toBe(t);
  });

  it('ratios clamp to 0.15..0.85', () => {
    const t = splitLeaf(g('a'), 'a', 'right', 'b', 'x');
    expect(setRatio(t, 'x', 0.01)).toMatchObject({ ratio: 0.15 });
    expect(setRatio(t, 'x', 0.99)).toMatchObject({ ratio: 0.85 });
  });

  it('presets build their trees and are recognised back', () => {
    expect(presetTree('single', ['a'], sid)).toEqual(g('a'));
    const cols = presetTree('columns', ['a', 'b'], sid);
    expect(cols).toMatchObject({ dir: 'row', a: g('a'), b: g('b') });
    expect(presetOf(cols)).toBe('columns');
    expect(presetOf(presetTree('rows', ['a', 'b'], sid))).toBe('rows');
    const grid = presetTree('grid', ['a', 'b', 'c', 'd'], sid);
    expect(leaves(grid)).toEqual(['a', 'b', 'c', 'd']);
    expect(presetOf(grid)).toBe('grid');
    expect(presetOf(g('a'))).toBe('single');
    expect(presetOf(splitLeaf(cols, 'b', 'down', 'c', 'q'))).toBe('custom');
  });

  it('lists its splits (for the splitters) and validates saved data', () => {
    const t = splitLeaf(splitLeaf(g('a'), 'a', 'right', 'b', 'x'), 'b', 'down', 'c', 'y');
    expect(splitsOf(t).map((s) => s.id)).toEqual(['x', 'y']);
    expect(isLayoutNode(t)).toBe(true);
    expect(isLayoutNode({ kind: 'split', id: 'x', dir: 'diagonal', ratio: 0.5, a: g('a'), b: g('b') })).toBe(false);
    expect(isLayoutNode({ kind: 'group' })).toBe(false);
    expect(isLayoutNode(null)).toBe(false);
  });
});
