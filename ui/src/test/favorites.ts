import type { Favorite, Snippet } from '../bindings';

/** A favorite as `favorites_list` returns it. */
export function fav(id: string, name: string, over: Partial<Favorite> = {}): Favorite {
  return {
    id,
    name,
    folder: null,
    sql: `SELECT '${name}';`,
    connection_id: null,
    database: null,
    created_at: '2026-10-20T10:00:00Z',
    updated_at: '2026-10-20T10:00:00Z',
    sort: 0,
    ...over,
  };
}

/** A snippet as `snippets_list` returns it. */
export function snip(id: string, prefix: string, over: Partial<Snippet> = {}): Snippet {
  return {
    id,
    prefix,
    label: `Label ${prefix}`,
    body: 'SELECT ${1:*} FROM ${2:t};',
    description: '',
    created_at: '2026-10-20T10:00:00Z',
    updated_at: '2026-10-20T10:00:00Z',
    ...over,
  };
}
