// Turns a completion context into suggestions (spec §7.3). Pure: the scope, the container
// names and the object loader come from the caller (`monacoProvider.ts`).
import type { CompletionObject, ObjectKind, SchemaCacheTable } from '../../bindings';
import type { Engine } from '../engine';
import { analyze, type TableRef } from './context';
import { functionsOf, keywordsOf, reservedOf } from './keywords';

export type { CompletionContext, StatementInfo, TableRef } from './context';
export { analyze } from './context';

export interface CompletionScope {
  database: string;
  tables: SchemaCacheTable[];
}

export interface Suggestion {
  label: string;
  kind: 'keyword' | 'function' | 'table' | 'view' | 'column' | 'schema' | 'database';
  detail?: string;
  insertText: string;
}

export interface SuggestInput {
  text: string;
  /** UTF-16 offset of the cursor in `text`. */
  offset: number;
  /** Statement ranges of `text` (`statement_ranges`), UTF-16 offsets. */
  ranges: [number, number][];
  engine: Engine;
  scope: CompletionScope | null;
  /** MySQL databases or PostgreSQL schemas: the qualifiers that `x.` lists objects of. */
  containers: string[];
  /** PostgreSQL: the schema unqualified names resolve to (default: `currentSchemaOf(scope)`). */
  currentSchema?: string | null;
  /** The objects of `qualifier.`: a value when already loaded, else a promise. */
  loadObjects(qualifier: string): CompletionObject[] | null | Promise<CompletionObject[] | null>;
}

/** `name` as the dialect needs it in SQL: quoted when it is not a plain lower/any-case word. */
export function quoteIdent(name: string, engine: Engine): string {
  const plain = engine === 'postgres' ? /^[a-z_][a-z0-9_$]*$/ : /^[A-Za-z_$][A-Za-z0-9_$]*$/;
  if (plain.test(name) && !/^[0-9$]/.test(name) && !reservedOf(engine).has(name.toUpperCase())) return name;
  const q = engine === 'postgres' ? '"' : '`';
  return q + name.split(q).join(q + q) + q;
}

/** The text of the statement around `offset`, and the offset inside it. */
function currentStatement(text: string, offset: number, ranges: [number, number][]): { stmt: string; offset: number } {
  const hit = ranges.find(([s, e]) => s <= offset && offset <= e);
  let lo = 0;
  let hi = text.length;
  if (hit) [lo, hi] = hit;
  else {
    // Between statements: from the end of the previous one to the start of the next one.
    let prevStart = -1;
    for (const [s, e] of ranges) {
      if (e <= offset && e >= lo) {
        lo = e;
        prevStart = s;
      }
      if (s > offset) hi = Math.min(hi, s);
    }
    // Ranges end at the last token: `SELECT * FROM |` (a blank gap, no `;`) is still that
    // statement, so the cursor keeps its FROM/JOIN/... context.
    if (prevStart >= 0 && /^\s*$/.test(text.slice(lo, offset))) lo = prevStart;
  }
  return { stmt: text.slice(lo, hi), offset: offset - lo };
}

const viewKinds: ObjectKind[] = ['View', 'MaterializedView'];
const objectKind = (kind: ObjectKind): Suggestion['kind'] => (viewKinds.includes(kind) ? 'view' : 'table');

const eqi = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** Exact match first, then case-insensitive (unquoted identifiers fold). */
function findBy<T>(items: T[], key: (t: T) => string, name: string): T | undefined {
  return items.find((t) => key(t) === name) ?? items.find((t) => eqi(key(t), name));
}

function resolve(scope: CompletionScope | null, ref: TableRef): SchemaCacheTable | undefined {
  if (!scope) return undefined;
  const candidates = ref.schema
    ? scope.tables.filter((t) => t.schema !== null && eqi(t.schema, ref.schema as string))
    : scope.tables;
  return findBy(candidates, (t) => t.name, ref.table);
}

function columnsOf(tables: (SchemaCacheTable | undefined)[], engine: Engine): Suggestion[] {
  const seen = new Set<string>();
  const out: Suggestion[] = [];
  for (const t of tables) {
    for (const c of t?.columns ?? []) {
      if (seen.has(c.name)) continue;
      seen.add(c.name);
      out.push({ label: c.name, kind: 'column', detail: c.data_type, insertText: quoteIdent(c.name, engine) });
    }
  }
  return out;
}

/**
 * The schema unqualified names resolve to on PostgreSQL. The scope does not say which
 * `search_path` entry comes first, so: `public` when the scope has it, else the schema
 * of the first table.
 */
