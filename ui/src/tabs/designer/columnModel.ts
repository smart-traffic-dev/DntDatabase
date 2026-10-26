import type { ColumnDef, DefaultSpec, DraftError, ElementRef, Locked, TableDef, TypeInfo, TypeSpec } from '../../bindings';
import { deepEqual } from '../../store/tableDesigns';

export type RowState = 'unchanged' | 'modified' | 'new' | 'drop';

/** One row of the Columns grid: a draft column, or a column of the opened table the draft drops. */
export interface ColumnRow {
  key: string;
  column: ColumnDef;
  /** Index in `draft.columns`; null for a dropped column. */
  index: number | null;
  state: RowState;
  /** The opened table's column (matched by `original`). */
  base: ColumnDef | null;
}

/**
 * Row keys of new columns (they have no `original`): given when first seen and carried to the
 * column's next versions by the edits below, so a new row keeps its key through reorders and edits.
 */
const newKeys = new WeakMap<ColumnDef, string>();
let newKeyCount = 0;

function newKeyOf(c: ColumnDef): string {
  let key = newKeys.get(c);
  if (key === undefined) {
    key = `new:${++newKeyCount}`;
    newKeys.set(c, key);
  }
  return key;
}

/** `next` is a new version of `prev`: it keeps the row key. */
function carry(prev: ColumnDef, next: ColumnDef): ColumnDef {
  const key = newKeys.get(prev);
  if (key !== undefined) newKeys.set(next, key);
  return next;
}

/** A stable key for a draft column's row. */
export const columnKey = (c: ColumnDef, inBase: boolean) => (c.original !== null && inBase ? `col:${c.original}` : newKeyOf(c));

/** Draft columns in order, each dropped column of `base` placed after the column it followed. */
export function columnRows(base: TableDef, draft: TableDef): ColumnRow[] {
  const baseByName = new Map(base.columns.map((c) => [c.name, c]));
  const rows: ColumnRow[] = draft.columns.map((column, index) => {
    const b = column.original === null ? null : (baseByName.get(column.original) ?? null);
    const state: RowState = !b ? 'new' : deepEqual(column, b) ? 'unchanged' : 'modified';
    return { key: columnKey(column, !!b), column, index, state, base: b };
  });
  const kept = new Set(draft.columns.map((c) => c.original).filter((o): o is string => o !== null));
  base.columns.forEach((b, i) => {
    // A column not on the server (a new table's starter `id`) has nothing to drop.
    if (b.original === null || kept.has(b.name)) return;
    // After the nearest earlier column that is still there (or a dropped one placed already).
    let at = 0;
    for (let j = i - 1; j >= 0; j--) {
      const prev = base.columns[j]!.name;
      const pos = rows.findIndex((r) => (r.state === 'drop' ? r.column.name : r.column.original) === prev);
      if (pos >= 0) {
        at = pos + 1;
        break;
      }
    }
    rows.splice(at, 0, { key: `drop:${b.name}`, column: b, index: null, state: 'drop', base: b });
  });
  return rows;
}

export const typeInfoOf = (catalog: TypeInfo[], spec: TypeSpec): TypeInfo | undefined =>
  spec.kind === 'known' ? catalog.find((t) => t.base === spec.base) : undefined;

const quoteValue = (v: string) => `'${v.replaceAll("'", "''")}'`;

/** The parameters as one text: `16`, `14,2`, `'a','b'`. */
export function paramsText(spec: TypeSpec): string {
  if (spec.kind === 'custom') return '';
  if (spec.values.length > 0) return spec.values.map(quoteValue).join(',');
  if (spec.precision !== null) return spec.scale !== null ? `${spec.precision},${spec.scale}` : String(spec.precision);
  return spec.length !== null ? String(spec.length) : '';
}

/** A type as one line: `decimal(14,2) unsigned`, `enum('a','b')`, or the custom text. */
export function typeText(spec: TypeSpec): string {
  if (spec.kind === 'custom') return spec.text;
  const params = paramsText(spec);
  return `${spec.base}${params ? `(${params})` : ''}${spec.unsigned ? ' unsigned' : ''}`;
}

export const sameType = (a: TypeSpec, b: TypeSpec) => deepEqual(a, b);

const toInt = (s: string | undefined): number | null => {
  const t = (s ?? '').trim();
  return /^\d+$/.test(t) ? Number(t) : null;
};

/** The grid's Length cell: the length, the precision (`decimal(14,2)` -> `14`) or the values. */
export function lengthText(spec: TypeSpec): string {
  if (spec.kind === 'custom') return '';
  if (spec.values.length > 0) return spec.values.map(quoteValue).join(',');
  if (spec.precision !== null) return String(spec.precision);
  return spec.length !== null ? String(spec.length) : '';
}

/** The grid's Decimals cell: the scale (`decimal(14,2)` -> `2`). */
export const scaleText = (spec: TypeSpec) => (spec.kind === 'known' && spec.scale !== null ? String(spec.scale) : '');

