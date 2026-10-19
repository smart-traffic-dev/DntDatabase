import { useEffect, useRef } from 'react';
import { useConnections } from '../store/connections';
import { useDialogs } from '../store/dialogs';
import { useTree, type TreeNode } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';
import { openNewQuery } from './newQuery';
import { openObjectTab } from './openObject';

interface Item {
  label: string;
  hint?: string;
  danger?: boolean;
  run(): void;
}

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
  const refresh: Item ={ label: 'Refresh', hint: 'F5', run: () => void tree.refresh(node.id) };

  if (node.kind === 'connection' && connectionId) {
    const isOpen = useConnections.getState().info.has(connectionId);
    return [
      isOpen
        ? { label: 'Close', run: () => void tree.closeConnection(connectionId) }
        : { label: 'Open', run: () => tree.expand(node.id) },
      newQuery,
      { label: 'Edit', run: () => dialogs.requestDialog({ kind: 'editConnection', id: connectionId }) },
      { label: 'Duplicate', run: () => void tree.duplicateConnection(connectionId) },
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
      { label: 'New group', run: () => dialogs.requestDialog({ kind: 'newGroup', parentId: node.parentId ? uuidOf(node.parentId) : null }) },
      refresh,
      copy,
    ];
  }
  if (node.kind === 'group') {
    return [
      { label: 'Edit', run: () => dialogs.requestDialog({ kind: 'renameGroup', id: uuidOf(node.id) }) },
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
      { label: 'New group', run: () => dialogs.requestDialog({ kind: 'newGroup', parentId: uuidOf(node.id) }) },
      refresh,
      copy,
    ];
  }
  if (node.kind === 'object') {
    return [{ label: 'Open', run: () => openObjectTab(node) }, newQuery, refresh, copy];
  }
  return [newQuery, refresh, copy];
}

export function TreeContextMenu({ node, x, y, onClose }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const items = itemsFor(node);

  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>('button')?.focus();
    const away = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const key = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    document.addEventListener('mousedown', away);
    document.addEventListener('keydown', key);
    return () => {
      document.removeEventListener('mousedown', away);
      document.removeEventListener('keydown', key);
    };
  }, [onClose]);

  return (
    <div ref={ref} role="menu" aria-label={`${node.label} actions`} className="context-menu" style={{ left: x, top: y }}>
      {items.map((item) => (
        <button
          key={item.label}
          type="button"
          role="menuitem"
          className={item.danger ? 'danger' : undefined}
          onClick={() => {
            onClose();
            item.run();
          }}
        >
          <span>{item.label}</span>
          {item.hint && <kbd>{item.hint}</kbd>}
        </button>
      ))}
    </div>
  );
}
