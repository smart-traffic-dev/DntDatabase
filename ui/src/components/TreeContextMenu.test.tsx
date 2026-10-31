import { beforeEach, describe, expect, it } from 'vitest';
import type { ObjectRef } from '../bindings';
import { useConnections } from '../store/connections';
import { designTabId, useTabs } from '../store/tabs';
import { useDdlEdits } from '../store/ddlEdits';
import { callsOf, config, ids, mockBackend, resetStores, serverInfo } from '../test/backend';
import type { TreeNode } from '../store/tree';
import { useDialogs } from '../store/dialogs';
import { ddlActionItems, designActionItems, importActionItems, seedActionItems } from './TreeContextMenu';

const table: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const tableNode: TreeNode = { id: `conn:${ids.prod}/obj`, kind: 'object', label: 'orders', objectRef: table };
const databaseNode: TreeNode = { id: `conn:${ids.prod}/db`, kind: 'database', label: 'shop', scope: { database: 'shop', schema: null } };

const entry = (node: TreeNode, label: string) => {
  const found = designActionItems(node).find((e) => 'label' in e && e.label === label);
  if (!found || !('run' in found)) throw new Error(`no "${label}" item`);
  return found;
};

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});

describe('table designer tree entries', () => {
  it('design_table_item_opens_design_tab', () => {
    entry(tableNode, 'Design table').run();
    const scope = { database: 'shop', schema: null };
    expect(useTabs.getState().tabs).toEqual([
      { id: designTabId(ids.prod, scope, 'orders'), kind: 'design', connectionId: ids.prod, scope, table: 'orders', title: 'orders' },
    ]);
    // A second click activates the same tab.
    entry(tableNode, 'Design table').run();
    expect(useTabs.getState().tabs).toHaveLength(1);
  });

  it('new_table_item_on_schema', () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    const schemaNode: TreeNode = { id: `conn:${ids.prod}/s`, kind: 'schema', label: 'sales', scope: { database: 'shop', schema: 'sales' } };
    entry(schemaNode, 'New table…').run();
    expect(useTabs.getState().tabs[0]).toMatchObject({ kind: 'design', table: null, scope: { database: 'shop', schema: 'sales' }, title: 'New table' });
    // PostgreSQL tables live in schemas: the database node has no such entry.
    expect(designActionItems({ ...databaseNode, scope: { database: 'shop', schema: null } })).toEqual([]);
  });

  it('new table on a MySQL database; disabled read-only; Design table still enabled', () => {
    expect(entry(databaseNode, 'New table…')).toMatchObject({ disabled: false });
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(entry(databaseNode, 'New table…')).toMatchObject({ disabled: true });
    expect(entry(tableNode, 'Design table')).not.toHaveProperty('disabled', true);
  });

  it('New table… on the Tables folder opens a design tab in its scope (MySQL and PostgreSQL)', () => {
    const mysqlFolder: TreeNode = { id: `conn:${ids.prod}/db:shop/folder:Table`, kind: 'folder', label: 'Tables', folderKind: 'Table', scope: { database: 'shop', schema: null } };
    entry(mysqlFolder, 'New table…').run();
    expect(useTabs.getState().tabs[0]).toMatchObject({ kind: 'design', table: null, scope: { database: 'shop', schema: null }, title: 'New table' });
    // Other folders have no designer entry.
    expect(designActionItems({ ...mysqlFolder, folderKind: 'View', label: 'Views' })).toEqual([]);
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(entry(mysqlFolder, 'New table…')).toMatchObject({ disabled: true });

    resetStores();
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
    const pgFolder: TreeNode = { ...mysqlFolder, id: `conn:${ids.prod}/db:shop/schema:sales/folder:Table`, scope: { database: 'shop', schema: 'sales' } };
    entry(pgFolder, 'New table…').run();
    expect(useTabs.getState().tabs[0]).toMatchObject({ kind: 'design', table: null, scope: { database: 'shop', schema: 'sales' } });
    expect(designActionItems({ ...pgFolder, scope: { database: 'shop', schema: null } })).toEqual([]);
  });

  it('Import data… on a table opens the import wizard pre-filled (also read-only); not on a view', () => {
    const found = importActionItems(tableNode).find((e) => 'label' in e && e.label === 'Import data…');
    if (!found || !('run' in found)) throw new Error('no "Import data…" item');
    found.run();
    expect(useDialogs.getState().request).toEqual({ kind: 'importData', connectionId: ids.prod, table });
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(importActionItems(tableNode)[0]).not.toHaveProperty('disabled', true);
    expect(importActionItems({ ...tableNode, objectRef: { ...table, kind: 'View' } })).toEqual([]);
    useConnections.getState().clearInfo(ids.prod);
    expect(importActionItems(tableNode)).toEqual([]);
  });

  it('Generate data… on a table opens the seeder; disabled read-only; not on a view', () => {
    const found = seedActionItems(tableNode).find((e) => 'label' in e && e.label === 'Generate data…');
    if (!found || !('run' in found)) throw new Error('no "Generate data…" item');
    expect(found).toMatchObject({ disabled: false });
    found.run();
    expect(useDialogs.getState().request).toEqual({ kind: 'seedData', connectionId: ids.prod, table });
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(seedActionItems(tableNode)[0]).toMatchObject({ disabled: true, title: 'Read-only connection' });
    expect(seedActionItems({ ...tableNode, objectRef: { ...table, kind: 'View' } })).toEqual([]);
    useConnections.getState().clearInfo(ids.prod);
    expect(seedActionItems(tableNode)).toEqual([]);
  });

  it('nothing while the connection is closed, and nothing on a view', () => {
    expect(designActionItems({ ...tableNode, objectRef: { ...table, kind: 'View' } })).toEqual([]);
    useConnections.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    expect(designActionItems(tableNode)).toEqual([]);
    expect(designActionItems(databaseNode)).toEqual([]);
  });
});

