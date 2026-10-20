// Reads the completion context from the tokens of the current statement (spec §7.3).
// No syntax tree: a light tokenizer plus the `name [AS] alias` pattern after FROM/JOIN/UPDATE.

export type Engine = 'mysql' | 'postgres';

export type CompletionContext =
  | { kind: 'default' }
  | { kind: 'table' }
  | { kind: 'member'; qualifier: string }
  | { kind: 'column' }
  /** Inside a string, a comment or a quoted identifier: nothing to suggest. */
  | { kind: 'none' };

export interface TableRef {
  schema: string | null;
  table: string;
}

export interface StatementInfo {
  aliases: Map<string, TableRef>;
  tables: TableRef[];
}

type TokenKind = 'word' | 'qident' | 'string' | 'comment' | 'number' | 'punct';

interface Token {
  kind: TokenKind;
  start: number;
  end: number;
  /** Words: the text; quoted identifiers: the unescaped name; punctuation: the character. */
  value: string;
  /** Strings, comments and quoted identifiers that run to the end of the text. */
  open?: boolean;
  /** Line comments contain the position right after their last character. */
  line?: boolean;
}

const isIdentStart = (c: string) => /[A-Za-z_\u0080-￿]/.test(c);
const isIdentPart = (c: string) => /[A-Za-z0-9_$\u0080-￿]/.test(c);

/** Index right after the closing `quote`, a doubled quote is an escaped one. */
function scanQuoted(text: string, from: number, quote: string, backslash: boolean): { end: number; open: boolean } {
  let i = from + 1;
  while (i < text.length) {
    const c = text[i];
    if (backslash && c === '\\') {
      i += 2;
      continue;
    }
    if (c === quote) {
      if (text[i + 1] === quote) {
        i += 2;
        continue;
      }
      return { end: i + 1, open: false };
    }
    i++;
  }
  return { end: text.length, open: true };
}

/** A PostgreSQL dollar-quote tag, matched in place (sticky) at `lastIndex`. */
const DOLLAR_TAG = /\$([A-Za-z_][A-Za-z0-9_]*)?\$/y;

function tokenize(text: string, engine: Engine): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  while (i < text.length) {
    const c = text.charAt(i);
    const start = i;
    if (/\s/.test(c)) {
      i++;
      continue;
    }
    const next = text.charAt(i + 1);
    const lineComment =
      (c === '-' && next === '-' && (engine === 'postgres' || i + 2 >= text.length || /\s/.test(text.charAt(i + 2)))) ||
      (c === '#' && engine === 'mysql');
    if (lineComment) {
      const nl = text.indexOf('\n', i);
      i = nl < 0 ? text.length : nl;
      tokens.push({ kind: 'comment', start, end: i, value: '', line: true });
      continue;
    }
    if (c === '/' && next === '*') {
      const close = text.indexOf('*/', i + 2);
      i = close < 0 ? text.length : close + 2;
      tokens.push({ kind: 'comment', start, end: i, value: '', open: close < 0 });
      continue;
    }
    if (c === "'" || (c === '"' && engine === 'mysql')) {
      const q = scanQuoted(text, i, c, engine === 'mysql');
      i = q.end;
      tokens.push({ kind: 'string', start, end: i, value: '', open: q.open });
      continue;
    }
    if ((c === '`' && engine === 'mysql') || (c === '"' && engine === 'postgres')) {
      const q = scanQuoted(text, i, c, false);
      i = q.end;
      const inner = text.slice(start + 1, q.open ? i : i - 1);
      tokens.push({ kind: 'qident', start, end: i, value: inner.split(c + c).join(c), open: q.open });
      continue;
    }
    if (c === '$' && engine === 'postgres') {
      DOLLAR_TAG.lastIndex = i;
      const tag = DOLLAR_TAG.exec(text);
      if (tag) {
        const close = text.indexOf(tag[0], i + tag[0].length);
        i = close < 0 ? text.length : close + tag[0].length;
        tokens.push({ kind: 'string', start, end: i, value: '', open: close < 0 });
        continue;
      }
    }
    if (isIdentStart(c)) {
      while (i < text.length && isIdentPart(text.charAt(i))) i++;
      tokens.push({ kind: 'word', start, end: i, value: text.slice(start, i) });
      continue;
    }
    if (/[0-9]/.test(c)) {
      while (i < text.length && /[0-9A-Za-z_.]/.test(text.charAt(i))) i++;
      tokens.push({ kind: 'number', start, end: i, value: text.slice(start, i) });
      continue;
    }
    i++;
    tokens.push({ kind: 'punct', start, end: i, value: c });
  }
  return tokens;
}

