import type { ConfirmLevel, DbKind, DumpObjects, EnvTag, RestorePreview, RestoreRequest, ToolsList } from '../../ui/src/bindings';

/** What `dump_pick_path` answers: the absolute path of the chosen output file. */
export const DUMP_FILE = 'C:\\backups\\dnt_fixture.sql';

/** What `restore_pick_file` answers: a plain SQL file the built-in runner can restore. */
export const RESTORE_FILE = 'C:\\backups\\dnt_fixture.sql';

/** A pg_dump plain file with COPY blocks: it needs psql (see `PSQL_FILE_MESSAGE`). */
export const PSQL_FILE = 'C:\\backups\\pg_copy.sql';

/** `PSQL_MISSING` of `src-tauri/src/backup/restore.rs`. */
export const PSQL_FILE_MESSAGE =
  'This file needs psql (it uses COPY … FROM stdin or psql commands) and psql was not found. Install the PostgreSQL client tools or set the External tools folder.';

/** `dnt_fixture`'s dump objects: `orders`, a view, and a procedure the built-in dump skips. */
export const fixtureDumpObjects: DumpObjects = {
  objects: [
    { schema: null, name: 'orders', kind: 'Table', data: true },
    { schema: null, name: 'order_totals', kind: 'View', data: false },
  ],
  skipped: [{ schema: null, name: 'archive_orders', kind: 'Procedure', reason: 'routines need an external dump tool' }],
};

/** No external tools installed: the built-in dump is the only method. */
export const noTools: ToolsList = { tools: [], missing: ['mysql_dump', 'mariadb_dump', 'pg_dump', 'pg_restore', 'psql'], searched: [] };

/** The confirmation `restore_preview` asks for on a connection of `env` (R20 / R32). */
export function restoreConfirm(env: EnvTag | null, database: string): ConfirmLevel {
  if (env === 'production') return { kind: 'type_name', name: database };
  if (env === 'staging') return { kind: 'click' };
  return { kind: 'none' };
}

/** `restore_preview`'s warning for every MySQL / MariaDB file (`src-tauri/src/backup/restore.rs`). */
export const MYSQL_NOT_TRANSACTIONAL = 'MySQL restores are not transactional (DDL commits at once): a failure leaves a partial restore.';

/**
 * The built-in runner's preview of `RESTORE_FILE`: 12 statements. As the backend decides: a
 * MySQL restore is never in one transaction (DDL commits at once); a PostgreSQL one is, and then
 * the file holds no transaction-control statements of its own.
 */
export function appRunnerPreview(req: RestoreRequest, planId: string, env: EnvTag | null, kind: DbKind): RestorePreview {
  const pg = kind === 'postgres';
  return {
    plan_id: planId,
    path: 'app_runner',
    file: { name: 'dnt_fixture.sql', size: 4096, sha256: 'a3f1c9e07b2d4e5f60718293a4b5c6d7e8f90123456789abcdef0123456789ab' },
    compressed: false,
    stats: { statements: 12, reads: 0, writes: 6, ddl: 4, transaction: pg ? 0 : 2, other: 0, drops: 0, copy_blocks: 0, client_commands: 0, app_dump: true },
    toc: [],
    toc_truncated: false,
    command: null,
    tool: null,
    transactional: pg,
    warnings: pg ? [] : [MYSQL_NOT_TRANSACTIONAL],
    confirm: restoreConfirm(env, req.database),
    expires_in_seconds: 600,
  };
}
