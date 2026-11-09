import type { DbKind, ObjectKind, ObjectRef } from '../bindings';
import { canSample, quoteIdent, sampleQuery, selectTopSql, type SelectOptions } from '../engine';
import { useConnections } from '../store/connections';
import { useDdlEdits } from '../store/ddlEdits';
import { useQueryDocs } from '../store/queryTabs';
import { useTabs } from '../store/tabs';
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

export { quoteIdent };

export { selectTopSql, type SelectOptions };

/** The kind of a connection, or `undefined` when unknown. */
const kindOf = (connectionId: string | null | undefined): DbKind | undefined =>
  connectionId ? useConnections.getState().summaries.get(connectionId)?.config.kind : undefined;

/** Whether a sample query (`SELECT * FROM`, `db.x.find()`) makes sense on an object of this kind in this connection. */
export const isQueryable = (connectionId: string | null | undefined, kind: ObjectKind | undefined): boolean => {
  const engine = kindOf(connectionId);
  return engine !== undefined && canSample(engine, kind);
};

/** Sets the sample query of `ref` on tab `id` when the connection's engine has one. */
function fillSample(id: string, connectionId: string | null | undefined, ref: ObjectRef | undefined) {
  const kind = kindOf(connectionId);
  const text = kind && ref ? sampleQuery(kind, ref) : null;
  if (text) useQueryDocs.getState().update(id, { sql: text });
}

/** Opens a new query tab on a table / view / collection node, with its first 1000 rows selected. */
export function openSelectTop(node: TreeNode): string {
  const id = openNewQuery(node);
  fillSample(id, connectionIdOf(node.id), node.objectRef);
  return id;
}

/** Ctrl+Q from the tree: on a table / view its first 1000 rows are selected, anything else just opens a query. */
export function openQueryOnNode(node: TreeNode): string {
  return node.kind === 'object' && isQueryable(connectionIdOf(node.id), node.objectRef?.kind) ? openSelectTop(node) : openNewQuery(node);
}

/** Opens a query bound to `connectionId` / `database`, with the first 1000 rows of `ref` selected when it is a table or view. */
function openQueryOn(connectionId: string, database: string | null, ref?: ObjectRef): string {
  const id = useQueryDocs.getState().newQuery(connectionId, database);
  fillSample(id, connectionId, ref);
  return id;
}

/**
 * Ctrl+Q anywhere outside the tree: a new query on what the active tab is about — an open table or
 * view gets `SELECT * FROM it LIMIT 1000`, a query tab's connection and database carry over.
 */
export function openQueryFromActiveTab(): string {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  switch (tab?.kind) {
    case 'object':
      return openQueryOn(tab.connectionId, tab.objectRef.database, tab.objectRef);
    case 'design':
      return openQueryOn(
        tab.connectionId,
        tab.scope.database,
        tab.table ? { database: tab.scope.database, schema: tab.scope.schema, name: tab.table, kind: 'Table' } : undefined,
      );
    case 'ddl': {
      const target = useDdlEdits.getState().byTab[tab.id]?.target;
      if (!target) return openQueryOn(tab.connectionId, null);
      const ref = target.name ? { database: target.scope.database, schema: target.scope.schema, name: target.name, kind: target.kind } : undefined;
      return openQueryOn(tab.connectionId, target.scope.database, ref);
    }
    case 'keys':
      return openQueryOn(tab.connectionId, tab.database);
    case 'query': {
      const doc = useQueryDocs.getState().docs.get(tab.id);
      return useQueryDocs.getState().newQuery(doc?.connectionId ?? tab.connectionId, doc?.database ?? null);
    }
    default:
      return openNewQuery();
  }
}
