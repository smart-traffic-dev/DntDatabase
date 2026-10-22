import type {
  ColumnDef,
  ColumnDetail,
  ColumnMeta,
  ConnectionConfig,
  ConnectionSummary,
  DatabaseInfo,
  DesignPreview,
  DesignRender,
  DesignReport,
  EditableInfo,
  HistoryEntry,
  IndexDetail,
  ObjectKind,
  ObjectRef,
  ObjectSummary,
  ParsedConnString,
  Scope,
  ServerInfo,
  TableDataPage,
  TableDef,
  TableDetail,
  Tree,
  TypeInfo,
  TypeSpec,
  Value,
} from '../../ui/src/bindings';
import { Failure, appError, fail, openResult, serverInfo } from '../../ui/src/test/fixtures/data';
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

/**
 * A minimal mirror of `dnt-core`'s connection-string parser, enough to drive the dialog in the
 * browser tests (the real one is exercised by the Rust unit tests).
 */
function parseConnString(input: string): ParsedConnString {
  const parsed: ParsedConnString = {
    engine: null,
    host: null,
    port: null,
    user: null,
    password: null,
    database: null,
    ssl_mode: null,
    application_name: null,
    connect_timeout_s: null,
    ignored: [],
    warnings: [],
  };
  const text = input.trim().replace(/^jdbc:/, '');
  const scheme = /^(postgres(?:ql)?|mysql|mariadb):\/\//i.exec(text);
  const url = scheme ? text.slice(scheme[0].length) : text;
  if (scheme) {
    const name = scheme[1]!.toLowerCase();
    parsed.engine = name.startsWith('postgres') ? 'postgres' : name === 'mariadb' ? 'mariadb' : 'mysql';
  } else if (!text.includes('=')) {
    throw appError('config', `unsupported scheme "${text.split('://')[0]}://"`, { field: 'conn_string' });
  }
  const [main, query] = url.split('?') as [string, string | undefined];
  const slash = main.indexOf('/');
  const authority = slash < 0 ? main : main.slice(0, slash);
  if (slash >= 0) parsed.database = main.slice(slash + 1) || null;
  const at = authority.lastIndexOf('@');
  const hosts = at < 0 ? authority : authority.slice(at + 1);
  if (at >= 0) {
    const [user, password] = authority.slice(0, at).split(/:(.*)/s) as [string, string | undefined];
    parsed.user = user || null;
    if (password !== undefined) parsed.password = decodeURIComponent(password);
  }
  const colon = hosts.lastIndexOf(':');
  parsed.host = (colon < 0 ? hosts : hosts.slice(0, colon)) || null;
  if (colon >= 0) parsed.port = Number(hosts.slice(colon + 1));
  for (const pair of (query ?? '').split('&').filter(Boolean)) {
    const [k, v = ''] = pair.split('=') as [string, string | undefined];
    const key = k.toLowerCase().replace(/-/g, '_');
    if (key === 'sslmode' || key === 'ssl_mode') {
      parsed.ssl_mode = v.toLowerCase().replace('-', '_') as ParsedConnString['ssl_mode'];
    } else if (key === 'application_name') {
      parsed.application_name = v;
    } else if (key === 'connect_timeout') {
      parsed.connect_timeout_s = Number(v);
    } else if (key === 'dbname' || key === 'database') {
      parsed.database = v;
    } else if (key === 'user' || key === 'username') {
      parsed.user = v;
    } else if (key === 'host') {
      parsed.host = v;
    } else if (key === 'port') {
      parsed.port = Number(v);
    } else {
      parsed.ignored.push(`${k}=${v}`);
    }
  }
  return parsed;
}

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
    charset: null,
    on_update: null,
    identity_kind: null,
    unsigned: false,
    enum_values: [],
    ...over,
  };
}

