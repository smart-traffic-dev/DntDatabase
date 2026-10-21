import { describe, expect, it } from 'vitest';
import { ROOT, computeRows, emptyLabel, type TreeNode, type TreeRow } from './treeModel';

const conn: TreeNode = { id: 'conn:abc', kind: 'connection', label: 'db' };
const folder: TreeNode = { id: 'conn:abc/db:shop/folder:Table', kind: 'folder', label: 'Tables', folderKind: 'Table' };

function rows(filter = '', folderChildren: string[] | null = []): TreeRow[] {
  const childIds = new Map<string, string[]>([
    [ROOT, [conn.id]],
    [conn.id, [folder.id]],
  ]);
  if (folderChildren) childIds.set(folder.id, folderChildren);
  return computeRows({
    nodes: new Map([
      [conn.id, conn],
      [folder.id, folder],
    ]),
    childIds,
    expanded: new Set([conn.id, folder.id]),
    filter,
  });
}

describe('computeRows placeholders', () => {
  it('shows an empty row under a loaded, expanded node with no children', () => {
    const list = rows();
    expect(list.map((r) => r.type)).toEqual(['node', 'node', 'empty']);
    expect(list[2]).toMatchObject({ key: `${folder.id}#empty`, level: 3, node: folder });
  });

  it('shows nothing for children that are not loaded yet', () => {
    expect(rows('', null).map((r) => r.type)).toEqual(['node', 'node']);
  });

  it('shows no placeholders while filtering', () => {
    expect(rows('nothing matches')).toEqual([]);
  });

  it('names what is missing', () => {
    expect(emptyLabel(folder)).toBe('No tables');
    expect(emptyLabel({ ...folder, folderKind: 'MaterializedView' })).toBe('No materialized views');
    expect(emptyLabel(conn)).toBe('No databases');
    expect(emptyLabel({ id: 'group:x', kind: 'group', label: 'G' })).toBe('Empty group');
    expect(emptyLabel({ id: 'conn:abc/db:shop', kind: 'database', label: 'shop' })).toBe('No schemas');
  });
});
