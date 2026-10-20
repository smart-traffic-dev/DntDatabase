import { beforeEach, describe, expect, it } from 'vitest';
import type { ServerVariant } from '../bindings';
import {
  callsOf,
  fail,
  ids,
  mockBackend,
  openResult,
  resetStores,
  sampleTree,
  serverInfo,
} from '../test/backend';
import { useConnections } from './connections';
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

function deferred<T>() {
  let resolve: (v: T) => void = () => {};
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
}

const tick = () => new Promise((r) => setTimeout(r, 0));

describe('tree store: stale loads', () => {
  beforeEach(() => {
    resetStores();
  });

  it('keeps the newest list_databases answer when responses arrive out of order', async () => {
    const gates = [deferred<{ name: string }[]>(), deferred<{ name: string }[]>()];
    let call = 0;
    mockBackend({
      list_tree: () => sampleTree(),
      open_connection: () => openResult(serverInfo('MySql8')),
      list_databases: () => gates[call++]?.promise,
    });
    await useTree.getState().reloadStored();
    useConnections.getState().setInfo(ids.prod, serverInfo('MySql8'));
    const first = useTree.getState().loadChildren(conn);
    const second = useTree.getState().loadChildren(conn);
    gates[1]?.resolve([{ name: 'new' }]);
    await second;
    gates[0]?.resolve([{ name: 'old' }]);
    await first;
    await tick();
    expect(children(conn)).toEqual(['new']);
    expect(useTree.getState().nodes.get(conn)?.loading).toBe(false);
  });

  it('a close during the load keeps the connection closed and childless', async () => {
    const gate = deferred<unknown>();
    mockBackend({
      list_tree: () => sampleTree(),
      open_connection: () => gate.promise,
      list_databases: () => [{ name: 'shop' }],
      close_connection: () => null,
    });
    await useTree.getState().reloadStored();
    useTree.getState().expand(conn);
    await useTree.getState().closeConnection(ids.prod);
    gate.resolve(openResult(serverInfo('MySql8')));
    await tick();
    await tick();
    expect(useConnections.getState().info.has(ids.prod)).toBe(false);
    expect(useTree.getState().childIds.has(conn)).toBe(false);
    expect(useTree.getState().nodes.get(conn)?.loading).toBe(false);
    expect(callsOf('list_databases')).toHaveLength(0);
  });
});

describe('tree store: names, errors, selection, drops', () => {
  const folder = `${conn}/db:shop/folder:Table`;

  beforeEach(() => {
    resetStores();
  });

  async function openFolder(objects: string[], extra: Parameters<typeof mockBackend>[0] = {}) {
    mockBackend({
      list_tree: () => sampleTree(),
      open_connection: () => openResult(serverInfo('MySql8')),
      list_databases: () => [{ name: 'shop' }],
      list_objects: () => objects.map((name) => ({ name, kind: 'Table', comment: null, row_estimate: null })),
      refresh_node: () => null,
      ...extra,
    });
    await useTree.getState().reloadStored();
    await open(conn);
    await open(`${conn}/db:shop`);
    await open(folder);
  }

  it('percent-encodes names in node ids and refreshes with the raw key', async () => {
    await openFolder(['a/b', 'a']);
    const slash = `${folder}/obj:Table:a%2Fb`;
    expect(children(folder)).toEqual(['a/b', 'a']);
    expect(useTree.getState().childIds.get(folder)).toEqual([slash, `${folder}/obj:Table:a`]);

    useTree.getState().select(slash);
    await useTree.getState().refresh(slash);
    expect(callsOf('refresh_node')[0]).toEqual({
      id: ids.prod,
      key: ['db:shop', 'folder:Table', 'obj:Table:a/b'],
    });
  });

  it('shows a refresh_node failure in the tree error state', async () => {
    await openFolder(['orders'], {
      refresh_node: () => {
        throw fail('internal');
      },
    });
    await useTree.getState().refresh(`${folder}/obj:Table:orders`);
    expect(useTree.getState().error?.message).toBe('An internal error occurred.');
  });

  it('clears the selection when the selected node disappears on reload', async () => {
    let objects = ['orders', 'products'];
    await openFolder(objects, {
      list_objects: () => objects.map((name) => ({ name, kind: 'Table', comment: null, row_estimate: null })),
    });
    const selected = `${folder}/obj:Table:products`;
    useTree.getState().select(selected);
    objects = ['orders'];
    await useTree.getState().refresh(folder);
    expect(useTree.getState().nodes.has(selected)).toBe(false);
    expect(useTree.getState().selectedId).toBeUndefined();
  });

  it('ignores drops onto rows that are neither groups nor connections', async () => {
    await openFolder(['orders'], { move_node: () => null });
    await useTree.getState().move(connNodeId(ids.legacy), `${conn}/db:shop`);
    await useTree.getState().move(connNodeId(ids.legacy), `${folder}/obj:Table:orders`);
    expect(callsOf('move_node')).toHaveLength(0);
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
