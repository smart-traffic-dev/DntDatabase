import type {
  ColumnPlan,
  ExistingRows,
  Generator,
  ObjectRef,
  SeedColumn,
  SeedPreview,
  SeedRequest,
  SeedSample,
  SeedTarget,
} from '../../bindings';
import type { GeneratorKind } from './seedFormat';

/**
 * The data seeder dialog's state (spec §6.2): a pure reducer, so what each change invalidates is
 * tested without React. The generators' rules and every check stay in the backend
 * (`seed_preview`, `seed_prepare`); this only collects choices.
 */

export const DEFAULT_ROWS = 1000;
export const DEFAULT_BATCH_ROWS = 1000;
export const MAX_ROWS = 1_000_000;

export type SeedStep = 'configure' | 'review';

export interface SeedState {
  connectionId: string;
  table: ObjectRef;
  target: SeedTarget | null;
  rows: number;
  batchRows: number;
  existing: ExistingRows;
  seed: number;
  /** One per column of the table, in table order. */
  plans: ColumnPlan[];
  step: SeedStep;
  sample: SeedSample | null;
  /** The reviewed plan (`seed_prepare`); any later change drops it. */
  preview: SeedPreview | null;
  /** The stored request of "Run again", applied when the target loads. */
  params: SeedRequest | null;
}

export type SeedAction =
  | { type: 'targetLoaded'; target: SeedTarget }
  | { type: 'setRows'; rows: number }
  | { type: 'setBatch'; batchRows: number }
  | { type: 'setExisting'; existing: ExistingRows }
  | { type: 'setSeed'; seed: number }
  | { type: 'setGenerator'; column: string; generator: Generator }
  | { type: 'setPlan'; column: string; patch: Partial<ColumnPlan> }
  | { type: 'sampled'; sample: SeedSample }
  | { type: 'prepared'; preview: SeedPreview }
  | { type: 'back' };

export function initialState(init: { connectionId: string; table: ObjectRef; params?: SeedRequest | null }): SeedState {
  const p = init.params ?? null;
  return {
    connectionId: init.connectionId,
    table: init.table,
    target: null,
    rows: p?.rows ?? DEFAULT_ROWS,
    batchRows: p?.batch_rows ?? DEFAULT_BATCH_ROWS,
    existing: p?.existing ?? 'keep',
    seed: p?.seed ?? 0,
    plans: [],
    step: 'configure',
    sample: null,
    preview: null,
    params: p,
  };
}

/** A change of the request: the reviewed plan no longer applies. */
const edited = (s: SeedState, patch: Partial<SeedState>): SeedState => ({ ...s, ...patch, preview: null, step: 'configure' });

const sameColumns = (plans: ColumnPlan[], target: SeedTarget) =>
  plans.length === target.columns.length && plans.every((p, i) => p.name === target.columns[i]?.column.name);

export function reduce(s: SeedState, a: SeedAction): SeedState {
  switch (a.type) {
    case 'targetLoaded': {
      const stored = s.params && sameColumns(s.params.columns, a.target) ? s.params.columns : null;
      return { ...s, target: a.target, plans: stored ?? a.target.columns.map((c) => c.plan), seed: s.params?.seed ?? a.target.seed };
    }
    case 'setRows':
      return edited(s, { rows: a.rows });
    case 'setBatch':
      return edited(s, { batchRows: a.batchRows });
    case 'setExisting':
      return edited(s, { existing: a.existing });
    case 'setSeed':
      return edited(s, { seed: a.seed });
    case 'setGenerator':
      return edited(s, { plans: s.plans.map((p) => (p.name === a.column ? { ...p, generator: a.generator } : p)) });
    case 'setPlan':
      return edited(s, { plans: s.plans.map((p) => (p.name === a.column ? { ...p, ...a.patch } : p)) });
    case 'sampled':
      return { ...s, sample: a.sample };
    case 'prepared':
      return { ...s, preview: a.preview, step: 'review' };
    case 'back':
      return { ...s, preview: null, step: 'configure' };
  }
}

/** What `seed_preview` / `seed_prepare` get; `null` until the table is read. */
export function buildRequest(s: SeedState): SeedRequest | null {
  if (!s.target) return null;
  return {
    connection_id: s.connectionId,
    table: s.table,
    rows: s.rows,
    batch_rows: s.batchRows,
    existing: s.existing,
    seed: s.seed,
    columns: s.plans,
  };
}

const iso = (d: Date) => d.toISOString().slice(0, 10);

/** The scale of `decimal(p,s)` / `numeric(p,s)` (2 otherwise). */
function scaleOf(dbType: string): number {
  const m = /\(\s*\d+\s*,\s*(\d+)\s*\)/.exec(dbType);
  return m ? Math.min(Number(m[1]), 12) : 2;
}

/**
 * The generator a column gets when the user picks `kind`: the inferred one when it is of that
 * kind, else sensible defaults (`today` anchors the date ranges).
 */
export function defaultGenerator(kind: GeneratorKind, col: SeedColumn, today: Date = new Date()): Generator {
  if (col.plan.generator.kind === kind) return col.plan.generator;
  const from = new Date(Date.UTC(today.getUTCFullYear() - 2, today.getUTCMonth(), today.getUTCDate()));
  const dbType = col.column.db_type.toLowerCase();
  switch (kind) {
    case 'skip':
      return { kind };
    case 'sequence':
      return { kind, start: 1, step: 1 };
    case 'int_range':
      return { kind, min: 1, max: dbType.startsWith('tinyint') ? 100 : 1000 };
    case 'decimal_range':
      return { kind, min: 0, max: 1000, scale: scaleOf(dbType) };
    case 'bool':
      return { kind, true_percent: 50 };
    case 'date_range':
      return { kind, from: iso(from), to: iso(today) };
    case 'date_time_range':
      return { kind, from: `${iso(from)} 00:00:00`, to: `${iso(today)} 23:59:59` };
    case 'time_range':
      return { kind, from: '00:00:00', to: '23:59:59' };
    case 'pick':
      return { kind, values: col.column.enum_values.length ? col.column.enum_values : col.check_values };
    case 'foreign_key':
      return { kind, constraint: col.foreign_key?.constraint ?? '' };
    case 'fake':
      return { kind, fake: 'word' };
    case 'uuid':
      return { kind };
    case 'constant':
      return { kind, value: '' };
    case 'pattern':
      return { kind, pattern: '???-#####' };
    case 'json_object':
      return { kind };
    case 'random_bytes':
      return { kind, len: 16 };
  }
}
