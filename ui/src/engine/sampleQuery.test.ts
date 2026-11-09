import { describe, expect, it } from 'vitest';
import type { ObjectKind, ObjectRef } from '../bindings';
import { canSample, sampleQuery } from './index';

const ref = (name: string, kind: ObjectKind = 'Collection', over: Partial<ObjectRef> = {}): ObjectRef => ({ database: 'shop', schema: null, name, kind, ...over });

describe('sampleQuery (mongo)', () => {
  it('uses db.<name> for a plain identifier', () => {
    expect(sampleQuery('mongo', ref('orders'))).toBe('db.orders.find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('_tmp$1'))).toBe('db._tmp$1.find({}).limit(1000)');
  });

  it('takes the limit', () => {
    expect(sampleQuery('mongo', ref('orders'), 50)).toBe('db.orders.find({}).limit(50)');
  });

  it('uses getCollection for a db method name', () => {
    for (const name of ['stats', 'version', 'getCollectionNames', 'help', 'constructor', 'toString']) {
      expect(sampleQuery('mongo', ref(name))).toBe(`db.getCollection("${name}").find({}).limit(1000)`);
    }
  });

  it('uses getCollection for dotted, dashed, digit-leading and spaced names', () => {
    expect(sampleQuery('mongo', ref('system.users'))).toBe('db.getCollection("system.users").find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('order-items'))).toBe('db.getCollection("order-items").find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('1st'))).toBe('db.getCollection("1st").find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('my coll'))).toBe('db.getCollection("my coll").find({}).limit(1000)');
  });

  it('escapes quotes and keeps unicode in the JSON string', () => {
    expect(sampleQuery('mongo', ref('say "hi"'))).toBe('db.getCollection("say \\"hi\\"").find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('back\\slash'))).toBe('db.getCollection("back\\\\slash").find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('khách_hàng'))).toBe('db.getCollection("khách_hàng").find({}).limit(1000)');
  });

  it('works for a view and nothing else', () => {
    expect(sampleQuery('mongo', ref('active', 'View'))).toBe('db.active.find({}).limit(1000)');
    expect(sampleQuery('mongo', ref('orders', 'Table'))).toBeNull();
    expect(sampleQuery('mongo', ref('orders', 'Key'))).toBeNull();
    expect(canSample('mongo', undefined)).toBe(false);
  });
});

describe('sampleQuery (other engines)', () => {
  it('keeps the SQL byte-identical', () => {
    expect(sampleQuery('mysql', ref('orders', 'Table'))).toBe('SELECT * FROM `shop`.`orders` LIMIT 1000;');
    expect(sampleQuery('postgres', ref('orders', 'View', { schema: 'public' }), 20)).toBe('SELECT * FROM "public"."orders" LIMIT 20;');
    expect(sampleQuery('mysql', ref('mv', 'MaterializedView'))).toBe('SELECT * FROM `shop`.`mv` LIMIT 1000;');
    expect(sampleQuery('mysql', ref('orders', 'Collection'))).toBeNull();
  });

  it('has none for Redis', () => {
    expect(sampleQuery('redis', ref('k', 'Key'))).toBeNull();
    expect(sampleQuery('redis', ref('k', 'Table'))).toBeNull();
  });
});
