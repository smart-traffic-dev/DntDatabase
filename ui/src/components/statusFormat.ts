import type { ServerInfo } from '../bindings';

const ENGINE: Record<ServerInfo['variant'], string> = {
  MySql57: 'MySQL',
  MySql8: 'MySQL',
  MariaDb: 'MariaDB',
  Postgres: 'PostgreSQL',
};

/** "PostgreSQL 17.11", "MySQL 8.0.46", "MariaDB 11.4.2": engine plus short version (the full string goes in a tooltip). */
export function shortVersion(info: ServerInfo): string {
  const engine = ENGINE[info.variant];
  if (info.variant === 'Postgres') {
    const m = /PostgreSQL\s+(\d+(?:\.\d+)*)/.exec(info.version_string);
    return `${engine} ${m?.[1] ?? `${info.version[0]}.${info.version[1]}`}`;
  }
  return `${engine} ${info.version.join('.')}`;
}

/** "450 ms", "3.1 s", "12 s", "1 m 05 s". */
export function formatElapsed(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))} ms`;
  if (ms < 10_000) return `${(ms / 1000).toFixed(1)} s`;
  const total = Math.floor(ms / 1000);
  if (total < 60) return `${total} s`;
  return `${Math.floor(total / 60)} m ${String(total % 60).padStart(2, '0')} s`;
}
