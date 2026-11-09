// Where the cursor of a MongoDB console script is, for completion: a pure reading of the
// text before the cursor, from the start of its statement. Statements end as the console
// splits them (`split` in `crates/driver-mongo/src/shell/parse.rs`): at `;`, or at the end
// of a line once the brackets are closed, unless the next line starts with `.`.

/** What the key being typed at a key position of an object may be. */
export type KeyRole =
  /** A filter: field names, and the query operators once the key starts with `$`. */
  | 'query'
  /** The operator object of a field (`{ total: { $gt: 1 } }`): query operators only. */
  | 'operators'
  /** The top level of an update document: update operators. */
  | 'update'
  /** Field names only: a projection, a sort, the body of `$set`, a document to insert. */
  | 'fields'
  /** A pipeline stage: stage names. */
  | 'stage';

export type MongoContext =
  | { kind: 'none' }
  /** The first word of a statement: `db`, `use`, `show`. */
  | { kind: 'statement' }
  /** After `use `: database names. */
  | { kind: 'use' }
  /** After `show `. */
  | { kind: 'show' }
  /** After `db.`; `db` is the `getSiblingDB` argument, `null` for the tab's database. */
  | { kind: 'db'; db: string | null }
  /** After `db.<coll>.`: collection methods. */
  | { kind: 'collection'; db: string | null; coll: string }
  /** After `find(…).` / `aggregate(…).`: cursor modifiers. */
  | { kind: 'modifier' }
  /** A key position of an object; `coll` is the collection it is about (null when unknown). */
  | { kind: 'key'; role: KeyRole; db: string | null; coll: string | null }
  /** A value position: the value helpers (`ObjectId(…)`, …). */
  | { kind: 'value' };

export interface MongoAnalysis {
  context: MongoContext;
  /** The word being typed before the cursor (with its `$`), replaced by a suggestion. */
  prefix: string;
  /** Offset where `prefix` starts. */
  from: number;
}

interface Token {
  kind: 'ident' | 'string' | 'number' | 'regex' | 'punct';
  text: string;
  start: number;
  end: number;
}

const IDENT_START = /[\p{L}_$]/u;
const IDENT_PART = /[\p{L}\p{N}_$]/u;
/** After these a `/` starts a regular expression, not a division (the console has no division anyway). */
const REGEX_AFTER = new Set(['(', ',', '=', ':', '[', '!', '&', '|', '?', '{', '}', ';']);

/** The tokens of `src` (the text before the cursor); null when it ends inside a string, comment or regex. */
function lex(src: string): Token[] | null {
  const out: Token[] = [];
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i]!;
    if (/\s/.test(c)) {
      i++;
      continue;
    }
    if (c === '/' && src[i + 1] === '/') {
      const nl = src.indexOf('\n', i);
      if (nl < 0) return null;
      i = nl + 1;
      continue;
    }
    if (c === '/' && src[i + 1] === '*') {
      const close = src.indexOf('*/', i + 2);
      if (close < 0) return null;
      i = close + 2;
      continue;
    }
    if (c === '"' || c === "'" || c === '`') {
      let j = i + 1;
      while (j < n && src[j] !== c) {
        // A plain string ends at a line break (the console reports it); a template does not.
        if (c !== '`' && src[j] === '\n') break;
        j += src[j] === '\\' ? 2 : 1;
      }
      if (j >= n) return null;
      if (src[j] !== c) {
        // Unterminated on its line: what follows is new text.
        out.push({ kind: 'string', text: src.slice(i, j), start: i, end: j });
        i = j;
        continue;
      }
      out.push({ kind: 'string', text: src.slice(i, j + 1), start: i, end: j + 1 });
      i = j + 1;
      continue;
    }
    if (c === '/') {
      const prev = out.at(-1);
      if (!prev || (prev.kind === 'punct' && REGEX_AFTER.has(prev.text))) {
        let j = i + 1;
        let cls = false;
        while (j < n && src[j] !== '\n' && (cls || src[j] !== '/')) {
          if (src[j] === '\\') j++;
          else if (src[j] === '[') cls = true;
          else if (src[j] === ']') cls = false;
          j++;
        }
        if (j >= n) return null;
        if (src[j] === '/') {
          j++;
          while (j < n && /[a-z]/i.test(src[j]!)) j++;
          out.push({ kind: 'regex', text: src.slice(i, j), start: i, end: j });
          i = j;
          continue;
        }
      }
      out.push({ kind: 'punct', text: c, start: i, end: i + 1 });
      i++;
      continue;
    }
    if (IDENT_START.test(c)) {
      let j = i + 1;
      while (j < n && IDENT_PART.test(src[j]!)) j++;
      out.push({ kind: 'ident', text: src.slice(i, j), start: i, end: j });
      i = j;
      continue;
    }
    if (/\d/.test(c)) {
      let j = i + 1;
      while (j < n && /[\w.]/.test(src[j]!)) j++;
      out.push({ kind: 'number', text: src.slice(i, j), start: i, end: j });
      i = j;
      continue;
    }
    out.push({ kind: 'punct', text: c, start: i, end: i + 1 });
    i++;
  }
  return out;
}

