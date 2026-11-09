// Formats a MongoDB console script (Ctrl+Shift+F in a MongoDB query tab). It only changes
// whitespace (and drops trailing commas and empty statements): every token, comment and
// statement of the script stays as written, in order. The script is read as the console
// reads it (`./lexer.ts`, a port of the console's lexer; text it would read another way is
// refused). The statements are those the console runs (`split` in
// `crates/driver-mongo/src/shell/parse.rs`): a statement ends at `;`, or at the end of a line
// once its brackets are closed unless the next token starts with `.`. So a line break is
// only ever added inside brackets or before a `.`. `format.pairs.test.ts` writes examples
// that the console's own test splits and parses before and after formatting.
//
// Layout: a document, array or argument list goes on one line when it fits the print width,
// else one entry per line; a chain `db.c.find(…).sort(…).limit(…)` that does not fit puts
// each `.modifier(…)` after its first call on a line of its own.
import { consoleCut, lexConsole, type ConsoleComment as Comment, type ConsoleToken as Token } from './lexer';

export interface FormatMongoOptions {
  /** Columns a line may take, its indent included. Default 80. */
  printWidth?: number;
}

export type FormatMongoResult = { ok: true; text: string } | { ok: false; message: string; offset: number };

const INDENT = 2;

// ---------------------------------------------------------------------------------------
// Layout documents: a small Wadler / Prettier printer.
// ---------------------------------------------------------------------------------------

type Doc =
  | string
  | Doc[]
  /** A break: a space (`soft`: nothing) when its group is flat; `hard` always breaks. */
  | { k: 'line'; soft?: boolean; hard?: boolean }
  | { k: 'group'; d: Doc; brk: boolean }
  | { k: 'indent'; d: Doc }
  /** Text printed at the end of the line (a `//` comment). */
  | { k: 'suffix'; s: string }
  /** Breaks every group around it. */
  | { k: 'breakParent' };

const line: Doc = { k: 'line' };
const softline: Doc = { k: 'line', soft: true };
const hardline: Doc = { k: 'line', hard: true };
const breakParent: Doc = { k: 'breakParent' };
const group = (d: Doc): Doc => ({ k: 'group', d, brk: false });
const indent = (d: Doc): Doc => ({ k: 'indent', d });

/** Marks the groups that hold a hard break as broken; true when `doc` holds one. */
function propagate(doc: Doc): boolean {
  if (typeof doc === 'string') return false;
  if (Array.isArray(doc)) {
    let any = false;
    for (const d of doc) any = propagate(d) || any;
    return any;
  }
  switch (doc.k) {
    case 'line':
      return doc.hard === true;
    case 'breakParent':
      return true;
    case 'group':
      doc.brk = propagate(doc.d) || doc.brk;
      return doc.brk;
    case 'indent':
      return propagate(doc.d);
    case 'suffix':
      return false;
  }
}

/** Display columns: characters, not UTF-16 units. */
function width(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    // The second half of a surrogate pair is the same character.
    if (c < 0xdc00 || c > 0xdfff) n++;
  }
  return n;
}

type Mode = 'flat' | 'break';
type Cmd = [indent: number, mode: Mode, doc: Doc];

/** Whether `next` fits in `rest` columns, followed by `stack` up to its first line break. */
function fits(next: Cmd, stack: Cmd[], rest: number): boolean {
  const cmds: Cmd[] = [next];
  let from = stack.length;
  while (rest >= 0) {
    if (cmds.length === 0) {
      if (from === 0) return true;
      cmds.push(stack[--from]!);
      continue;
    }
    const [ind, mode, doc] = cmds.pop()!;
    if (typeof doc === 'string') {
      const nl = doc.indexOf('\n');
      if (nl >= 0) return rest - width(doc.slice(0, nl)) >= 0;
      rest -= width(doc);
    } else if (Array.isArray(doc)) {
      for (let i = doc.length - 1; i >= 0; i--) cmds.push([ind, mode, doc[i]!]);
    } else {
      switch (doc.k) {
        case 'line':
          if (mode === 'break' || doc.hard) return true;
          if (!doc.soft) rest -= 1;
          break;
        case 'group':
          cmds.push([ind, doc.brk ? 'break' : mode, doc.d]);
          break;
        case 'indent':
          cmds.push([ind + INDENT, mode, doc.d]);
          break;
        case 'suffix':
        case 'breakParent':
          break;
      }
    }
  }
  return false;
}

