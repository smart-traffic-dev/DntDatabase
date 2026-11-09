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
  /**
   * After `db.<coll>.`: collection methods. `explained`: after `db.<coll>.explain(…).`, where
   * only a method follows (no dotted collection name).
   */
  | { kind: 'collection'; db: string | null; coll: string; explained?: true }
  /** After `find(…).` / `aggregate(…).`: cursor modifiers; `countable`: `.count()` fits (after find only). */
  | { kind: 'modifier'; countable: boolean }
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

/** A token of a console script: `text` is `src.slice(start, end)`. */
export interface Token {
  kind: 'ident' | 'string' | 'number' | 'regex' | 'punct';
  text: string;
  start: number;
  end: number;
}

/** A comment of a console script; `line`: a `//` comment (its text stops before the line break). */
export interface Comment {
  text: string;
  start: number;
  end: number;
  line: boolean;
}

/** Where a script stops lexing: the start of the string, comment or regex that does not end. */
export interface LexProblem {
  offset: number;
  message: string;
}

/**
 * What the formatter keeps beside the tokens (completion skips both): the comments, and the
 * first text that does not lex. Comments never go to the token list, so a `/` after one is
 * read exactly as without the sink.
 */
interface LexSink {
  comments: Comment[];
  problem: LexProblem | null;
}

const SPACE = /\s/;
const LETTER = /\p{L}/u;
const LETTER_OR_DIGIT = /[\p{L}\p{N}]/u;

const isSpace = (c: number) => c === 32 || c === 10 || c === 13 || c === 9 || (c > 127 && SPACE.test(String.fromCharCode(c)));
const isDigit = (c: number) => c >= 48 && c <= 57;
const isAsciiLetter = (c: number) => (c >= 97 && c <= 122) || (c >= 65 && c <= 90);
/** `[\p{L}_$]`. */
const isIdentStart = (c: number) => isAsciiLetter(c) || c === 95 || c === 36 || (c > 127 && LETTER.test(String.fromCharCode(c)));
/** `[\p{L}\p{N}_$]`. */
const isIdentPart = (c: number) =>
  isAsciiLetter(c) || isDigit(c) || c === 95 || c === 36 || (c > 127 && LETTER_OR_DIGIT.test(String.fromCharCode(c)));
/** `[\w.]`: the rest of a number. */
const isNumberPart = (c: number) => isAsciiLetter(c) || isDigit(c) || c === 95 || c === 46;

/** After these a `/` starts a regular expression, not a division (the console has no division anyway). */
const REGEX_AFTER = new Set(['(', ',', '=', ':', '[', '!', '&', '|', '?', '{', '}', ';']);

const SLASH = 47;
const STAR = 42;
const BACKSLASH = 92;
const NEWLINE = 10;
const BACKTICK = 96;

/**
 * Appends the tokens of `src` from `i` to `out`; true when `src` ends inside a string, comment
 * or regex. `out` already holds the tokens before `i` (a `/` after them may start a regex).
 * With a `sink`, the comments go there, a `//` comment ending the text is not open, and the
 * first text that does not lex is recorded (a string or regex stopped by its line end too).
 */
