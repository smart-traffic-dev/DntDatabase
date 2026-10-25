import { beforeEach, describe, expect, it } from 'vitest';
import type { ObjectRef } from '../../bindings';
import type { MenuEntry } from '../../components/ActionMenu';
import { useConnections } from '../../store/connections';
import { useDialogs } from '../../store/dialogs';
import type { TreeNode } from '../../store/tree';
import { config, ids, resetStores, serverInfo } from '../../test/backend';
import { backupActionItems } from './backupMenu';

const table: ObjectRef = { database: 'shop', schema: null, name: 'orders', kind: 'Table' };
const tableNode: TreeNode = { id: `conn:${ids.prod}/obj`, kind: 'object', label: 'orders', objectRef: table };
const databaseNode: TreeNode = { id: `conn:${ids.prod}/db`, kind: 'database', label: 'shop', scope: { database: 'shop', schema: null } };
const schemaNode: TreeNode = { id: `conn:${ids.prod}/s`, kind: 'schema', label: 'sales', scope: { database: 'shop', schema: 'sales' } };

const labels = (items: MenuEntry[]) => items.map((e) => ('label' in e ? e.label : '—'));
const entry = (node: TreeNode, label: string) => {
  const found = backupActionItems(node).find((e) => 'label' in e && e.label === label);
  if (!found || !('run' in found)) throw new Error(`no "${label}" item`);
  return found;
};

const usePg = () => {
  useConnections.getState().setStored([], [config(ids.prod, 'pg', { kind: 'postgres' })]);
  useConnections.getState().setInfo(ids.prod, serverInfo('Postgres'));
};

beforeEach(() => {
  resetStores();
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod')]);
  useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
});

describe('backup tree entries', () => {
  it('MySQL database: Dump… and Restore…', () => {
    expect(labels(backupActionItems(databaseNode))).toEqual(['Dump…', 'Restore…']);
    entry(databaseNode, 'Dump…').run();
    expect(useDialogs.getState().request).toEqual({ kind: 'dump', connectionId: ids.prod, database: 'shop', schema: null });
    entry(databaseNode, 'Restore…').run();
    expect(useDialogs.getState().request).toEqual({ kind: 'restore', connectionId: ids.prod, database: 'shop' });
  });

  it('PostgreSQL database and schema: Dump… and Restore…', () => {
    usePg();
    expect(labels(backupActionItems(databaseNode))).toEqual(['Dump…', 'Restore…']);
    entry(schemaNode, 'Dump…').run();
    expect(useDialogs.getState().request).toEqual({ kind: 'dump', connectionId: ids.prod, database: 'shop', schema: 'sales' });
    entry(schemaNode, 'Restore…').run();
    expect(useDialogs.getState().request).toEqual({ kind: 'restore', connectionId: ids.prod, database: 'shop' });
  });

  it('a table: Dump table… pre-selecting it; nothing on a view', () => {
    expect(labels(backupActionItems(tableNode))).toEqual(['Dump table…']);
    entry(tableNode, 'Dump table…').run();
    expect(useDialogs.getState().request).toEqual({ kind: 'dump', connectionId: ids.prod, database: 'shop', schema: null, table: 'orders' });
    expect(backupActionItems({ ...tableNode, objectRef: { ...table, kind: 'View' } })).toEqual([]);
  });

  it('read-only: Dump stays enabled, Restore is disabled; nothing while closed', () => {
    useConnections.getState().setStored([], [config(ids.prod, 'ro', { read_only: true })]);
    expect(entry(databaseNode, 'Dump…')).not.toHaveProperty('disabled', true);
    expect(entry(databaseNode, 'Restore…')).toMatchObject({ disabled: true });
    useConnections.getState().clearInfo(ids.prod);
    expect(backupActionItems(databaseNode)).toEqual([]);
    expect(backupActionItems(tableNode)).toEqual([]);
  });
});