/** The type with the Length cell's number as its precision (types with one) or its length. */
export function withLengthText(spec: TypeSpec, info: TypeInfo | undefined, text: string): TypeSpec {
  if (spec.kind === 'custom' || !info) return spec;
  if (info.precision !== 'None') {
    // `14,2` typed out in full still sets both; no precision leaves no scale behind.
    const [p, s] = text.split(',');
    const precision = toInt(p);
    const scale = info.scale === 'None' || precision === null ? null : s === undefined ? spec.scale : toInt(s);
    return { ...spec, precision, scale };
  }
  if (info.length !== 'None') return { ...spec, length: toInt(text) };
  return spec;
}

/** The type with the Decimals cell's number as its scale. */
export function withScaleText(spec: TypeSpec, info: TypeInfo | undefined, text: string): TypeSpec {
  if (spec.kind === 'custom' || !info || info.scale === 'None') return spec;
  return { ...spec, scale: toInt(text) };
}

/** A number field's text as a parameter value. */
export const parseParam = (text: string) => toInt(text);

/**
 * The type for what the user typed in a Type field: a catalog name keeps the parameters the
 * new type takes; anything else is a custom type.
 */
export function withBase(spec: TypeSpec, text: string, catalog: TypeInfo[]): TypeSpec {
  const info = catalog.find((t) => t.base === text.trim().toLowerCase());
  if (!info) return { kind: 'custom', text };
  const prev = spec.kind === 'known' ? spec : null;
  return {
    kind: 'known',
    base: info.base,
    length: info.length !== 'None' ? (prev?.length ?? null) : null,
    precision: info.precision !== 'None' ? (prev?.precision ?? null) : null,
    scale: info.scale !== 'None' ? (prev?.scale ?? null) : null,
    unsigned: info.unsigned && !!prev?.unsigned,
    values: info.values ? (prev?.values ?? []) : [],
  };
}

export type Engine = 'mysql' | 'postgres';

const INTEGERS: Record<Engine, string[]> = {
  mysql: ['tinyint', 'smallint', 'mediumint', 'int', 'bigint'],
  postgres: ['smallint', 'integer', 'bigint'],
};

/** An integer type: MySQL AUTO_INCREMENT and PostgreSQL identity apply only to these. */
export const isInteger = (spec: TypeSpec, engine: Engine) => spec.kind === 'known' && INTEGERS[engine].includes(spec.base);

/** MySQL `ON UPDATE CURRENT_TIMESTAMP` applies to `timestamp` and `datetime` only. */
export const takesOnUpdate = (spec: TypeSpec, engine: Engine) =>
  engine === 'mysql' && spec.kind === 'known' && (spec.base === 'timestamp' || spec.base === 'datetime');

/** `CURRENT_TIMESTAMP`, with the type's fractional-seconds precision when it has one. */
export const onUpdateText = (spec: TypeSpec) =>
  spec.kind === 'known' && spec.precision !== null ? `CURRENT_TIMESTAMP(${spec.precision})` : 'CURRENT_TIMESTAMP';

const precisionOf = (spec: TypeSpec) => (spec.kind === 'known' ? spec.precision : null);

/**
 * The fields of `c` once its type is `spec`: whatever the new type does not take is cleared
 * (collation and charset, AUTO_INCREMENT / identity, ON UPDATE, values), and `using` goes when the
 * type is back to the opened table's (`base`).
 */
export function withType(c: ColumnDef, spec: TypeSpec, catalog: TypeInfo[], engine: Engine, base: ColumnDef | null | undefined): Partial<ColumnDef> {
  const info = typeInfoOf(catalog, spec);
  const textual = spec.kind === 'custom' || !!info?.collation;
  const keepsOnUpdate = takesOnUpdate(spec, engine) && c.on_update !== null;
  return {
    data_type: spec,
    collation: textual ? c.collation : null,
    charset: textual && engine === 'mysql' ? c.charset : null,
    auto_increment: engine === 'mysql' && isInteger(spec, engine) && c.auto_increment,
    identity: engine === 'postgres' && isInteger(spec, engine) ? c.identity : null,
    // ON UPDATE follows a change of the fractional-seconds precision.
    on_update: !keepsOnUpdate ? null : precisionOf(spec) === precisionOf(c.data_type) ? c.on_update : onUpdateText(spec),
    using: engine === 'postgres' && base && !sameType(base.data_type, spec) ? c.using : null,
  };
}

/** SQL keywords that are values the server computes (they take no parentheses, or optional ones). */
const SQL_EXPRESSION_KEYWORDS = /^(current_timestamp|current_date|current_time|current_user|session_user|localtime|localtimestamp)(\s*\(\s*\d*\s*\))?$/i;
/**
 * A function call (`now()`, `uuid()`, `nextval('s')`, the name right before its parenthesis, so a
 * phrase like `Untitled (1)` stays text) or a parenthesised expression.
 */
const SQL_CALL = /^([a-z_][a-z0-9_.]*\(.*\)|\(.*\))$/is;

/** Text typed as a default that is an expression, not a literal (`CURRENT_TIMESTAMP`, `now()`, `(a + 1)`). */
export const looksLikeExpression = (text: string) => SQL_EXPRESSION_KEYWORDS.test(text.trim()) || SQL_CALL.test(text.trim());

