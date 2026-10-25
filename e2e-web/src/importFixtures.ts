import type {
  DataImportPreview,
  EnvTag,
  ImportMode,
  ImportRequest,
  ImportTarget,
  Inspection,
  KeyChoice,
  Mapping,
  MappingReport,
  ObjectRef,
  ServerInfo,
  TargetColumn,
} from '../../ui/src/bindings';

/** What `import_pick_file` answers: the absolute path of the picked file. */
export const IMPORT_FILE = 'C:\\data\\orders.csv';

/** `orders.csv`: a header and three records (CSV line numbers 2–4). */
export const ordersInspection: Inspection = {
  columns: ['id', 'customer_id', 'amount'],
  rows: [
    { number: 2, cells: [{ t: 'Text', v: '1' }, { t: 'Text', v: '10' }, { t: 'Text', v: '19.90' }] },
    { number: 3, cells: [{ t: 'Text', v: '2' }, { t: 'Text', v: '11' }, { t: 'Null' }] },
    { number: 4, cells: [{ t: 'Text', v: '3' }, { t: 'Text', v: '10' }, { t: 'Text', v: '42.00' }] },
  ],
  sheets: [],
  delimiter: ',',
  total_bytes: 64,
};

function targetColumn(name: string, dbType: string, logical: TargetColumn['logical'], over: Partial<TargetColumn> = {}): TargetColumn {
  return {
    name,
    db_type: dbType,
    logical,
    nullable: false,
    has_default: false,
    auto_increment: false,
    identity: null,
    generated: false,
    enum_values: [],
    max_chars: null,
    collation: null,
    ...over,
  };
}

/** The canned MySQL 8 `orders` (the same columns as the tree's table detail). */
export function ordersTarget(env: EnvTag | null, server: ServerInfo, readOnly = false): ImportTarget {
  return {
    columns: [
      targetColumn('id', 'bigint', 'Integer', { auto_increment: true, has_default: true }),
      targetColumn('customer_id', 'bigint', 'Integer'),
      targetColumn('total', 'decimal(12,2)', 'Decimal', { has_default: true }),
      targetColumn('created_at', 'datetime', 'DateTime', { has_default: true }),
    ],
    keys: [{ name: 'PRIMARY', columns: ['id'], primary: true }],
    atomicity: 'yes',
    env,
    read_only: readOnly,
    server,
  };
}

/** A tiny mirror of the backend's auto map: exact (case-insensitive) name matches only. */
export function autoMap(source: string[], targets: TargetColumn[]): Mapping[] {
  return targets.map((t) => {
    const at = source.findIndex((s) => s.toLowerCase() === t.name.toLowerCase());
    return { target: t.name, source: at < 0 ? null : at };
  });
}

/** The backend's check, reduced to what the wizard needs: an upsert needs a usable key. */
export function checkMapping(mode: ImportMode, keys: KeyChoice[]): MappingReport {
  if (mode.kind === 'upsert' && !keys.some((k) => k.columns.join(',') === mode.key.join(','))) {
    return { errors: [{ column: null, message: 'Choose a primary key or a NOT NULL unique index for the upsert.' }], warnings: [] };
  }
  return { errors: [], warnings: [] };
}

const q = (name: string) => `\`${name}\``;

/** The review of `request` (MySQL syntax); Production asks for the table name. */
export function importPreview(request: ImportRequest, planId: string, env: EnvTag | null): DataImportPreview {
  const table: ObjectRef = request.table;
  const columns = request.mapping.filter((m) => m.source !== null).map((m) => q(m.target));
  const head = `INSERT INTO ${q(table.database)}.${q(table.name)} (${columns.join(', ')}) VALUES `;
  const update = request.mapping.filter((m) => m.source !== null && !(request.mode.kind === 'upsert' && request.mode.key.includes(m.target)));
  const tail = request.mode.kind === 'upsert' ? ` ON DUPLICATE KEY UPDATE ${update.map((m) => `${q(m.target)} = VALUES(${q(m.target)})`).join(', ')}` : '';
  return {
    plan_id: planId,
    file: { name: 'orders.csv', size: ordersInspection.total_bytes, sha256: '3f2a9c0d81b7e4f5a6c7d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d' },
    table,
    transactional: true,
    pre_step: null,
    insert_head: head,
    insert_tail: tail,
    sample: null,
    warnings: [],
    confirm: env === 'production' ? { kind: 'type_name', name: table.name } : env === 'staging' ? { kind: 'click' } : { kind: 'none' },
    irreversible_name: null,
    expires_in_seconds: 600,
  };
}
