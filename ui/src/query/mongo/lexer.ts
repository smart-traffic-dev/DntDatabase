// How the MongoDB console reads a script, for the formatter: a port of `lex_strict` in
// `crates/driver-mongo/src/shell/lexer.rs` (tokens; comments kept here too) with the `use`
// names of `lex_script` (`crates/driver-mongo/src/shell/mod.rs`). Where the console's
// lexer stops, so does this one (`problem`): the console would read that text a line at a
// time, so the formatter refuses it. Completion keeps its own forgiving lexer
// (`query/completion/mongo/context.ts`): it reads unfinished text before the cursor.
//
// Offsets are UTF-16 code units of the source (the Rust lexer counts bytes).

export interface ConsoleToken {
  kind: 'ident' | 'string' | 'number' | 'regex' | 'punct';
  /** `src.slice(start, end)`. */
  text: string;
  start: number;
  end: number;
}

/** A comment; `line`: a `//` comment (its text stops before the line terminator). */
export interface ConsoleComment {
  text: string;
  start: number;
  end: number;
  line: boolean;
}

export interface LexProblem {
  offset: number;
  message: string;
}

/** `NOT_LIKE_JS` in lexer.rs: where JavaScript may read the text another way. */
export const NOT_LIKE_JS = 'JavaScript may read this differently';

/** Keywords after which JavaScript reads a `/` as the start of a regex (`BEFORE_EXPRESSION`). */
const BEFORE_EXPRESSION = new Set([
  'await',
  'case',
  'delete',
  'do',
  'else',
  'extends',
  'in',
  'instanceof',
  'new',
  'of',
  'return',
  'throw',
  'typeof',
  'void',
  'yield',
]);

const ALPHABETIC = /\p{Alphabetic}/u;
const ALPHANUMERIC = /[\p{Alphabetic}\p{N}]/u;
const WHITE_SPACE = /\p{White_Space}/u;

const isLineEnd = (c: string | undefined) => c === '\n' || c === '\r' || c === ' ' || c === ' ';
const isAscii = (c: string) => c.charCodeAt(0) < 0x80;
const isDigit = (c: string | undefined) => c !== undefined && c >= '0' && c <= '9';
const isHex = (c: string | undefined) => c !== undefined && /^[0-9a-fA-F]$/.test(c);
/** Rust's `char::is_alphabetic`. */
const isAlphabetic = (c: string) => (isAscii(c) ? /[A-Za-z]/.test(c) : ALPHABETIC.test(c));
const isIdentStart = (c: string) => c === '$' || c === '_' || isAlphabetic(c);
const isIdentContinue = (c: string) => c === '$' || c === '_' || (isAscii(c) ? /[A-Za-z0-9]/.test(c) : ALPHANUMERIC.test(c));
/** Whitespace as JavaScript has it: Unicode whitespace and U+FEFF. */
const isSpace = (c: string) => c === '﻿' || WHITE_SPACE.test(c);

class Fail {
  constructor(
    readonly message: string,
    readonly offset: number,
  ) {}
}

interface Lexed {
  tokens: ConsoleToken[];
  comments: ConsoleComment[];
  error: LexProblem | null;
}

