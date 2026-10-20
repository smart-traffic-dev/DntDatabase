import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { useTree, type TreeNode } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';

/**
 * The (connection, database) a node stands for: a database or deeper node gives its database,
 * a connection node its default database, anything else (a group, nothing) neither.
 */
export function queryTargetOf(node: TreeNode | undefined): { connectionId: string | null; database: string | null } {
  const connectionId = node ? (connectionIdOf(node.id) ?? null) : null;
  if (!node || !connectionId) return { connectionId: null, database: null };
  const database = node.scope?.database ?? node.objectRef?.database;
  if (database) return { connectionId, database };
  const fallback = node.kind === 'connection' ? useConnections.getState().summaries.get(connectionId)?.config.default_db : null;
  return { connectionId, database: fallback ?? null };
}

/** Opens a new query tab bound to a tree node (the selected one by default). */
export function openNewQuery(node?: TreeNode): string {
  const tree = useTree.getState();
  const from = node ?? (tree.selectedId ? tree.nodes.get(tree.selectedId) : undefined);
  const { connectionId, database } = queryTargetOf(from);
  return useQueryDocs.getState().newQuery(connectionId, database);
}
