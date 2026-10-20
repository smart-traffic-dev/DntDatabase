import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { useTree, type TreeNode } from '../store/tree';
import { ROOT, connNodeId } from '../store/treeModel';
import { ids, resetStores } from '../test/backend';
import { ConnectionTree } from './ConnectionTree';

describe('tree virtualization', () => {
  beforeEach(() => {
    resetStores();
  });

  it('renders only the visible window of 5,000 object nodes', () => {
    const conn = connNodeId(ids.prod);
    const folder = `${conn}/db:shop/folder:Table`;
    const nodes = new Map<string, TreeNode>();
    nodes.set(conn, { id: conn, kind: 'connection', label: 'shop-prod', engine: 'MY' });
    nodes.set(`${conn}/db:shop`, { id: `${conn}/db:shop`, parentId: conn, kind: 'database', label: 'shop' });
    nodes.set(folder, { id: folder, parentId: `${conn}/db:shop`, kind: 'folder', label: 'Tables', folderKind: 'Table' });
    const objects: string[] = [];
    for (let i = 0; i < 5000; i++) {
      const id = `${folder}/obj:Table:t${i}`;
      objects.push(id);
      nodes.set(id, {
        id,
        parentId: folder,
        kind: 'object',
        label: `t${i}`,
        objectRef: { database: 'shop', schema: null, name: `t${i}`, kind: 'Table' },
      });
    }
    useTree.setState({
      nodes,
      childIds: new Map([
        [ROOT, [conn]],
        [conn, [`${conn}/db:shop`]],
        [`${conn}/db:shop`, [folder]],
        [folder, objects],
      ]),
      expanded: new Set([conn, `${conn}/db:shop`, folder]),
    });

    expect(useTree.getState().visibleRows()).toHaveLength(5003);
    render(<ConnectionTree />);

    const items = screen.getAllByRole('treeitem');
    expect(items.length).toBeGreaterThan(10);
    expect(items.length).toBeLessThan(100);
    expect(screen.getByRole('treeitem', { name: 't0' })).toBeInTheDocument();
    expect(screen.queryByRole('treeitem', { name: 't4999' })).not.toBeInTheDocument();
  });
});