/** `lex_strict(text[from..to], resumed)`, offsets in `text`. On an error, what was read before it. */
function lexStrict(text: string, from: number, to: number, resumed: boolean): Lexed {
  const out: ConsoleToken[] = [];
  const comments: ConsoleComment[] = [];
  let pos = from;
  /** The character at `i` (a whole code point), or undefined at the end. */
  const at = (i: number): string | undefined => {
    if (i >= to) return undefined;
    const cp = text.codePointAt(i)!;
    const c = String.fromCodePoint(cp);
    return i + c.length <= to ? c : text[i];
  };
  const peek = () => at(pos);
  const peek2 = () => {
    const c = at(pos);
    return c === undefined ? undefined : at(pos + c.length);
  };
  const bump = () => {
    const c = at(pos);
    if (c !== undefined) pos += c.length;
    return c;
  };
  const eatWhile = (f: (c: string) => boolean) => {
    for (let c = peek(); c !== undefined && f(c); c = peek()) pos += c.length;
  };
  const startsWith = (s: string) => pos + s.length <= to && text.startsWith(s, pos);

  const regexAllowed = () => {
    const last = out.at(-1);
    if (!last) return true;
    return last.kind === 'punct' && !')]}'.includes(last.text);
  };

  const notLikeJs = (c: string) => {
    if (startsWith('<!--') || startsWith('-->') || startsWith('#!')) return true;
    if (!isAscii(c) && !isIdentStart(c)) return true;
    if (c !== '/' || startsWith('//') || startsWith('/*')) return false;
    const n = out.length;
    const last = out.at(-1);
    if (!last) return resumed;
    if (last.kind === 'punct' && (last.text === ')' || last.text === '}' || last.text === '.')) return true;
    if (last.kind === 'ident') return BEFORE_EXPRESSION.has(last.text);
    // `a++ /x/`: JavaScript reads division.
    if (last.kind === 'punct' && (last.text === '+' || last.text === '-') && n >= 2) {
      const a = out[n - 2]!;
      return a.kind === 'punct' && a.text === last.text && a.end === last.start;
    }
    return false;
  };

  const hex = (n: number): number | null => {
    if (pos + n > to) return null;
    const digits = text.slice(pos, pos + n);
    if (![...digits].every(isHex) || digits.length !== n) return null;
    pos += n;
    return parseInt(digits, 16);
  };
  const validChar = (n: number) => n <= 0x10ffff && (n < 0xd800 || n > 0xdfff);
  const unicodeEscape = (): boolean => {
    if (peek() === '{') {
      bump();
      const start = pos;
      eatWhile(isHex);
      const digits = text.slice(start, pos);
      if (digits.length === 0 || digits.length > 6 || bump() !== '}') return false;
      return validChar(parseInt(digits, 16));
    }
    const hi = hex(4);
    if (hi === null) return false;
    if (hi < 0xd800 || hi >= 0xdc00) return validChar(hi);
    if (bump() !== '\\' || bump() !== 'u') return false;
    const lo = hex(4);
    return lo !== null && lo >= 0xdc00 && lo < 0xe000;
  };
  /** One escape after `\` (at `escAt`). */
  const escape = (escAt: number, unterminated: string, start: number) => {
    const bad = () => new Fail('Invalid escape sequence', escAt);
    const c = bump();
    if (c === undefined) throw new Fail(unterminated, start);
    if (c === '0' && !isDigit(peek())) return;
    if (isDigit(c)) throw bad();
    if (c === 'x') {
      if (hex(2) === null) throw bad();
    } else if (c === 'u') {
      if (!unicodeEscape()) throw bad();
    } else if (c === '\r') {
      if (peek() === '\n') bump();
    }
    // `\n`, `\t`, a line continuation and any other character stand as they are.
  };

  const string = (quote: string, start: number) => {
    bump();
    for (;;) {
      const escAt = pos;
      const c = bump();
      if (c === undefined || c === '\n' || c === '\r') throw new Fail('Unterminated string', start);
      if (c === quote) return;
      if (c === '\\') escape(escAt, 'Unterminated string', start);
    }
  };

  const template = (start: number) => {
    const UNTERMINATED = 'Unterminated template literal';
    bump();
    for (;;) {
      const c0 = pos;
      const c = bump();
      if (c === undefined) throw new Fail(UNTERMINATED, start);
      if (c === '`') return;
      if (c === '$' && peek() === '{') throw new Fail('Template literal substitutions (${…}) are not supported', c0);
      if (c === '\\') escape(c0, UNTERMINATED, start);
    }
  };

  const regex = (start: number) => {
    const unterminated = () => new Fail('Unterminated regular expression', start);
    bump();
    let inClass = false;
    for (;;) {
      const c = bump();
      if (c === undefined || isLineEnd(c)) throw unterminated();
      if (c === '\\') {
        const d = bump();
        if (d === undefined || isLineEnd(d)) throw unterminated();
      } else if (c === '[') inClass = true;
      else if (c === ']') inClass = false;
      else if (c === '/' && !inClass) break;
    }
    eatWhile(isIdentContinue);
  };

  const number = (start: number) => {
    const bad = () => new Fail('Invalid number', start);
    const digits = () => {
      const s = pos;
      eatWhile(isDigit);
      return pos > s;
    };
    if (peek() === '0' && (peek2() === 'x' || peek2() === 'X')) {
      pos += 2;
      const s = pos;
      eatWhile(isHex);
      if (pos === s) throw bad();
    } else {
      const int = digits();
      if (int && pos - start > 1 && text[start] === '0') throw bad();
      if (peek() === '.') {
        bump();
        const frac = digits();
        if (!int && !frac) throw bad();
      }
      if (peek() === 'e' || peek() === 'E') {
        bump();
        if (peek() === '+' || peek() === '-') bump();
        if (!digits()) throw bad();
      }
    }
    const next = peek();
    if (next !== undefined && (isIdentContinue(next) || next === '.')) throw bad();
  };

  try {
    for (let c = peek(); c !== undefined; c = peek()) {
      const start = pos;
      if (isSpace(c)) {
        bump();
        continue;
      }
      if (notLikeJs(c)) throw new Fail(NOT_LIKE_JS, start);
      const c2 = peek2();
      let kind: ConsoleToken['kind'];
      if (c === '/' && c2 === '/') {
        eatWhile((d) => !isLineEnd(d));
        comments.push({ text: text.slice(start, pos), start, end: pos, line: true });
        continue;
      } else if (c === '/' && c2 === '*') {
        const close = text.indexOf('*/', start + 2);
        if (close < 0 || close + 2 > to) throw new Fail('Unterminated comment', start);
        pos = close + 2;
        comments.push({ text: text.slice(start, pos), start, end: pos, line: false });
        continue;
      } else if (c === '/' && regexAllowed()) {
        regex(start);
        kind = 'regex';
      } else if (c === "'" || c === '"') {
        string(c, start);
        kind = 'string';
      } else if (c === '`') {
        template(start);
        kind = 'string';
      } else if (isDigit(c) || (c === '.' && isDigit(c2))) {
        number(start);
        kind = 'number';
      } else if (isIdentStart(c)) {
        eatWhile(isIdentContinue);
        kind = 'ident';
      } else {
        bump();
        kind = 'punct';
      }
      out.push({ kind, text: text.slice(start, pos), start, end: pos });
    }
  } catch (e) {
    if (!(e instanceof Fail)) throw e;
    return { tokens: out, comments, error: { offset: e.offset, message: e.message } };
  }
  return { tokens: out, comments, error: null };
}

