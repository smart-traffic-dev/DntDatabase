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

/** The backend's answer to a path its file dialog did not admit (`file_picks`). */
const NOT_PICKED = 'Choose the file in the file dialog.';

/**
 * A path that was not picked in this run of the app (a "Run again" after a restart): the file
 * must be chosen again. `null` for any other error.
 */
export function pickedFileMessage(err: AppError): string | null {
  if (err.kind === 'config' && err.field === 'path' && err.message === NOT_PICKED) {
    return 'Choose the file again (files picked before a restart must be chosen again).';
  }
  return null;
}

/**
 * The restore dialog's message for a failed analysis or start. `unsupported` carries the reason
 * (psql / pg_restore missing) in its detail, followed by the server version, which is dropped.
 */
export function restoreMessage(err: AppError): string {
  const picked = pickedFileMessage(err);
  if (picked) return picked;
  switch (err.kind) {
    case 'plan_stale':
      return `The file or the connection changed; analyze again.${err.detail ? ` (${err.detail})` : ''}`;
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
