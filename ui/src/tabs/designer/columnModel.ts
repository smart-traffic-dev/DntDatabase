import type { ColumnDef, DraftError, ElementRef, Locked, TableDef, TypeInfo, TypeSpec } from '../../bindings';
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

/** Draft columns in order, each dropped column of `base` placed after the column it followed. */
export function columnRows(base: TableDef, draft: TableDef): ColumnRow[] {
  const baseByName = new Map(base.columns.map((c) => [c.name, c]));
  let fresh = 0;
  const rows: ColumnRow[] = draft.columns.map((column, index) => {
    const b = column.original === null ? null : (baseByName.get(column.original) ?? null);
    const state: RowState = !b ? 'new' : deepEqual(column, b) ? 'unchanged' : 'modified';
    return { key: column.original !== null && b ? `col:${column.original}` : `new:${fresh++}`, column, index, state, base: b };
  });
  const kept = new Set(draft.columns.map((c) => c.original).filter((o): o is string => o !== null));
  base.columns.forEach((b, i) => {
    if (kept.has(b.name)) return;
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

/** The parameters as the grid's Length cell shows them: `16`, `14,2`, `'a','b'`. */
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

/** The type with its parameters taken from the Length cell's text (`16` or `14,2`). */
export function withParamsText(spec: TypeSpec, info: TypeInfo | undefined, text: string): TypeSpec {
  if (spec.kind === 'custom' || !info) return spec;
  if (info.precision !== 'None') {
    const [p, s] = text.split(',');
    return { ...spec, precision: toInt(p), scale: info.scale !== 'None' ? toInt(s) : null };
  }
  if (info.length !== 'None') return { ...spec, length: toInt(text) };
  return spec;
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

export const baseText = (spec: TypeSpec) => (spec.kind === 'known' ? spec.base : spec.text);

export const columnErrors = (errors: DraftError[], name: string) =>
  errors.filter((e) => e.element.kind === 'column' && e.element.name === name);

const sameRef = (a: ElementRef, b: ElementRef) => deepEqual(a, b);

export const lockOf = (locked: Locked[], column: ColumnDef): Locked | undefined =>
  column.locked ? locked.find((l) => sameRef(l.element, { kind: 'column', name: column.original ?? column.name })) : undefined;

const replaceAt = <T,>(list: T[], index: number, value: T) => list.map((x, i) => (i === index ? value : x));

export function updateColumn(d: TableDef, index: number, patch: Partial<ColumnDef>): TableDef {
  const c = d.columns[index];
  return c ? { ...d, columns: replaceAt(d.columns, index, { ...c, ...patch }) } : d;
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
    columns: replaceAt(d.columns, index, { ...c, name }),
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

/** A new nullable column at the end, named `column_N`, of the catalog's integer type. */
export function addColumn(d: TableDef, catalog: TypeInfo[]): TableDef {
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
  return { ...d, columns: [...d.columns, column] };
}

/** Adds the column to the primary key, or takes it out (an empty key is no key). */
export function togglePrimaryKey(d: TableDef, name: string): TableDef {
  const pk = d.primary_key;
  if (pk?.locked) return d;
  if (!pk) return { ...d, primary_key: { original: null, name: null, columns: [name], locked: false } };
  const columns = pk.columns.includes(name) ? pk.columns.filter((c) => c !== name) : [...pk.columns, name];
  return { ...d, primary_key: columns.length === 0 ? null : { ...pk, columns } };
}