/** `raw_name_char` in mod.rs: a character of a raw `use` name. */
function rawNameChar(c: string | undefined): boolean {
  if (c === undefined) return false;
  if (isAscii(c)) return /[A-Za-z0-9_$-]/.test(c);
  return !WHITE_SPACE.test(c) && c !== '﻿';
}

/**
 * `use_name` in mod.rs: when the error at `bad` (lexing from `pos`) is inside the name of a
 * `use` that starts a statement, the tokens of `text[pos..]` up to and including that name.
 */
function rawUseName(text: string, pos: number, bad: number, cur: ConsoleToken[]): Lexed | null {
  const charAt = (i: number) => String.fromCodePoint(text.codePointAt(i)!);
  if (bad >= text.length || !rawNameChar(charAt(bad))) return null;
  let start = bad;
  while (start > pos) {
    const prev = text.codePointAt(start - 1)!;
    // The low half of a surrogate pair: step over the whole character.
    const len = prev >= 0xdc00 && prev <= 0xdfff && start - 2 >= pos ? 2 : 1;
    if (!rawNameChar(text.slice(start - len, start))) break;
    start -= len;
  }
  let end = bad;
  while (end < text.length && rawNameChar(charAt(end))) end += charAt(end).length;
  // `use`, then at least one blank.
  let kwEnd = start;
  while (kwEnd > pos && (text[kwEnd - 1] === ' ' || text[kwEnd - 1] === '\t')) kwEnd--;
  if (kwEnd === start || kwEnd < pos + 3 || text.slice(kwEnd - 3, kwEnd) !== 'use') return null;
  const kw = kwEnd - 3;
  if (kw > 0 && /[A-Za-z0-9_$]|[^\0-\x7f]/.test(text[kw - 1]!)) return null;
  const before = lexStrict(text, pos, kw, pos > 0);
  if (before.error) return null;
  // `use` must be a token of its own (the text before it may be cut inside a comment).
  const withKw = lexStrict(text, pos, kwEnd, pos > 0);
  if (withKw.error || withKw.tokens.length !== before.tokens.length + 1 || withKw.tokens.at(-1)!.start !== kw) return null;
  // A statement start: depth 0 (as `split` counts it), after `;` or a line break.
  let depth = 0;
  for (const t of [...cur, ...before.tokens]) {
    if (t.kind !== 'punct') continue;
    if ('([{'.includes(t.text)) depth++;
    else if (')]}'.includes(t.text)) depth = Math.max(0, depth - 1);
    else if (t.text === ';') depth = 0;
  }
  const last = before.tokens.at(-1) ?? cur.at(-1);
  const starts = !last || (last.kind === 'punct' && last.text === ';') || /[\n\r]/.test(text.slice(last.end, kw));
  if (depth !== 0 || !starts) return null;
  return {
    tokens: [
      ...before.tokens,
      { kind: 'ident', text: 'use', start: kw, end: kwEnd },
      { kind: 'ident', text: text.slice(start, end), start, end },
    ],
    comments: before.comments,
    error: null,
  };
}

