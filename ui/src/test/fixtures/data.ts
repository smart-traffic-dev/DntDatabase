// Fixtures of the backend contract, shared by the unit tests (vitest) and the browser
// E2E tests (`e2e-web`). Nothing here may import `vitest` or a store: the E2E runner
// loads this module in Node.
import type {
  AppError,
  ConnectionConfig,
  ConnectionSummary,
  Group,
  ServerInfo,
  ServerVariant,
  Tree,
} from '../../bindings';
// `with { type: "json" }` keeps this module loadable by Node as-is (the browser E2E
// runner loads it directly); Vite resolves the JSON at build time either way.
import backendErrors from './app-errors.json' with { type: 'json' };
import { sqlCaps } from './caps';

/** Thrown by a handler to make the command return `{status: "error"}`. */
export class Failure {
  constructor(public readonly error: AppError) {}
}

/** Names of the real backend errors in `fixtures/app-errors.json` (generated from Rust). */
export type BackendErrorName = keyof typeof backendErrors;

/**
 * A real `AppError` as the backend sends it, from the fixtures the Rust test
 * `app_error_fixtures` generates. Use these instead of hand-written error shapes.
 */
export function backendError(name: BackendErrorName, extra: Partial<AppError> = {}): AppError {
  return { ...structuredClone(backendErrors[name] as AppError), ...extra };
}

/** Throwable `backendError`, for handlers. */
export const fail = (name: BackendErrorName, extra: Partial<AppError> = {}) =>
  new Failure(backendError(name, extra));

/** Hand-written error shape: only for failures that are not about the backend's error contract. */
export const appError = (kind: string, message: string, extra: Partial<AppError> = {}): AppError => ({
  kind,
  message,
  detail: null,
  stage: null,
  timings: null,
  server_code: null,
  sqlstate: null,
  position: null,
  retryable: false,
  outcome_unknown: false,
  hint: null,
  fingerprint: null,
  key_openssh: null,
  host: null,
  port: null,
  field: null,
  tx_state: null,
  ...extra,
});

export const ids = {
  prod: '11111111-1111-4111-8111-111111111111',
  analytics: '22222222-2222-4222-8222-222222222222',
  legacy: '33333333-3333-4333-8333-333333333333',
  groupA: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
  groupB: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb',
};

export function config(id: string, name: string, over: Partial<ConnectionConfig> = {}): ConnectionSummary {
  return {
    config: {
      id,
      group_id: null,
      name,
      sort_order: 0,
      kind: 'mysql',
      host: 'db.local',
      port: 3306,
      username: 'root',
      default_db: null,
      env_tag: null,
      read_only: false,
      secret_mode: 'saved',
      ssh: null,
      ssl: null,
      options: { version: 1, connect_timeout_s: 10, ssh_timeout_s: 10, charset: null, application_name: null, query_timeout_s: 0 },
      ...over,
    },
    has_db_password: true,
    has_ssh_secret: false,
    last_connected_at: null,
  };
}

/** Two groups, three connections (MySQL, PostgreSQL, and a MySQL-kind one that turns out to be MariaDB). */
export function sampleTree(): Tree {
  const groups: Group[] = [
    { id: ids.groupA, parent_id: null, name: 'Production', sort_order: 0 },
    { id: ids.groupB, parent_id: null, name: 'Local', sort_order: 1 },
  ];
  return {
    groups,
    connections: [
      config(ids.prod, 'shop-prod', { group_id: ids.groupA, env_tag: 'production', ssh: { host: 'bastion-01', port: 22, user: 'ops', auth: 'key', key_path: null } }),
      config(ids.analytics, 'analytics', { group_id: ids.groupA, kind: 'postgres', port: 5432 }),
      config(ids.legacy, 'legacy-crm', { group_id: ids.groupB }),
    ],
  };
}

export function serverInfo(variant: ServerVariant, version: [number, number, number] = [8, 0, 36]): ServerInfo {
  return {
    kind: variant === 'Postgres' ? 'postgres' : 'mysql',
    variant,
    version,
    version_string: variant === 'Postgres' ? `PostgreSQL ${version.join('.')}` : version.join('.'),
    capabilities: sqlCaps(variant === 'Postgres' ? 'db_schema' : 'db'),
  };
}

export const openResult = (info: ServerInfo) => ({ info, timings: [] });