function lexInto(src: string, i: number, out: Token[], sink?: LexSink): boolean {
  const n = src.length;
  const problem = (offset: number, message: string) => {
    if (sink && !sink.problem) sink.problem = { offset, message };
  };
  while (i < n) {
    const c = src.charCodeAt(i);
    if (isSpace(c)) {
      i++;
      continue;
    }
    const c2 = src.charCodeAt(i + 1);
    if (c === SLASH && c2 === SLASH) {
      const nl = src.indexOf('\n', i);
      const stop = nl < 0 ? n : nl;
      sink?.comments.push({ text: src.slice(i, stop), start: i, end: stop, line: true });
      if (nl < 0) return !sink;
      i = nl + 1;
      continue;
    }
    if (c === SLASH && c2 === STAR) {
      const close = src.indexOf('*/', i + 2);
      if (close < 0) {
        problem(i, 'Unterminated comment');
        return true;
      }
      sink?.comments.push({ text: src.slice(i, close + 2), start: i, end: close + 2, line: false });
      i = close + 2;
      continue;
    }
    if (c === 34 || c === 39 || c === BACKTICK) {
      let j = i + 1;
      while (j < n) {
        const d = src.charCodeAt(j);
        if (d === c) break;
        // A plain string ends at a line break (the console reports it); a template does not.
        if (c !== BACKTICK && d === NEWLINE) break;
        j += d === BACKSLASH ? 2 : 1;
      }
      const unterminated = c === BACKTICK ? 'Unterminated template literal' : 'Unterminated string';
      if (j >= n) {
        problem(i, unterminated);
        return true;
      }
      // Unterminated on its line: what follows is new text.
      const stop = src.charCodeAt(j) === c ? j + 1 : j;
      if (stop === j) problem(i, unterminated);
      out.push({ kind: 'string', text: src.slice(i, stop), start: i, end: stop });
      i = stop;
      continue;
    }
    if (c === SLASH) {
      const prev = out.at(-1);
      if (!prev || (prev.kind === 'punct' && REGEX_AFTER.has(prev.text))) {
        let j = i + 1;
        let cls = false;
        while (j < n) {
          const d = src.charCodeAt(j);
          if (d === NEWLINE || (!cls && d === SLASH)) break;
          if (d === BACKSLASH) j++;
          else if (d === 91) cls = true;
          else if (d === 93) cls = false;
          j++;
        }
        if (j >= n) {
          problem(i, 'Unterminated regular expression');
          return true;
        }
        if (src.charCodeAt(j) === SLASH) {
          j++;
          while (j < n && isAsciiLetter(src.charCodeAt(j))) j++;
          out.push({ kind: 'regex', text: src.slice(i, j), start: i, end: j });
          i = j;
          continue;
        }
        problem(i, 'Unterminated regular expression');
      }
      out.push({ kind: 'punct', text: '/', start: i, end: i + 1 });
      i++;
      continue;
    }
    if (isIdentStart(c)) {
      let j = i + 1;
      while (j < n && isIdentPart(src.charCodeAt(j))) j++;
      out.push({ kind: 'ident', text: src.slice(i, j), start: i, end: j });
      i = j;
      continue;
    }
    if (isDigit(c)) {
      let j = i + 1;
      while (j < n && isNumberPart(src.charCodeAt(j))) j++;
      out.push({ kind: 'number', text: src.slice(i, j), start: i, end: j });
      i = j;
      continue;
    }
    out.push({ kind: 'punct', text: src[i]!, start: i, end: i + 1 });
    i++;
  }
  return false;
}

/**
 * The tokens of the previous request. Typing changes the text near the cursor, so the next
 * request keeps the tokens before the first changed character and lexes only the rest: a
 * keystroke at the end of a long script does not re-read the whole script.
 */
let lastLex: { src: string; tokens: Token[]; open: boolean } | null = null;

/** Length of the common prefix of `a` and `b`. */
function commonPrefix(a: string, b: string): number {
  if (b.startsWith(a)) return a.length;
  if (a.startsWith(b)) return b.length;
  const n = Math.min(a.length, b.length);
  let i = 0;
  while (i < n && a.charCodeAt(i) === b.charCodeAt(i)) i++;
  return i;
}

/** The tokens of `src` (the text before the cursor); null when it ends inside a string, comment or regex. */
function lex(src: string): Token[] | null {
  const prev = lastLex;
  if (prev && prev.src === src) return prev.open ? null : prev.tokens;
  let tokens: Token[] = [];
  let from = 0;
  if (prev) {
    // Reuse up to the start of the first changed line. Every token decision reads at most
    // its own text and the character after it, except a `/` in regex position without a
    // closing slash: it became punctuation by scanning to the end of its line, so a change
    // later on that line can turn it into a regex. No scan that fails reads past its own
    // line break, so a token ending before the changed line is unchanged.
    const changed = commonPrefix(prev.src, src);
    const d = changed > 0 ? src.lastIndexOf('\n', changed - 1) + 1 : 0;
    let lo = 0;
    let hi = prev.tokens.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (prev.tokens[mid]!.end < d) lo = mid + 1;
      else hi = mid;
    }
    tokens = prev.tokens;
    tokens.length = lo;
    from = lo > 0 ? tokens[lo - 1]!.end : 0;
  }
  const open = lexInto(src, from, tokens);
  lastLex = { src, tokens, open };
  return open ? null : tokens;
}

/**
 * The tokens and comments of a whole script, for the formatter: the same reading as
 * completion's. `problem`: where the text stops lexing, if it does.
 */
export function lexMongoScript(src: string): { tokens: Token[]; comments: Comment[]; problem: LexProblem | null } {
  const tokens: Token[] = [];
  const sink: LexSink = { comments: [], problem: null };
  lexInto(src, 0, tokens, sink);
  return { tokens, comments: sink.comments, problem: sink.problem };
}

