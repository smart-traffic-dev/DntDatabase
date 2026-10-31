import type { DbKind, Dialect } from '../bindings';

/**
 * SQL engine of a query document (follows its connection; MariaDB counts as `mysql`). The single
 * definition shared by the stores, the completion and the editor.
 */
export type Engine = Dialect['engine'];

/** The SQL engine of a connection kind; `undefined` for none or for a kind without SQL (MongoDB, Redis). */
export function sqlEngine(kind: DbKind | null | undefined): Engine | undefined {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return kind;
    case 'mongo':
    case 'redis':
    case null:
    case undefined:
      return undefined;
  }
}