/** The value of a string token (quotes removed, simple escapes resolved). */
function stringValue(t: Token): string {
  const body = t.text.slice(1, t.text.endsWith(t.text[0]!) && t.text.length > 1 ? -1 : undefined);
  return body.replace(/\\(.)/g, '$1');
}

/**
 * Index of the first token of the statement the cursor is in (`tokens.length` for an empty one).
 * `next` is where the word at the cursor starts, or null when nothing is typed there yet.
 */
function statementStart(src: string, tokens: Token[], next: number | null): number {
  let first: number | null = null;
  let depth = 0;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (t.kind === 'punct' && t.text === ';') {
      first = null;
      depth = 0;
      continue;
    }
    first ??= i;
    if (t.kind === 'punct') {
      if ('([{'.includes(t.text)) depth++;
      else if (')]}'.includes(t.text)) depth = Math.max(0, depth - 1);
    }
    if (depth > 0) continue;
    const after = tokens[i + 1];
    const nextStart = after ? after.start : (next ?? src.length);
    const gap = src.slice(t.end, nextStart);
    const continues = after ? after.kind === 'punct' && after.text === '.' : false;
    if (/[\r\n]/.test(gap) && !continues) first = null;
  }
  return first ?? tokens.length;
}

interface Target {
  db: string | null;
  coll: string | null;
}

type Call = Target & { on: 'db' | 'coll' | 'modifier' | 'other'; method: string; cursor: boolean };

type ArrayRole = 'pipeline' | 'queries' | 'documents' | 'values' | 'ref';

type Frame =
  | (Target & { t: 'call'; call: Call; arg: number; state: 'value' | 'after'; items: number; str: string | null })
  | (Target & { t: 'object'; role: KeyRole | null; state: 'key' | 'colon' | 'value' | 'after'; key: string | null })
  | (Target & { t: 'array'; role: ArrayRole; state: 'value' | 'after'; items: number; str: string | null });

/** What the statement is so far at its top level (outside every bracket). */
type Top =
  | { t: 'start' }
  | { t: 'db'; db: string | null; dot: boolean }
  /** `db.a.b`, `db["x"]`, `db.getCollection("x")`: `base` is the bracket / getCollection name. */
  | { t: 'path'; db: string | null; base: string | null; names: string[]; dot: boolean }
  | { t: 'result'; db: string | null; coll: string | null; cursor: boolean; dot: boolean }
  | { t: 'member'; db: string | null; coll: string | null; cursor: boolean; name: string }
  | { t: 'other' };

/** Methods whose result takes the cursor modifiers. */
const CURSOR_METHODS = new Set(['find', 'aggregate']);
/** Modifiers after which no other modifier follows. */
const FINAL_MODIFIERS = new Set(['count', 'toArray', 'explain']);
const FILTER_FIRST = new Set([
  'find',
  'findOne',
  'countDocuments',
  'deleteOne',
  'deleteMany',
  'updateOne',
  'updateMany',
  'replaceOne',
  'findOneAndUpdate',
  'findOneAndReplace',
  'findOneAndDelete',
]);
const UPDATES = new Set(['updateOne', 'updateMany', 'findOneAndUpdate']);
const LOGICAL = new Set(['$and', '$or', '$nor']);
const FIELD_STAGES = new Set(['$sort', '$project', '$addFields', '$set']);

/** The role of an object passed as argument `arg` of `call`. */
function argObjectRole(call: Call, arg: number): KeyRole | null {
  const m = call.method;
  if (call.on === 'modifier') return arg === 0 && (m === 'sort' || m === 'project' || m === 'projection') ? 'fields' : null;
  if (call.on !== 'coll') return null;
  if (arg === 0 && FILTER_FIRST.has(m)) return 'query';
  if (arg === 1 && m === 'distinct') return 'query';
  if (arg === 1 && UPDATES.has(m)) return 'update';
  if (arg === 1 && (m === 'find' || m === 'findOne' || m === 'replaceOne' || m === 'findOneAndReplace')) return 'fields';
  if (arg === 0 && (m === 'insertOne' || m === 'createIndex')) return 'fields';
  return null;
}

function argArrayRole(call: Call, arg: number): ArrayRole {
  if (call.on !== 'coll') return 'values';
  const m = call.method;
  if (arg === 0 && m === 'aggregate') return 'pipeline';
  if (arg === 1 && UPDATES.has(m)) return 'pipeline';
  if (arg === 0 && (m === 'insertMany' || m === 'createIndexes')) return 'documents';
  return 'values';
}

