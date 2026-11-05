/**
 * Engines of the connection dialog and the values an empty field stands for.
 *
 * An empty Host / Port / User field is not an error: it means the engine's default, shown in
 * the field as a grey placeholder and filled in when the config is built.
 */
import type { DbKind, DriverOptions, SslMode } from '../bindings';
import { assertNever } from '../engine';

/** Brand marks (EngineLogo); MariaDB has its own mark but shares the MySQL driver. */
export type Engine = 'mysql' | 'mariadb' | 'postgres' | 'mongo' | 'redis';

export interface EngineInfo {
  /** The stored `kind`: MySQL and MariaDB share one driver (the variant is detected on connect). */
  id: DbKind;
  label: string;
  /** Marks shown on the engine choice; the first one stands for the engine elsewhere. */
  logos: Engine[];
  port: number;
  user: string;
  /** Placeholder of the connection URL field. */
  urlExample: string;
}

const MYSQL: EngineInfo = {
  id: 'mysql',
  label: 'MySQL / MariaDB',
  logos: ['mysql', 'mariadb'],
  port: 3306,
  user: 'root',
  urlExample: 'mysql://user:password@host:3306/database',
};
const POSTGRES: EngineInfo = {
  id: 'postgres',
  label: 'PostgreSQL',
  logos: ['postgres'],
  port: 5432,
  user: 'postgres',
  urlExample: 'postgresql://user:password@host:5432/database',
};
const MONGO: EngineInfo = {
  id: 'mongo',
  label: 'MongoDB',
  logos: ['mongo'],
  port: 27017,
  user: '',
  urlExample: 'mongodb://user:password@host:27017/database',
};
const REDIS: EngineInfo = {
  id: 'redis',
  label: 'Redis',
  logos: ['redis'],
  port: 6379,
  user: '',
  urlExample: 'redis://user:password@host:6379/0',
};

/** Every engine, in `DbKind` order (the picker shows those the build has a driver for). */
export const ENGINES: EngineInfo[] = [MYSQL, POSTGRES, MONGO, REDIS];

export const DEFAULT_HOST = 'localhost';

export function engineInfo(kind: DbKind): EngineInfo {
  switch (kind) {
    case 'mysql':
      return MYSQL;
    case 'postgres':
      return POSTGRES;
    case 'mongo':
      return MONGO;
    case 'redis':
      return REDIS;
    default:
      return assertNever(kind);
  }
}

/** The port an empty field stands for; `null` for a MongoDB SRV record (it names the hosts and ports). */
export const defaultPort = (kind: DbKind, srv = false): number | null => (srv ? null : engineInfo(kind).port);
export const defaultUser = (kind: DbKind) => engineInfo(kind).user;

/** SSL mode of a new connection: MongoDB and Redis have no "prefer", so they start in plaintext. */
export function defaultSsl(kind: DbKind): SslMode {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return 'prefer';
    case 'mongo':
    case 'redis':
      return 'disable';
    default:
      return assertNever(kind);
  }
}

/** `options.driver` of a new connection of `kind` (and after switching to it). */
export function defaultDriverOptions(kind: DbKind): DriverOptions {
  switch (kind) {
    case 'mysql':
    case 'postgres':
      return { kind: 'none' };
    case 'mongo':
      return { kind: 'mongo', srv: false, auth_source: null, replica_set: null, direct_connection: false, extra_hosts: [] };
    case 'redis':
      return { kind: 'redis', db: 0 };
    default:
      return assertNever(kind);
  }
}

/** The name a connection gets when the Name field is left empty: `database @ host`, or the host. */
export function autoName(host: string, database: string): string {
  const h = host.trim() || DEFAULT_HOST;
  const db = database.trim();
  return db ? `${db} @ ${h}` : h;
}
