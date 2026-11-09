import type {
  ApplyReport,
  CellInput,
  ColumnMeta,
  ConnectionConfig,
  DataFilter,
  EditPreview,
  EditSet,
  EditableInfo,
  ObjectRef,
  RowEdit,
  ServerInfo,
  SortSpec,
  TableDataPage,
  TableDetail,
  Value,
} from '../../ui/src/bindings';
import { Failure, appError } from '../../ui/src/test/fixtures/data';
import type { CommandFakeState, MongoDoc } from './commandFixtures';
import type { Handler } from './shim';

// The Data tab of a MongoDB collection against the fake Command servers, in the R4a shapes:
// documents are flattened into one column per top-level field (`_id` first, spec §7.3), a JSON
// filter (`raw_json`) matches fields by equality, edits are planned as `updateOne` / `insertOne` /
// `deleteOne` commands and applied one document at a time (no transactions: not atomic).

/** `NOT_ATOMIC_COLLECTION_ON_PRODUCTION` of `src-tauri/src/edit_plans.rs`. */
const NOT_ATOMIC_ON_PRODUCTION =
  'Changes to this collection cannot be rolled back as a unit, so the data view does not edit it on a Production connection. Use the console instead.';

const filterError = (message: string) => new Failure(appError('config', message, { field: 'filter' }));

/** A document field as a grid value. */
function toValue(v: unknown): Value {
  if (v === undefined || v === null) return { t: 'Null' };
  if (typeof v === 'string') return { t: 'Text', v };
  if (typeof v === 'boolean') return { t: 'Bool', v };
  if (typeof v === 'number') return Number.isInteger(v) ? { t: 'Int', v } : { t: 'Float', v };
  return { t: 'Json', v: JSON.stringify(v) };
}

function logicalOf(v: unknown): ColumnMeta['logical'] {
  if (typeof v === 'string') return 'Text';
  if (typeof v === 'boolean') return 'Bool';
  if (typeof v === 'number') return Number.isInteger(v) ? 'Integer' : 'Float';
  return 'Json';
}

/** The top-level fields of `docs`, `_id` first, then in the order they first appear. */
function fieldsOf(docs: MongoDoc[]): string[] {
  const out = ['_id'];
  for (const d of docs) for (const k of Object.keys(d)) if (!out.includes(k)) out.push(k);
  return out;
}

/** What a filter keeps: the JSON filter's fields by equality, or the bar's `=` / `<>` conditions. */
function matcher(filter: DataFilter | null): (d: MongoDoc) => boolean {
  if (!filter) return () => true;
  if (filter.raw_where && filter.raw_json) throw filterError('Use either a WHERE filter or a JSON filter, not both');
  if (filter.raw_where) throw filterError('This filter type is not available for this engine');
  if (filter.raw_json) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(filter.raw_json);
    } catch {
      throw filterError('The filter must be a JSON object');
    }
    if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) throw filterError('The filter must be a JSON object');
    const want = Object.entries(parsed as Record<string, unknown>);
    if (want.some(([k]) => k === '$where' || k === '$function')) throw filterError('Server-side JavaScript is not allowed in a filter');
    return (d) => want.every(([k, v]) => JSON.stringify(d[k]) === JSON.stringify(v));
  }
  const tests = filter.conditions.map((c) => {
    if (c.op !== 'eq' && c.op !== 'ne') throw filterError('This filter type is not available for this engine');
    const eq = (d: MongoDoc) => d[c.column] !== undefined && String(d[c.column]) === c.values[0];
    return c.op === 'eq' ? eq : (d: MongoDoc) => !eq(d);
  });
  return (d) => (filter.mode === 'or' ? tests.some((t) => t(d)) : tests.every((t) => t(d)));
}

function sorted(docs: MongoDoc[], sort: SortSpec[]): MongoDoc[] {
  const keys = sort.length ? sort : [{ column: '_id', dir: 'asc' as const }];
  return [...docs].sort((a, b) => {
    for (const k of keys) {
      const x = JSON.stringify(a[k.column] ?? null);
      const y = JSON.stringify(b[k.column] ?? null);
      if (x !== y) return (x < y ? -1 : 1) * (k.dir === 'asc' ? 1 : -1);
    }
    return 0;
  });
}