describe('DDL editor tree entries', () => {
  const ddlEntry = (node: TreeNode, label: string) => {
    const found = ddlActionItems(node).find((e) => 'label' in e && e.label === label);
    if (!found || !('run' in found)) throw new Error(`no "${label}" item`);
    return found;
  };
  const labels = (node: TreeNode) => ddlActionItems(node).map((e) => ('label' in e ? e.label : '-'));
  const folder = (kind: ObjectRef['kind'], schema: string | null = null): TreeNode => ({
    id: `conn:${ids.prod}/db/folder:${kind}`,
    kind: 'folder',
    label: kind,
    folderKind: kind,
    scope: { database: 'shop', schema },
  });
  const usePg = () => {
    useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
    useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
  };

  beforeEach(() => {
    mockBackend({ ddl_edit_open: () => new Promise(() => undefined), ddl_edit_close: () => null });
  });

  it('edit_definition_on_view', () => {
    const view: TreeNode = { ...tableNode, label: 'v', objectRef: { ...table, kind: 'View', name: 'v' } };
    ddlEntry(view, 'Edit definition').run();
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useTabs.getState().tabs[0]).toMatchObject({ kind: 'ddl', connectionId: ids.prod, objectKind: 'View', title: 'v' });
    expect(useTabs.getState().tabs[0]!.id).toMatch(/^ddl\//);
    expect(callsOf('ddl_edit_open')[0]).toEqual({ connectionId: ids.prod, scope: { database: 'shop', schema: null }, kind: 'View', name: 'v', table: null });
    // A second click activates the same tab.
    ddlEntry(view, 'Edit definition').run();
    expect(useTabs.getState().tabs).toHaveLength(1);
    // Edit definition stays enabled on a read-only connection (the editor shows why it cannot apply).
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(ddlEntry(view, 'Edit definition')).not.toHaveProperty('disabled', true);
    // Sequences are not edited through DDL.
    expect(ddlActionItems({ ...tableNode, objectRef: { ...table, kind: 'Sequence' } })).toEqual([]);
  });

  it('new_function_on_folder', () => {
    expect(ddlEntry(folder('Function'), 'New function…')).toMatchObject({ disabled: false });
    ddlEntry(folder('Function'), 'New function…').run();
    ddlEntry(folder('Function'), 'New function…').run();
    // New objects always get their own tab.
    expect(useTabs.getState().tabs.map((t) => t.title)).toEqual(['New function', 'New function']);
    expect(callsOf('ddl_edit_open')[0]).toEqual({ connectionId: ids.prod, scope: { database: 'shop', schema: null }, kind: 'Function', name: null, table: null });
    expect(labels(folder('View'))).toEqual(['New view…']);
    expect(labels(folder('Procedure'))).toEqual(['New procedure…']);
    expect(labels(folder('Trigger'))).toEqual(['New trigger…']);
    expect(labels(folder('Event'))).toEqual(['New event…']);
    expect(labels(folder('Table'))).toEqual([]);
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(ddlEntry(folder('Function'), 'New function…')).toMatchObject({ disabled: true });
  });

  it('new_trigger_on_table', () => {
    ddlEntry(tableNode, 'New trigger…').run();
    expect(useTabs.getState().tabs[0]).toMatchObject({ kind: 'ddl', objectKind: 'Trigger', title: 'New trigger' });
    const ddlId = useTabs.getState().tabs[0]!.id;
    expect(useDdlEdits.getState().byTab[ddlId]!.target).toEqual({ connectionId: ids.prod, scope: { database: 'shop', schema: null }, kind: 'Trigger', name: null, table: 'orders' });
    expect(callsOf('ddl_edit_open')[0]).toMatchObject({ kind: 'Trigger', name: null, table: 'orders' });
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(ddlEntry(tableNode, 'New trigger…')).toMatchObject({ disabled: true });
  });

  it('no_entries_when_closed', () => {
    useConnections.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    expect(ddlActionItems({ ...tableNode, objectRef: { ...table, kind: 'View' } })).toEqual([]);
    expect(ddlActionItems(folder('View'))).toEqual([]);
    expect(ddlActionItems(tableNode)).toEqual([]);
  });

  it('pg_has_materialized_view_entries', () => {
    usePg();
    expect(labels(folder('MaterializedView', 'public'))).toEqual(['New materialized view…']);
    expect(labels(folder('Trigger', 'public'))).toEqual(['New trigger…']);
    // PostgreSQL has no events.
    expect(labels(folder('Event', 'public'))).toEqual([]);
    const mv: TreeNode = { ...tableNode, objectRef: { database: 'shop', schema: 'public', name: 'mv', kind: 'MaterializedView' } };
    ddlEntry(mv, 'Edit definition').run();
    expect(useTabs.getState().tabs[0]).toMatchObject({ kind: 'ddl', objectKind: 'MaterializedView', title: 'mv' });
  });
});
