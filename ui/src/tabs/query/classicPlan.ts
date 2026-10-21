// The classic tabular EXPLAIN of MySQL / MariaDB (the Explain panel's Grid), read in plain words.
// Pure functions; spec: docs/superpowers/specs/2026-10-21-explain-panel-redesign-design.md.
import type { PlanTable, PlanTree } from '../../bindings';

/** The server's columns and rows; a SQL NULL is null. */
export type ClassicTable = PlanTable;

/** Access types from best to worst, as the panel draws the ladder. */
export const ACCESS_LADDER = ['system', 'const', 'eq_ref', 'ref', 'range', 'index', 'ALL'] as const;

/**
 * The classic table of a plan: a MySQL 8 / MariaDB tree plan's `classic`, or a MySQL 5.7 table
 * plan (whose cells are text, `NULL` for a SQL NULL). null when there is none (PostgreSQL, or the
 * classic explain failed).
 */
export function classicOf(plan: PlanTree | null): ClassicTable | null {
  if (plan === null) return null;
  if (plan.kind === 'table') {
    return { columns: plan.columns, rows: plan.rows.map((r) => r.map((v) => (v === 'NULL' ? null : v))) };
  }
  return plan.classic ?? null;
}

/** The value of `column` (case-insensitive) in row `index`; null when missing. */
export function cellOf(t: ClassicTable, index: number, column: string): string | null {
  const at = t.columns.findIndex((c) => c.toLowerCase() === column.toLowerCase());
  return at < 0 ? null : (t.rows[index]?.[at] ?? null);
}

const num = (v: string | null): number | null => {
  if (v === null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
};

const fmt = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });

/** good: one row per lookup or better; slow: a full table or index scan. */
export function accessTone(type: string | null): 'good' | 'slow' | null {
  if (type === null) return null;
  if (type === 'system' || type === 'const' || type === 'eq_ref') return 'good';
  if (type === 'ALL' || type === 'index') return 'slow';
  return null;
}

/** One sentence on how the table is read. */
function accessText(type: string | null, rows: number | null): string {
  const about = rows === null ? '' : `, about ${fmt.format(rows)} rows`;
  switch (type) {
    case null:
      return 'No table is read.';
    case 'system':
      return 'The table has one row: it is read once.';
    case 'const':
      return 'At most one matching row, read once at the start (a primary or unique key equals a constant).';
    case 'eq_ref':
      return 'One row per row of the previous table, through a unique index.';
    case 'ref':
      return `Rows matching a key value, through a non-unique index${rows === null ? '' : `, about ${fmt.format(rows)} rows per lookup`}.`;
    case 'fulltext':
      return 'A FULLTEXT index serves the match.';
    case 'ref_or_null':
      return 'Like ref, plus a second lookup for NULL.';
    case 'index_merge':
      return 'Several indexes are read and their results merged.';
    case 'unique_subquery':
    case 'index_subquery':
      return 'A subquery is answered through an index.';
    case 'range':
      return `Only a range of an index is read${about}.`;
    case 'index':
      return `Full index scan: every entry of the index is read${about}.`;
    case 'ALL':
      return `Full table scan. MySQL reads every row of the table${about}.`;
    default:
      return `Access type ${type}.`;
  }
}

/** One sentence on the index the access uses. */
function indexText(possible: string | null, key: string | null, keyLen: string | null, ref: string | null): string {
  if (key !== null) {
    const len = keyLen === null ? '' : ` (${keyLen} bytes of the key)`;
    const on = ref === null ? '' : `, matched on ${ref}`;
    return `Uses ${key}${len}${on}.`;
  }
  if (possible !== null) return `An index exists (${possible}) but the optimizer did not use it.`;
  return 'No index can serve this access.';
}

export interface ExtraItem {
  text: string;
  note: string | null;
  /** The phrase costs time (tinted). */
  slow: boolean;
}

