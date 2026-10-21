import { beforeEach, describe, expect, it } from 'vitest';
import type { ObjectRef } from '../bindings';
import { useConnections } from '../store/connections';
import { designTabId, useTabs } from '../store/tabs';
import { config, ids, resetStores, serverInfo } from '../test/backend';
import type { TreeNode } from '../store/tree';
import { designActionItems } from './TreeContextMenu';

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

  it('nothing while the connection is closed, and nothing on a view', () => {
    expect(designActionItems({ ...tableNode, objectRef: { ...table, kind: 'View' } })).toEqual([]);
    useConnections.getState().reset();
    useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
    expect(designActionItems(tableNode)).toEqual([]);
    expect(designActionItems(databaseNode)).toEqual([]);
  });
});