function index(name: string, columns: string[], over: Partial<IndexDetail> = {}): IndexDetail {
  return { name, unique: false, primary: false, partial: false, columns, expression: null, method: null, definition: null, parts: [], visible: true, include: [], nulls_not_distinct: false, comment: null, ...over };
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
      charset: null,
      row_format: null,
      auto_increment: null,
      extra: [],
      is_partition: false,
      inherits: [],
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

const known = (base: string, over: Partial<Extract<TypeSpec, { kind: 'known' }>> = {}): TypeSpec => ({
  kind: 'known',
  base,
  length: null,
  precision: null,
  scale: null,
  unsigned: false,
  values: [],
  ...over,
});

function columnDef(name: string, data_type: TypeSpec, over: Partial<ColumnDef> = {}): ColumnDef {
  return {
    original: name,
    name,
    data_type,
    nullable: false,
    default: null,
    auto_increment: false,
    identity: null,
    on_update: null,
    charset: null,
    collation: null,
    comment: null,
    using: null,
    generated: null,
    locked: false,
    ...over,
  };
}

/** The canned MySQL 8 `orders` design: eight columns, primary key, one index. */
function ordersDesign(totalPrecision: number): TableDef {
  return {
    original_name: 'orders',
    name: 'orders',
    columns: [
      columnDef('id', known('bigint'), { auto_increment: true }),
      columnDef('customer_id', known('bigint')),
      columnDef('status', known('varchar', { length: 20 }), { default: { kind: 'value', text: 'new' } }),
      columnDef('total', known('decimal', { precision: totalPrecision, scale: 2 }), { default: { kind: 'value', text: '0.00' } }),
      columnDef('currency', known('char', { length: 3 }), { default: { kind: 'value', text: 'USD' } }),
      columnDef('note', known('text'), { nullable: true }),
      columnDef('created_at', known('datetime'), { default: { kind: 'expression', text: 'CURRENT_TIMESTAMP' } }),
      columnDef('updated_at', known('datetime'), { nullable: true }),
    ],
    primary_key: { original: 'PRIMARY', name: 'PRIMARY', columns: ['id'], locked: false },
    indexes: [
      {
        original: 'orders_customer_id_idx',
        name: 'orders_customer_id_idx',
        unique: false,
        parts: [{ column: 'customer_id', expression: null, prefix: null, descending: false }],
        method: 'BTREE',
        include: [],
        visible: true,
        comment: null,
        constraint: false,
        locked: false,
      },
    ],
    foreign_keys: [],
    checks: [],
    options: { engine: 'InnoDB', charset: null, collation: null, row_format: null, locked: false },
    comment: null,
    read_only_reason: null,
  };
}

const info = (base: string, over: Partial<TypeInfo> = {}): TypeInfo => ({
  base,
  length: 'None',
  precision: 'None',
  scale: 'None',
  unsigned: false,
  values: false,
  collation: false,
  ...over,
});
const CATALOG: TypeInfo[] = [
  info('bigint', { unsigned: true }),
  info('decimal', { precision: 'Optional', scale: 'Optional', unsigned: true }),
  info('varchar', { length: 'Required', collation: true }),
  info('char', { length: 'Optional', collation: true }),
  info('text', { collation: true }),
  info('datetime', { precision: 'Optional' }),
];

const lostDesign = () => new Failure(appError('config', 'unknown design', { field: 'design_id' }));

/** The one statement the canned render produces: only `total`'s precision is modelled. */
function renderOrders(draft: TableDef, basePrecision: number): string[] {
  const total = draft.columns.find((c) => c.original === 'total');
  if (!total || total.data_type.kind !== 'known') return [];
  const { precision, scale } = total.data_type;
  if (precision === basePrecision && scale === 2) return [];
  const type = precision === null ? 'decimal' : scale === null ? `decimal(${precision})` : `decimal(${precision},${scale})`;
  return [`ALTER TABLE \`dnt_fixture\`.\`orders\` MODIFY COLUMN \`total\` ${type} NOT NULL DEFAULT '0.00'`];
}

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
  // Table designer state: `orders.total`'s base precision, open designs (id -> connection), plans (id -> precision).
  let totalPrecision = 12;
  let designSeq = 0;
  const designs = new Map<string, string>();
  const plans = new Map<string, number>();

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

    // The real parser is in Rust (dnt-core); this echo is enough to drive the dialog.
    parse_connection_string: ({ input }) => parseConnString(String(input)),

    connection_state: ({ id }) => ({ state: open.has(String(id)) ? 'connected' : 'disconnected' }),

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

    // The table designer: one canned MySQL 8 `orders` design; only `total`'s precision is editable
    // end to end (apply stores the planned precision as the new base).
    table_design_open: ({ connectionId }) => {
      const id = connectionId as string;
      const server = requireOpen(id);
      const designId = `design-${++designSeq}`;
      designs.set(designId, id);
      return {
        design_id: designId,
        def: ordersDesign(totalPrecision),
        locked: [],
        catalog: CATALOG,
        server,
        env: connections.find((c) => c.id === id)?.env_tag ?? null,
        read_only: false,
      };
    },
    table_design_render: ({ designId, draft }): DesignRender => {
      if (!designs.has(designId as string)) throw lostDesign();
      return { statements: renderOrders(draft as TableDef, totalPrecision), impacts: [], errors: [], error: null };
    },
    table_design_preview: ({ designId, draft }): DesignPreview => {
      const conn = designs.get(designId as string);
      if (!conn) throw lostDesign();
      const planId = `plan-${++designSeq}`;
      const statements = renderOrders(draft as TableDef, totalPrecision);
      const total = (draft as TableDef).columns.find((c) => c.original === 'total');
      plans.set(planId, total?.data_type.kind === 'known' ? (total.data_type.precision ?? totalPrecision) : totalPrecision);
      const env = connections.find((c) => c.id === conn)?.env_tag ?? null;
      return { plan_id: planId, statements, impacts: [], confirm_name: env === 'production' ? 'orders' : null, expires_in_seconds: 600, row_estimate: 3 };
    },
    table_design_apply: ({ planId, confirmed, name }): DesignReport => {
      const precision = plans.get(planId as string);
      if (precision === undefined) throw fail('plan_stale');
      const prod = [...designs.values()].some((c) => connections.find((x) => x.id === c)?.env_tag === 'production');
      if (!confirmed || (prod && name !== 'orders')) return { outcome: 'not_applied', error: null, warnings: [], statements_sent: 0 };
      totalPrecision = precision;
      plans.delete(planId as string);
      return { outcome: 'applied', error: null, warnings: [], statements_sent: 1 };
    },
    table_design_cancel: () => null,
    table_design_close: ({ designId }) => {
      designs.delete(designId as string);
      return null;
    },

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
