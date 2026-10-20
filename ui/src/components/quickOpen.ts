import type { ObjectKind, ObjectRef } from '../bindings';
import { useCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { objectTabId } from '../store/tabs';
import { useTree } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';
import { openObjectRef } from './openObject';
import { rankItems, type Ranked } from './paletteMatch';

/** The object kinds quick open searches. */
export const QUICK_OPEN_KINDS: readonly ObjectKind[] = ['Table', 'View', 'MaterializedView', 'Function', 'Procedure'];

export interface ObjectEntry {
  /** Unique per connection and object (the object tab id). */
  key: string;
  connectionId: string;
  objectRef: ObjectRef;
  /** The object's name. */
  name: string;
  /** "connection › database" (and the schema on PostgreSQL). */
  path: string;
}

/**
 * Every table, view, function and procedure known without asking a server: the nodes the
 * connection tree has loaded, plus the tables and views of the schemas autocomplete has cached.
 * Reads stores only, so it never starts a load (call it when the overlay opens, not per key).
 */
export function collectObjectEntries(): ObjectEntry[] {
  const { nodes } = useTree.getState();
  const connections = useConnections.getState().summaries;
  const out = new Map<string, ObjectEntry>();
  const connectionName = (id: string) => connections.get(id)?.config.name ?? nodes.get(`conn:${id}`)?.label ?? id;
  const add = (connectionId: string, objectRef: ObjectRef) => {
    if (!QUICK_OPEN_KINDS.includes(objectRef.kind)) return;
    const key = objectTabId(connectionId, objectRef);
    if (out.has(key)) return;
    const path = [connectionName(connectionId), objectRef.database, objectRef.schema].filter((p): p is string => !!p).join(' › ');
    out.set(key, { key, connectionId, objectRef, name: objectRef.name, path });
  };

  for (const node of nodes.values()) {
    const connectionId = connectionIdOf(node.id);
    if (node.kind === 'object' && node.objectRef && connectionId) add(connectionId, node.objectRef);
  }
  for (const { connId, db, scope } of useCompletion.loadedScopes()) {
    const postgres = connections.get(connId)?.config.kind === 'postgres';
    for (const t of scope.tables) {
      add(connId, { database: db, schema: postgres ? t.schema : null, name: t.name, kind: t.kind });
    }
  }
  return [...out.values()].sort(
    (a, b) => a.path.localeCompare(b.path) || a.name.localeCompare(b.name) || a.objectRef.kind.localeCompare(b.objectRef.kind),
  );
}

/** Entries matching `query` (the object's name, else its whole path), best first, at most `limit`. */
export function searchObjects(entries: readonly ObjectEntry[], query: string, limit = 50): Ranked<ObjectEntry>[] {
  return rankItems(entries, query, (e) => [e.name, `${e.path} › ${e.name}`]).slice(0, limit);
}

/** Opens the entry's tab: Data for tables and views, DDL for the rest (or always with `ddl`). */
export function openObjectEntry(entry: ObjectEntry, ddl = false): string {
  return openObjectRef(entry.connectionId, entry.objectRef, entry.name, { ddl });
}