function print(doc: Doc, printWidth: number, eol: string): string {
  propagate(doc);
  const out: string[] = [];
  let pos = 0;
  let suffixes: string[] = [];
  /** Nothing but the indent on the current line yet. */
  let atLineStart = true;
  const trimEnd = () => {
    while (out.length > 0) {
      const last = out[out.length - 1]!;
      const trimmed = last.replace(/[ \t]+$/, '');
      if (trimmed !== '') {
        out[out.length - 1] = trimmed;
        return;
      }
      out.pop();
    }
  };
  const stack: Cmd[] = [[0, 'break', doc]];
  while (stack.length > 0) {
    const [ind, mode, d] = stack.pop()!;
    if (typeof d === 'string') {
      out.push(d);
      const nl = d.lastIndexOf('\n');
      pos = nl >= 0 ? width(d.slice(nl + 1)) : pos + width(d);
      if (d !== '') atLineStart = false;
    } else if (Array.isArray(d)) {
      for (let i = d.length - 1; i >= 0; i--) stack.push([ind, mode, d[i]!]);
    } else {
      switch (d.k) {
        case 'line':
          if (mode === 'flat' && !d.hard) {
            if (!d.soft) {
              out.push(' ');
              pos++;
            }
          } else if (suffixes.length > 0) {
            // The end of the line: its `//` comment first (alone on it if it has nothing else).
            // A line has one `//` comment: the others start the next lines, at the indent of
            // what follows, as comments before its first token. (No line break is added
            // between two tokens: they all come at this one.)
            stack.push([ind, mode, d]);
            const [first, ...rest] = suffixes;
            suffixes = [];
            out.push(atLineStart ? first!.trimStart() : first!);
            for (const s of rest) {
              trimEnd();
              out.push(eol + ' '.repeat(ind), s.trimStart());
            }
            atLineStart = false;
          } else {
            trimEnd();
            out.push(eol + ' '.repeat(ind));
            pos = ind;
            atLineStart = true;
          }
          break;
        case 'group':
          if (mode === 'flat' && !d.brk) stack.push([ind, 'flat', d.d]);
          else if (d.brk) stack.push([ind, 'break', d.d]);
          else {
            const flat: Cmd = [ind, 'flat', d.d];
            stack.push(fits(flat, stack, printWidth - pos) ? flat : [ind, 'break', d.d]);
          }
          break;
        case 'indent':
          stack.push([ind + INDENT, mode, d.d]);
          break;
        case 'suffix':
          suffixes.push(d.s);
          break;
        case 'breakParent':
          break;
      }
    }
  }
  // The end of the text: as at a line break, at the indent of the top level.
  suffixes.forEach((s, i) => {
    if (i > 0) {
      trimEnd();
      out.push(eol);
    }
    out.push(i > 0 || atLineStart ? s.trimStart() : s);
  });
  trimEnd();
  return out.join('');
}

// ---------------------------------------------------------------------------------------
// The script as statements of terms.
// ---------------------------------------------------------------------------------------

/** A comment before a token on a line of its own; `newlineAfter`: a line break follows it. */
interface Leading {
  c: Comment;
  newlineAfter: boolean;
}

/** A token with the comments on its line after it (`trail`) and on the lines before it (`lead`). */
interface Tok {
  tok: Token;
  lead: Leading[];
  trail: Comment[];
}

type Term = { t: 'tok'; tok: Tok } | { t: 'group'; open: Tok; items: Item[]; close: Tok };

