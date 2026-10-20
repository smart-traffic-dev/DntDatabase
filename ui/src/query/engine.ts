import type { Dialect } from '../bindings';

/**
 * SQL engine of a query document (follows its connection; MariaDB counts as `mysql`). The single
 * definition shared by the stores, the completion and the editor.
 */
export type Engine = Dialect['engine'];
