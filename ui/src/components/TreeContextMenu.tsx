import type { ObjectAction } from '../bindings';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTree, type TreeNode } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';
import { ActionMenu, type MenuEntry } from './ActionMenu';
import { openNewQuery, openSelectTop } from './newQuery';
import { openObjectTab } from './openObject';

type Item = MenuEntry;
const SEP: Item = { separator: true };

interface Props {
  node: TreeNode;
  x: number;
  y: number;
  onClose(): void;
}

const uuidOf = (nodeId: string) => nodeId.slice(nodeId.indexOf(':') + 1);

export function objectActionItems(node: TreeNode): Item[] {
  const connectionId = connectionIdOf(node.id);
  if (!connectionId) return [];
  const stored = useConnections.getState();
  const cfg = stored.summaries.get(connectionId)?.config;
  if (!cfg || !stored.info.has(connectionId)) return [];
  const pg = cfg.kind === 'postgres';
  const item = (label: string, action: ObjectAction, danger = false): Item => ({
    label, danger, disabled: cfg.read_only,
    run: () => useDialogs.getState().requestDialog({ kind: 'objectAction', connectionId, action }),
  });
  if (node.kind === 'connection' && !pg) return [item('Create database\u2026', { kind: 'create_namespace', database: '', name: '' })];
  if (node.kind === 'database' && node.scope) {
    if (pg) return [item('Create schema\u2026', { kind: 'create_namespace', database: node.scope.database, name: '' })];
    return [item('Drop database\u2026', { kind: 'drop_namespace', database: node.scope.database, name: node.scope.database }, true)];
  }
  if (node.kind === 'schema' && node.scope?.schema && pg) return [item('Drop schema\u2026', { kind: 'drop_namespace', database: node.scope.database, name: node.scope.schema }, true)];
  const object = node.objectRef;
  if (node.kind !== 'object' || !object) return [];
  if (object.kind === 'Table') return [
    item('Rename table\u2026', { kind: 'rename_table', object, name: object.name }),
    item('Truncate table\u2026', { kind: 'truncate_table', object }, true),
    item('Drop table\u2026', { kind: 'drop_object', object }, true),
  ];
  if (object.kind === 'View' || (pg && object.kind === 'MaterializedView')) return [item('Drop view\u2026', { kind: 'drop_object', object }, true)];
  return [];
}

function itemsFor(node: TreeNode): Item[] {
  const tree = useTree.getState();
  const dialogs = useDialogs.getState();
  const connectionId = connectionIdOf(node.id);
  const copy: Item = {
    label: 'Copy name',
    run: () => void navigator.clipboard?.writeText(node.label).catch(() => undefined),
  };
  const newQuery: Item = { label: 'New query', run: () => void openNewQuery(node) };
  const refresh: Item = { label: 'Refresh', hint: 'F5', run: () => void tree.refresh(node.id) };

  if (node.kind === 'connection' && connectionId) {
    const isOpen = useConnections.getState().info.has(connectionId);
    return [
      isOpen
        ? { label: 'Close', run: () => void tree.closeConnection(connectionId) }
        : { label: 'Open', hint: 'Enter', run: () => tree.expand(node.id) },
      newQuery,
      ...objectActionItems(node),
      SEP,
      { label: 'Edit', run: () => dialogs.requestDialog({ kind: 'editConnection', id: connectionId }) },
      { label: 'Duplicate', run: () => void tree.duplicateConnection(connectionId) },
      { label: 'New group', run: () => dialogs.requestDialog({ kind: 'newGroup', parentId: node.parentId ? uuidOf(node.parentId) : null }) },
      SEP,
      refresh,
      copy,
      SEP,
      {
        label: 'Delete',
        danger: true,
        run: () =>
          dialogs.requestDialog({
            kind: 'deleteNode',
            node: { type: 'connection', id: connectionId },
            label: node.label,
          }),
      },
    ];
  }
  if (node.kind === 'group') {
    return [
      { label: 'Edit', run: () => dialogs.requestDialog({ kind: 'renameGroup', id: uuidOf(node.id) }) },
      { label: 'New group', run: () => dialogs.requestDialog({ kind: 'newGroup', parentId: uuidOf(node.id) }) },
      SEP,
      refresh,
      copy,
      SEP,
      {
        label: 'Delete',
        danger: true,
        run: () =>
          dialogs.requestDialog({
            kind: 'deleteNode',
            node: { type: 'group', id: uuidOf(node.id) },
            label: node.label,
          }),
      },
    ];
  }
  if (node.kind === 'object') {
    const kind = node.objectRef?.kind;
    const queryable = kind === 'Table' || kind === 'View' || kind === 'MaterializedView';
    return [
      { label: 'Open', hint: 'Enter', run: () => openObjectTab(node) },
      ...(queryable ? [{ label: 'Select top 1000', run: () => void openSelectTop(node) }] : []),
      newQuery,
      ...objectActionItems(node),
      SEP,
      refresh,
      copy,
    ];
  }
  return [newQuery, ...objectActionItems(node), SEP, refresh, copy];
}

export function TreeContextMenu({ node, x, y, onClose }: Props) {
  return <ActionMenu label={`${node.label} actions`} x={x} y={y} items={itemsFor(node)} onClose={onClose} />;
}