/** The role of the object that is the value of `key` in an object of `role`. */
function valueObjectRole(role: KeyRole | null, key: string | null): KeyRole | null {
  if (key === null) return null;
  switch (role) {
    case 'query':
      if (key === '$not' || key === '$elemMatch') return 'operators';
      return key.startsWith('$') ? null : 'operators';
    case 'operators':
      return key === '$not' || key === '$elemMatch' ? 'operators' : null;
    case 'update':
      return key.startsWith('$') ? 'fields' : null;
    case 'stage':
      if (key === '$match') return 'query';
      return FIELD_STAGES.has(key) ? 'fields' : null;
    default:
      return null;
  }
}

function valueArrayRole(role: KeyRole | null, key: string | null): ArrayRole {
  return role === 'query' && key !== null && LOGICAL.has(key) ? 'queries' : 'values';
}

const NONE: MongoContext = { kind: 'none' };

/** The completion context at `offset` of `text`. */
export function analyzeMongo(text: string, offset: number): MongoAnalysis {
  const src = text.slice(0, offset);
  const none = { context: NONE, prefix: '', from: offset };
  const tokens = lex(src);
  if (!tokens) return none;
  // The word being typed at the cursor (an identifier, possibly `$`-led).
  let prefix = '';
  let from = offset;
  const last = tokens.at(-1);
  if (last && last.end === offset && (last.kind === 'number' || last.kind === 'regex')) return none;
  if (last && last.end === offset && last.kind === 'ident') {
    tokens.pop();
    prefix = last.text;
    from = last.start;
  }
  const done = (context: MongoContext): MongoAnalysis => ({ context, prefix, from });
  const stmt = tokens.slice(statementStart(src, tokens, prefix ? from : null));
  if (stmt.length === 0) return done({ kind: 'statement' });
  const head = stmt[0]!;
  if (head.kind === 'ident' && (head.text === 'use' || head.text === 'show')) {
    return done(stmt.length === 1 ? { kind: head.text === 'use' ? 'use' : 'show' } : NONE);
  }

  let top: Top = { t: 'start' };
  const stack: Frame[] = [];
  for (let i = 0; i < stmt.length; i++) {
    const tok = stmt[i]!;
    const prev = stmt[i - 1];
    const p = tok.kind === 'punct' ? tok.text : null;
    const f = stack.at(-1);
    if (p === ')' || p === ']' || p === '}') {
      const want = p === ')' ? 'call' : p === ']' ? 'array' : 'object';
      if (!f || f.t !== want) return done(NONE);
      stack.pop();
      const parent = stack.at(-1);
      if (parent) {
        parent.state = 'after';
        if (parent.t !== 'object') parent.items++;
      } else top = closeTop(f);
      continue;
    }
    if (p === '(' || p === '[' || p === '{') {
      stack.push(open(p, f, top, prev));
      if (!f) top = { t: 'other' };
      continue;
    }
    if (!f) {
      top = stepTop(top, tok);
      continue;
    }
    // A token inside the innermost bracket.
    if (f.t === 'object') {
      if (p === ',') f.state = 'key';
      else if (f.state === 'key') {
        f.key = tok.kind === 'string' ? stringValue(tok) : tok.text;
        f.state = 'colon';
      } else if (f.state === 'colon') f.state = p === ':' ? 'value' : 'after';
      else if (f.state === 'value' && p !== '-' && p !== '+') f.state = 'after';
      continue;
    }
    if (p === ',') {
      if (f.t === 'call') f.arg++;
      f.state = 'value';
      continue;
    }
    if (p === '-' || p === '+') continue;
    if (f.state === 'value' && f.items === 0 && tok.kind === 'string') f.str = stringValue(tok);
    f.items++;
    f.state = 'after';
  }

  const f = stack.at(-1);
  if (!f) {
    switch (top.t) {
      case 'db':
        return done(top.dot ? { kind: 'db', db: top.db } : NONE);
      case 'path': {
        const coll = [...(top.base === null ? [] : [top.base]), ...top.names].join('.');
        return done(top.dot && coll ? { kind: 'collection', db: top.db, coll } : NONE);
      }
      case 'result':
        return done(top.dot && top.cursor ? { kind: 'modifier' } : NONE);
      default:
        return done(NONE);
    }
  }
  switch (f.t) {
    case 'object':
      if (f.state === 'key') return done(f.role ? { kind: 'key', role: f.role, db: f.db, coll: f.coll } : NONE);
      return done(f.state === 'value' ? { kind: 'value' } : NONE);
    case 'array':
      return done(f.state === 'value' && f.role === 'values' ? { kind: 'value' } : NONE);
    default:
      return done(NONE);
  }
}

