/**
 * Engines of the connection dialog and the values an empty field stands for.
 *
 * An empty Host / Port / User field is not an error: it means the engine's default, shown in
 * the field as a grey placeholder and filled in when the config is built.
 */
import type { DbKind } from '../bindings';

/** Brand marks (EngineLogo); MariaDB has its own mark but shares the MySQL driver. */
export type Engine = 'mysql' | 'mariadb' | 'postgres';

export interface EngineInfo {
  /** The stored `kind`: MySQL and MariaDB share one driver (the variant is detected on connect). */
  id: DbKind;
  label: string;
  /** Marks shown on the engine choice; the first one stands for the engine elsewhere. */
  logos: Engine[];
  port: number;
  user: string;
}

export const ENGINES: EngineInfo[] = [
  { id: 'mysql', label: 'MySQL / MariaDB', logos: ['mysql', 'mariadb'], port: 3306, user: 'root' },
  { id: 'postgres', label: 'PostgreSQL', logos: ['postgres'], port: 5432, user: 'postgres' },
];

export const DEFAULT_HOST = 'localhost';

export const engineInfo = (engine: DbKind): EngineInfo => ENGINES.find((e) => e.id === engine) ?? ENGINES[0]!;

export const defaultPort = (engine: DbKind) => engineInfo(engine).port;
export const defaultUser = (engine: DbKind) => engineInfo(engine).user;

/** The name a connection gets when the Name field is left empty: `database @ host`, or the host. */
export function autoName(host: string, database: string): string {
  const h = host.trim() || DEFAULT_HOST;
  const db = database.trim();
  return db ? `${db} @ ${h}` : h;
}
