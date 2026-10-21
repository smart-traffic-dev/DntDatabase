/**
 * The editor area as a split tree (editor v2 §1): each leaf is an editor group, each split node
 * divides its box between two children in a direction, with its own ratio. Pure functions; the tab
 * store keeps the tree next to its groups.
 */

export type SplitDir = 'row' | 'col';
export type Side = 'left' | 'right' | 'up' | 'down';
/** Named shapes the presets build; any other tree is `custom`. */
export type LayoutPreset = 'single' | 'columns' | 'rows' | 'grid';

export type LayoutNode =
  | { kind: 'group'; id: string }
  | { kind: 'split'; id: string; dir: SplitDir; ratio: number; a: LayoutNode; b: LayoutNode };

export type SplitNode = Extract<LayoutNode, { kind: 'split' }>;

/** No split squeezes a side below 15 % (the render also keeps 320 × 200 px when it can). */
export const RATIO_MIN = 0.15;
export const clampRatio = (v: number) => Math.min(1 - RATIO_MIN, Math.max(RATIO_MIN, v));

/** Group ids in reading order (left to right, top to bottom). */
export function leaves(node: LayoutNode): string[] {
  return node.kind === 'group' ? [node.id] : [...leaves(node.a), ...leaves(node.b)];
}

/** Every split node, outermost first. */
export function splitsOf(node: LayoutNode): SplitNode[] {
  return node.kind === 'group' ? [] : [node, ...splitsOf(node.a), ...splitsOf(node.b)];
}

/** Replaces leaf `groupId` by a split holding it and a new group `newId` on `side`. */
export function splitLeaf(node: LayoutNode, groupId: string, side: Side, newId: string, splitId: string): LayoutNode {
  if (node.kind === 'group') {
    if (node.id !== groupId) return node;
    const fresh: LayoutNode = { kind: 'group', id: newId };
    const dir: SplitDir = side === 'left' || side === 'right' ? 'row' : 'col';
    const first = side === 'left' || side === 'up';
    return { kind: 'split', id: splitId, dir, ratio: 0.5, a: first ? fresh : node, b: first ? node : fresh };
  }
  const a = splitLeaf(node.a, groupId, side, newId, splitId);
  const b = a === node.a ? splitLeaf(node.b, groupId, side, newId, splitId) : node.b;
  return a === node.a && b === node.b ? node : { ...node, a, b };
}

/** Removes leaf `groupId`; its sibling takes the parent's place. `null` when the tree was that leaf. */
export function removeLeaf(node: LayoutNode, groupId: string): LayoutNode | null {
  if (node.kind === 'group') return node.id === groupId ? null : node;
  const a = removeLeaf(node.a, groupId);
  if (a === null) return node.b;
  const b = removeLeaf(node.b, groupId);
  if (b === null) return node.a;
  return a === node.a && b === node.b ? node : { ...node, a, b };
}

export function setRatio(node: LayoutNode, splitId: string, ratio: number): LayoutNode {
  if (node.kind === 'group') return node;
  if (node.id === splitId) return { ...node, ratio: clampRatio(ratio) };
  const a = setRatio(node.a, splitId, ratio);
  const b = setRatio(node.b, splitId, ratio);
  return a === node.a && b === node.b ? node : { ...node, a, b };
}

/** The tree of a preset over `ids` (as many as the preset needs: 1, 2, 2 or 4). */
export function presetTree(preset: LayoutPreset, ids: string[], splitId: () => string): LayoutNode {
  const g = (i: number): LayoutNode => ({ kind: 'group', id: ids[i]! });
  const split = (dir: SplitDir, a: LayoutNode, b: LayoutNode): LayoutNode => ({ kind: 'split', id: splitId(), dir, ratio: 0.5, a, b });
  if (preset === 'single') return g(0);
  if (preset === 'columns') return split('row', g(0), g(1));
  if (preset === 'rows') return split('col', g(0), g(1));
  return split('col', split('row', g(0), g(1)), split('row', g(2), g(3)));
}

/** Which preset the tree is, if any. */
export function presetOf(node: LayoutNode): LayoutPreset | 'custom' {
  if (node.kind === 'group') return 'single';
  const leafPair = node.a.kind === 'group' && node.b.kind === 'group';
  if (leafPair) return node.dir === 'row' ? 'columns' : 'rows';
  const isRowPair = (n: LayoutNode) => n.kind === 'split' && n.dir === 'row' && n.a.kind === 'group' && n.b.kind === 'group';
  return node.dir === 'col' && isRowPair(node.a) && isRowPair(node.b) ? 'grid' : 'custom';
}

/** Saved data shaped like a tree (restore never trusts storage). */
export function isLayoutNode(v: unknown): v is LayoutNode {
  const n = v as LayoutNode | null;
  if (!n || typeof n !== 'object') return false;
  if (n.kind === 'group') return typeof n.id === 'string';
  return (
    n.kind === 'split' &&
    typeof n.id === 'string' &&
    (n.dir === 'row' || n.dir === 'col') &&
    typeof n.ratio === 'number' &&
    isLayoutNode(n.a) &&
    isLayoutNode(n.b)
  );
}
