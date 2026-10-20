import type { ObjectKind } from '../bindings';
import { useTree, type TreeNode } from '../store/tree';
import { connNodeId } from '../store/treeModel';

export interface SeedFolder {
  db: string;
  kind: ObjectKind;
  names: string[];
}

/**
 * Fills the tree store with an already-loaded connection → database → folder → objects
 * branch, as if the user had expanded it (no backend involved).
 */
export function seedTree(connectionId: string, folders: SeedFolder[]) {
  const nodes = new Map(useTree.getState().nodes);
  const childIds = new Map(useTree.getState().childIds);
  const connId = connNodeId(connectionId);
  nodes.set(connId, { id: connId, kind: 'connection', label: connectionId });
  for (const { db, kind, names } of folders) {
    const dbId = `${connId}/db:${db}`;
    const folderId = `${dbId}/folder:${kind}`;
    nodes.set(dbId, { id: dbId, parentId: connId, kind: 'database', label: db, scope: { database: db, schema: null } });
    nodes.set(folderId, { id: folderId, parentId: dbId, kind: 'folder', label: kind, folderKind: kind });
    childIds.set(
      folderId,
      names.map((name) => {
        const id = `${folderId}/obj:${kind}:${name}`;
        nodes.set(id, { id, parentId: folderId, kind: 'object', label: name, objectRef: { database: db, schema: null, name, kind } } as TreeNode);
        return id;
      }),
    );
  }
  useTree.setState({ nodes, childIds });
}
