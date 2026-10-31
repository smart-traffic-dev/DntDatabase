import type { Capabilities, NamespaceModel } from '../../bindings';

/**
 * What the MySQL/PostgreSQL drivers declare (`Capabilities::sql_full` in Rust): every feature,
 * objects listed in full. Shared by the vitest suites and the browser e2e backend.
 */
export const sqlCaps = (namespaces: NamespaceModel): Capabilities => ({
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
});
