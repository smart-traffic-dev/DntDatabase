import { describe, expect, it } from 'vitest';
import type { ServerInfo } from '../bindings';
import { sqlCaps } from '../test/fixtures/caps';
import { formatElapsed, shortVersion } from './statusFormat';

const info = (variant: ServerInfo['variant'], version: [number, number, number], version_string: string): ServerInfo => ({
  kind: variant === 'Postgres' ? 'postgres' : 'mysql',
  variant,
  version,
  version_string,
  capabilities: sqlCaps(variant === 'Postgres' ? 'db_schema' : 'db'),
});

describe('shortVersion', () => {
  it('shortens the PostgreSQL version() string', () => {
    const full = 'PostgreSQL 17.11 (Debian 17.11-1.pgdg120+1) on x86_64-pc-linux-gnu, compiled by gcc (Debian 12.2.0-14) 12.2.0, 64-bit';
    expect(shortVersion(info('Postgres', [17, 11, 0], full))).toBe('PostgreSQL 17.11');
  });

  it('names MySQL and MariaDB by variant', () => {
    expect(shortVersion(info('MySql8', [8, 0, 46], '8.0.46'))).toBe('MySQL 8.0.46');
    expect(shortVersion(info('MySql57', [5, 7, 44], '5.7.44-log'))).toBe('MySQL 5.7.44');
    expect(shortVersion(info('MariaDb', [11, 4, 2], '11.4.2-MariaDB-log'))).toBe('MariaDB 11.4.2');
  });

  it('falls back to the triple when the string has no version', () => {
    expect(shortVersion(info('Postgres', [16, 3, 0], 'unknown'))).toBe('PostgreSQL 16.3');
  });
});

describe('formatElapsed', () => {
  it('formats milliseconds, seconds and minutes', () => {
    expect(formatElapsed(0)).toBe('0 ms');
    expect(formatElapsed(450)).toBe('450 ms');
    expect(formatElapsed(3093)).toBe('3.1 s');
    expect(formatElapsed(12_000)).toBe('12 s');
    expect(formatElapsed(65_000)).toBe('1 m 05 s');
    expect(formatElapsed(3_725_000)).toBe('62 m 05 s');
  });
});