/** The value a cell input sets on a field whose current value is `was` (`undefined`: none). */
function inputValue(input: CellInput, was: unknown): unknown {
  switch (input.kind) {
    case 'null':
      return null;
    case 'text':
      // A string field stays a string; another type reads the text as JSON when it parses.
      if (typeof was === 'string' || was === undefined) return input.text;
      try {
        return JSON.parse(input.text);
      } catch {
        return input.text;
      }
    case 'value':
      return input.value.t === 'Null' ? null : 'v' in input.value ? input.value.v : null;
    default:
      throw new Failure(appError('config', `${input.kind} values are not faked for documents`, { field: 'edit' }));
  }
}

/** One planned command: its text and what it does to the collection (the number of documents it matched). */
interface Planned {
  text: string;
  run(docs: MongoDoc[]): number;
}

const idOf = (row: { key: [string, Value][] }) => {
  const v = row.key.find(([c]) => c === '_id')?.[1];
  return v && 'v' in v ? v.v : null;
};

/** The command of `edit` on `collection`, whose documents are `docs` now (values follow their field types). */
function plan(collection: string, docs: MongoDoc[], edit: RowEdit): Planned {
  const q = (v: unknown) => JSON.stringify(v);
  if (edit.op === 'update') {
    const id = idOf(edit.row);
    const now = docs.find((x) => x._id === id);
    const set = edit.set.filter(([, i]) => i.kind !== 'unset').map(([c, i]) => [c, inputValue(i, now?.[c])] as const);
    const unset = edit.set.filter(([, i]) => i.kind === 'unset').map(([c]) => c);
    const parts = [
      ...(set.length ? [`"$set": { ${set.map(([c, v]) => `${q(c)}: ${q(v)}`).join(', ')} }`] : []),
      ...(unset.length ? [`"$unset": { ${unset.map((c) => `${q(c)}: ""`).join(', ')} }`] : []),
    ];
    return {
      text: `db.${collection}.updateOne({ "_id": ${q(id)} }, { ${parts.join(', ')} })`,
      run: (list) => {
        const d = list.find((x) => x._id === id);
        if (!d) return 0;
        for (const [c, v] of set) d[c] = v;
        for (const c of unset) delete d[c];
        return 1;
      },
    };
  }
  if (edit.op === 'insert') {
    const doc: MongoDoc = {};
    for (const [c, i] of edit.values) if (i.kind !== 'default' && i.kind !== 'unset') doc[c] = inputValue(i, undefined);
    return {
      text: `db.${collection}.insertOne(${q(doc)})`,
      run: (list) => {
        list.push(doc);
        return 1;
      },
    };
  }
  const id = idOf(edit.row);
  return {
    text: `db.${collection}.deleteOne({ "_id": ${q(id)} })`,
    run: (list) => {
      const at = list.findIndex((x) => x._id === id);
      if (at < 0) return 0;
      list.splice(at, 1);
      return 1;
    },
  };
}

/**
 * `table_data`, `table_row_count`, `data_editable`, `data_preview` and `data_apply` for the
 * collections of MongoDB connections. `connectionOf` finds a stored connection, `requireOpen`
 * throws `not_open`; `fallback` answers every other object (the SQL tables).
 */