interface Item {
  terms: Term[];
  /** The `,` after the entry (none after the last one). */
  comma: Tok | null;
}

const OPENERS: Record<string, string> = { '(': ')', '[': ']', '{': '}' };
const CLOSERS = new Set([')', ']', '}']);

const isPunct = (t: Token, p: string) => t.kind === 'punct' && t.text === p;

class Refusal {
  constructor(
    readonly message: string,
    readonly offset: number,
  ) {}
}

/** Brackets must pair up within each statement (the console ends one at `;`, even inside brackets). */
function checkBrackets(tokens: Token[]): void {
  const open: Token[] = [];
  for (const t of tokens) {
    if (t.kind !== 'punct') continue;
    if (t.text === ';' && open.length > 0) break;
    if (OPENERS[t.text]) open.push(t);
    else if (CLOSERS.has(t.text)) {
      const o = open.pop();
      if (!o || OPENERS[o.text] !== t.text) throw new Refusal(`Unexpected '${t.text}'`, t.start);
    }
  }
  const o = open.at(-1);
  if (o) throw new Refusal(`Unclosed '${o.text}'`, o.start);
}

interface Statement {
  /** Token indexes `[from, to)`, the `;` excluded. */
  from: number;
  to: number;
  semi: number | null;
}

/** The statements as the console splits them (`token_spans` in parse.rs); a lone `;` is none. */
function statements(src: string, tokens: Token[]): Statement[] {
  const out: Statement[] = [];
  let first = -1;
  let depth = 0;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (isPunct(t, ';')) {
      if (first >= 0) out.push({ from: first, to: i, semi: i });
      first = -1;
      depth = 0;
      continue;
    }
    if (first < 0) first = i;
    if (t.kind === 'punct' && OPENERS[t.text]) depth++;
    else if (t.kind === 'punct' && CLOSERS.has(t.text)) depth = Math.max(0, depth - 1);
    if (depth > 0) continue;
    const next = tokens[i + 1];
    const ends = !next || (/[\n\r]/.test(src.slice(t.end, next.start)) && !next.text.startsWith('.'));
    if (ends) {
      out.push({ from: first, to: i + 1, semi: null });
      first = -1;
    }
  }
  if (first >= 0) out.push({ from: first, to: tokens.length, semi: null });
  return out;
}

