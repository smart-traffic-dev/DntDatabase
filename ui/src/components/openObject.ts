import type { ObjectRef } from '../bindings';
import { useObjectData } from '../store/objectData';
import { useTabs, objectTabId } from '../store/tabs';
import type { TreeNode } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';

export interface OpenOptions {
  /** Open on the DDL sub-tab (Ctrl+Enter) instead of the tab's own (Data for tables and views). */
  ddl?: boolean;
}

/** Opens (or activates) the tab of an object; returns its id. */
export function openObjectRef(connectionId: string, objectRef: ObjectRef, title: string, opts: OpenOptions = {}): string {
  const id = objectTabId(connectionId, objectRef);
  useTabs.getState().open({ id, kind: 'object', connectionId, objectRef, title });
  if (opts.ddl) useObjectData.getState().setSub(id, 'ddl');
  return id;
}

/** Opens (or activates) the tab of an object node. */
export function openObjectTab(node: TreeNode, opts: OpenOptions = {}) {
  const connectionId = connectionIdOf(node.id);
  if (!node.objectRef || !connectionId) return;
  openObjectRef(connectionId, node.objectRef, node.label, opts);
}
