// Snippets offered by the completion provider (Monaco snippet syntax: `${1:placeholder}`).
// The built-ins are fixed; user snippets are fed by `setUserSnippets` (another module owns
// their storage) and replace the previous list as a whole.

export interface SqlSnippet {
  /** What the user types to get it (`sel`). */
  prefix: string;
  /** Shown in the suggest list. */
  label: string;
  /** Monaco snippet body. */
  body: string;
  description: string;
}

export const BUILTIN_SNIPPETS: SqlSnippet[] = [
  { prefix: 'sel', label: 'sel', body: 'SELECT ${1:*} FROM ${2:table} LIMIT ${3:100};', description: 'SELECT ... FROM ... LIMIT' },
  {
    prefix: 'selw',
    label: 'selw',
    body: 'SELECT ${1:*} FROM ${2:table} WHERE ${3:condition} LIMIT ${4:100};',
    description: 'SELECT ... FROM ... WHERE ... LIMIT',
  },
  {
    prefix: 'ins',
    label: 'ins',
    body: 'INSERT INTO ${1:table} (${2:columns}) VALUES (${3:values});',
    description: 'INSERT INTO ... VALUES',
  },
  {
    prefix: 'upd',
    label: 'upd',
    body: 'UPDATE ${1:table} SET ${2:column} = ${3:value} WHERE ${4:condition};',
    description: 'UPDATE ... SET ... WHERE',
  },
  { prefix: 'del', label: 'del', body: 'DELETE FROM ${1:table} WHERE ${2:condition};', description: 'DELETE FROM ... WHERE' },
  {
    prefix: 'join',
    label: 'join',
    body: 'INNER JOIN ${1:table} ${2:t} ON ${2:t}.${3:id} = ${4:other}.${5:id}',
    description: 'INNER JOIN ... ON',
  },
  {
    prefix: 'lj',
    label: 'lj',
    body: 'LEFT JOIN ${1:table} ${2:t} ON ${2:t}.${3:id} = ${4:other}.${5:id}',
    description: 'LEFT JOIN ... ON',
  },
  {
    prefix: 'cte',
    label: 'cte',
    body: 'WITH ${1:name} AS (\n  ${2:SELECT 1}\n)\nSELECT ${3:*} FROM ${1:name};',
    description: 'WITH ... AS (...) SELECT',
  },
  { prefix: 'cnt', label: 'cnt', body: 'SELECT COUNT(*) FROM ${1:table};', description: 'SELECT COUNT(*) FROM' },
  {
    prefix: 'grp',
    label: 'grp',
    body: 'SELECT ${1:column}, COUNT(*) AS ${2:cnt} FROM ${3:table} GROUP BY ${1:column} ORDER BY ${2:cnt} DESC;',
    description: 'GROUP BY with COUNT(*)',
  },
];

let userSnippets: SqlSnippet[] = [];

/** Replaces the user's snippets (entries without a prefix or body are dropped). */
export function setUserSnippets(list: SqlSnippet[]): void {
  userSnippets = list.filter((s) => s.prefix.trim() !== '' && s.body !== '');
}

/** Built-ins first, then the user's; a user snippet with a built-in's prefix comes after it. */
export function allSnippets(): SqlSnippet[] {
  return userSnippets.length === 0 ? BUILTIN_SNIPPETS : [...BUILTIN_SNIPPETS, ...userSnippets];
}
