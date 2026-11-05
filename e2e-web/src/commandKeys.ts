import type {
  ApplyReport,
  ConnectionConfig,
  EditPreview,
  KeyEdit,
  KeyName,
  KeyPage,
  KeySummary,
  KeyType,
  KeyValue,
  KeyValueView,
  ScanRequest,
  ServerInfo,
  Value,
  ValueRange,
} from '../../ui/src/bindings';
import { Failure, appError, fail } from '../../ui/src/test/fixtures/data';
import type { CommandFakeState, RedisValue } from './commandFixtures';
import type { Handler } from './shim';

// The key browser of a Redis connection against the fake Command server (spec §7.6): `keys_scan`
// pages the sorted keys of a db (the cursor is the index of the page's first key), `key_value`
// reads one key, `key_edit_preview` plans one Redis command per edit (Confirm on Production, Block
// on a read-only connection, as `command_edit_verdict`), and `data_apply` runs it.

/** `READ_ONLY_EDIT` of `src-tauri/src/edit_plans.rs`. */
const READ_ONLY_EDIT = 'the connection is read-only';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

const fromB64 = (b64: string): string => {
  const binary = atob(b64);
  return decoder.decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
};
const toB64 = (text: string): string => btoa(String.fromCharCode(...encoder.encode(text)));
const nameOf = (key: string): KeyName => ({ display: key, raw: toB64(key) });
const text = (v: string): Value => ({ t: 'Text', v });

/** A Redis glob (`*`, `?`) as a RegExp. */
const globRegex = (glob: string) =>
  new RegExp(`^${glob.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.')}$`);

const typeOf = (v: RedisValue): KeyType => {
  switch (v.type) {
    case 'string':
      return 'String';
    case 'hash':
      return 'Hash';
    case 'list':
      return 'List';
    case 'set':
      return 'Set';
    case 'zset':
      return 'ZSet';
  }
};

