import type { MenuEntry } from '../../components/ActionMenu';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import type { TreeNode } from '../../store/tree';
import { connectionIdOf } from '../../store/treeModel';

/**
 * The tree's backup entries (spec §7.4): **Dump…** and **Restore…** on a database (MySQL) or a
 * database and a schema (PostgreSQL), **Dump table…** on a table. Dumps are reads and stay enabled
 * on read-only connections; restoring is blocked there (R20), so its entry is disabled. Nothing
 * while the connection is closed.
 */
export function backupActionItems(node: TreeNode): MenuEntry[] {
  const connectionId = connectionIdOf(node.id);
  if (!connectionId) return [];
  const stored = useConnections.getState();
  const cfg = stored.summaries.get(connectionId)?.config;
  if (!cfg || !stored.info.has(connectionId)) return [];
  const { requestDialog } = useDialogs.getState();
  const object = node.objectRef;
  if (node.kind === 'object') {
    if (object?.kind !== 'Table') return [];
    return [
      {
        label: 'Dump table…',
        run: () => requestDialog({ kind: 'dump', connectionId, database: object.database, schema: object.schema, table: object.name }),
      },
    ];
  }
  const scope = node.scope;
  if (!scope) return [];
  const pg = cfg.kind === 'postgres';
  const fits = node.kind === 'database' || (pg && node.kind === 'schema' && !!scope.schema);
  if (!fits) return [];
  return [
    { label: 'Dump…', run: () => requestDialog({ kind: 'dump', connectionId, database: scope.database, schema: scope.schema }) },
    {
      label: 'Restore…',
      disabled: cfg.read_only,
      run: () => requestDialog({ kind: 'restore', connectionId, database: scope.database }),
    },
  ];
}
