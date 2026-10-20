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

/** `SELECT * ... LIMIT 1000` of a table or view, identifiers quoted for the connection's engine. */
export function selectTopSql(node: TreeNode, kind: 'mysql' | 'postgres'): string {
  const ref = node.objectRef;
  if (!ref) return '';
  const q = kind === 'postgres' ? (s: string) => `"${s.replaceAll('"', '""')}"` : (s: string) => `\`${s.replaceAll('`', '``')}\``;
  // PostgreSQL addresses a table by schema (a connection is one database); MySQL by database.
  const qualifier = kind === 'postgres' ? ref.schema : ref.database;
  const table = qualifier ? `${q(qualifier)}.${q(ref.name)}` : q(ref.name);
  return `SELECT * FROM ${table} LIMIT 1000;`;
}

/** Opens a new query tab on a table / view node, with its first 1000 rows selected. */
export function openSelectTop(node: TreeNode): string {
  const id = openNewQuery(node);
  const connectionId = connectionIdOf(node.id);
  const kind = connectionId ? useConnections.getState().summaries.get(connectionId)?.config.kind : undefined;
  useQueryDocs.getState().update(id, { sql: selectTopSql(node, kind ?? 'mysql') });
  return id;
}
