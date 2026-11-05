import type { ConnectionConfig, DatabaseInfo, ObjectKind, ObjectSummary, Scope, ServerInfo } from '../../ui/src/bindings';
import { defaultCaps } from '../../ui/src/engine';

/** A Redis value of the fake store, by type (as `TYPE key` names it). */
export type RedisValue =
  | { type: 'string'; value: string }
  | { type: 'hash'; value: Record<string, string> }
  | { type: 'list'; value: string[] }
  | { type: 'set'; value: string[] }
  | { type: 'zset'; value: [member: string, score: number][] };

/** A MongoDB document of the fake store (plain JSON; `_id` first). */
export type MongoDoc = Record<string, unknown>;

/** The Command-family servers behind every MongoDB / Redis connection of the fake backend. */
export interface CommandFakeState {
  /** Redis: database index -> key -> value. */
  redis: Map<number, Map<string, RedisValue>>;
  /** MongoDB: database -> collection -> documents. */
  mongo: Map<string, Map<string, MongoDoc[]>>;
}

/** Redis db 0 holds one key of each type, db 2 three strings; MongoDB `shop.orders` three orders. */
export function createCommandFake(): CommandFakeState {
  const db0 = new Map<string, RedisValue>([
    ['user:1', { type: 'hash', value: { name: 'Ada Lovelace', email: 'ada@example.com', plan: 'pro' } }],
    ['queue', { type: 'list', value: ['job-1', 'job-2', 'job-3'] }],
    ['tags', { type: 'set', value: ['blue', 'green', 'red'] }],
    [
      'rank',
      {
        type: 'zset',
        value: [
          ['ada', 30],
          ['grace', 20],
          ['linus', 10],
        ],
      },
    ],
    ['greeting', { type: 'string', value: 'hello' }],
  ]);
  const db2 = new Map<string, RedisValue>([
    ['session:a', { type: 'string', value: 'alpha' }],
    ['session:b', { type: 'string', value: 'beta' }],
    ['session:c', { type: 'string', value: 'gamma' }],
  ]);
  const orders: MongoDoc[] = [
    { _id: 'o1', customer: 'ada', total: 19.9, items: [{ sku: 'pen', qty: 2 }], paid: true },
    { _id: 'o2', customer: 'grace', total: 5, items: [{ sku: 'ink', qty: 1 }], paid: false },
    { _id: 'o3', customer: 'ada', total: 42, items: [{ sku: 'pad', qty: 3 }], paid: true },
  ];
  return {
    redis: new Map([
      [0, db0],
      [2, db2],
    ]),
    mongo: new Map([['shop', new Map([['orders', orders]])]]),
  };
}

/** What the fake MongoDB / Redis servers report on open (capabilities as their drivers declare them). */
export function commandInfo(kind: 'mongo' | 'redis'): ServerInfo {
  return kind === 'mongo'
    ? { kind, variant: 'Mongo', version: [7, 0, 12], version_string: '7.0.12', capabilities: defaultCaps(kind) }
    : { kind, variant: 'Redis', version: [7, 2, 5], version_string: '7.2.5', capabilities: defaultCaps(kind) };
}

/**
 * `list_databases` of a Command connection: Redis lists the non-empty numbered databases and the
 * selected one (as the real driver does), MongoDB its databases.
 */
export function commandDatabases(state: CommandFakeState, cfg: ConnectionConfig): DatabaseInfo[] {
  if (cfg.kind === 'redis') {
    const selected = cfg.options.driver.kind === 'redis' ? cfg.options.driver.db : 0;
    const dbs = [...state.redis].filter(([i, keys]) => i === selected || keys.size > 0).map(([i]) => i);
    if (!dbs.includes(selected)) dbs.push(selected);
    return dbs.sort((a, b) => a - b).map((i) => ({ name: String(i), key_count: state.redis.get(i)?.size ?? 0 }));
  }
  return [...state.mongo.keys()].sort().map((name) => ({ name, key_count: null }));
}

/** `list_objects` of a Command connection: the collections of a MongoDB database (no views, no keys). */
export function commandObjects(state: CommandFakeState, cfg: ConnectionConfig, scope: Scope, kind: ObjectKind): ObjectSummary[] {
  if (cfg.kind !== 'mongo' || kind !== 'Collection') return [];
  return [...(state.mongo.get(scope.database)?.entries() ?? [])]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, docs]) => ({ name, kind, comment: null, row_estimate: docs.length }));
}