export function currentSchemaOf(scope: CompletionScope | null): string | null {
  const schemas = (scope?.tables ?? []).map((t) => t.schema).filter((s): s is string => s !== null);
  return schemas.includes('public') ? 'public' : (schemas[0] ?? null);
}

/** `tablesOf` per scope object: rebuilt only when the scope is (re)loaded, not per keystroke. */
const tablesMemo = new WeakMap<CompletionScope, Map<string, Suggestion[]>>();

function tablesOf(scope: CompletionScope | null, engine: Engine, currentSchema: string | null): Suggestion[] {
  if (!scope) return [];
  let perScope = tablesMemo.get(scope);
  if (!perScope) tablesMemo.set(scope, (perScope = new Map()));
  const k = `${engine}\u0000${currentSchema ?? ''}`;
  let list = perScope.get(k);
  if (!list) perScope.set(k, (list = buildTables(scope, engine, currentSchema)));
  return list;
}

function buildTables(scope: CompletionScope, engine: Engine, currentSchema: string | null): Suggestion[] {
  return scope.tables.map((t) => {
    const name = quoteIdent(t.name, engine);
    // A PG table of another schema than the current one is inserted as `schema.table`.
    const qualify = engine === 'postgres' && t.schema !== null && currentSchema !== null && t.schema !== currentSchema;
    return {
      label: t.name,
      kind: objectKind(t.kind),
      detail: t.schema ?? undefined,
      insertText: qualify ? `${quoteIdent(t.schema as string, engine)}.${name}` : name,
    };
  });
}

/** Keyword and function suggestions per dialect: static, built once. */
const staticLists = new Map<string, Suggestion[]>();
function staticList(engine: Engine, kind: 'keyword' | 'function'): Suggestion[] {
  const k = `${kind}:${engine}`;
  let list = staticLists.get(k);
  if (!list) {
    const words = kind === 'keyword' ? keywordsOf(engine) : functionsOf(engine);
    staticLists.set(k, (list = words.map((w) => ({ label: w, kind, insertText: w }))));
  }
  return list;
}
const keywords = (engine: Engine) => staticList(engine, 'keyword');
const functions = (engine: Engine) => staticList(engine, 'function');

const objectSuggestions = (objects: CompletionObject[] | null, engine: Engine): Suggestion[] =>
  (objects ?? []).map((o) => ({ label: o.name, kind: objectKind(o.kind), insertText: quoteIdent(o.name, engine) }));

/**
 * The suggestions at the cursor. Synchronous unless the objects of a `db.` qualifier still
 * have to be loaded (then a promise).
 */
export function suggest(input: SuggestInput): Suggestion[] | Promise<Suggestion[]> {
  const { engine, scope } = input;
  const { stmt, offset } = currentStatement(input.text, input.offset, input.ranges);
  const { context, info } = analyze(stmt, offset, engine);
  const current = input.currentSchema === undefined ? currentSchemaOf(scope) : input.currentSchema;
  switch (context.kind) {
    case 'none':
      return [];
    case 'table': {
      const containerKind = engine === 'postgres' ? 'schema' : 'database';
      return [
        ...tablesOf(scope, engine, current),
        ...input.containers.map((c) => ({ label: c, kind: containerKind, insertText: quoteIdent(c, engine) }) as Suggestion),
      ];
    }
    case 'column':
      return [...columnsOf(info.tables.map((r) => resolve(scope, r)), engine), ...keywords(engine), ...functions(engine)];
    case 'member': {
      const q = context.qualifier;
      // An alias, then a table of the statement, then a database / schema.
      const alias = info.aliases.get(q) ?? findBy([...info.aliases], ([k]) => k, q)?.[1];
      const ref = alias ?? findBy(info.tables, (t) => t.table, q);
      if (ref) return columnsOf([resolve(scope, ref)], engine);
      const container = findBy(input.containers, (c) => c, q);
      if (container !== undefined) {
        const objects = input.loadObjects(container);
        return objects !== null && !Array.isArray(objects)
          ? objects.then((o) => objectSuggestions(o, engine))
          : objectSuggestions(objects, engine);
      }
      // A table of the database not (yet) named in FROM, e.g. while typing the SELECT list.
      return columnsOf([resolve(scope, { schema: null, table: q })], engine);
    }
    default:
      return [...keywords(engine), ...functions(engine), ...tablesOf(scope, engine, current)];
  }
}
