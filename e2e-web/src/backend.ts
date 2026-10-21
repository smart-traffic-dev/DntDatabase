import type {
  ColumnDetail,
  ColumnMeta,
  ConnectionConfig,
  ConnectionSummary,
  DatabaseInfo,
  EditableInfo,
  HistoryEntry,
  IndexDetail,
  ObjectKind,
  ObjectRef,
  ObjectSummary,
  Scope,
  ServerInfo,
  TableDataPage,
  TableDetail,
  Tree,
  Value,
} from '../../ui/src/bindings';
import { fail, openResult, serverInfo } from '../../ui/src/test/fixtures/data';
import type { Handler } from './shim';

/** The id the dialog sends while creating (see `NIL_UUID` in ConnectionDialog). */
const NIL_UUID = '00000000-0000-0000-0000-000000000000';

const DDL: Record<string, string> = {
  MySql8: [
    'CREATE TABLE `orders` (',
    '  `id` bigint NOT NULL AUTO_INCREMENT,',
    '  `customer_id` bigint NOT NULL,',
    "  `total` decimal(12,2) NOT NULL DEFAULT '0.00',",
    '  `created_at` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,',
    '  PRIMARY KEY (`id`),',
    '  KEY `orders_customer_id_idx` (`customer_id`)',
    ') ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci',
  ].join('\n'),
  Postgres: [
    'CREATE TABLE public.orders (',
    '    id bigint NOT NULL,',
    '    customer_id bigint NOT NULL,',
    '    total numeric(12,2) DEFAULT 0 NOT NULL,',
    '    created_at timestamp with time zone DEFAULT now() NOT NULL',
    ');',
  ].join('\n'),
};

const infoOf = (cfg: ConnectionConfig): ServerInfo =>
  cfg.kind === 'postgres' ? serverInfo('Postgres', [17, 0, 0]) : serverInfo('MySql8', [8, 0, 36]);

function column(name: string, ordinal: number, dbType: string, over: Partial<ColumnDetail> = {}): ColumnDetail {
  return {
    name,
    ordinal,
    db_type: dbType,
    nullable: false,
    default: null,
    auto_increment: false,
    identity: null,
    generated: null,
    invisible: false,
    collation: null,
    comment: null,
    ...over,
  };
}

function index(name: string, columns: string[], over: Partial<IndexDetail> = {}): IndexDetail {
  return { name, unique: false, primary: false, partial: false, columns, expression: null, method: null, definition: null, ...over };
}

function detailOf(variant: ServerInfo['variant'], object: ObjectRef): TableDetail {
  const mysql = variant !== 'Postgres';
  return {
    object,
    columns: [
      column('id', 1, 'bigint', { auto_increment: mysql, identity: mysql ? null : 'ALWAYS' }),
      column('customer_id', 2, 'bigint', { comment: 'Who placed the order' }),
      column('total', 3, mysql ? 'decimal(12,2)' : 'numeric(12,2)', { default: '0.00' }),
      column('created_at', 4, mysql ? 'datetime' : 'timestamp with time zone', { default: mysql ? 'CURRENT_TIMESTAMP' : 'now()' }),
    ],
    primary_key: index(mysql ? 'PRIMARY' : 'orders_pkey', ['id'], {
      unique: true,
      primary: true,
      method: mysql ? 'BTREE' : 'btree',
      definition: 'PRIMARY KEY (id)',
    }),
    indexes: [
      index('orders_customer_id_idx', ['customer_id'], {
        method: mysql ? 'BTREE' : 'btree',
        definition: 'CREATE INDEX orders_customer_id_idx ON orders (customer_id)',
      }),
    ],
    foreign_keys: [],
    checks: [],
    triggers: [],
    options: {
      engine: mysql ? 'InnoDB' : null,
      collation: mysql ? 'utf8mb4_0900_ai_ci' : null,
      tablespace: null,
      partitioned_by: null,
      system_versioned: false,
    },
    row_estimate: 3,
    size_bytes: mysql ? 16_384 : 32_768,
    comment: '',
  };
}

const meta = (name: string, dbType: string, logical: ColumnMeta['logical']): ColumnMeta => ({
  name,
  db_type: dbType,
  logical,
  nullable: false,
  origin_table: null,
  origin_column: null,
});

/** The Data sub-tab (tables open on it): three orders, one page. */
function pageOf(): TableDataPage {
  const int = (v: number): Value => ({ t: 'Int', v });
  return {
    columns: [meta('id', 'bigint', 'Integer'), meta('customer_id', 'bigint', 'Integer'), meta('total', 'decimal(12,2)', 'Decimal')],
    rows: [
      [int(1), int(10), { t: 'Decimal', v: '19.90' }],
      [int(2), int(11), { t: 'Decimal', v: '5.00' }],
      [int(3), int(10), { t: 'Decimal', v: '42.00' }],
    ],
    has_more: false,
    truncated_cells: 0,
    truncated: [],
    size_capped: false,
    key_order: ['id'],
    row_versions: null,
    hidden_key: [],
    hidden_key_values: [],
    elapsed_ms: 1,
  };
}

/** Browsing only: these tests do not edit data. */
const READ_ONLY: EditableInfo = {
  editable: false,
  reason: 'read_only_connection',
  key: ['id'],
  hidden_key: [],
  editable_columns: [],
  atomic: 'yes',
  protection: 'row_version',
};