/** An argument of a command's canonical text: quoted when it holds a space or a quote, or is empty. */
const arg = (s: string) => (s === '' || /[\s"]/.test(s) ? JSON.stringify(s) : s);
const cmd = (...parts: string[]) => parts.map((p, i) => (i === 0 ? p : arg(p))).join(' ');

const configError = (message: string) => new Failure(appError('config', message, { field: 'edit' }));

interface Planned {
  text: string;
  run(): number;
}

/**
 * `keys_scan`, `key_value`, `key_edit_preview` and `data_apply` (key plans) for the Redis
 * connections; `fallback` answers the other `data_apply` plans.
 */
export function createCommandKeys(
  state: CommandFakeState,
  connectionOf: (id: string) => ConnectionConfig | undefined,
  requireOpen: (id: string) => ServerInfo,
  fallback: Record<string, Handler>,
): Record<string, Handler> {
  const plans = new Map<string, Planned>();
  /** db -> key -> when it expires (ms since the epoch). */
  const expiry = new Map<number, Map<string, number>>();
  let seq = 0;

  /** The keys of db `db` of Redis connection `id` (expired ones dropped). */
  const keysOf = (id: string, db: string): Map<string, RedisValue> => {
    if (connectionOf(id)?.kind !== 'redis') throw fail('internal', { detail: 'e2e: the key browser is only faked for Redis' });
    requireOpen(id);
    const index = Number(db);
    if (!state.redis.has(index)) state.redis.set(index, new Map());
    const keys = state.redis.get(index)!;
    for (const [key, at] of expiry.get(index) ?? []) {
      if (at <= Date.now()) {
        keys.delete(key);
        expiry.get(index)!.delete(key);
      }
    }
    return keys;
  };
  const ttlOf = (db: string, key: string): number | null => {
    const at = expiry.get(Number(db))?.get(key);
    return at === undefined ? null : Math.max(0, at - Date.now());
  };
  const setExpiry = (db: string, key: string, at: number | null) => {
    const index = Number(db);
    if (!expiry.has(index)) expiry.set(index, new Map());
    if (at === null) expiry.get(index)!.delete(key);
    else expiry.get(index)!.set(key, at);
  };

  /** The command of `edit` on db `db`, and what it does to the store. */
  function plan(keys: Map<string, RedisValue>, db: string, edit: KeyEdit): Planned {
    const key = fromB64(edit.key.raw);
    const existing = keys.get(key);
    const of = <T extends RedisValue['type']>(type: T, empty: Extract<RedisValue, { type: T }>['value']) => {
      if (existing && existing.type !== type) throw configError('WRONGTYPE Operation against a key holding the wrong kind of value');
      const current = (existing ?? { type, value: empty }) as Extract<RedisValue, { type: T }>;
      return { current, save: () => keys.set(key, current) };
    };
    switch (edit.op) {
      case 'set_string': {
        const value = fromB64(edit.value);
        return { text: cmd('SET', key, value), run: () => (keys.set(key, { type: 'string', value }), setExpiry(db, key, null), 1) };
      }
      case 'hash_set': {
        const [field, value] = [fromB64(edit.field), fromB64(edit.value)];
        const h = of('hash', {});
        return { text: cmd('HSET', key, field, value), run: () => ((h.current.value[field] = value), h.save(), 1) };
      }
      case 'hash_delete': {
        const field = fromB64(edit.field);
        return {
          text: cmd('HDEL', key, field),
          run: () => {
            const h = keys.get(key);
            if (h?.type !== 'hash' || !(field in h.value)) return 0;
            delete h.value[field];
            return 1;
          },
        };
      }
      case 'list_push': {
        const value = fromB64(edit.value);
        const l = of('list', []);
        return {
          text: cmd(edit.head ? 'LPUSH' : 'RPUSH', key, value),
          run: () => (edit.head ? l.current.value.unshift(value) : l.current.value.push(value), l.save(), 1),
        };
      }
      case 'list_set': {
        const value = fromB64(edit.value);
        return {
          text: cmd('LSET', key, String(edit.index), value),
          run: () => {
            const l = keys.get(key);
            if (l?.type !== 'list' || edit.index >= l.value.length) return 0;
            l.value[edit.index] = value;
            return 1;
          },
        };
      }
      case 'list_remove': {
        const value = fromB64(edit.value);
        return {
          text: cmd('LREM', key, String(edit.count), value),
          run: () => {
            const l = keys.get(key);
            const at = l?.type === 'list' ? l.value.indexOf(value) : -1;
            if (at < 0 || l?.type !== 'list') return 0;
            l.value.splice(at, 1);
            return 1;
          },
        };
      }
      case 'set_add': {
        const member = fromB64(edit.member);
        const s = of('set', []);
        return { text: cmd('SADD', key, member), run: () => (s.current.value.includes(member) || s.current.value.push(member), s.save(), 1) };
      }
      case 'set_remove': {
        const member = fromB64(edit.member);
        return {
          text: cmd('SREM', key, member),
          run: () => {
            const s = keys.get(key);
            if (s?.type !== 'set' || !s.value.includes(member)) return 0;
            s.value = s.value.filter((m) => m !== member);
            return 1;
          },
        };
      }
      case 'z_set_add': {
        const member = fromB64(edit.member);
        const z = of('zset', []);
        return {
          text: cmd('ZADD', key, String(edit.score), member),
          run: () => ((z.current.value = [...z.current.value.filter(([m]) => m !== member), [member, edit.score]]), z.save(), 1),
        };
      }
      case 'z_set_remove': {
        const member = fromB64(edit.member);
        return {
          text: cmd('ZREM', key, member),
          run: () => {
            const z = keys.get(key);
            if (z?.type !== 'zset' || !z.value.some(([m]) => m === member)) return 0;
            z.value = z.value.filter(([m]) => m !== member);
            return 1;
          },
        };
      }
      case 'expire':
        return edit.ttl_ms === null
          ? { text: cmd('PERSIST', key), run: () => (keys.has(key) ? (setExpiry(db, key, null), 1) : 0) }
          : { text: cmd('PEXPIRE', key, String(edit.ttl_ms)), run: () => (keys.has(key) ? (setExpiry(db, key, Date.now() + edit.ttl_ms!), 1) : 0) };
      case 'rename': {
        const to = fromB64(edit.to.raw);
        return {
          text: cmd('RENAME', key, to),
          run: () => {
            const v = keys.get(key);
            if (!v) return 0;
            const ttl = ttlOf(db, key);
            keys.delete(key);
            setExpiry(db, key, null);
            keys.set(to, v);
            setExpiry(db, to, ttl === null ? null : Date.now() + ttl);
            return 1;
          },
        };
      }
      case 'delete':
        return { text: cmd('DEL', key), run: () => (keys.delete(key) ? (setExpiry(db, key, null), 1) : 0) };
    }
  }

  function valueOf(v: RedisValue, range: ValueRange): KeyValue {
    const page = <T>(items: T[]) => items.slice(range.offset, range.offset + range.count);
    switch (v.type) {
      case 'string':
        return { String: { value: text(v.value) } };
      case 'hash': {
        const all = Object.entries(v.value);
        const end = range.offset + range.count;
        return { Hash: { fields: page(all).map(([f, x]) => [text(f), text(x)]), total: all.length, next: end < all.length ? String(end) : null } };
      }
      case 'list':
        return { List: { items: page(v.value).map(text), total: v.value.length } };
      case 'set': {
        const end = range.offset + range.count;
        return { Set: { members: page([...v.value].sort()).map(text), total: v.value.length, next: end < v.value.length ? String(end) : null } };
      }
      case 'zset':
        return { ZSet: { members: page([...v.value].sort((a, b) => a[1] - b[1])).map(([m, s]) => [text(m), s]), total: v.value.length } };
    }
  }

  return {
    keys_scan: (args) => {
      const req = args.req as ScanRequest;
      const db = args.db as string;
      const keys = keysOf(args.id as string, db);
      const match = globRegex(req.pattern ?? '*');
      const all: KeySummary[] = [...keys]
        .filter(([k, v]) => match.test(k) && (req.key_type === null || JSON.stringify(req.key_type) === JSON.stringify(typeOf(v))))
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([k, v]) => ({ name: nameOf(k), key_type: typeOf(v), ttl_ms: ttlOf(db, k) }));
      const from = req.cursor === null ? 0 : Number(req.cursor);
      const keysPage = all.slice(from, from + req.count);
      const end = from + keysPage.length;
      return { keys: keysPage, next_cursor: end < all.length ? String(end) : null } satisfies KeyPage;
    },

    key_value: (args) => {
      const db = args.db as string;
      const keys = keysOf(args.id as string, db);
      const key = fromB64((args.key as KeyName).raw);
      const v = keys.get(key);
      if (!v) throw fail('key_not_found', { detail: key });
      return { value: valueOf(v, args.range as ValueRange), ttl_ms: ttlOf(db, key), memory_bytes: 64 + JSON.stringify(v.value).length } satisfies KeyValueView;
    },

    key_edit_preview: (args) => {
      const id = args.id as string;
      const db = args.db as string;
      const keys = keysOf(id, db);
      const cfg = connectionOf(id)!;
      const planned = plan(keys, db, args.edit as KeyEdit);
      const planId = `key-edit-${++seq}`;
      plans.set(planId, planned);
      const preview: EditPreview = {
        plan_id: planId,
        statements: [planned.text],
        verdict: cfg.read_only ? 'block' : cfg.env_tag === 'production' ? 'confirm' : 'allow',
        atomic: 'yes',
        protections: [{ level: 'key_only' }],
        reason: cfg.read_only ? READ_ONLY_EDIT : null,
      };
      return preview;
    },

    data_apply: (args) => {
      const planned = plans.get(args.planId as string);
      if (!planned) {
        const other = fallback.data_apply;
        if (!other) throw fail('plan_expired');
        return other(args);
      }
      plans.delete(args.planId as string);
      const affected = planned.run();
      const outcome: ApplyReport['outcome'] = affected > 0 ? { outcome: 'committed' } : { outcome: 'partially_applied', applied: 0 };
      return { outcome, statements: [{ index: 0, affected, error: null, conflict: false }], inserted: [], error: null } satisfies ApplyReport;
    },
  };
}
