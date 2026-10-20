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
      SEP,
      refresh,
      copy,
    ];
  }
  return [newQuery, SEP, refresh, copy];
}

export function TreeContextMenu({ node, x, y, onClose }: Props) {
  return <ActionMenu label={`${node.label} actions`}x={x} y={y} items={itemsFor(node)} onClose={onClose} />;
}
