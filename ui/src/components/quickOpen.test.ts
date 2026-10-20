import { beforeEach, describe, expect, it } from 'vitest';
import { useCompletion } from '../store/completion';
import { useConnections } from '../store/connections';
import { useObjectData } from '../store/objectData';
import { objectTabId, useTabs } from '../store/tabs';
import { calls, config, ids, mockBackend, resetStores } from '../test/backend';
import { seedTree } from '../test/treeSeed';
import { collectObjectEntries, openObjectEntry, searchObjects } from './quickOpen';

beforeEach(() => {
  resetStores();
  useCompletion.reset();
  useObjectData.setState({ byTab: {} });
  useConnections.getState().setStored([], [config(ids.prod, 'shop-prod'), config(ids.analytics, 'analytics', { kind: 'postgres' })]);
  mockBackend({});
});

const describeEntry = (e: { path: string; name: string }) => `${e.path} › ${e.name}`;

describe('quick open entries', () => {
  it('lists tables, views, functions and procedures of the loaded tree with their path', () => {
    seedTree(ids.prod, [
      { db: 'shop', kind: 'Table', names: ['orders', 'customers'] },
      { db: 'shop', kind: 'View', names: ['recent_orders'] },
      { db: 'shop', kind: 'Function', names: ['order_total'] },
      { db: 'shop', kind: 'Trigger', names: ['trg_orders'] },
    ]);
    expect(collectObjectEntries().map(describeEntry)).toEqual([
      'shop-prod › shop › customers',
      'shop-prod › shop › order_total',
      'shop-prod › shop › orders',
      'shop-prod › shop › recent_orders',
    ]);
  });

  it('searches fuzzily and ranks the name above the path', () => {
    seedTree(ids.prod, [
      { db: 'orders_db', kind: 'Table', names: ['customers'] },
      { db: 'shop', kind: 'Table', names: ['orders', 'order_items'] },
    ]);
    const entries = collectObjectEntries();
    expect(searchObjects(entries, 'ord').map((r) => r.item.name)).toEqual(['orders', 'order_items', 'customers']);
    expect(searchObjects(entries, 'zzz')).toEqual([]);
    // An empty query lists everything (capped).
    expect(searchObjects(entries, '', 2)).toHaveLength(2);
  });

  it('adds the tables of cached completion schemas without loading anything', async () => {
    mockBackend({
      completion_scope: () => ({
        database: 'analytics',
        tables: [
          { schema: 'public', name: 'events', kind: 'Table', columns: [] },
          { schema: 'public', name: 'daily', kind: 'View', columns: [] },
        ],
      }),
    });
    await useCompletion.scope(ids.analytics, 'analytics');
    seedTree(ids.prod, [{ db: 'shop', kind: 'Table', names: ['orders'] }]);
    const before = calls().length;
    const entries = collectObjectEntries();
    expect(entries.map(describeEntry)).toContain('analytics › analytics › public › events');
    expect(entries.map((e) => e.name)).toEqual(expect.arrayContaining(['events', 'daily', 'orders']));
    // Searching again and again never asks the server.
    searchObjects(entries, 'ev');
    collectObjectEntries();
    expect(calls().length).toBe(before);
  });

  it('does not list the same object twice (tree and completion)', async () => {
    mockBackend({
      completion_scope: () => ({ database: 'shop', tables: [{ schema: null, name: 'orders', kind: 'Table', columns: [] }] }),
    });
    await useCompletion.scope(ids.prod, 'shop');
    seedTree(ids.prod, [{ db: 'shop', kind: 'Table', names: ['orders'] }]);
    expect(collectObjectEntries().filter((e) => e.name === 'orders')).toHaveLength(1);
  });

  it('a scope still loading is not listed and not awaited', () => {
    mockBackend({ completion_scope: () => new Promise(() => {}) });
    void useCompletion.scope(ids.prod, 'shop');
    expect(useCompletion.loadedScopes()).toEqual([]);
    expect(collectObjectEntries()).toEqual([]);
  });
});

describe('opening an entry', () => {
  it('opens the table tab (Data first) and Ctrl+Enter opens its DDL', () => {
    seedTree(ids.prod, [{ db: 'shop', kind: 'Table', names: ['orders'] }]);
    const [entry] = collectObjectEntries();
    const id = openObjectEntry(entry!);
    expect(id).toBe(objectTabId(ids.prod, entry!.objectRef));
    expect(useTabs.getState().activeId).toBe(id);
    expect(useObjectData.getState().byTab[id]?.sub).toBeUndefined();
    openObjectEntry(entry!, true);
    expect(useTabs.getState().tabs).toHaveLength(1);
    expect(useObjectData.getState().byTab[id]?.sub).toBe('ddl');
  });
});
