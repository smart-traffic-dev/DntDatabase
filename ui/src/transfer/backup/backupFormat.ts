import type { AppError, RestorePath, Tool } from '../../bindings';

/** The program name of each external tool, as the user knows it. */
export const TOOL_LABEL: Record<Tool, string> = {
  mysql_dump: 'mysqldump',
  mariadb_dump: 'mariadb-dump',
  pg_dump: 'pg_dump',
  pg_restore: 'pg_restore',
  psql: 'psql',
};

/** The restore path chip (R30). */
export const PATH_LABEL: Record<RestorePath, string> = {
  app_runner: 'Built-in runner',
  psql: 'psql',
  pg_restore: 'pg_restore',
};

/**
 * The restore dialog's message for a failed analysis or start. `unsupported` carries the reason
 * (psql / pg_restore missing) in its detail, followed by the server version, which is dropped.
 */
export function restoreMessage(err: AppError): string {
  switch (err.kind) {
    case 'plan_stale':
      return 'The file or the connection changed; analyze again.';
    case 'plan_expired':
      return 'The analysis expired; analyze again.';
    case 'blocked':
      return 'This connection is read-only: restoring is blocked.';
    case 'unsupported':
      return err.detail ? err.detail.replace(/\s*\(server version [^)]*\)\s*$/, '') : err.message;
    default:
      return err.message;
  }
}