/** Keywords after which a table or view is expected. */
const TABLE_KEYWORDS = new Set(['FROM', 'JOIN', 'UPDATE', 'INTO', 'TABLE']);
/** Keywords after which a column is expected. */
const COLUMN_KEYWORDS = new Set(['SELECT', 'WHERE', 'ON', 'BY', 'AND', 'OR', 'HAVING', 'DISTINCT', 'SET']);
/** Other clause keywords: they end the reach of an earlier FROM / SELECT. */
const OTHER_CLAUSES = new Set(['VALUES', 'LIMIT', 'OFFSET', 'USING']);
/** Where `FROM a, b` lists end: never read as an alias. */
const NOT_ALIAS = new Set([
  'WHERE', 'JOIN', 'INNER', 'LEFT', 'RIGHT', 'FULL', 'OUTER', 'CROSS', 'NATURAL', 'STRAIGHT_JOIN', 'ON',
  'USING', 'GROUP', 'ORDER', 'HAVING', 'LIMIT', 'OFFSET', 'SET', 'UNION', 'EXCEPT', 'INTERSECT', 'WINDOW',
  'FOR', 'VALUES', 'SELECT', 'RETURNING', 'PARTITION', 'USE', 'IGNORE', 'FORCE', 'LOCK', 'INTO', 'FETCH',
  'TABLESAMPLE', 'WITH', 'DEFAULT', 'AS', 'LATERAL', 'ONLY', 'FROM', 'UPDATE', 'DELETE', 'INSERT', 'AND',
  'OR', 'NOT', 'WHEN', 'THEN', 'ELSE', 'END',
]);

const upper = (t: Token | undefined) => (t?.kind === 'word' ? t.value.toUpperCase() : '');
const isIdent = (t: Token | undefined): t is Token =>
  !!t && (t.kind === 'qident' || (t.kind === 'word' && !NOT_ALIAS.has(upper(t))));
const isPunct = (t: Token | undefined, p: string): boolean => t?.kind === 'punct' && t.value === p;

/** `FROM`/`JOIN`/`UPDATE`/`INTO` references and their aliases. */
function readStatement(tokens: Token[]): StatementInfo {
  const info: StatementInfo = { aliases: new Map(), tables: [] };
  for (let i = 0; i < tokens.length; i++) {
    const kw = upper(tokens[i]);
    if (kw !== 'FROM' && kw !== 'JOIN' && kw !== 'UPDATE' && kw !== 'INTO') continue;
    let j = i + 1;
    for (;;) {
      while (upper(tokens[j]) === 'ONLY' || upper(tokens[j]) === 'LATERAL') j++;
      if (isPunct(tokens[j], '(')) {
        // A subquery or derived table: its columns are unknown, skip it and its alias.
        let depth = 0;
        for (; j < tokens.length; j++) {
          if (isPunct(tokens[j], '(')) depth++;
          else if (isPunct(tokens[j], ')') && --depth === 0) break;
        }
        j++;
        if (upper(tokens[j]) === 'AS') j++;
        if (isIdent(tokens[j])) j++;
      } else if (isIdent(tokens[j])) {
        let ref: TableRef = { schema: null, table: tokens[j]?.value ?? '' };
        j++;
        if (isPunct(tokens[j], '.')) {
          // `db.` being typed: the qualifier is not a table.
          const name = tokens[j + 1];
          if (!isIdent(name)) break;
          ref = { schema: ref.table, table: name.value };
          j += 2;
        }
        info.tables.push(ref);
        let alias: Token | undefined;
        if (upper(tokens[j]) === 'AS' && isIdent(tokens[j + 1])) {
          alias = tokens[j + 1];
          j += 2;
        } else if (isIdent(tokens[j]) && !isPunct(tokens[j + 1], '.')) {
          alias = tokens[j];
          j++;
        }
        if (alias) info.aliases.set(alias.value, ref);
      } else {
        break;
      }
      // Only FROM takes a comma-separated list.
      if (kw === 'FROM' && isPunct(tokens[j], ',')) {
        j++;
        continue;
      }
      break;
    }
  }
  return info;
}

