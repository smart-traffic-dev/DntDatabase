// "Design table" from anywhere (command palette): the table of the active tab, the table under
// the cursor of the active SQL editor, or the table selected in the connection tree.
import type { ObjectRef } from '../bindings';
import { useTabs } from '../store/tabs';
import { useTree } from '../store/tree';
import { connectionIdOf } from '../store/treeModel';
import { getActiveEditor, runEditorCommand } from '../tabs/query/activeEditor';
import { designerOn } from '../tabs/query/editorCommands';
import { openDesignTab } from './openObject';

export interface DesignTarget {
  connectionId: string;
  object: ObjectRef;
}

const asTable = (connectionId: string | undefined, object: ObjectRef | undefined): DesignTarget | null =>
  connectionId && object?.kind === 'Table' && designerOn(connectionId) ? { connectionId, object } : null;

/**
 * The table "Design table" opens without an editor: the active table tab's, else the table
 * selected in the connection tree. Null when neither is a table on a connection with a designer.
 */
export function designTargetNow(): DesignTarget | null {
  const { tabs, activeId } = useTabs.getState();
  const tab = tabs.find((t) => t.id === activeId);
  if (tab?.kind === 'object') return asTable(tab.connectionId, tab.objectRef);
  const tree = useTree.getState();
  const node = tree.selectedId ? tree.nodes.get(tree.selectedId) : undefined;
  return node?.kind === 'object' ? asTable(connectionIdOf(node.id), node.objectRef) : null;
}

/** Opens the designer of a table (see `designTargetNow`). */
export function openDesigner(target: DesignTarget): string {
  const { connectionId, object } = target;
  return openDesignTab(connectionId, { database: object.database, schema: object.schema }, object.name);
}

/** Whether "Design table" has something to act on now. */
export const canDesignNow = () => getActiveEditor() !== null || designTargetNow() !== null;

/**
 * Design table: in a SQL editor the table under the cursor (the editor's F4 action, which
 * explains a miss under the cursor), else `designTargetNow`. Returns whether it ran.
 */
export function designCurrent(): boolean {
  if (getActiveEditor()) return runEditorCommand('dnt.designTable');
  const target = designTargetNow();
  if (!target) return false;
  openDesigner(target);
  return true;
}
