// Statement ranges of a script, computed in the UI: a port of the backend splitter
// (`crates/core/src/sql/split.rs` + `lexer.rs`, default MySQL mode) so autocomplete never
// waits on a `statement_ranges` round trip per keystroke. One linear pass over char codes.
//
// UI ONLY: autocomplete context and the current-statement highlight. It must never decide
// what is executed or guarded: running (`runQuery` → backend `plan`/`split`) and the
// destructive-statement guard always use the backend splitter, which also knows the
// server's SQL mode (ANSI_QUOTES, NO_BACKSLASH_ESCAPES) that this port assumes off.
import type { Engine } from '../engine';

const NL = 10;
const SPACE = 32;
const TAB = 9;
const BACKSLASH = 92;

/** `lexer::is_ident_byte` on UTF-16 code units: anything non-ASCII is part of an identifier. */
function isIdentCode(c: number): boolean {
  return (
    (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95 || c === 36 || c >= 0x80
  );
}

/** ASCII whitespace as the splitter sees it (`u8::is_ascii_whitespace`). */
const isAsciiSpace = (c: number) => c === SPACE || c === TAB || c === NL || c === 13 || c === 12;

/** Whitespace as `str::trim` sees it (Unicode), with an ASCII fast path. */
const isSpaceAt = (text: string, i: number) => {
  const c = text.charCodeAt(i);
  return c < 128 ? isAsciiSpace(c) || c === 11 : /\s/.test(text.charAt(i));
};

const DELIMITER_KW = 'delimiter';

function lineEnd(text: string, i: number): number {
  const nl = text.indexOf('\n', i);
  return nl < 0 ? text.length : nl;
}

function blockCommentEnd(text: string, i: number, nested: boolean): number {
  if (!nested) {
    const close = text.indexOf('*/', i + 2);
    return close < 0 ? text.length : close + 2;
  }
  let depth = 1;
  let j = i + 2;
  const n = text.length;
  while (j < n) {
    const c = text.charCodeAt(j);
    if (c === 42 && text.charCodeAt(j + 1) === 47) {
      j += 2;
      if (--depth === 0) return j;
    } else if (c === 47 && text.charCodeAt(j + 1) === 42) {
      depth++;
      j += 2;
    } else {
      j++;
    }
  }
  return n;
}

/** `open` is the index of the opening quote. Doubled quotes always escape; backslashes when `backslash`. */
function quoteEnd(text: string, open: number, q: number, backslash: boolean): number {
  let j = open + 1;
  const n = text.length;
  while (j < n) {
    const c = text.charCodeAt(j);
    if (backslash && c === BACKSLASH) j += 2;
    else if (c === q) {
      if (text.charCodeAt(j + 1) === q) j += 2;
      else return j + 1;
    } else j++;
  }
  return n;
}

function dollarTagEnd(text: string, i: number): number | null {
  let j = i + 1;
  const c = text.charCodeAt(j);
  if ((c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95 || c >= 0x80) {
    while (j < text.length && isIdentCode(text.charCodeAt(j)) && text.charCodeAt(j) !== 36) j++;
  }
  return text.charCodeAt(j) === 36 ? j + 1 : null;
}

const prevIsIdent = (text: string, i: number) => i > 0 && isIdentCode(text.charCodeAt(i - 1));

type Skip = { comment: boolean; end: number };

/** `lexer::skip_at`: a comment, string, quoted identifier or dollar body starting at `i`. */
function skipAt(text: string, i: number, mysql: boolean): Skip | null {
  const c = text.charCodeAt(i);
  const next = text.charCodeAt(i + 1);
  switch (c) {
    case 45: // -
      if (next !== 45) return null;
      if (mysql && i + 2 < text.length && text.charCodeAt(i + 2) > SPACE) return null;
      return { comment: true, end: lineEnd(text, i) };
    case 35: // #
      return mysql ? { comment: true, end: lineEnd(text, i) } : null;
    case 47: {
      // /
      if (next !== 42) return null;
      const exec = mysql && (text.charCodeAt(i + 2) === 33 || (text.charCodeAt(i + 2) === 77 && text.charCodeAt(i + 3) === 33));
      return { comment: !exec, end: blockCommentEnd(text, i, !mysql) };
    }
    case 39: // '
      return { comment: false, end: quoteEnd(text, i, 39, mysql) };
    case 34: // "
      return { comment: false, end: quoteEnd(text, i, 34, mysql) };
    case 96: // `
      return mysql ? { comment: false, end: quoteEnd(text, i, 96, false) } : null;
    case 69:
    case 101: // E e
      if (!mysql && next === 39 && !prevIsIdent(text, i)) return { comment: false, end: quoteEnd(text, i + 1, 39, true) };
      return null;
    case 36: {
      // $
      if (mysql || prevIsIdent(text, i)) return null;
      const body = dollarTagEnd(text, i);
      if (body === null) return null;
      const close = text.indexOf(text.slice(i, body), body);
      return { comment: false, end: close < 0 ? text.length : close + (body - i) };
    }
    default:
      return null;
  }
}

/** A MySQL `DELIMITER <tok>` line at `i`: the new delimiter and the offset past its line. */
function delimiterDirective(text: string, i: number): { delim: string; next: number } | null {
  // ASCII case-insensitive match without allocating (`| 0x20` lowercases letters).
  for (let k = 0; k < 9; k++) if ((text.charCodeAt(i + k) | 0x20) !== DELIMITER_KW.charCodeAt(k)) return null;
  for (let k = i - 1; k >= 0; k--) {
    const c = text.charCodeAt(k);
    if (c === NL) break;
    if (c !== SPACE && c !== TAB) return null;
  }
  let j = i + 9;
  if (text.charCodeAt(j) !== SPACE && text.charCodeAt(j) !== TAB) return null;
  while (text.charCodeAt(j) === SPACE || text.charCodeAt(j) === TAB) j++;
  const tokStart = j;
  while (j < text.length && !isAsciiSpace(text.charCodeAt(j))) j++;
  if (j === tokStart) return null;
  const nl = text.indexOf('\n', j);
  return { delim: text.slice(tokStart, j), next: nl < 0 ? text.length : nl + 1 };
}

/** Offset of the first significant character of `text[from, to)` (`to` when only blanks and comments). */
function significantStart(text: string, from: number, to: number, mysql: boolean): number {
  let i = from;
  while (i < to) {
    if (isSpaceAt(text, i)) {
      i++;
      continue;
    }
    const s = skipAt(text, i, mysql);
    if (s && s.comment) i = Math.min(s.end, to);
    else return i;
  }
  return to;
}

function push(out: [number, number][], text: string, from: number, to: number, mysql: boolean): void {
  if (significantStart(text, from, to, mysql) >= to) return;
  let lo = from;
  let hi = to;
  while (lo < hi && isSpaceAt(text, lo)) lo++;
  while (hi > lo && isSpaceAt(text, hi - 1)) hi--;
  out.push([lo, hi]);
}

/**
 * The statements of `text` as trimmed `[start, end)` UTF-16 ranges without their delimiter,
 * like the `statement_ranges` command: strings, quoted identifiers, comments, PG dollar
 * quotes / E-strings and MySQL `DELIMITER` directives are honoured; blank fragments dropped.
 */
export function splitRanges(text: string, engine: Engine): [number, number][] {
  return splitIncremental(null, text, engine, 0).ranges;
}

/** A segment start: the splitter can resume there with `delim` and the first `nRanges` ranges. */
interface Bound {
  at: number;
  delim: string;
  nRanges: number;
}

/** The result of a split, reusable by the next `splitIncremental` of an edited text. */
export interface SplitState {
  engine: Engine;
  ranges: [number, number][];
  bounds: Bound[];
}

/**
 * `splitRanges` that reuses `prev` (the split of the text before an edit): everything before
 * the last statement boundary ahead of `changedFrom` (the smallest offset the edit touched,
 * in both texts) is kept, only the rest is scanned again. Typing near the end of a long
 * script rescans one statement instead of the whole text.
 */
export function splitIncremental(prev: SplitState | null, text: string, engine: Engine, changedFrom: number): SplitState {
  const mysql = engine !== 'postgres';
  let start: Bound = { at: 0, delim: ';', nRanges: 0 };
  let bounds: Bound[] = [start];
  let out: [number, number][] = [];
  if (prev && prev.engine === engine && changedFrom > 0) {
    // The last boundary strictly before the edit (binary search: bounds are ascending).
    let lo = 0;
    let hi = prev.bounds.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if ((prev.bounds[mid] as Bound).at < changedFrom) lo = mid;
      else hi = mid - 1;
    }
    start = prev.bounds[lo] as Bound;
    bounds = prev.bounds.slice(0, lo + 1);
    out = prev.ranges.slice(0, start.nRanges);
  }
  scan(text, mysql, start, out, bounds);
  return { engine, ranges: out, bounds };
}

function scan(text: string, mysql: boolean, start: Bound, out: [number, number][], bounds: Bound[]): void {
  const n = text.length;
  let delim = start.delim;
  let delimCode = delim.length === 1 ? delim.charCodeAt(0) : -1;
  let seg = start.at;
  let i = start.at;
  while (i < n) {
    const c = text.charCodeAt(i);
    if (isAsciiSpace(c)) {
      i++;
      continue;
    }
    if (mysql && (c === 100 || c === 68)) {
      const d = delimiterDirective(text, i);
      if (d) {
        push(out, text, seg, i, mysql);
        delim = d.delim;
        delimCode = delim.length === 1 ? delim.charCodeAt(0) : -1;
        seg = i = d.next;
        bounds.push({ at: seg, delim, nRanges: out.length });
        continue;
      }
    }
    const s = skipAt(text, i, mysql);
    if (s) i = s.end;
    else if (delimCode >= 0 ? c === delimCode : text.startsWith(delim, i)) {
      push(out, text, seg, i, mysql);
      i += delim.length;
      seg = i;
      bounds.push({ at: seg, delim, nRanges: out.length });
    } else i++;
  }
  push(out, text, seg, n, mysql);
}
