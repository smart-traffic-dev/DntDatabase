import type { ObjectKind, ObjectRef } from '../bindings';
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

/** Quotes an identifier the way the engine does (the quote character inside it doubled). */
export function quoteIdent(kind: 'mysql' | 'postgres', name: string): string {
  return kind === 'postgres' ? `"${name.replaceAll('"', '""')}"` : `\`${name.replaceAll('`', '``')}\``;
}

export interface SelectOptions {
  /** Default 1000. */
  limit?: number;
  /** Rows to skip (`OFFSET`); omitted when 0. */
  offset?: number;
  orderBy?: { column: string; dir: 'asc' | 'desc' } | null;
}

/**
 * `SELECT * ... LIMIT 1000` of a table or view, identifiers quoted for the connection's engine.
 * The one helper for this SQL: the tree's "Select top 1000", the object tab's Query button and
 * the data view's "Open in query" (which passes its page size, offset and sort).
 */
export function selectTopSql(target: { objectRef?: ObjectRef }, kind: 'mysql' | 'postgres', opts: SelectOptions = {}): string {
  const ref = target.objectRef;
  if (!ref) return '';
  // PostgreSQL addresses a table by schema (a connection is one database); MySQL by database.
  const qualifier = kind === 'postgres' ? ref.schema : ref.database;
  const table = qualifier ? `${quoteIdent(kind, qualifier)}.${quoteIdent(kind, ref.name)}` : quoteIdent(kind, ref.name);
  const order = opts.orderBy ? ` ORDER BY ${quoteIdent(kind, opts.orderBy.column)} ${opts.orderBy.dir === 'desc' ? 'DESC' : 'ASC'}` : '';
  const offset = opts.offset ? ` OFFSET ${Math.trunc(opts.offset)}` : '';
  return `SELECT * FROM ${table}${order} LIMIT ${Math.trunc(opts.limit ?? 1000)}${offset};`;
}

/** Whether `SELECT * FROM` makes sense on an object of this kind. */
export const isQueryable = (kind: ObjectKind | undefined) => kind === 'Table' || kind === 'View' || kind === 'MaterializedView';

const engineOf = (connectionId: string | null | undefined) =>
  (connectionId ? useConnections.getState().summaries.get(connectionId)?.config.kind : undefined) ?? 'mysql';

/** Opens a new query tab on a table / view node, with its first 1000 rows selected. */
export function openSelectTop(node: TreeNode): string {
  const id = openNewQuery(node);
  useQueryDocs.getState().update(id, { sql: selectTopSql(node, engineOf(connectionIdOf(node.id))) });
  return id;
}

/** Ctrl+Q from the tree: on a table / view its first 1000 rows are selected, anything else just opens a query. */
export function openQueryOnNode(node: TreeNode): string {
  return node.kind === 'object' && isQueryable(node.objectRef?.kind) ? openSelectTop(node) : openNewQuery(node);
}

/** Opens a query bound to `connectionId` / `database`, with the first 1000 rows of `ref` selected when it is a table or view. */
function openQueryOn(connectionId: string, database: string | null, ref?: ObjectRef): string {
  const id = useQueryDocs.getState().newQuery(connectionId, database);
  if (ref && isQueryable(ref.kind)) useQueryDocs.getState().update(id, { sql: selectTopSql({ objectRef: ref }, engineOf(connectionId)) });
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
    case 'query': {
      const doc = useQueryDocs.getState().docs.get(tab.id);
      return useQueryDocs.getState().newQuery(doc?.connectionId ?? tab.connectionId, doc?.database ?? null);
    }
    default:
      return openNewQuery();
  }
}