/** The value of a string token (quotes removed, simple escapes resolved). */
function stringValue(t: Token): string {
  const body = t.text.slice(1, t.text.endsWith(t.text[0]!) && t.text.length > 1 ? -1 : undefined);
  return body.replace(/\\(.)/g, '$1');
}

/**
 * Index of the first token of the statement the cursor is in (`end` for an empty one), among
 * `tokens[0..end)`. `next` is where the word at the cursor starts, or null when nothing is
 * typed there yet.
 */
function statementStart(src: string, tokens: Token[], end: number, next: number | null): number {
  let first = -1;
  let depth = 0;
  // The next line break at or after the current token (-1: none), looked up as tokens advance.
  let nl = -2;
  let cr = -2;
  for (let i = 0; i < end; i++) {
    const t = tokens[i]!;
    if (t.kind === 'punct') {
      const p = t.text;
      if (p === ';') {
        first = -1;
        depth = 0;
        continue;
      }
      if (p === '(' || p === '[' || p === '{') depth++;
      else if (p === ')' || p === ']' || p === '}') depth = depth > 0 ? depth - 1 : 0;
    }
    if (first < 0) first = i;
    if (depth > 0) continue;
    const after = i + 1 < end ? tokens[i + 1] : undefined;
    // A line continued by a leading `.` stays one statement.
    if (after && after.kind === 'punct' && after.text === '.') continue;
    const nextStart = after ? after.start : (next ?? src.length);
    if (nl !== -1 && nl < t.end) nl = src.indexOf('\n', t.end);
    if (cr !== -1 && cr < t.end) cr = src.indexOf('\r', t.end);
    if ((nl >= 0 && nl < nextStart) || (cr >= 0 && cr < nextStart)) first = -1;
  }
  return first < 0 ? end : first;
}

interface Target {
  db: string | null;
  coll: string | null;
}

/**
 * `cursor`: modifiers may follow the call's result; `countable`: `.count()` may (after `find`
 * only); `explained`: a method after `db.<coll>.explain(…).` (no modifiers after it).
 */
type Call = Target & {
  on: 'db' | 'coll' | 'modifier' | 'other';
  method: string;
  cursor: boolean;
  countable: boolean;
  explained: boolean;
};

type ArrayRole = 'pipeline' | 'queries' | 'documents' | 'values' | 'ref';

type Frame =
  | (Target & { t: 'call'; call: Call; arg: number; state: 'value' | 'after'; items: number; str: string | null })
  | (Target & { t: 'object'; role: KeyRole | null; state: 'key' | 'colon' | 'value' | 'after'; key: string | null })
  | (Target & { t: 'array'; role: ArrayRole; state: 'value' | 'after'; items: number; str: string | null });

/** What the statement is so far at its top level (outside every bracket). Changed in place. */
type Top =
  | { t: 'start' }
  | { t: 'db'; db: string | null; dot: boolean }
  /**
   * `db.a.b`, `db["x"]`, `db.getCollection("x")`: `base` is the bracket / getCollection name.
   * `explained`: after `db.<coll>.explain(…)`, `base` being the collection.
   */
  | { t: 'path'; db: string | null; base: string | null; names: string[]; dot: boolean; explained: boolean }
  | { t: 'result'; db: string | null; coll: string | null; cursor: boolean; countable: boolean; dot: boolean }
  | { t: 'member'; db: string | null; coll: string | null; cursor: boolean; countable: boolean; name: string }
  | { t: 'other' };

/** Methods whose result takes the cursor modifiers. */
const CURSOR_METHODS = new Set(['find', 'aggregate']);
/** Modifiers after which no other modifier follows (parse.rs closes the chain after them). */
const FINAL_MODIFIERS = new Set(['count', 'explain']);
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
  // The word being typed at the cursor (an identifier, possibly `$`-led). `tokens` is the
  // lexer's cache: read here, never changed.
  let prefix = '';
  let from = offset;
  let end = tokens.length;
  const last = tokens.at(-1);
  if (last && last.end === offset && (last.kind === 'number' || last.kind === 'regex')) return none;
  if (last && last.end === offset && last.kind === 'ident') {
    end--;
    prefix = last.text;
    from = last.start;
  }
  const done = (context: MongoContext): MongoAnalysis => ({ context, prefix, from });
  const stmt = tokens.slice(statementStart(src, tokens, end, prefix ? from : null), end);
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
        if (top.explained) {
          const method = top.dot && top.names.length === 0 && top.base !== null;
          return done(method ? { kind: 'collection', db: top.db, coll: top.base as string, explained: true } : NONE);
        }
        const coll = [...(top.base === null ? [] : [top.base]), ...top.names].join('.');
        return done(top.dot && coll ? { kind: 'collection', db: top.db, coll } : NONE);
      }
      case 'result':
        return done(top.dot && top.cursor ? { kind: 'modifier', countable: top.countable } : NONE);
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

