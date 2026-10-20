// Pure formatting of copied grid cells: TSV (pastes into Excel), CSV and INSERT statements.
import type { ObjectRef, Value } from '../../bindings';
import { quoteIdent } from '../../components/newQuery';
import { cellView } from './format';

export type Engine = 'mysql' | 'postgres';

/** The most bytes a cell can carry; the backend cuts longer text to this (data views). */
export const CELL_CAP_BYTES = 64 * 1024;

/** Used when no table is known. */
export const TABLE_PLACEHOLDER = 'table_name';

/** The text of a value as copied: NULL is empty, JSON and text are verbatim (no sanitising). */
export function plainText(v: Value): string {
  switch (v.t) {
    case 'Null':
      return '';
    case 'Json':
      return v.v;
    default:
      return cellView(v).text;
  }
}

const utf8Length = (s: string) => new TextEncoder().encode(s).length;

/**
 * Whether `v` may be a cell the backend cut at 64 KB. Cut cells carry no mark of their own (the page
 * only counts them), so with `capped` (the page reports cut cells) any text whose UTF-8 size sits
 * at the cap, within a character's width below it, is suspect.
 */
export function maybeTruncated(v: Value, capped: boolean): boolean {
  if (!capped) return false;
  switch (v.t) {
    case 'Text':
    case 'Json':
      return v.v.length >= CELL_CAP_BYTES / 4 && atCap(v.v);
    case 'Other':
      return v.v.text.length >= CELL_CAP_BYTES / 4 && atCap(v.v.text);
    case 'Array':
      return v.v.some((x) => maybeTruncated(x, capped));
    default:
      return false;
  }
}

function atCap(s: string): boolean {
  const n = utf8Length(s);
  return n <= CELL_CAP_BYTES && n > CELL_CAP_BYTES - 4;
}

/** A TSV field: quoted Excel-style when it holds a tab, line break or quote. */
export function tsvField(text: string): string {
  return /[\t\r\n"]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

/** A CSV field: quoted when it holds a comma, line break or quote. */
export function csvField(text: string): string {
  return /[,\r\n"]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

function delimited(rows: Value[][], header: string[] | null, sep: string, field: (t: string) => string): string {
  const lines: string[] = [];
  if (header) lines.push(header.map(field).join(sep));
  for (const row of rows) lines.push(row.map((v) => field(plainText(v))).join(sep));
  return lines.join('\r\n');
}

/** Tab-separated, CRLF between rows, NULL as empty; `header` (column names) goes first when given. */
export const toTsv = (rows: Value[][], header: string[] | null = null) => delimited(rows, header, '\t', tsvField);

/** Comma-separated with the same rules as `toTsv`. */
export const toCsv = (rows: Value[][], header: string[] | null = null) => delimited(rows, header, ',', csvField);

/** `ref` as a quoted, qualified name for `engine` (PostgreSQL by schema, MySQL by database). */
export function qualifiedTable(ref: ObjectRef, engine: Engine): string {
  const qualifier = engine === 'postgres' ? ref.schema : ref.database;
  const name = quoteIdent(engine, ref.name);
  return qualifier ? `${quoteIdent(engine, qualifier)}.${name}` : name;
}

/**
 * The one table every column of a query result comes from, or null (no column has an origin, or
 * they differ).
 */
export function singleSourceTable(columns: { origin_table: ObjectRef | null }[]): ObjectRef | null {
  let found: ObjectRef | null = null;
  for (const c of columns) {
    const t = c.origin_table;
    if (!t) continue;
    if (found && (found.name !== t.name || found.schema !== t.schema || found.database !== t.database)) return null;
    found = t;
  }
  return found;
}

export type Literal = { ok: true; sql: string } | { ok: false; reason: string };

const INTEGER = /^-?\d+$/;
const NUMBER = /^-?\d+(\.\d+)?([eE][+-]?\d+)?$/;

function quoteString(s: string, engine: Engine): string {
  let out = s.replaceAll("'", "''");
  if (engine === 'mysql') out = out.replaceAll('\\', '\\\\');
  return `'${out}'`;
}

/**
 * `v` as a SQL literal. Refuses (never guesses) what cannot be written exactly: binary values (the
 * grid holds only a preview), possibly truncated text, non-finite numbers.
 */
export function sqlLiteral(v: Value, engine: Engine, capped = false): Literal {
  if (maybeTruncated(v, capped)) return { ok: false, reason: 'a value was truncated' };
  switch (v.t) {
    case 'Null':
      return { ok: true, sql: 'NULL' };
    case 'Bool':
      return { ok: true, sql: v.v ? 'TRUE' : 'FALSE' };
    case 'Int':
    case 'UInt': {
      const s = String(v.v);
      return INTEGER.test(s) ? { ok: true, sql: s } : { ok: false, reason: `not an integer: ${s}` };
    }
    case 'Float':
      return Number.isFinite(v.v) ? { ok: true, sql: String(v.v) } : { ok: false, reason: `cannot write ${v.v}` };
    case 'Decimal':
      return { ok: true, sql: NUMBER.test(v.v) ? v.v : quoteString(v.v, engine) };
    case 'Bytes':
      return { ok: false, reason: 'a binary value cannot be copied exactly' };
    case 'Other':
      return { ok: true, sql: quoteString(v.v.text, engine) };
    case 'Array': {
      if (engine !== 'postgres') return { ok: false, reason: 'arrays are PostgreSQL only' };
      if (v.v.length === 0) return { ok: true, sql: "'{}'" };
      const parts: string[] = [];
      for (const item of v.v) {
        const lit = sqlLiteral(item, engine, capped);
        if (!lit.ok) return lit;
        parts.push(lit.sql);
      }
      return { ok: true, sql: `ARRAY[${parts.join(', ')}]` };
    }
    default:
      return { ok: true, sql: quoteString(v.v, engine) };
  }
}

export type InsertResult = { ok: true; text: string } | { ok: false; message: string };

/**
 * One `INSERT INTO <table> (cols) VALUES (...);` per row. `table` is the already quoted name (see
 * `qualifiedTable`) or `TABLE_PLACEHOLDER`. `rowNumbers` are the numbers shown for `rows` in a
 * refusal message (default 1, 2, ...).
 */
export function toInsertStatements(opts: {
  table: string;
  engine: Engine;
  columns: string[];
  rows: Value[][];
  capped?: boolean;
  rowNumbers?: number[];
}): InsertResult {
  const { table, engine, columns, rows } = opts;
  const cols = columns.map((c) => quoteIdent(engine, c)).join(', ');
  const out: string[] = [];
  for (let r = 0; r < rows.length; r++) {
    const values: string[] = [];
    for (let c = 0; c < columns.length; c++) {
      const lit = sqlLiteral(rows[r]![c] ?? { t: 'Null' }, engine, opts.capped ?? false);
      if (!lit.ok) {
        return {
          ok: false,
          message: `Cannot copy as INSERT: column ${columns[c]}, row ${opts.rowNumbers?.[r] ?? r + 1}: ${lit.reason}`,
        };
      }
      values.push(lit.sql);
    }
    out.push(`INSERT INTO ${table} (${cols}) VALUES (${values.join(', ')});`);
  }
  return { ok: true, text: out.join('\n') };
}
