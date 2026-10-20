// Autocomplete data per (connection, database), fetched once and kept until invalidated
// (spec §7.3): the scope is one `completion_scope` payload, never fetched per keystroke.
// Failures resolve to `null` / `[]` silently (no toast) and are retried after 10 s, the
// same window the backend keeps a failed load.
import { commands, type CompletionObject, type Result } from '../bindings';
import type { CompletionScope } from '../query/completion/provider';
import type { Engine } from '../query/engine';

/** How long a failed fetch is remembered before the next completion tries again. */
const FAILURE_TTL_MS = 10_000;

interface Entry<T> {
  value: Promise<T>;
  /** Set once the fetch failed: the entry is dropped after this time. */
  expires?: number;
}

const entries = new Map<string, Entry<unknown>>();

const key = (...parts: (string | null)[]) => parts.map((p) => p ?? '').join('\u0000');

async function value<T>(r: Promise<Result<T, unknown>>): Promise<T | null> {
  try {
    const res = await r;
    return res.status === 'ok' ? res.data : null;
  } catch {
    return null;
  }
}

/** The cached value under `k`, or the result of `load` (cached; a failure for 10 s only). */
function cached<T>(k: string, load: () => Promise<T | null>, fallback: T): Promise<T> {
  const hit = entries.get(k) as Entry<T> | undefined;
  if (hit && (hit.expires === undefined || hit.expires > Date.now())) return hit.value;
  const entry: Entry<T> = {
    value: load().then((v) => {
      if (v !== null) return v;
      // Only the entry still in the map records the failure (an invalidation may have dropped it).
      if (entries.get(k) === entry) entry.expires = Date.now() + FAILURE_TTL_MS;
      return fallback;
    }),
  };
  entries.set(k, entry as Entry<unknown>);
  return entry.value;
}

export const useCompletion = {
  /** The tables, views and columns of `db` for the tab's connection. */
  scope(connId: string, db: string): Promise<CompletionScope | null> {
    return cached<CompletionScope | null>(
      key('scope', connId, db),
      () => value(commands.completionScope(connId, db)),
      null,
    );
  },
  /** The qualifiers of `x.`: databases on MySQL, schemas of `db` on PostgreSQL. */
  containers(connId: string, db: string, engine: Engine): Promise<string[]> {
    return cached<string[]>(
      engine === 'postgres' ? key('containers', connId, db) : key('containers', connId, null),
      async () => {
        const list = await value(
          engine === 'postgres' ? commands.listSchemas(connId, db, false) : commands.listDatabases(connId, false),
        );
        return list ? list.map((x) => x.name) : null;
      },
      [],
    );
  },
  /** Tables and views after `qualifier.` (a MySQL database, or a schema of `db` on PostgreSQL). */
  objects(connId: string, db: string | null, engine: Engine, qualifier: string): Promise<CompletionObject[] | null> {
    if (engine === 'postgres' && !db) return Promise.resolve(null);
    const database = engine === 'postgres' ? (db as string) : qualifier;
    const schema = engine === 'postgres' ? qualifier : null;
    return cached<CompletionObject[] | null>(
      key('objects', connId, database, schema),
      () => value(commands.completionObjects(connId, database, schema)),
      null,
    );
  },
  /** Drops what is cached for `db` of the connection (and its qualifier lists). */
  invalidate(connId: string, db: string): void {
    invalidateCompletion(connId, db);
  },
  /** Drops everything (tests, app reset). */
  reset(): void {
    entries.clear();
  },
};

/** Called when a connection closes (or is deleted): drops everything cached for it. */
export function forgetCompletionConnection(connectionId: string): void {
  invalidateCompletion(connectionId);
}

/**
 * Called after a DDL statement ran without error in a tab (spec §7.3), or when a connection
 * closes. Without `database`, every database of the connection is dropped.
 */
export function invalidateCompletion(connectionId: string, database?: string): void {
  for (const k of [...entries.keys()]) {
    const [kind, conn, db] = k.split('\u0000');
    if (conn !== connectionId) continue;
    // Databases / schemas and the objects lists can change with any DDL.
    if (database === undefined || kind !== 'scope' || db === database) entries.delete(k);
  }
}