/** Index of the first token starting at or after `offset` (binary search). */
function tokenAt(tokens: { start: number }[], offset: number): number {
  let lo = 0;
  let hi = tokens.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (tokens[mid]!.start < offset) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/**
 * The top level of the script: statements and the comments on lines of their own between
 * them. `blank`: an empty line before it; `sameLine`: it goes on the line of the entry before
 * (a `/* … *\/` comment that code follows on its line).
 */
type EntryBody = { e: 'comment'; c: Comment } | { e: 'statement'; terms: Term[]; first: Tok; last: Tok; semi: Tok | null };
type Entry = EntryBody & { blank: boolean; sameLine: boolean };

function buildEntries(src: string, tokens: Token[], comments: Comment[]): Entry[] {
  const stmts = statements(src, tokens);
  // The printed tokens: those of the statements and their `;` (a lone `;` goes).
  const kept: Token[] = [];
  for (const s of stmts) {
    for (let i = s.from; i < s.to; i++) kept.push(tokens[i]!);
    if (s.semi !== null) kept.push(tokens[s.semi]!);
  }
  const toks = new Map<Token, Tok>(kept.map((t) => [t, { tok: t, lead: [], trail: [] }]));
  // Comments with no token after them, on lines of their own.
  const endComments: Comment[] = [];
  // A comment is on the line of the token before it when no line break comes between them
  // outside other comments (after a `/* … */` over lines, it is where that comment ends).
  const parts: string[] = [];
  let at = 0;
  for (const c of comments) {
    parts.push(src.slice(at, c.start), ' '.repeat(c.end - c.start));
    at = c.end;
  }
  const code = parts.join('') + src.slice(at);
  for (const c of comments) {
    const k = tokenAt(kept, c.end);
    const prev = kept[k - 1];
    if (prev && !code.slice(prev.end, c.start).includes('\n')) toks.get(prev)!.trail.push(c);
    else if (k < kept.length) toks.get(kept[k]!)!.lead.push({ c, newlineAfter: false });
    else endComments.push(c);
  }
  for (const t of toks.values()) {
    t.lead.forEach((l, i) => {
      const nextStart = i + 1 < t.lead.length ? t.lead[i + 1]!.c.start : t.tok.start;
      l.newlineAfter = l.c.line || src.slice(l.c.end, nextStart).includes('\n');
    });
  }

  const entries: Entry[] = [];
  let prevEnd = 0;
  let prevInline = false;
  const push = (entry: EntryBody, start: number, end: number) => {
    const gap = src.slice(prevEnd, start);
    const blank = entries.length > 0 && (gap.match(/\n/g)?.length ?? 0) >= 2;
    entries.push({ ...entry, blank, sameLine: prevInline });
    prevEnd = end;
  };
  const pushComments = (list: { c: Comment; newlineAfter: boolean }[]) => {
    for (const { c, newlineAfter } of list) {
      push({ e: 'comment', c }, c.start, c.end);
      prevInline = !newlineAfter;
    }
  };
  for (const s of stmts) {
    const first = toks.get(tokens[s.from]!)!;
    pushComments(first.lead);
    first.lead = [];
    const last = toks.get(tokens[s.to - 1]!)!;
    const semi = s.semi === null ? null : toks.get(tokens[s.semi]!)!;
    const end = semi ?? last;
    push({ e: 'statement', terms: parseTerms(tokens, s.from, s.to, toks), first, last, semi }, first.tok.start, end.trail.at(-1)?.end ?? end.tok.end);
    prevInline = false;
  }
  pushComments(endComments.map((c) => ({ c, newlineAfter: true })));
  return entries;
}

/** The last token of `terms` (a group's closing bracket). */
function lastOf(terms: Term[]): Tok | null {
  const t = terms.at(-1);
  if (!t) return null;
  return t.t === 'tok' ? t.tok : t.close;
}

function firstOf(terms: Term[]): Tok | null {
  const t = terms[0];
  if (!t) return null;
  return t.t === 'tok' ? t.tok : t.open;
}

/** The terms of `tokens[from, to)`: brackets are checked to pair up already. */
function parseTerms(tokens: Token[], from: number, to: number, toks: Map<Token, Tok>): Term[] {
  let i = from;
  const tok = (k: number) => toks.get(tokens[k]!)!;
  const seq = (stop: (t: Token) => boolean): Term[] => {
    const terms: Term[] = [];
    while (i < to && !stop(tokens[i]!)) {
      const t = tokens[i]!;
      if (t.kind === 'punct' && OPENERS[t.text]) {
        const open = tok(i++);
        const items: Item[] = [];
        const close = OPENERS[t.text]!;
        while (i < to && !isPunct(tokens[i]!, close)) {
          const terms = seq((u) => isPunct(u, ',') || isPunct(u, close));
          const comma = i < to && isPunct(tokens[i]!, ',') ? tok(i++) : null;
          items.push({ terms, comma });
        }
        terms.push({ t: 'group', open, items, close: tok(i++) });
      } else terms.push({ t: 'tok', tok: tok(i++) });
    }
    return terms;
  };
  return seq(() => false);
}

// ---------------------------------------------------------------------------------------
// Terms to layout.
// ---------------------------------------------------------------------------------------

const isClose = (t: Token) => t.kind === 'punct' && CLOSERS.has(t.text);
/** What may come right before a `.` member, a call or an index. */
const isOperand = (t: Token) => t.kind === 'ident' || t.kind === 'string' || t.kind === 'regex' || t.kind === 'number' || isClose(t);

/**
 * Whether a space goes between the tokens `a` and `b` (comments aside). Only these pairs lose
 * the space they had, none of which the console reads another way once joined; every other
 * pair keeps whether it had one (`x/2`, `a + +/x/`, `1 .toString()`).
 */
function spaced(a: Token, b: Token): boolean {
  if (isPunct(b, '.') && a.kind !== 'number' && isOperand(a)) return false;
  if (isPunct(a, '.') && b.kind === 'ident') return false;
  if (isPunct(b, ':') || isPunct(b, ',') || isPunct(b, ';')) return false;
  if (isPunct(a, ':') || isPunct(a, ',')) return true;
  if ((isPunct(b, '(') || isPunct(b, '[')) && (a.kind === 'ident' || isClose(a))) return false;
  return a.end < b.start;
}

const hasInlineTrail = (t: Tok) => t.trail.some((c) => !c.line);

/**
 * The comments on the line after a token: `/* … *\/` in place, `//` at the line end.
 * `tight`: no space before a first `/* … *\/` (after `(` or `[`).
 */
function trailDoc(t: Tok, tight = false): Doc[] {
  return t.trail.map((c, i) =>
    c.line ? [{ k: 'suffix', s: ' ' + c.text } as Doc, breakParent] : (tight && i === 0 ? '' : ' ') + c.text,
  );
}

/** The comments on lines before a token; `atLineStart`: the token begins an entry, so a line is already open. */
function leadDoc(t: Tok, atLineStart: boolean): Doc[] {
  if (t.lead.length === 0) return [];
  const out: Doc[] = atLineStart ? [] : [hardline];
  for (const l of t.lead) out.push(l.c.text, l.newlineAfter ? hardline : ' ');
  return out;
}

/** A token with its comments. */
function tokDoc(t: Tok, atLineStart: boolean): Doc[] {
  return [...leadDoc(t, atLineStart), t.tok.text, ...trailDoc(t)];
}

/** Whether an entry holds a bracket: a call whose arguments are all plain never breaks. */
const hasGroup = (item: Item) => item.terms.some((t) => t.t === 'group');

function groupDoc(g: Extract<Term, { t: 'group' }>): Doc {
  const kind = g.open.tok.text;
  const items = g.items.map((it) => ({ ...it }));
  const close: Tok = { ...g.close, lead: [...g.close.lead] };
  // A `,` that starts its line (after comments on lines of their own) goes back right after
  // the value; its comments go, in order, before what follows it.
  items.forEach((it, idx) => {
    const comma = it.comma;
    if (!comma || comma.lead.length === 0) return;
    const moved: Leading[] = [...comma.lead, ...comma.trail.map((c) => ({ c, newlineAfter: c.line }))];
    const next = items[idx + 1];
    const nextFirst = next ? firstOf(next.terms) : null;
    if (next && nextFirst) next.terms = replaceFirst(next.terms, { ...nextFirst, lead: [...moved, ...nextFirst.lead] });
    else if (!next) close.lead = [...moved, ...close.lead];
    // Before a hole (`[1\n// x\n, , 2]`) the `,` keeps its comments.
    else return;
    it.comma = { ...comma, lead: [], trail: [] };
  });
  // A trailing comma goes (not after a hole, `[1, ,]`): the comments on its line stay after
  // the last entry's last token.
  const last = items.at(-1);
  const lastTok = last ? lastOf(last.terms) : null;
  if (last?.comma && lastTok) {
    last.terms = replaceLast(last.terms, { ...lastTok, trail: [...lastTok.trail, ...last.comma.trail] });
    last.comma = null;
  }
  // `(/* c */ 1)`, `[/* c */]`, but `{ /* c */ }`.
  const openDoc = [...leadDoc(g.open, false), g.open.tok.text, ...trailDoc(g.open, kind !== '{')];
  const closeDoc = g.close.tok.text;
  if (items.length === 0 && close.lead.length === 0 && g.open.trail.length === 0) return [openDoc, closeDoc];

  const entries: Doc[] = [];
  items.forEach((it, idx) => {
    entries.push(termsDoc(it.terms));
    if (it.comma) {
      entries.push(tokDoc(it.comma, false));
      if (idx + 1 < items.length) entries.push(line);
    }
  });
  const closeLead: Doc[] = close.lead.flatMap((l) => [hardline, l.c.text]);
  // `{ a: 1 }`, `[1, 2]`, `(a, b)`; a `/* … */` comment after `(` or `[` keeps a space after
  // it, flat too.
  const pad = kind === '{' || hasInlineTrail(g.open) ? line : softline;
  const padClose = kind === '{' ? line : softline;
  return group([openDoc, indent([...(items.length ? [pad, ...entries] : []), ...closeLead]), padClose, closeDoc]);
}

function replaceFirst(terms: Term[], tok: Tok): Term[] {
  const out = [...terms];
  const t = out[0]!;
  out[0] = t.t === 'tok' ? { t: 'tok', tok } : { ...t, open: tok };
  return out;
}

function replaceLast(terms: Term[], tok: Tok): Term[] {
  const out = [...terms];
  const t = out.at(-1)!;
  out[out.length - 1] = t.t === 'tok' ? { t: 'tok', tok } : { ...t, close: tok };
  return out;
}

/** A call's argument list: one document or array as the only argument hugs the parentheses. */
function argsDoc(g: Extract<Term, { t: 'group' }>): Doc {
  if (!g.items.some(hasGroup)) return flatArgs(g);
  const only = g.items.length === 1 ? g.items[0]! : null;
  const arg = only?.terms.length === 1 ? only.terms[0]! : null;
  const plainParens = g.open.trail.length === 0 && g.close.lead.length === 0 && !only?.comma;
  if (arg && arg.t === 'group' && arg.open.tok.text !== '(' && arg.open.lead.length === 0 && plainParens) {
    return [g.open.tok.text, groupDoc(arg), ...trailDoc(arg.close), g.close.tok.text];
  }
  return groupDoc(g);
}

/** Arguments without brackets (`limit(10)`, `ObjectId("…")`): always on one line. */
function flatArgs(g: Extract<Term, { t: 'group' }>): Doc {
  const doc = groupDoc(g);
  return hasComments(g) ? doc : flatten(doc);
}

function hasComments(g: Extract<Term, { t: 'group' }>): boolean {
  const toks: Tok[] = [g.open, g.close];
  for (const it of g.items) {
    if (it.comma) toks.push(it.comma);
    for (const t of it.terms) {
      if (t.t === 'tok') toks.push(t.tok);
      else if (hasComments(t)) return true;
    }
  }
  return toks.some((t) => t.lead.length > 0 || t.trail.length > 0);
}

/** `doc` as it prints when flat. */
function flatten(doc: Doc): Doc {
  if (typeof doc === 'string') return doc;
  if (Array.isArray(doc)) return doc.map(flatten);
  switch (doc.k) {
    case 'line':
      return doc.soft ? '' : ' ';
    case 'group':
    case 'indent':
      return flatten(doc.d);
    default:
      return doc;
  }
}

const isCallee = (t: Term | undefined) => t !== undefined && (t.t === 'group' ? t.open.tok.text !== '{' : t.tok.tok.kind === 'ident');
const isParens = (t: Term) => t.t === 'group' && t.open.tok.text === '(';

/** A sequence of terms: an entry, or a statement. A chain after its first call may break before each `.`. */
function termsDoc(terms: Term[]): Doc {
  if (terms.length === 0) return '';
  const firstCall = terms.findIndex((t, i) => isParens(t) && isCallee(terms[i - 1]));
  const cuts: number[] = [];
  if (firstCall > 0) {
    for (let i = firstCall + 1; i < terms.length; i++) {
      const t = terms[i]!;
      const prev = terms[i - 1]!;
      if (t.t === 'tok' && isPunct(t.tok.tok, '.') && prev.t === 'group' && prev.open.tok.text !== '{') cuts.push(i);
    }
  }
  if (cuts.length === 0) return run(terms, 0, terms.length);
  const parts: Doc[] = [run(terms, 0, cuts[0]!)];
  const segments: Doc[] = [];
  cuts.forEach((c, k) => {
    const prev = lastOf(terms.slice(0, c))!;
    const dot = (terms[c] as Extract<Term, { t: 'tok' }>).tok;
    const inline = hasInlineTrail(prev) || dot.lead.some((l) => !l.newlineAfter);
    segments.push(inline ? line : softline, run(terms, c, cuts[k + 1] ?? terms.length));
  });
  parts.push(indent(segments));
  return group(parts);
}

/** Terms `[from, to)` side by side; the first one begins an entry (or a line of a chain). */
function run(terms: Term[], from: number, to: number): Doc {
  const out: Doc[] = [];
  for (let i = from; i < to; i++) {
    const t = terms[i]!;
    const first = t.t === 'tok' ? t.tok : t.open;
    if (i > from) {
      const prev = lastOf([terms[i - 1]!])!;
      // Comments on lines before a token start a line of their own.
      if (first.lead.length === 0 && (hasInlineTrail(prev) || spaced(prev.tok, first.tok))) out.push(' ');
    }
    const lineStart = i === from;
    if (t.t === 'tok') out.push(tokDoc(t.tok, lineStart));
    else {
      const callee = isCallee(terms[i - 1]);
      const lead = leadDoc(t.open, lineStart);
      const g: Extract<Term, { t: 'group' }> = { ...t, open: { ...t.open, lead: [] } };
      out.push(lead, callee && t.open.tok.text === '(' ? argsDoc(g) : groupDoc(g), ...trailDoc(t.close));
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------
// Entry points.
// ---------------------------------------------------------------------------------------

/** `text` with every line break as `\n`, and a map from its offsets back to `text`'s. */
function normalize(text: string): { src: string; original(offset: number): number } {
  const crlf: number[] = [];
  const src = text.replace(/\r\n?/g, (m, at: number) => {
    if (m.length === 2) crlf.push(at - crlf.length);
    return '\n';
  });
  return { src, original: (offset) => offset + crlf.filter((p) => p < offset).length };
}

/**
 * Formats a MongoDB console script. A script that does not lex, or whose brackets do not pair
 * up within a statement, is refused with the offset (in `text`) of the cause. Line breaks
 * stay those of `text` (`\r\n` when it has them).
 */
export function formatMongo(text: string, opts?: FormatMongoOptions): FormatMongoResult {
  if (text.trim() === '') return { ok: true, text };
  const { src, original } = normalize(text);
  const { tokens, comments, problem } = lexConsole(src);
  if (problem) return { ok: false, message: problem.message, offset: original(problem.offset) };
  try {
    checkBrackets(tokens);
  } catch (e) {
    if (e instanceof Refusal) return { ok: false, message: e.message, offset: original(e.offset) };
    throw e;
  }
  if (src !== text) {
    // A token or comment over lines keeps its own line breaks.
    for (const t of [...tokens, ...comments]) if (t.text.includes('\n')) t.text = text.slice(original(t.start), original(t.end));
  }
  for (const c of comments) {
    // A `//` comment that U+2028 / U+2029 ends goes with it, in place: a line break of the
    // layout after it would end a command there.
    const end = src[c.end];
    if (c.line && (end === '\u2028' || end === '\u2029')) {
      c.text += end;
      c.line = false;
    }
  }
  const entries = buildEntries(src, tokens, comments);
  const docs: Doc[] = [];
  entries.forEach((entry, i) => {
    if (i > 0) docs.push(entry.sameLine ? ' ' : entry.blank ? [hardline, hardline] : hardline);
    if (entry.e === 'comment') docs.push(entry.c.text);
    else {
      // The comments on the statement's last line go after its `;`.
      // (Before a `;` there are only `/* … */` comments: they stay in place.)
      const end = entry.semi ?? entry.last;
      const trail = end.trail;
      end.trail = [];
      docs.push(
        rawStatement(entry, text, original) ?? termsDoc(entry.terms),
        entry.semi ? ';' : '',
        ...trail.map((c): Doc => (c.line ? { k: 'suffix', s: ' ' + c.text } : ' ' + c.text)),
      );
    }
  });
  const eol = text.includes('\r\n') ? '\r\n' : '\n';
  const out = print(docs, opts?.printWidth ?? 80, eol);
  return { ok: true, text: /\n[ \t]*$/.test(src) ? out + eol : out };
}

/**
 * After `use` / `show` the console reads raw text (`parse_use` in parse.rs): such a statement
 * stays as written, but for one blank after its keyword. Null for any other statement.
 */
function rawStatement(entry: Extract<Entry, { e: 'statement' }>, text: string, original: (offset: number) => number): string | null {
  const first = entry.first.tok;
  if (first.kind !== 'ident' || (first.text !== 'use' && first.text !== 'show')) return null;
  const end = original(entry.last.tok.end);
  const second = entry.terms[1];
  const next = second ? (second.t === 'tok' ? second.tok : second.open) : null;
  if (next && next.lead.length === 0 && entry.first.trail.length === 0) {
    const from = original(next.tok.start);
    if (/^[ \t]+$/.test(text.slice(original(first.end), from))) return `${first.text} ${text.slice(from, end)}`;
  }
  return text.slice(original(first.start), end);
}

export type FormatMongoRangeResult = { ok: true; from: number; to: number; text: string } | { ok: false; message: string; offset: number };

/**
 * Format with a selection: the whole statements the selection `[from, to)` of `text` touches
 * (the console's statements, with their `;`), formatted. The result replaces exactly
 * `[from, to)` of the result, from the first statement's first token to the last one's last:
 * never a part of a comment, so commented-out code stays a comment. Null when the selection
 * touches no statement (it lies in comments or blank space). Refused, as the whole script is,
 * when the script stops lexing and these statements reach the line where the console's
 * reading stops (`consoleCut`), and when they would read another way without the text before
 * them (a leading `/`).
 */
export function formatMongoRange(text: string, from: number, to: number, opts?: FormatMongoOptions): FormatMongoRangeResult | null {
  const lexed = lexConsole(text);
  const { tokens, comments, problem } = lexed;
  // From here on the console reads the text another way (a line it cannot lex is one refused
  // command): no formatted statement may reach it.
  const cut = consoleCut(text, lexed);
  // The text that does not lex, as one token of its statement.
  if (problem) tokens.splice(tokenAt(tokens, problem.offset), Infinity, { kind: 'punct', text: '', start: problem.offset, end: text.length });
  let lo = Infinity;
  let hi = -Infinity;
  let bad = false;
  for (const s of statements(text, tokens)) {
    const start = tokens[s.from]!.start;
    const end = tokens[s.semi ?? s.to - 1]!.end;
    const touches = from === to ? start <= from && from <= end : start < to && end > from;
    if (!touches) continue;
    lo = Math.min(lo, start);
    hi = Math.max(hi, end);
    if (problem && s.to === tokens.length) bad = true;
  }
  if (lo > hi) return null;
  if (problem && (bad || hi > cut)) return { ok: false, message: problem.message, offset: problem.offset };
  // On their own, the statements must read as they do in the script.
  const own = lexConsole(text.slice(lo, hi));
  const inScript = tokens.filter((t) => t.start >= lo && t.end <= hi);
  const ownComments = comments.filter((c) => c.start >= lo && c.end <= hi);
  const same =
    !own.problem &&
    own.tokens.length === inScript.length &&
    own.tokens.every((t, i) => t.start + lo === inScript[i]!.start && t.end + lo === inScript[i]!.end) &&
    own.comments.length === ownComments.length;
  if (!same) return { ok: false, message: 'This selection reads differently without the text before it', offset: lo };
  const r = formatMongo(text.slice(lo, hi), opts);
  return r.ok ? { ok: true, from: lo, to: hi, text: r.text } : { ok: false, message: r.message, offset: lo + r.offset };
}