/** The frame a `(`, `[` or `{` opens inside `f` (or at the top level, `top`). */
function open(p: '(' | '[' | '{', f: Frame | undefined, top: Top, prev: Token | undefined): Frame {
  const target: Target = f ? { db: f.db, coll: f.coll } : { db: null, coll: null };
  if (p === '(') {
    const call = f ? otherCall(target, prev) : topCall(top, prev);
    return { t: 'call', db: call.db, coll: call.coll, call, arg: 0, state: 'value', items: 0, str: null };
  }
  const atValue = f !== undefined && f.state === 'value';
  if (p === '[') {
    let role: ArrayRole = 'values';
    if (!f) role = top.t === 'db' && !top.dot ? 'ref' : 'values';
    else if (atValue && f.t === 'call') role = argArrayRole(f.call, f.arg);
    else if (atValue && f.t === 'object') role = valueArrayRole(f.role, f.key);
    if (!f && top.t === 'db') return { t: 'array', db: top.db, coll: null, role, state: 'value', items: 0, str: null };
    return { t: 'array', ...target, role, state: 'value', items: 0, str: null };
  }
  let role: KeyRole | null = null;
  if (f && atValue) {
    if (f.t === 'call') role = argObjectRole(f.call, f.arg);
    else if (f.t === 'object') role = valueObjectRole(f.role, f.key);
    else role = f.role === 'pipeline' ? 'stage' : f.role === 'queries' ? 'query' : f.role === 'documents' ? 'fields' : null;
  }
  return { t: 'object', ...target, role, state: 'key', key: null };
}

function otherCall(target: Target, prev: Token | undefined): Call {
  return { ...target, on: 'other', method: prev?.kind === 'ident' ? prev.text : '', cursor: false };
}

/** The call a top-level `(` makes, from what precedes it. */
function topCall(top: Top, prev: Token | undefined): Call {
  if (top.t === 'path' && !top.dot && top.names.length > 0) {
    const method = top.names.at(-1)!;
    const names = top.names.slice(0, -1);
    if (top.base === null && names.length === 0) return { db: top.db, coll: null, on: 'db', method, cursor: false };
    const coll = [...(top.base === null ? [] : [top.base]), ...names].join('.');
    return { db: top.db, coll, on: 'coll', method, cursor: false };
  }
  if (top.t === 'member') return { db: top.db, coll: top.coll, on: 'modifier', method: top.name, cursor: top.cursor };
  return otherCall({ db: null, coll: null }, prev);
}

/** The top-level state after the bracket `f` closed. */
function closeTop(f: Frame): Top {
  const single = f.t !== 'object' && f.items === 1 ? f.str : null;
  if (f.t === 'array') return f.role === 'ref' && single !== null ? { t: 'path', db: f.db, base: single, names: [], dot: false } : { t: 'other' };
  if (f.t !== 'call') return { t: 'other' };
  const { call } = f;
  switch (call.on) {
    case 'db':
      if (call.method === 'getSiblingDB' && single !== null) return { t: 'db', db: single, dot: false };
      if (call.method === 'getCollection' && single !== null) return { t: 'path', db: call.db, base: single, names: [], dot: false };
      return { t: 'result', db: call.db, coll: null, cursor: false, dot: false };
    case 'coll':
      return { t: 'result', db: call.db, coll: call.coll, cursor: CURSOR_METHODS.has(call.method), dot: false };
    case 'modifier':
      return { t: 'result', db: call.db, coll: call.coll, cursor: call.cursor && !FINAL_MODIFIERS.has(call.method), dot: false };
    default:
      return { t: 'other' };
  }
}

/** The top-level state after a token outside every bracket. */
function stepTop(top: Top, tok: Token): Top {
  const dot = tok.kind === 'punct' && tok.text === '.';
  const ident = tok.kind === 'ident' ? tok.text : null;
  switch (top.t) {
    case 'start':
      return ident === 'db' ? { t: 'db', db: null, dot: false } : { t: 'other' };
    case 'db':
      if (!top.dot) return dot ? { ...top, dot: true } : { t: 'other' };
      return ident ? { t: 'path', db: top.db, base: null, names: [ident], dot: false } : { t: 'other' };
    case 'path':
      if (!top.dot) return dot ? { ...top, dot: true } : { t: 'other' };
      return ident ? { ...top, names: [...top.names, ident], dot: false } : { t: 'other' };
    case 'result':
      if (!top.dot) return dot ? { ...top, dot: true } : { t: 'other' };
      return ident ? { t: 'member', db: top.db, coll: top.coll, cursor: top.cursor, name: ident } : { t: 'other' };
    default:
      return { t: 'other' };
  }
}