/** Half the window `localStatementRange` reads around the cursor. */
const LOCAL_WINDOW = 32 * 1024;

/**
 * The statement around `offset` without `statement_ranges` (for very large texts): the
 * tokens of a ~64 KB window around the cursor, cut at the nearest `;` on each side that
 * is outside strings and comments. Returns absolute UTF-16 offsets.
 */
export function localStatementRange(text: string, offset: number, engine: Engine): [number, number] {
  let from = Math.max(0, offset - LOCAL_WINDOW);
  if (from > 0) {
    // Start at a line start: far less likely to fall inside a string than an arbitrary cut.
    const nl = text.indexOf('\n', from);
    if (nl >= 0 && nl < offset) from = nl + 1;
  }
  const to = Math.min(text.length, offset + LOCAL_WINDOW);
  const local = offset - from;
  let lo = 0;
  let hi = to - from;
  for (const t of tokenize(text.slice(from, to), engine)) {
    if (t.kind !== 'punct' || t.value !== ';') continue;
    if (t.end <= local) lo = t.end;
    else if (t.start >= local) {
      hi = t.start;
      break;
    }
  }
  return [from + lo, from + hi];
}

/** The context at `offset` of `stmt`, the identifier being typed and the statement's tables. */
export function analyze(
  stmt: string,
  offset: number,
  engine: Engine,
): { context: CompletionContext; prefix: string; info: StatementInfo } {
  const all = tokenize(stmt, engine);
  // Only the statement around the cursor: a `;` ends it on either side.
  let first = 0;
  let last = all.length;
  for (const [k, t] of all.entries()) {
    if (t.kind !== 'punct' || t.value !== ';') continue;
    if (t.end <= offset) first = k + 1;
    else if (t.start >= offset) {
      last = k;
      break;
    }
  }
  const tokens = all.slice(first, last);
  const info = readStatement(tokens.filter((t) => t.kind !== 'comment'));

  const inside = tokens.find(
    (t) =>
      (t.kind === 'string' || t.kind === 'comment' || t.kind === 'qident') &&
      t.start < offset &&
      (offset < t.end || ((t.open || t.line) && offset <= t.end)),
  );
  if (inside) return { context: { kind: 'none' }, prefix: '', info };

  const typing = tokens.find((t) => t.kind === 'word' && t.start < offset && offset <= t.end);
  const prefix = typing ? stmt.slice(typing.start, offset) : '';
  const prefixStart = typing ? typing.start : offset;
  const before = tokens.filter((t) => t.kind !== 'comment' && t.end <= prefixStart);
  const prev = before[before.length - 1];

  let context: CompletionContext = { kind: 'default' };
  if (isPunct(prev, '.')) {
    const q = before[before.length - 2];
    if (q && (q.kind === 'word' || q.kind === 'qident') && q.end === prev?.start) {
      context = { kind: 'member', qualifier: q.value };
    }
  } else if (TABLE_KEYWORDS.has(upper(prev))) {
    context = { kind: 'table' };
  } else if (COLUMN_KEYWORDS.has(upper(prev))) {
    context = { kind: 'column' };
  } else if (prev?.kind === 'punct' && prev.value !== ')') {
    // After a comma or an operator: the clause that governs it decides.
    for (let k = before.length - 2; k >= 0; k--) {
      const w = upper(before[k]);
      if (TABLE_KEYWORDS.has(w)) {
        if (w === 'FROM' && prev.value === ',') context = { kind: 'table' };
        break;
      }
      if (COLUMN_KEYWORDS.has(w)) {
        context = { kind: 'column' };
        break;
      }
      if (OTHER_CLAUSES.has(w)) break;
    }
  }
  return { context, prefix, info };
}
