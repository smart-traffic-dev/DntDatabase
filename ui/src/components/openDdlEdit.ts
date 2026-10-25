import type { ObjectKind, Scope } from '../bindings';
import { useDdlEdits } from '../store/ddlEdits';
import { useTabs } from '../store/tabs';

/** The kinds whose DDL can be edited and created in a DDL editor tab, per engine. */
export const DDL_EDITABLE_KINDS: Record<'mysql' | 'postgres', ObjectKind[]> = {
  mysql: ['View', 'Function', 'Procedure', 'Trigger', 'Event'],
  postgres: ['View', 'MaterializedView', 'Function', 'Procedure', 'Trigger'],
};

const NEW_TITLES: Partial<Record<ObjectKind, string>> = {
  View: 'New view',
  MaterializedView: 'New materialized view',
  Function: 'New function',
  Procedure: 'New procedure',
  Trigger: 'New trigger',
  Event: 'New event',
};

/** The title of a DDL editor tab: the object's name, or "New view" / "New function" … for a new one. */
export const ddlTabTitle = (kind: ObjectKind, name: string | null): string => name ?? NEW_TITLES[kind] ?? `New ${kind.toLowerCase()}`;

/**
 * Opens the DDL editor of an object (`name` null: a new object of `kind` in `scope`, `table` a new
 * trigger's table) and returns the tab id. An object that already has a DDL tab activates it; a
 * new object always gets a new tab.
 */
export function openDdlEditTab(connectionId: string, scope: Scope, kind: ObjectKind, name: string | null, table: string | null): string {
  const target = { connectionId, scope, kind, name, table };
  const tabs = useTabs.getState();
  const existing = useDdlEdits.getState().findTab(target);
  if (existing && tabs.tabs.some((t) => t.id === existing)) {
    tabs.activate(existing);
    return existing;
  }
  const id = `ddl/${crypto.randomUUID()}`;
  void useDdlEdits.getState().load(id, target);
  tabs.open({ id, kind: 'ddl', connectionId, objectKind: kind, title: ddlTabTitle(kind, name) });
  tabs.keepTab(id);
  return id;
}
