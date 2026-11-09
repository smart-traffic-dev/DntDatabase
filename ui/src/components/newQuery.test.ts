import { beforeEach, describe, expect, it } from 'vitest';
import type { ObjectRef } from '../bindings';
import { useConnections } from '../store/connections';
import { useQueryDocs } from '../store/queryTabs';
import { objectTabId, useTabs } from '../store/tabs';
import type { TreeNode } from '../store/tree';
import { config, ids, mockBackend, resetStores } from '../test/backend';
import { openQueryFromActiveTab, openQueryOnNode, openSelectTop, selectTopSql } from './newQuery';

const table = (name: string, database: string, schema: string | null): TreeNode => ({
  id: 'conn:c/obj',
  kind: 'object',
  label: name,
  objectRef: { kind: 'Table', database, schema, name },
});

describe('selectTopSql', () => {
  it('qualifies a MySQL table by database', () => {
    expect(selectTopSql(table('orders', 'shop', null), 'mysql')).toBe('SELECT * FROM `shop`.`orders` LIMIT 1000;');
  });

  it('qualifies a PostgreSQL table by schema', () => {
    expect(selectTopSql(table('orders', 'shop', 'public'), 'postgres')).toBe('SELECT * FROM "public"."orders" LIMIT 1000;');
  });

  it('omits the qualifier of a PostgreSQL table without schema', () => {
    expect(selectTopSql(table('orders', 'shop', null), 'postgres')).toBe('SELECT * FROM "orders" LIMIT 1000;');
  });

  it('doubles the quote character inside names', () => {
    expect(selectTopSql(table('a`b', 'd`b', null), 'mysql')).toBe('SELECT * FROM `d``b`.`a``b` LIMIT 1000;');
    expect(selectTopSql(table('a"b', 'shop', 's"x'), 'postgres')).toBe('SELECT * FROM "s""x"."a""b" LIMIT 1000;');
  });

  it('keeps non-ASCII names', () => {
    expect(selectTopSql(table('khách_hàng', 'cửa_hàng', null), 'mysql')).toBe('SELECT * FROM `cửa_hàng`.`khách_hàng` LIMIT 1000;');
  });

  it('takes a limit, offset and quoted ORDER BY column', () => {
    expect(selectTopSql(table('orders', 'shop', null), 'mysql', { limit: 100 })).toBe('SELECT * FROM `shop`.`orders` LIMIT 100;');
    expect(
      selectTopSql(table('đơn_hàng', 'shop', 'public'), 'postgres', { limit: 50, offset: 100, orderBy: { column: 'we"ird', dir: 'desc' } }),
    ).toBe('SELECT * FROM "public"."đơn_hàng" ORDER BY "we""ird" DESC LIMIT 50 OFFSET 100;');
    expect(selectTopSql(table('t', 'shop', null), 'mysql', { limit: 10, offset: 0, orderBy: { column: 'a`b', dir: 'asc' } })).toBe(
      'SELECT * FROM `shop`.`t` ORDER BY `a``b` ASC LIMIT 10;',
    );
  });
});

describe('sample query entry points', () => {
  const ref = (name: string, kind: ObjectRef['kind']): ObjectRef => ({ database: 'shop', schema: null, name, kind });
  const nodeOf = (conn: string, objectRef: ObjectRef): TreeNode => ({ id: `conn:${conn}/obj:${objectRef.name}`, kind: 'object', label: objectRef.name, objectRef });
  const docOf = (id: string) => useQueryDocs.getState().docs.get(id)!;

  beforeEach(() => {
    resetStores();
    mockBackend({ tabs_save: () => null, tabs_delete: () => null, query_close_tab: () => null });
    useConnections.getState().setStored([], [
      config(ids.prod, 'sql'),
      config(ids.legacy, 'docs', { kind: 'mongo', port: 27017 }),
      config(ids.analytics, 'cache', { kind: 'redis', port: 6379 }),
    ]);
  });

  it('opens a find() on a Mongo collection and view, bound to its database', () => {
    const id = openSelectTop(nodeOf(ids.legacy, ref('orders', 'Collection')));
    expect(docOf(id)).toMatchObject({ sql: 'db.orders.find({}).limit(1000)', connectionId: ids.legacy, database: 'shop' });
    const view = openQueryOnNode(nodeOf(ids.legacy, ref('system.views', 'View')));
    expect(docOf(view).sql).toBe('db.getCollection("system.views").find({}).limit(1000)');
  });

  it('Ctrl+Q on an open Mongo collection tab', () => {
    const objectRef = ref('orders', 'Collection');
    useTabs.getState().open({ id: objectTabId(ids.legacy, objectRef), kind: 'object', connectionId: ids.legacy, objectRef, title: 'orders' });
    const id = openQueryFromActiveTab();
    expect(docOf(id)).toMatchObject({ sql: 'db.orders.find({}).limit(1000)', connectionId: ids.legacy, database: 'shop' });
  });

  it('keeps the SQL output for MySQL', () => {
    const id = openSelectTop(nodeOf(ids.prod, ref('orders', 'Table')));
    expect(docOf(id).sql).toBe('SELECT * FROM `shop`.`orders` LIMIT 1000;');
  });

  it('opens blank for Redis keys and for a Mongo database node', () => {
    expect(docOf(openQueryOnNode(nodeOf(ids.analytics, ref('k', 'Key')))).sql).toBe('');
    const dbNode: TreeNode = { id: `conn:${ids.legacy}/db`, kind: 'database', label: 'shop', scope: { database: 'shop', schema: null } };
    expect(docOf(openQueryOnNode(dbNode)).sql).toBe('');
  });
});
