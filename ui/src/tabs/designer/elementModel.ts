import type { CheckDef, DraftError, ElementRef, FkDef, IndexDef, IndexPartDef, Locked, ServerInfo, TableDef } from '../../bindings';
import { deepEqual } from '../../store/tableDesigns';
import type { RowState } from './columnModel';

/** The element lists of a table the secondary grids edit. */
export type ElementKey = 'indexes' | 'foreign_keys' | 'checks';
export type ElementKind = 'index' | 'foreign_key' | 'check';
type Named = { original: string | null; name: string; locked: boolean };

export const KIND_OF: Record<ElementKey, ElementKind> = { indexes: 'index', foreign_keys: 'foreign_key', checks: 'check' };

/** One row of an element grid: a draft element, or an element of the opened table the draft drops. */
export interface ElementRow<T> {
  key: string;
  item: T;
  /** Index in the draft's list; null for a dropped element. */
  index: number | null;
  state: RowState;
  /** The opened table's element (matched by `original`). */
  base: T | null;
}

/** Draft elements in order, each dropped element of `base` placed after the one it followed. */
export function elementRows<T extends Named>(base: T[], draft: T[]): ElementRow<T>[] {
  const baseByName = new Map(base.map((b) => [b.name, b]));
  const rows: ElementRow<T>[] = draft.map((item, index) => {
    const b = item.original === null ? null : (baseByName.get(item.original) ?? null);
    const state: RowState = !b ? 'new' : deepEqual(item, b) ? 'unchanged' : 'modified';
    return { key: b ? `el:${item.original}` : `new:${index}`, item, index, state, base: b };
  });
  const kept = new Set(draft.map((c) => c.original).filter((o): o is string => o !== null));
  base.forEach((b, i) => {
    // An element not on the server (part of a new table's starter) has nothing to drop.
    if (b.original === null || kept.has(b.name)) return;
    let at = 0;
    for (let j = i - 1; j >= 0; j--) {
      const prev = base[j]!.name;
      const pos = rows.findIndex((r) => (r.state === 'drop' ? r.item.name : r.item.original) === prev);
      if (pos >= 0) {
        at = pos + 1;
        break;
      }
    }
    rows.splice(at, 0, { key: `drop:${b.name}`, item: b, index: null, state: 'drop', base: b });
  });
  return rows;
}

/** The server's reason a (locked) element is read-only. */
export const lockFor = (locked: Locked[], kind: ElementKind, el: Named): Locked | undefined =>
  el.locked ? locked.find((l) => deepEqual(l.element, { kind, name: el.original ?? el.name } satisfies ElementRef)) : undefined;

/** The draft errors of one element (matched by its current name). */
export const elementErrors = (errors: DraftError[], kind: ElementKind, name: string) =>
  errors.filter((e) => e.element.kind === kind && 'name' in e.element && e.element.name === name);

const replaceAt = <T,>(list: T[], index: number, value: T) => list.map((x, i) => (i === index ? value : x));

export function updateElement<K extends ElementKey>(d: TableDef, key: K, index: number, patch: Partial<TableDef[K][number]>): TableDef {
  const list = d[key] as Named[];
  const item = list[index];
  return item ? { ...d, [key]: replaceAt(list, index, { ...item, ...patch }) } : d;
}

export const removeElement = (d: TableDef, key: ElementKey, index: number): TableDef => ({ ...d, [key]: (d[key] as Named[]).filter((_, i) => i !== index) });

/** Puts a dropped element of `base` back at its old position (or the end). */
export function restoreElement(base: TableDef, d: TableDef, key: ElementKey, name: string): TableDef {
  const baseList = base[key] as Named[];
  const at = baseList.findIndex((c) => c.name === name);
  const item = baseList[at];
  const list = d[key] as Named[];
  if (!item || list.some((c) => c.original === name)) return d;
  const next = [...list];
  next.splice(Math.min(at, next.length), 0, item);
  return { ...d, [key]: next };
}

/** Index, constraint and key names share one namespace: a free `prefix_N`. */
function freeName(d: TableDef, prefix: string): string {
  const taken = new Set(
    [...d.indexes, ...d.foreign_keys, ...d.checks].map((e) => e.name.toLowerCase()).concat(d.primary_key?.name ? [d.primary_key.name.toLowerCase()] : []),
  );
  let n = 1;
  while (taken.has(`${prefix}_${n}`)) n++;
  return `${prefix}_${n}`;
}

export function addIndex(d: TableDef): TableDef {
  const first = d.columns[0]?.name ?? null;
  const index: IndexDef = {
    original: null,
    name: freeName(d, 'idx'),
    unique: false,
    parts: [{ column: first, expression: null, prefix: null, descending: false }],
    method: null,
    include: [],
    visible: true,
    comment: null,
    constraint: false,
    locked: false,
  };
  return { ...d, indexes: [...d.indexes, index] };
}

export function addForeignKey(d: TableDef): TableDef {
  const fk: FkDef = {
    original: null,
    name: freeName(d, 'fk'),
    columns: d.columns[0] ? [d.columns[0].name] : [],
    ref_schema: null,
    ref_table: '',
    ref_columns: [],
    on_update: 'NO ACTION',
    on_delete: 'NO ACTION',
    match_type: null,
    deferrable: false,
    initially_deferred: false,
    locked: false,
  };
  return { ...d, foreign_keys: [...d.foreign_keys, fk] };
}

export function addCheck(d: TableDef): TableDef {
  const check: CheckDef = { original: null, name: freeName(d, 'chk'), expression: '', locked: false };
  return { ...d, checks: [...d.checks, check] };
}

export const ADD: Record<ElementKey, (d: TableDef) => TableDef> = { indexes: addIndex, foreign_keys: addForeignKey, checks: addCheck };

export function updatePart(d: TableDef, index: number, part: number, patch: Partial<IndexPartDef>): TableDef {
  const idx = d.indexes[index];
  return idx ? updateElement(d, 'indexes', index, { parts: replaceAt(idx.parts, part, { ...idx.parts[part]!, ...patch }) }) : d;
}

/** A new part on the first column the index does not use yet. */
export function addPart(d: TableDef, index: number): TableDef {
  const idx = d.indexes[index];
  if (!idx) return d;
  const used = new Set(idx.parts.map((p) => p.column));
  const column = d.columns.find((c) => !used.has(c.name))?.name ?? d.columns[0]?.name ?? null;
  return updateElement(d, 'indexes', index, { parts: [...idx.parts, { column, expression: null, prefix: null, descending: false }] });
}

export function removePart(d: TableDef, index: number, part: number): TableDef {
  const idx = d.indexes[index];
  return idx && idx.parts.length > 1 ? updateElement(d, 'indexes', index, { parts: idx.parts.filter((_, i) => i !== part) }) : d;
}

/** A comma-separated list as names. */
export const parseList = (text: string) => text.split(',').map((s) => s.trim()).filter((s) => s !== '');

/** CHECK constraints need MySQL 8.0.16 (MariaDB and PostgreSQL always have them). */
export function checksUnsupported(server: ServerInfo): boolean {
  if (server.variant === 'MySql57') return true;
  const [major, minor, patch] = server.version;
  return server.variant === 'MySql8' && (major < 8 || (major === 8 && minor === 0 && patch < 16));
}

export const CHECKS_UNSUPPORTED = 'CHECK constraints need MySQL 8.0.16';
