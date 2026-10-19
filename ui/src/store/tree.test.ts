import { beforeEach, describe, expect, it } from 'vitest';
import type { ServerVariant } from '../bindings';
import { ids, callsOf, mockBackend, openResult, resetStores, sampleTree, serverInfo } from '../test/backend';
import { useTabs } from './tabs';
import { useTree } from './tree';
import { connNodeId, groupNodeId } from './treeModel';

const conn = connNodeId(ids.prod);

function backend(variant: ServerVariant) {
  return mockBackend({
    list_tree: () => sampleTree(),
    open_connection: () => openResult(serverInfo(variant)),
    list_databases: () => [{ name: 'shop' }],
    list_schemas: () => [{ name: 'public' }, { name: 'sales' }],
    list_objects: () => [{ name: 'orders', kind: 'Table', comment: null, row_estimate: 12 }],
    move_node: () => null,
  });
}

/** Labels of the children of `id`, in order. */
function children(id: string): string[] {
  const { childIds, nodes } = useTree.getState();
  return (childIds.get(id) ?? []).map((c) => nodes.get(c)?.label ?? '?');
}

async function open(id: string) {
  useTree.getState().expand(id);
  await expect.poll(() => useTree.getState().childIds.has(id)).toBe(true);
}

describe('tree store', () => {
  beforeEach(() => {
    resetStores();
  });

  it.each([
    ['MySql57', ['Tables', 'Views', 'Functions', 'Procedures', 'Triggers', 'Events']],
    ['MySql8', ['Tables', 'Views', 'Functions', 'Procedures', 'Triggers', 'Events']],
    ['MariaDb', ['Tables', 'Views', 'Functions', 'Procedures', 'Triggers', 'Events', 'Sequences']],
  ] as const)('MySQL family (%s) has no schema level and these folders', async (variant, folders) => {
    backend(variant);
    await useTree.getState().reloadStored();
    await open(conn);
    await open(`${conn}/db:shop`);
    expect(children(`${conn}/db:shop`)).toEqual(folders);
    expect(callsOf('list_schemas')).toHaveLength(0);
  });

  it('PostgreSQL shows schemas between the database and the folders', async () => {
    backend('Postgres');
    await useTree.getState().reloadStored();
    await open(conn);
    await open(`${conn}/db:shop`);
    expect(children(`${conn}/db:shop`)).toEqual(['public', 'sales']);
    expect(callsOf('list_schemas')[0]).toEqual({ id: ids.prod, db: 'shop', showSystem: false });

    await open(`${conn}/db:shop/schema:public`);
    expect(children(`${conn}/db:shop/schema:public`)).toEqual([
      'Tables',
      'Views',
      'Materialized views',
      'Functions',
      'Procedures',
      'Sequences',
      'Types',
    ]);

    await open(`${conn}/db:shop/schema:public/folder:MaterializedView`);
    expect(callsOf('list_objects')[0]).toEqual({
      id: ids.prod,
      scope: { database: 'shop', schema: 'public' },
      kind: 'MaterializedView',
    });
    const object = useTree.getState().nodes.get(`${conn}/db:shop/schema:public/folder:MaterializedView/obj:MaterializedView:orders`);
    expect(object?.objectRef).toEqual({ database: 'shop', schema: 'public', name: 'orders', kind: 'MaterializedView' });
  });

  it('moves a connection into a group with the index among its siblings', async () => {
    backend('MySql8');
    await useTree.getState().reloadStored();
    await useTree.getState().move(connNodeId(ids.legacy), groupNodeId(ids.groupA));
    // Group A already holds two connections: the dropped one is appended.
    expect(callsOf('move_node')[0]).toEqual({
      node: { type: 'connection', id: ids.legacy },
      parentId: ids.groupA,
      index: 2,
    });
    expect(callsOf('list_tree')).toHaveLength(2);
  });

  it('drops a connection before another one and onto the root', async () => {
    backend('MySql8');
    await useTree.getState().reloadStored();
    await useTree.getState().move(connNodeId(ids.analytics), connNodeId(ids.prod));
    expect(callsOf('move_node')[0]).toEqual({
      node: { type: 'connection', id: ids.analytics },
      parentId: ids.groupA,
      index: 0,
    });
    await useTree.getState().move(connNodeId(ids.analytics), null);
    expect(callsOf('move_node')[1]).toEqual({
      node: { type: 'connection', id: ids.analytics },
      parentId: null,
      index: 0,
    });
  });

  it('never moves a group below itself', async () => {
    backend('MySql8');
    await useTree.getState().reloadStored();
    await useTree.getState().move(groupNodeId(ids.groupA), connNodeId(ids.prod));
    expect(callsOf('move_node')).toHaveLength(0);
  });

  it('closing a connection forgets its server and children', async () => {
    mockBackend({
      list_tree: () => sampleTree(),
      open_connection: () => openResult(serverInfo('MariaDb')),
      list_databases: () => [{ name: 'shop' }],
      close_connection: () => null,
    });
    await useTree.getState().reloadStored();
    await open(conn);
    expect(useTree.getState().nodes.get(conn)?.engine).toBe('MA');

    await useTree.getState().closeConnection(ids.prod);
    expect(useTree.getState().childIds.has(conn)).toBe(false);
    expect(useTree.getState().expanded.has(conn)).toBe(false);
    expect(useTree.getState().nodes.get(conn)?.engine).toBe('MY');
    expect(useTree.getState().nodes.has(`${conn}/db:shop`)).toBe(false);
  });
});

describe('tabs store', () => {
  beforeEach(() => {
    resetStores();
  });

  const tab = (name: string) => ({
    id: `c/${name}`,
    kind: 'object' as const,
    connectionId: 'c',
    objectRef: { database: 'shop', schema: null, name, kind: 'Table' as const },
    title: name,
  });

  it('opens each object once and activates the neighbour on close', () => {
    const { open: openTab, close } = useTabs.getState();
    openTab(tab('a'));
    openTab(tab('b'));
    openTab(tab('c'));
    openTab(tab('a'));
    expect(useTabs.getState().tabs.map((t) => t.title)).toEqual(['a', 'b', 'c']);
    expect(useTabs.getState().activeId).toBe('c/a');

    close('c/a');
    expect(useTabs.getState().activeId).toBe('c/b');
    close('c/c');
    close('c/b');
    expect(useTabs.getState().tabs).toEqual([]);
    expect(useTabs.getState().activeId).toBeUndefined();
  });
});
