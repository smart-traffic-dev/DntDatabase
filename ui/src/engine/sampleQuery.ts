import type { DbKind, ObjectKind, ObjectRef } from '../bindings';
import { assertNever } from './assertNever';
import { isKind, quoteIdent, type SqlEngine } from './index';

export interface SelectOptions {
  /** Default 1000. */
  limit?: number;
  /** Rows to skip (`OFFSET`); omitted when 0. */
  offset?: number;
  orderBy?: { column: string; dir: 'asc' | 'desc' } | null;
}

/**
 * `SELECT * ... LIMIT 1000` of a table or view, identifiers quoted for the connection's engine.
 * The one helper for this SQL: the tree's "Select top 1000", the object tab's Query button and
 * the data view's "Open in query" (which passes its page size, offset and sort).
 */
export function selectTopSql(target: { objectRef?: ObjectRef }, kind: SqlEngine, opts: SelectOptions = {}): string {
  const ref = target.objectRef;
  if (!ref) return '';
  // PostgreSQL addresses a table by schema (a connection is one database); MySQL by database.
  const qualifier = isKind(kind, 'postgres') ? ref.schema : ref.database;
  const table = qualifier ? `${quoteIdent(kind, qualifier)}.${quoteIdent(kind, ref.name)}` : quoteIdent(kind, ref.name);
  const order = opts.orderBy ? ` ORDER BY ${quoteIdent(kind, opts.orderBy.column)} ${opts.orderBy.dir === 'desc' ? 'DESC' : 'ASC'}` : '';
  const offset = opts.offset ? ` OFFSET ${Math.trunc(opts.offset)}` : '';
  return `SELECT * FROM ${table}${order} LIMIT ${Math.trunc(opts.limit ?? 1000)}${offset};`;
}

/** Names that `db.<name>` resolves to a database method or property, not a collection. */
const DB_MEMBERS: ReadonlySet<string> = new Set([
  'adminCommand', 'aggregate', 'auth', 'changeUserPassword', 'cloneDatabase', 'commandHelp', 'copyDatabase', 'createCollection',
  'createRole', 'createUser', 'createView', 'currentOp', 'dropAllRoles', 'dropAllUsers', 'dropDatabase', 'dropRole', 'dropUser',
  'fsyncLock', 'fsyncUnlock', 'getCollection', 'getCollectionInfos', 'getCollectionNames', 'getLogComponents', 'getMongo', 'getName',
  'getProfilingLevel', 'getProfilingStatus', 'getReplicationInfo', 'getRole', 'getRoles', 'getSiblingDB', 'getUser', 'getUsers',
  'grantPrivilegesToRole', 'grantRolesToRole', 'grantRolesToUser', 'help', 'hello', 'hostInfo', 'isMaster', 'killOp', 'listCommands',
  'logout', 'printCollectionStats', 'printReplicationInfo', 'printSecondaryReplicationInfo', 'printShardingStatus',
  'revokePrivilegesFromRole', 'revokeRolesFromRole', 'revokeRolesFromUser', 'rotateCertificates', 'runCommand', 'serverBuildInfo',
  'serverCmdLineOpts', 'serverStatus', 'setLogLevel', 'setProfilingLevel', 'setSecondaryOk', 'setWriteConcern', 'shutdownServer',
  'stats', 'unsetWriteConcern', 'updateRole', 'updateUser', 'version', 'watch',
  // Inherited from Object.prototype.
  'constructor', 'hasOwnProperty', 'isPrototypeOf', 'propertyIsEnumerable', 'toLocaleString', 'toString', 'valueOf', '__proto__',
]);

const JS_IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

/** What follows `db.` to reach a collection: its plain name, else `getCollection("<name>")`. */
export function mongoCollectionMember(name: string): string {
  return JS_IDENT.test(name) && !DB_MEMBERS.has(name) ? name : `getCollection(${JSON.stringify(name)})`;
}

/** `db.<name>` for a plain collection name, else `db.getCollection("<name>")`. */
function mongoCollection(name: string): string {
  return `db.${mongoCollectionMember(name)}`;
}

/** Object kinds a sample query is offered for, per engine; Redis has no collections. */
export function canSample(kind: DbKind, object: ObjectKind | undefined): boolean {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return object === 'Table' || object === 'View' || object === 'MaterializedView';
    case 'mongo':
      return object === 'Collection' || object === 'View';
    case 'redis':
      return false;
    default:
      return assertNever(kind);
  }
}

/**
 * The query a new tab on `ref` starts with: the first `limit` rows / documents. `null` when the
 * engine or the object kind has none (the tab opens blank).
 */
export function sampleQuery(kind: DbKind, ref: ObjectRef, limit = 1000): string | null {
  if (!canSample(kind, ref.kind)) return null;
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return selectTopSql({ objectRef: ref }, kind, { limit });
    case 'mongo':
      return `${mongoCollection(ref.name)}.find({}).limit(${Math.trunc(limit)})`;
    case 'redis':
      return null;
    default:
      return assertNever(kind);
  }
}