const EXTRA: [RegExp, string, boolean][] = [
  [/^using where$/i, 'Rows are checked against the WHERE after they are read.', false],
  [/^using index$/i, 'Answered from the index alone, without reading the table rows (covering index).', false],
  [/^using index condition/i, 'Part of the WHERE is checked inside the index before rows are read.', false],
  [/^using temporary/i, 'A temporary table holds intermediate rows.', true],
  [/^using filesort/i, 'The ORDER BY sorts rows without an index.', true],
  [/^using join buffer/i, 'Rows of the earlier tables are kept in a join buffer: no index joins this table.', true],
  [/^range checked for each record/i, 'No good index up front: the index choice is checked again for every row.', true],
  [/^using mrr/i, 'Rows are read in key order to cut random disk reads.', false],
  [/^impossible where/i, 'The WHERE can never be true: no rows are read.', false],
  [/^select tables optimized away/i, 'Answered from index data without reading rows.', false],
  [/^no tables used/i, 'The statement reads no table.', false],
  [/^distinct/i, 'Stops at the first matching row per combination.', false],
  [/^not exists/i, 'The LEFT JOIN stops after the first match.', false],
  [/^backward index scan/i, 'The index is read in reverse order.', false],
  [/^using index for group-by/i, 'GROUP BY is resolved by reading the index.', false],
  [/^using (union|intersect|sort_union)/i, 'Index merge: several indexes are combined.', false],
  [/^zero limit/i, 'LIMIT 0: no rows are read.', false],
  [/^(start temporary|end temporary|firstmatch|loosescan)/i, 'A semi-join strategy for IN / EXISTS.', false],
];

/** The phrases of an Extra cell (`;`-separated), each with what it means. */
export function extraItems(extra: string | null): ExtraItem[] {
  if (extra === null) return [];
  return extra
    .split(/;\s*/)
    .map((t) => t.trim())
    .filter((t) => t.length > 0)
    .map((text) => {
      const hit = EXTRA.find(([re]) => re.test(text));
      return { text, note: hit?.[1] ?? null, slow: hit?.[2] ?? false };
    });
}

/** Rows that pass the table's condition: rows × filtered %. */
export function passRows(rows: string | null, filtered: string | null): number | null {
  const r = num(rows);
  if (r === null) return null;
  return Math.round((r * (num(filtered) ?? 100)) / 100);
}

export interface RowReading {
  table: string | null;
  type: string | null;
  access: string;
  index: string;
  possibleKeys: string | null;
  key: string | null;
  examined: number | null;
  filtered: string | null;
  passing: number | null;
  extra: ExtraItem[];
}

/** Row `index` of the classic table, in plain words (the inspector). */
export function explainRow(t: ClassicTable, index: number): RowReading {
  const cell = (c: string) => cellOf(t, index, c);
  const type = cell('type');
  const examined = num(cell('rows'));
  const possibleKeys = cell('possible_keys');
  const key = cell('key');
  return {
    table: cell('table'),
    type,
    access: accessText(type, examined),
    index: indexText(possibleKeys, key, cell('key_len'), cell('ref')),
    possibleKeys,
    key,
    examined,
    filtered: cell('filtered'),
    passing: passRows(cell('rows'), cell('filtered')),
    extra: extraItems(cell('Extra')),
  };
}

export interface ClassicSummary {
  tables: number;
  /** Rows the first table reads (the outer loop of the join). */
  firstRows: number | null;
  fullScans: number;
  /** Slow Extra phrases of the whole plan, first seen first. */
  slowExtra: string[];
}

/** The chips above the grid. */
export function classicSummary(t: ClassicTable): ClassicSummary {
  let tables = 0;
  let fullScans = 0;
  const slow: string[] = [];
  t.rows.forEach((_, i) => {
    if (cellOf(t, i, 'table') !== null) tables += 1;
    if (cellOf(t, i, 'type') === 'ALL') fullScans += 1;
    for (const item of extraItems(cellOf(t, i, 'Extra'))) {
      const name = item.text.replace(/\s*\(.*\)$/, '');
      if (item.slow && !slow.includes(name)) slow.push(name);
    }
  });
  return { tables, firstRows: t.rows.length === 0 ? null : num(cellOf(t, 0, 'rows')), fullScans, slowExtra: slow };
}
