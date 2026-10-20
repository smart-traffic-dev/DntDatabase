import { describe, expect, it } from 'vitest';
import type { TreeNode } from '../store/tree';
import { selectTopSql } from './newQuery';

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
