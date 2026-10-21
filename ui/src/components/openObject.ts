import type { ObjectRef, Scope } from '../bindings';
import { useObjectData } from '../store/objectData';
import { designTabId, useTabs, objectTabId } from '../store/tabs';
import type { TreeNode } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';

export interface OpenOptions {
  /** Open on the DDL sub-tab (Ctrl+Enter) instead of the tab's own (Data for tables and views). */
  ddl?: boolean;
  /**
   * Open as the group's preview tab (a single click in the tree): the next preview replaces it.
   * Without it the tab is kept, and an open preview of the same object becomes a normal tab.
   */
  preview?: boolean;
}

/** Opens (or activates) the tab of an object; returns its id. */
export function openObjectRef(connectionId: string, objectRef: ObjectRef, title: string, opts: OpenOptions = {}): string {
  const id = objectTabId(connectionId, objectRef);
  const tabs = useTabs.getState();
  const tab = { id, kind: 'object' as const, connectionId, objectRef, title };
  if (opts.preview) tabs.openPreview(tab);
  else {
    tabs.open(tab);
    tabs.keepTab(id);
  }
  if (opts.ddl) useObjectData.getState().setSub(id, 'ddl');
  return id;
}

/** Opens (or activates) the designer tab of a table (`table` null: a new table in `scope`); returns its id. */
export function openDesignTab(connectionId: string, scope: Scope, table: string | null): string {
  const id = designTabId(connectionId, scope, table);
  const tabs = useTabs.getState();
  tabs.open({ id, kind: 'design', connectionId, scope, table, title: table ?? 'New table' });
  tabs.keepTab(id);
  return id;
}

/** Opens (or activates) the tab of an object node. */
export function openObjectTab(node: TreeNode, opts: OpenOptions = {}) {
  const connectionId = connectionIdOf(node.id);
  if (!node.objectRef || !connectionId) return;
  openObjectRef(connectionId, node.objectRef, node.label, opts);
}
