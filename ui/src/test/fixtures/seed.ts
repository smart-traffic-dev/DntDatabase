import type { ColumnPlan, SeedColumn, SeedPreview, SeedRequest, SeedSample, SeedTarget, TargetColumn } from '../../bindings';

/**
 * The data seeder's canned `shop.orders` (MySQL 8): what `seed_target` answers for it, a preview and
 * a reviewed plan. Shared by the vitest suites and the browser e2e backend.
 */

export const seedTable = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };

export const seedColumn = (name: string, over: Partial<TargetColumn> = {}): TargetColumn => ({
  name,
  db_type: 'int',
  logical: 'Integer',
  nullable: false,
  has_default: false,
  auto_increment: false,
  identity: null,
  generated: false,
  enum_values: [],
  max_chars: null,
  collation: null,
  unsigned: false,
  ...over,
});

const plan = (name: string, generator: ColumnPlan['generator'], over: Partial<ColumnPlan> = {}): ColumnPlan => ({
  name,
  generator,
  null_percent: 0,
  unique: false,
  ...over,
});

const col = (column: TargetColumn, p: ColumnPlan, over: Partial<SeedColumn> = {}): SeedColumn => ({
  column,
  primary: false,
  unique_index: false,
  foreign_key: null,
  check_values: [],
  locked: false,
  plan: p,
  ...over,
});

export const seedColumns: SeedColumn[] = [
  col(seedColumn('id', { auto_increment: true, has_default: true }), plan('id', { kind: 'skip' }, { unique: true }), {
    primary: true,
    unique_index: true,
  }),
  col(seedColumn('customer_id'), plan('customer_id', { kind: 'foreign_key', constraint: 'fk_orders_customer' }), {
    foreign_key: {
      constraint: 'fk_orders_customer',
      ref_schema: 'shop',
      ref_table: 'customers',
      columns: ['customer_id'],
      ref_columns: ['id'],
    },
  }),
  col(
    seedColumn('email', { db_type: 'varchar(120)', logical: 'Text', max_chars: 120 }),
    plan('email', { kind: 'fake', fake: 'email' }, { unique: true }),
    { unique_index: true },
  ),
  col(
    seedColumn('status', { db_type: "enum('new','paid','shipped')", logical: 'Enum', enum_values: ['new', 'paid', 'shipped'] }),
    plan('status', { kind: 'pick', values: ['new', 'paid', 'shipped'] }),
  ),
  col(
    seedColumn('total', { db_type: 'decimal(12,2)', logical: 'Decimal', nullable: true }),
    plan('total', { kind: 'decimal_range', min: 1, max: 1000, scale: 2 }),
  ),
  col(seedColumn('note', { db_type: 'text', logical: 'Text', nullable: true }), plan('note', { kind: 'fake', fake: 'paragraph' })),
  col(
    seedColumn('total_with_tax', { db_type: 'decimal(12,2)', logical: 'Decimal', generated: true, nullable: true }),
    plan('total_with_tax', { kind: 'skip' }),
    { locked: true },
  ),
];

export const seedTarget = (over: Partial<SeedTarget> = {}): SeedTarget => ({
  columns: seedColumns,
  env: null,
  read_only: false,
  server: { kind: 'mysql', variant: 'MySql8', version: [8, 0, 36], version_string: '8.0.36' },
  transactional: true,
  seed: 48213,
  ...over,
});

export const seedRequest = (connectionId: string, over: Partial<SeedRequest> = {}): SeedRequest => ({
  connection_id: connectionId,
  table: seedTable,
  rows: 1000,
  batch_rows: 1000,
  existing: 'keep',
  seed: 48213,
  columns: seedColumns.map((c) => c.plan),
  ...over,
});

/** Ten rows of the canned preview; `status` follows a `Constant` generator when it has one. */
export const seedSample = (request?: SeedRequest): SeedSample => {
  const status = request?.columns.find((c) => c.name === 'status')?.generator;
  const statusOf = (i: number) => (status?.kind === 'constant' ? status.value : ['new', 'paid', 'shipped'][i % 3]);
  const n = Math.min(10, request?.rows ?? 10);
  return {
    columns: ['customer_id', 'email', 'status', 'total', 'note'],
    rows: Array.from({ length: n }, (_, i) => [
      { t: 'Int', v: (i % 3) + 1 },
      { t: 'Text', v: `user${i}@example.com` },
      { t: 'Text', v: statusOf(i) },
      i === 4 ? { t: 'Null' } : { t: 'Decimal', v: `${(i + 1) * 10}.50` },
      { t: 'Text', v: 'Quick brown fox.' },
    ],
    ),
    sql: "INSERT INTO `shop`.`orders` (`customer_id`, `email`, `status`, `total`, `note`) VALUES (1, 'user0@example.com', 'new', 10.50, 'Quick brown fox.')",
    issues: [],
    fk_keys: [{ constraint: 'fk_orders_customer', count: 3 }],
  } as SeedSample;
};

export const seedPreview = (over: Partial<SeedPreview> = {}): SeedPreview => ({
  plan_id: 'seed-plan-1',
  table: seedTable,
  rows: 1000,
  batches: 1,
  transactional: true,
  pre_step: null,
  insert_head: 'INSERT INTO `shop`.`orders` (`customer_id`, `email`, `status`, `total`, `note`) VALUES ',
  insert_tail: '',
  sample: "INSERT INTO `shop`.`orders` (`customer_id`, `email`, `status`, `total`, `note`) VALUES (1, 'user0@example.com', 'new', 10.50, NULL)",
  warnings: ['Keys of referenced tables are read again when the job starts; rows may differ from the preview.'],
  confirm: { kind: 'none' },
  irreversible_name: null,
  expires_in_seconds: 300,
  ...over,
});
