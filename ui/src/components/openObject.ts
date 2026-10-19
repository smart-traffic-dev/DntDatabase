import { useTabs, objectTabId } from '../store/tabs';
import type { TreeNode } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';

/** Opens (or activates) the tab of an object node. */
export function openObjectTab(node: TreeNode) {
  const connectionId = connectionIdOf(node.id);
  if (!node.objectRef || !connectionId) return;
  useTabs.getState().open({
    id: objectTabId(connectionId, node.objectRef),
    kind: 'object',
    connectionId,
    objectRef: node.objectRef,
    title: node.label,
  });
}