const PLAIN = { cursor: false, countable: false, explained: false };

function otherCall(target: Target, prev: Token | undefined): Call {
  return { ...target, on: 'other', method: prev?.kind === 'ident' ? prev.text : '', ...PLAIN };
}

/** The call a top-level `(` makes, from what precedes it. */
function topCall(top: Top, prev: Token | undefined): Call {
  if (top.t === 'path' && !top.dot && top.names.length > 0) {
    const method = top.names.at(-1)!;
    const names = top.names.slice(0, -1);
    if (top.base === null && names.length === 0) return { db: top.db, coll: null, on: 'db', method, ...PLAIN };
    const coll = [...(top.base === null ? [] : [top.base]), ...names].join('.');
    return { db: top.db, coll, on: 'coll', method, ...PLAIN, explained: top.explained };
  }
  if (top.t === 'member') {
    return { db: top.db, coll: top.coll, on: 'modifier', method: top.name, ...PLAIN, cursor: top.cursor, countable: top.countable };
  }
  return otherCall({ db: null, coll: null }, prev);
}

/** The top-level state after the bracket `f` closed. */
function closeTop(f: Frame): Top {
  const single = f.t !== 'object' && f.items === 1 ? f.str : null;
  if (f.t === 'array') {
    return f.role === 'ref' && single !== null
      ? { t: 'path', db: f.db, base: single, names: [], dot: false, explained: false }
      : { t: 'other' };
  }
  if (f.t !== 'call') return { t: 'other' };
  const { call } = f;
  const result = (cursor: boolean, countable: boolean): Top => ({ t: 'result', db: call.db, coll: call.coll, cursor, countable, dot: false });
  switch (call.on) {
    case 'db':
      if (call.method === 'getSiblingDB' && single !== null) return { t: 'db', db: single, dot: false };
      if (call.method === 'getCollection' && single !== null) {
        return { t: 'path', db: call.db, base: single, names: [], dot: false, explained: false };
      }
      return result(false, false);
    case 'coll':
      // `db.<coll>.explain(…).<method>(…)`: a collection method follows, and nothing after it.
      if (call.method === 'explain' && !call.explained) {
        return { t: 'path', db: call.db, base: call.coll, names: [], dot: false, explained: true };
      }
      if (call.explained) return result(false, false);
      return result(CURSOR_METHODS.has(call.method), call.method === 'find');
    case 'modifier':
      return result(call.cursor && !FINAL_MODIFIERS.has(call.method), call.countable);
    default:
      return { t: 'other' };
  }
}

/** The top-level state after a token outside every bracket (`top` is changed in place). */
function stepTop(top: Top, tok: Token): Top {
  const dot = tok.kind === 'punct' && tok.text === '.';
  const ident = tok.kind === 'ident' ? tok.text : null;
  switch (top.t) {
    case 'start':
      return ident === 'db' ? { t: 'db', db: null, dot: false } : { t: 'other' };
    case 'db':
      if (!top.dot) return dotted(top, dot);
      return ident ? { t: 'path', db: top.db, base: null, names: [ident], dot: false, explained: false } : { t: 'other' };
    case 'path':
      if (!top.dot) return dotted(top, dot);
      // After `explain(…).` a single method name follows.
      if (!ident || (top.explained && top.names.length > 0)) return { t: 'other' };
      top.names.push(ident);
      top.dot = false;
      return top;
    case 'result':
      if (!top.dot) return dotted(top, dot);
      return ident
        ? { t: 'member', db: top.db, coll: top.coll, cursor: top.cursor, countable: top.countable, name: ident }
        : { t: 'other' };
    default:
      return { t: 'other' };
  }
}

/** `top` followed by a `.` (when `dot`), else the end of what completion follows. */
function dotted(top: Top & { dot: boolean }, dot: boolean): Top {
  if (!dot) return { t: 'other' };
  top.dot = true;
  return top;
}