/**
 * The default for text typed in a Default field: empty or `NULL` is no default (a nullable column
 * defaults to NULL), an expression-like text is an expression, anything else a value of `kind`
 * (an `expression` stays one: `ARRAY['a']::text[]` does not look like one but is).
 */
export function defaultFromText(text: string, kind: DefaultSpec['kind'] = 'value'): DefaultSpec | null {
  const t = text.trim();
  if (t === '' || /^null$/i.test(t)) return null;
  return { kind: kind === 'value' && looksLikeExpression(t) ? 'expression' : kind, text };
}

export const baseText =(spec: TypeSpec) => (spec.kind === 'known' ? spec.base : spec.text);

export const columnErrors = (errors: DraftError[], name: string) =>
  errors.filter((e) => e.element.kind === 'column' && e.element.name === name);

const sameRef = (a: ElementRef, b: ElementRef) => deepEqual(a, b);

export const lockOf = (locked: Locked[], column: ColumnDef): Locked | undefined =>
  column.locked ? locked.find((l) => sameRef(l.element, { kind: 'column', name: column.original ?? column.name })) : undefined;

const replaceAt = <T,>(list: T[], index: number, value: T) => list.map((x, i) => (i === index ? value : x));

export function updateColumn(d: TableDef, index: number, patch: Partial<ColumnDef>): TableDef {
  const c = d.columns[index];
  return c ? { ...d, columns: replaceAt(d.columns, index, carry(c, { ...c, ...patch })) } : d;
}

/** Renames a column and the references to it in the (unlocked) primary key, indexes and foreign keys. */
export function renameColumn(d: TableDef, index: number, name: string): TableDef {
  const c = d.columns[index];
  if (!c || c.name === name) return d;
  const from = c.name;
  const swap = (n: string) => (n === from ? name : n);
  const pk = d.primary_key;
  return {
    ...d,
    columns: replaceAt(d.columns, index, carry(c, { ...c, name })),
    primary_key: pk && !pk.locked && pk.columns.includes(from) ? { ...pk, columns: pk.columns.map(swap) } : pk,
    indexes: d.indexes.map((i) =>
      i.locked || !i.parts.some((p) => p.column === from) ? i : { ...i, parts: i.parts.map((p) => (p.column === from ? { ...p, column: name } : p)) },
    ),
    foreign_keys: d.foreign_keys.map((f) => (f.locked || !f.columns.includes(from) ? f : { ...f, columns: f.columns.map(swap) })),
  };
}

export function moveColumn(d: TableDef, index: number, to: number): TableDef {
  if (to < 0 || to >= d.columns.length || to === index || !d.columns[index]) return d;
  const columns = [...d.columns];
  const [c] = columns.splice(index, 1);
  columns.splice(to, 0, c!);
  return { ...d, columns };
}

export const dropColumn = (d: TableDef, index: number): TableDef => ({ ...d, columns: d.columns.filter((_, i) => i !== index) });

/** Puts a dropped column of `base` back, after the nearest earlier column still in the draft. */
export function restoreColumn(base: TableDef, d: TableDef, name: string): TableDef {
  const at = base.columns.findIndex((c) => c.name === name);
  const column = base.columns[at];
  if (!column || d.columns.some((c) => c.original === name)) return d;
  let pos = 0;
  for (let j = at - 1; j >= 0; j--) {
    const found = d.columns.findIndex((c) => c.original === base.columns[j]!.name);
    if (found >= 0) {
      pos = found + 1;
      break;
    }
  }
  const columns = [...d.columns];
  columns.splice(pos, 0, column);
  return { ...d, columns };
}

/** A new nullable column named `column_N`, of the catalog's integer type: at `at`, or at the end. */
export function addColumn(d: TableDef, catalog: TypeInfo[], at = d.columns.length): TableDef {
  const taken = new Set(d.columns.map((c) => c.name.toLowerCase()));
  let n = d.columns.length + 1;
  while (taken.has(`column_${n}`)) n++;
  const base = ['int', 'integer'].find((b) => catalog.some((t) => t.base === b)) ?? catalog[0]?.base ?? 'int';
  const column: ColumnDef = {
    original: null,
    name: `column_${n}`,
    data_type: { kind: 'known', base, length: null, precision: null, scale: null, unsigned: false, values: [] },
    nullable: true,
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
  };
  const columns = [...d.columns];
  columns.splice(Math.max(0, Math.min(at, columns.length)), 0, column);
  return { ...d, columns };
}

/** Adds the column to the primary key, or takes it out (an empty key is no key). */
export function togglePrimaryKey(d: TableDef, name: string): TableDef {
  const pk = d.primary_key;
  if (pk?.locked) return d;
  if (!pk) return { ...d, primary_key: { original: null, name: null, columns: [name], locked: false } };
  const columns = pk.columns.includes(name) ? pk.columns.filter((c) => c !== name) : [...pk.columns, name];
  return { ...d, primary_key: columns.length === 0 ? null : { ...pk, columns } };
}