/** Two statements the app ran earlier, for the right sidebar's History panel. */
const HISTORY: HistoryEntry[] = [
  {
    id: 2,
    connection_id: 'e2e-history-conn',
    database: 'dnt_fixture',
    sql_text: 'UPDATE orders SET total = 0 WHERE id = 99',
    redacted: false,
    kind: 'write',
    started_at: new Date().toISOString(),
    elapsed_ms: 4,
    status: 'error',
    row_count: null,
    affected: null,
    error_kind: 'query',
    error_code: '1054',
    origin: 'query',
    outcome_unknown: false,
  },
  {
    id: 1,
    connection_id: 'e2e-history-conn',
    database: 'dnt_fixture',
    sql_text: 'SELECT 42 AS answer',
    redacted: false,
    kind: 'read',
    started_at: new Date().toISOString(),
    elapsed_ms: 7,
    status: 'ok',
    row_count: 1,
    affected: null,
    error_kind: null,
    error_code: null,
    origin: 'query',
    outcome_unknown: false,
  },
];

export interface FakeBackend {
  /** Commands the app called, in order — for asserting what the UI asked the backend. */
  calls: { cmd: string; args: Record<string, unknown> }[];
  handlers: Record<string, Handler>;
}

/**
 * A small in-memory backend for the browser E2E tests: one server per connection kind
 * with a `dnt_fixture` database holding `orders`. Assertions about real SQL belong to the
 * Rust tests and the tauri-driver smoke test; this one exercises the UI.
 */
export function createBackend(overrides: Record<string, Handler> = {}): FakeBackend {
  const connections: ConnectionConfig[] = [];
  const open = new Map<string, ServerInfo>();
  const calls: FakeBackend['calls'] = [];
  let history = [...HISTORY];

  const summary = (cfg: ConnectionConfig): ConnectionSummary => ({
    config: cfg,
    has_db_password: cfg.secret_mode === 'saved',
    has_ssh_secret: !!cfg.ssh,
    last_connected_at: null,
  });

  const requireOpen = (id: string): ServerInfo => {
    const info = open.get(id);
    if (!info) throw fail('not_open');
    return info;
  };

  const handlers: Record<string, Handler> = {
    // Startup: the stored connections, the restored query tabs, favorites and snippets.
    list_tree: (): Tree => ({ groups: [], connections: connections.map(summary) }),
    tabs_load: () => [],
    tabs_save: () => null,
    tabs_delete: () => null,
    favorites_list: () => [],
    snippets_list: () => [],

    create_connection: ({ cfg }) => {
      const config = cfg as ConnectionConfig;
      const saved: ConnectionConfig = { ...config, id: config.id === NIL_UUID ? crypto.randomUUID() : config.id };
      connections.push(saved);
      return saved;
    },

    update_connection: ({ cfg }) => {
      const config = cfg as ConnectionConfig;
      const at = connections.findIndex((c) => c.id === config.id);
      if (at < 0) throw new Error(`e2e: update of unknown connection ${config.id}`);
      connections[at] = config;
      return config;
    },

    delete_connection: ({ id }) => {
      const at = connections.findIndex((c) => c.id === id);
      if (at >= 0) connections.splice(at, 1);
      return null;
    },

    test_connection: ({ cfg }) => openResult(infoOf(cfg as ConnectionConfig)),

    open_connection: ({ id }) => {
      const cfg = connections.find((c) => c.id === id);
      if (!cfg) throw new Error(`e2e: open of unknown connection ${String(id)}`);
      const info = infoOf(cfg);
      open.set(cfg.id, info);
      return openResult(info);
    },

    close_connection: ({ id }) => {
      open.delete(id as string);
      return null;
    },

    list_databases: (): DatabaseInfo[] => [{ name: 'dnt_fixture' }],

    list_schemas: () => [{ name: 'public' }],

    list_objects: ({ scope, kind }): ObjectSummary[] => {
      if (kind !== 'Table') return [];
      if ((scope as Scope).database !== 'dnt_fixture') return [];
      return [{ name: 'orders', kind: kind as ObjectKind, comment: null, row_estimate: 3 }];
    },

    table_detail: ({ id, obj }) => detailOf(requireOpen(id as string).variant, obj as ObjectRef),

    object_ddl: ({ id }) => DDL[requireOpen(id as string).variant] ?? '',

    // The Data sub-tab.
    get_table_page_size: () => 100,
    data_editable: ({ id }) => {
      requireOpen(id as string);
      return READ_ONLY;
    },
    table_data: ({ id }) => {
      requireOpen(id as string);
      return pageOf();
    },
    table_data_cancel: () => true,

    // The cache is a backend optimisation; nothing to invalidate here.
    refresh_node: () => null,

    // The right sidebar's History panel.
    history_list: ({ filter }) => {
      const f = filter as { connection_id: string | null; search: string | null };
      return history.filter(
        (h) => (f.connection_id === null || h.connection_id === f.connection_id) && (f.search === null || h.sql_text.includes(f.search)),
      );
    },
    history_delete: ({ ids }) => {
      history = history.filter((h) => !(ids as number[]).includes(h.id));
      return null;
    },
    history_clear: () => {
      history = [];
      return null;
    },

    get_log_level: () => 'Info',
    set_log_level: () => null,
    open_log_folder: () => null,

    // What @tauri-apps/plugin-dialog / plugin-opener call. No native picker in a browser.
    'plugin:dialog|open': () => null,
    'plugin:dialog|save': () => null,
    'plugin:dialog|message': () => null,
    'plugin:opener|open_url': () => null,
    'plugin:opener|reveal_item_in_dir': () => null,
  };

  const all = { ...handlers, ...overrides };
  return {
    calls,
    handlers: Object.fromEntries(
      Object.entries(all).map(([cmd, handler]) => [
        cmd,
        (args: Record<string, unknown>) => {
          calls.push({ cmd, args });
          return handler(args);
        },
      ]),
    ),
  };
}