/** `line_start` in mod.rs: the start of the line holding `i` (lines end at LF, CR or CRLF). */
function lineStart(text: string, i: number): number {
  for (let k = Math.min(i, text.length) - 1; k >= 0; k--) {
    if (text[k] === '\n' || text[k] === '\r') return k + 1;
  }
  return 0;
}

/**
 * Up to where the console still reads `text` as the tokens `lexed` holds, when it stops
 * lexing (`lex_script` in mod.rs): the start of the line where the failing construct begins,
 * backed off to earlier line starts while the text before it does not lex, as
 * `lexable_prefix` does (a string continued over lines, a comment). From there on the console
 * reads the text as one refused command, or not at all. Conservative: it backs off from the
 * start of the text, never only to where the console resumed after a raw `use` name. The
 * whole text when it lexes.
 */
export function consoleCut(text: string, lexed: ReturnType<typeof lexConsole>): number {
  const { problem } = lexed;
  if (!problem) return text.length;
  // The failing construct begins at the first character after what was read.
  let begin = Math.max(lexed.tokens.at(-1)?.end ?? 0, lexed.comments.at(-1)?.end ?? 0);
  while (begin < problem.offset) {
    const c = String.fromCodePoint(text.codePointAt(begin)!);
    if (!isSpace(c)) break;
    begin += c.length;
  }
  let cut = lineStart(text, Math.min(begin, problem.offset));
  for (let i = 0; i < 16; i++) {
    const before = lexConsole(text.slice(0, cut)).problem;
    if (!before) return cut;
    const next = lineStart(text, before.offset);
    if (next >= cut) return 0;
    cut = next;
  }
  return 0;
}

/**
 * The tokens and comments of a script as the console reads it. `problem`: where it stops
 * (the tokens and comments before it are returned); the console would read the text from
 * that line on another way.
 */
export function lexConsole(src: string): { tokens: ConsoleToken[]; comments: ConsoleComment[]; problem: LexProblem | null } {
  const tokens: ConsoleToken[] = [];
  const comments: ConsoleComment[] = [];
  let pos = 0;
  while (pos < src.length) {
    const r = lexStrict(src, pos, src.length, pos > 0);
    if (!r.error) {
      tokens.push(...r.tokens);
      comments.push(...r.comments);
      break;
    }
    const named = rawUseName(src, pos, r.error.offset, tokens);
    if (!named) {
      tokens.push(...r.tokens);
      comments.push(...r.comments);
      return { tokens, comments, problem: r.error };
    }
    tokens.push(...named.tokens);
    comments.push(...named.comments);
    pos = named.tokens.at(-1)!.end;
  }
  return { tokens, comments, problem: null };
}
