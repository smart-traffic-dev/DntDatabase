/**
 * Everything the UI decides per engine kind. The ESLint rule `no-restricted-syntax` forbids comparing
 * a kind with a literal anywhere else, so a new engine is added here and the compiler (through
 * `assertNever`) lists what is missing.
 */
import type { Capabilities, DbKind, Dialect, EngineFamily, NamespaceModel, ServerInfo } from '../bindings';
import { assertNever } from './assertNever';

export { assertNever };

/** SQL engine of a query document (follows its connection; MariaDB counts as `mysql`). */
export type SqlEngine = Dialect['engine'];

export function familyOf(kind: DbKind): EngineFamily {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return 'sql';
    case 'mongo':
    case 'redis':
      return 'command';
    default:
      return assertNever(kind);
  }
}

/** The SQL engine of a connection kind; `null` for the Command family. There is no MySQL fallback. */
export function sqlEngine(kind: DbKind): SqlEngine | null {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return kind;
    case 'mongo':
    case 'redis':
      return null;
    default:
      return assertNever(kind);
  }
}

/** What the drivers declare (`Capabilities::for_kind` in Rust), used until the server reports its own. */
export function defaultCaps(kind: DbKind): Capabilities {
  switch (kind) {
    case 'mysql':
      return sqlFull('db');
    case 'postgres':
      return sqlFull('db_schema');
    case 'mongo':
      return { ...commandBase('db'), data_edit: true };
    case 'redis':
      return { ...commandBase('numbered_db'), data_edit: true, paged_objects: true };
    default:
      return assertNever(kind);
  }
}

function sqlFull(namespaces: Capabilities['namespaces']): Capabilities {
  return {
    family: 'sql',
    namespaces,
    transactions: true,
    data_edit: true,
    designer: true,
    ddl_edit: true,
    object_actions: true,
    import: true,
    dump: true,
    seed: true,
    explain: true,
    paged_objects: false,
  };
}

function commandBase(namespaces: Capabilities['namespaces']): Capabilities {
  return {
    family: 'command',
    namespaces,
    transactions: false,
    data_edit: false,
    designer: false,
    ddl_edit: false,
    object_actions: false,
    import: false,
    dump: false,
    seed: false,
    explain: false,
    paged_objects: false,
  };
}

/** What the connected server reports, else what its driver declares. */
export function capsOf(info: ServerInfo | null | undefined, kind: DbKind): Capabilities {
  return info?.capabilities ?? defaultCaps(kind);
}

/** Quote an identifier the way the engine's query language does; Command names are not quoted. */
export function quoteIdent(kind: DbKind, name: string): string {
  switch (kind) {
    case 'postgres':
      return `"${name.replaceAll('"', '""')}"`;
    case 'mysql':
      return `\`${name.replaceAll('`', '``')}\``;
    case 'mongo':
    case 'redis':
      return name;
    default:
      return assertNever(kind);
  }
}

/** The hint an empty query editor shows, in the tab's language. */
export function editorPlaceholder(kind: DbKind): string {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return 'Write SQL  ·  Ctrl+Enter runs the statement at the cursor  ·  Ctrl+Space suggests';
    case 'mongo':
      return 'Write a command, e.g. db.orders.find({ status: "paid" })  ·  Ctrl+Enter runs it';
    case 'redis':
      return 'Write a command, e.g. GET key  ·  Ctrl+Enter runs it';
    default:
      return assertNever(kind);
  }
}

export function editorLanguage(kind: DbKind): 'mysql' | 'pgsql' | 'mongo' | 'redis' {
  switch (kind) {
    case 'mysql':
      return 'mysql';
    case 'postgres':
      return 'pgsql';
    case 'mongo':
      return 'mongo';
    case 'redis':
      return 'redis';
    default:
      return assertNever(kind);
  }
}

/** The Monaco languages of the SQL family: SQL completion and Format SQL serve these only. */
export const SQL_LANGUAGES: readonly string[] = [editorLanguage('mysql'), editorLanguage('postgres')];

/** Monaco context expression "the editor shows a SQL language" (an action's precondition). */
export const SQL_EDITOR_WHEN = SQL_LANGUAGES.map((l) => `editorLangId == ${l}`).join(' || ');

/** The Monaco language the MongoDB console completion serves. */
export const MONGO_LANGUAGE: string = editorLanguage('mongo');

/** Whether the query editor of a kind gets the MongoDB console completion (SQL kinds have their own; Redis none). */
export function hasMongoCompletion(kind: DbKind): boolean {
  switch (kind) {
    case 'mongo':
      return true;
    case 'mysql':
    case 'postgres':
    case 'redis':
      return false;
    default:
      return assertNever(kind);
  }
}

/**
 * Whether the query tabs of a kind get AI suggestions (ghost text): the SQL kinds and the
 * MongoDB console. Redis has no prompt yet.
 */
export function hasAiSuggestions(kind: DbKind): boolean {
  switch (kind) {
    case 'mysql':
    case 'postgres':
    case 'mongo':
      return true;
    case 'redis':
      return false;
    default:
      return assertNever(kind);
  }
}

/** The Monaco languages the AI inline provider serves (those of {@link hasAiSuggestions}). */
export const AI_LANGUAGES: readonly string[] = [editorLanguage('mysql'), editorLanguage('postgres'), MONGO_LANGUAGE];

export function engineChip(kind: DbKind): 'MY' | 'PG' | 'MG' | 'RD' {
  switch (kind) {
    case 'mysql':
      return 'MY';
    case 'postgres':
      return 'PG';
    case 'mongo':
      return 'MG';
    case 'redis':
      return 'RD';
    default:
      return assertNever(kind);
  }
}

export function engineLabel(kind: DbKind): string {
  switch (kind) {
    case 'mysql':
      return 'MySQL / MariaDB';
    case 'postgres':
      return 'PostgreSQL';
    case 'mongo':
      return 'MongoDB';
    case 'redis':
      return 'Redis';
    default:
      return assertNever(kind);
  }
}

/** What the tree calls a namespace of this kind (the top level a user creates). */
export function namespaceLabel(kind: DbKind): 'Database' | 'Schema' {
  switch (kind) {
    case 'mysql':
    case 'mongo':
    case 'redis':
      return 'Database';
    case 'postgres':
      return 'Schema';
    default:
      return assertNever(kind);
  }
}

/** The `Table` / `JSON` result toggle: MongoDB only. */
export function hasDocumentView(kind: DbKind): boolean {
  switch (kind) {
    case 'mongo':
      return true;
    case 'mysql':
    case 'postgres':
    case 'redis':
      return false;
    default:
      return assertNever(kind);
  }
}

/** The dialect passed to `statementRanges`; the backend ignores it when the connection has a language. */
export function rangesDialect(kind: DbKind): Dialect {
  switch (kind) {
    case 'mysql':
      return { engine: 'mysql' };
    case 'postgres':
    case 'mongo':
    case 'redis':
      return { engine: 'postgres' };
    default:
      return assertNever(kind);
  }
}

/** Escape hatch for SQL-only code that already holds a `SqlEngine` (from `sqlEngine(kind)`); prefer a `switch`. */
export function isKind(engine: SqlEngine, k: SqlEngine): boolean {
  return engine === k;
}

/** How the kind's namespaces nest: `db_schema` addresses an object by schema, `db` by database. */
export function namespaceModel(kind: DbKind): NamespaceModel {
  return defaultCaps(kind).namespaces;
}

export { canSample, mongoCollectionMember, sampleQuery, selectTopSql, type SelectOptions } from './sampleQuery';
