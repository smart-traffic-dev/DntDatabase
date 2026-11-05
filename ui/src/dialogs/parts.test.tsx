import { describe, expect, it } from 'vitest';
import type { ServerInfo } from '../bindings';
import { defaultCaps } from '../engine';
import { serverLabel } from './parts';

const info = (kind: ServerInfo['kind'], variant: ServerInfo['variant'], version: [number, number, number]): ServerInfo => ({
  kind,
  variant,
  version,
  version_string: version.join('.'),
  capabilities: defaultCaps(kind),
});

describe('serverLabel', () => {
  it('keeps the SQL wording of the dialog context bar', () => {
    expect(serverLabel(info('mysql', 'MySql8', [8, 0, 36]))).toBe('MySQL 8.0.36');
    expect(serverLabel(info('mysql', 'MySql57', [5, 7, 44]))).toBe('MySQL 5.7.44');
    expect(serverLabel(info('mysql', 'MariaDb', [11, 4, 2]))).toBe('MariaDB 11.4.2');
    expect(serverLabel(info('postgres', 'Postgres', [16, 2, 0]))).toBe('PostgreSQL 16.2.0');
  });

  it('names the Command engines and has nothing for no server', () => {
    expect(serverLabel(info('redis', 'Redis', [7, 2, 0]))).toBe('Redis 7.2.0');
    expect(serverLabel(undefined)).toBeNull();
  });
});