export function createCommandData(
  state: CommandFakeState,
  connectionOf: (id: string) => ConnectionConfig | undefined,
  requireOpen: (id: string) => ServerInfo,
  fallback: Record<string, Handler>,
): Record<string, Handler> {
  const plans = new Map<string, { connectionId: string; obj: ObjectRef; commands: Planned[] }>();
  let seq = 0;
  /** The documents of `obj` when it is a collection of a MongoDB connection, else null (not ours). */
  const docsOf = (id: string, obj: ObjectRef): MongoDoc[] | null => {
    if (connectionOf(id)?.kind !== 'mongo' || obj.kind !== 'Collection') return null;
    requireOpen(id);
    return state.mongo.get(obj.database)?.get(obj.name) ?? [];
  };
  const or = (cmd: string, args: Record<string, unknown>) => {
    const handler = fallback[cmd];
    if (!handler) throw new Error(`e2e: ${cmd} is only faked for MongoDB collections`);
    return handler(args);
  };

  return {
    table_data: (args) => {
      const docs = docsOf(args.id as string, args.obj as ObjectRef);
      if (!docs) return or('table_data', args);
      const fields = fieldsOf(docs);
      const kept = sorted(docs.filter(matcher(args.filter as DataFilter | null)), (args.sort as SortSpec[]) ?? []);
      const offset = args.offset as number;
      const limit = args.limit as number;
      const rows = kept.slice(offset, offset + limit);
      const page: TableDataPage = {
        columns: fields.map((name) => ({
          name,
          db_type: name === '_id' ? 'objectId' : 'mixed',
          logical: logicalOf(docs.find((d) => d[name] !== undefined)?.[name]),
          nullable: name !== '_id',
          origin_table: null,
          origin_column: null,
        })),
        rows: rows.map((d) => fields.map((f) => toValue(d[f]))),
        has_more: offset + limit < kept.length,
        truncated_cells: 0,
        truncated: [],
        size_capped: false,
        key_order: ['_id'],
        row_versions: null,
        hidden_key: [],
        hidden_key_values: [],
        elapsed_ms: 1,
      };
      return page;
    },

    // As the driver: one column per sampled top-level field, no primary key, the `_id_` index.
    table_detail: (args) => {
      const obj = args.obj as ObjectRef;
      const docs = docsOf(args.id as string, obj);
      if (!docs) return or('table_detail', args);
      const base = or('table_detail', args) as TableDetail;
      const detail: TableDetail = {
        ...base,
        object: obj,
        columns: fieldsOf(docs).map((name, i) => ({
          ...base.columns[0]!,
          name,
          ordinal: i + 1,
          db_type: name === '_id' ? 'objectId' : 'mixed',
          nullable: name !== '_id',
          default: null,
          auto_increment: false,
          identity: null,
          comment: null,
        })),
        primary_key: null,
        indexes: [],
      };
      return detail;
    },

    table_row_count: (args) => {
      const docs = docsOf(args.id as string, args.obj as ObjectRef);
      if (!docs) return or('table_row_count', args);
      return { rows: docs.filter(matcher(args.filter as DataFilter | null)).length };
    },

    data_editable: (args) => {
      const docs = docsOf(args.id as string, args.obj as ObjectRef);
      if (!docs) return or('data_editable', args);
      const cfg = connectionOf(args.id as string)!;
      const info: EditableInfo = {
        editable: !cfg.read_only,
        reason: cfg.read_only ? 'read_only_connection' : null,
        key: ['_id'],
        hidden_key: [],
        editable_columns: fieldsOf(docs).filter((f) => f !== '_id'),
        atomic: requireOpen(cfg.id).capabilities.transactions ? 'yes' : 'no',
        protection: 'key',
      };
      return info;
    },

    // As `command_edit_preview`: one command per edit; Production refuses a non-atomic edit.
    data_preview: (args) => {
      const set = args.set as EditSet;
      const id = args.id as string;
      const docs = docsOf(id, set.object);
      if (!docs) return or('data_preview', args);
      if (set.edits.length === 0) throw new Failure(appError('config', 'there are no changes to apply', { field: 'edit' }));
      const cfg = connectionOf(id)!;
      const commands = set.edits.map((e) => plan(set.object.name, docs, e));
      const atomic = requireOpen(id).capabilities.transactions ? 'yes' : 'no';
      const blocked = cfg.read_only ? 'the connection is read-only' : cfg.env_tag === 'production' && atomic !== 'yes' ? NOT_ATOMIC_ON_PRODUCTION : null;
      const planId = `cmd-edit-${++seq}`;
      plans.set(planId, { connectionId: id, obj: set.object, commands });
      const preview: EditPreview = {
        plan_id: planId,
        statements: commands.map((c) => c.text),
        verdict: blocked ? 'block' : 'allow',
        atomic,
        protections: commands.map(() => ({ level: 'key_only' })),
        reason: blocked,
      };
      return preview;
    },

    // One command at a time: a command that matches no document stops the apply there.
    data_apply: (args) => {
      const stored = plans.get(args.planId as string);
      if (!stored) return or('data_apply', args);
      plans.delete(args.planId as string);
      const docs = docsOf(stored.connectionId, stored.obj) ?? [];
      const statements: ApplyReport['statements'] = [];
      for (const [index, c] of stored.commands.entries()) {
        const affected = c.run(docs);
        statements.push({ index, affected, error: null, conflict: affected === 0 });
        if (affected === 0) {
          return { outcome: { outcome: 'partially_applied', applied: index }, statements, inserted: [], error: null } satisfies ApplyReport;
        }
      }
      return { outcome: { outcome: 'committed' }, statements, inserted: [], error: null } satisfies ApplyReport;
    },
  };
}
