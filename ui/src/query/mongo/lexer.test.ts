import { describe, expect, it } from 'vitest';
import { lexConsole, NOT_LIKE_JS } from './lexer';

// The cases of `crates/driver-mongo/src/shell/lexer.rs` (`lex_strict`) and of `lex_script`'s
// `use` names: the formatter reads a script exactly as the console does.

const texts = (src: string) => {
  const r = lexConsole(src);
  if (r.problem) throw new Error(`${src}: ${r.problem.message} at ${r.problem.offset}`);
  return r.tokens.map((t) => `${t.kind}:${t.text}`);
};
const problem = (src: string) => lexConsole(src).problem;

describe('lexConsole', () => {
  it('lexes_tokens_with_offsets_and_keeps_the_comments', () => {
    const src = 'db.c.find({a: "x;y", é: \'z\'}) // done; really\n/* a; b */ .sort({b: -1.5e3})';
    const r = lexConsole(src);
    expect(r.problem).toBeNull();
    expect(r.tokens.map((t) => src.slice(t.start, t.end))).toEqual(
      ['db', '.', 'c', '.', 'find', '(', '{', 'a', ':', '"x;y"', ',', 'é', ':', "'z'", '}', ')', '.', 'sort', '(', '{', 'b', ':', '-', '1.5e3', '}', ')'],
    );
    expect(r.tokens.map((t) => t.text)).toEqual(r.tokens.map((t) => src.slice(t.start, t.end)));
    expect(r.comments).toEqual([
      { text: '// done; really', start: 30, end: 45, line: true },
      { text: '/* a; b */', start: 46, end: 56, line: false },
    ]);
  });

  it('strings_in_every_quote_style', () => {
    expect(texts(`'it\\'s' "a\\"b\\n\\t\\\\" \`multi\nline;\` 'é\\u{1F600}\\x41\\/\\d' "😀"`)).toEqual([
      "string:'it\\'s'",
      'string:"a\\"b\\n\\t\\\\"',
      'string:`multi\nline;`',
      "string:'é\\u{1F600}\\x41\\/\\d'",
      'string:"😀"',
    ]);
    expect(texts('`$a`')).toEqual(['string:`$a`']);
    expect(problem('x = `a${b}`')).toEqual({ offset: 6, message: 'Template literal substitutions (${…}) are not supported' });
    expect(problem("{a: 'abc\n}")).toEqual({ offset: 4, message: 'Unterminated string' });
    expect(problem("{a: 'abc\r}")).toEqual({ offset: 4, message: 'Unterminated string' });
    expect(problem('`never closed')).toEqual({ offset: 0, message: 'Unterminated template literal' });
    expect(problem("'\\uD83D'")).toEqual({ offset: 1, message: 'Invalid escape sequence' });
    expect(problem("'\\1'")).toEqual({ offset: 1, message: 'Invalid escape sequence' });
    // A line continuation, and U+2028 inside a string.
    expect(texts("'a\\\nb' 'c\u2028d'")).toEqual(["string:'a\\\nb'", "string:'c\u2028d'"]);
  });

  it('regex_versus_division', () => {
    expect(texts('/a;b/i')).toEqual(['regex:/a;b/i']);
    expect(texts('find({n: /x\\/y[/]z/m})')).toEqual(['ident:find', 'punct:(', 'punct:{', 'ident:n', 'punct::', 'regex:/x\\/y[/]z/m', 'punct:}', 'punct:)']);
    expect(texts('[/a/, /b/]')).toEqual(['punct:[', 'regex:/a/', 'punct:,', 'regex:/b/', 'punct:]']);
    expect(texts('x = /a/')).toEqual(['ident:x', 'punct:=', 'regex:/a/']);
    // After a value, `/` is division.
    expect(texts('a / b / c')).toEqual(['ident:a', 'punct:/', 'ident:b', 'punct:/', 'ident:c']);
    expect(texts('{a: x/2}')).toEqual(['punct:{', 'ident:a', 'punct::', 'ident:x', 'punct:/', 'number:2', 'punct:}']);
    // After any other punctuation it is a regex: `+`, `-`, `>`, …
    expect(texts('1 - /a(b)/')).toEqual(['number:1', 'punct:-', 'regex:/a(b)/']);
    expect(texts('d => /(a,b)/.test(d)')).toEqual(['ident:d', 'punct:=', 'punct:>', 'regex:/(a,b)/', 'punct:.', 'ident:test', 'punct:(', 'ident:d', 'punct:)']);
    expect(texts('a + +/x/')).toEqual(['ident:a', 'punct:+', 'punct:+', 'regex:/x/']);
    expect(problem('x(/abc\n/)')).toEqual({ offset: 2, message: 'Unterminated regular expression' });
  });

  it('refuses_what_javascript_reads_another_way', () => {
    for (const [src, at] of [
      ['if (a) /x/', 7],
      ['{} /x/', 3],
      ['typeof /x/', 7],
      ['return /x/', 7],
      ['a++ /x/', 4],
      ['a-- /x/', 4],
      ['a./x/', 2],
      ['f() / 2', 4],
      ['x <!-- y', 2],
      ['--> y', 0],
      ['#!x', 0],
      ['á', 1],
    ] as const) {
      expect(problem(src), src).toEqual({ offset: at, message: NOT_LIKE_JS });
    }
    for (const src of ['db.c.find({a: /x/, b: [/y/i]}, /z/)', 'a / b', '[1] / 2', "'s' / 2", 'new Date()', 'é', '// c\n/* d */ x']) {
      expect(problem(src), src).toBeNull();
    }
  });

  it('line_comments_end_at_every_line_terminator', () => {
    for (const src of ['a // b\rc', 'a // b\u2028c', 'a // b\u2029c', 'a // b\r\nc']) {
      const r = lexConsole(src);
      expect(r.tokens.map((t) => t.text), JSON.stringify(src)).toEqual(['a', 'c']);
      expect(r.comments.map((c) => c.text)).toEqual(['// b']);
    }
    // U+FEFF is whitespace, as in JavaScript.
    expect(texts('﻿a﻿')).toEqual(['ident:a']);
    expect(problem('x(/a\u2028/)')).toEqual({ offset: 2, message: 'Unterminated regular expression' });
    // A `//` comment ending the text is closed; a block comment must close.
    expect(lexConsole('a // b').comments).toEqual([{ text: '// b', start: 2, end: 6, line: true }]);
    expect(problem('a /* open')).toEqual({ offset: 2, message: 'Unterminated comment' });
  });

  it('numbers', () => {
    expect(texts('0 12 1.5 .5 1e3 2E-2 0x1F 3.0')).toEqual(['0', '12', '1.5', '.5', '1e3', '2E-2', '0x1F', '3.0'].map((n) => `number:${n}`));
    for (const bad of ['1e', '5abc', '01', '0x', '1.5.2']) expect(problem(bad), bad).toEqual({ offset: 0, message: 'Invalid number' });
    expect(texts('=>')).toEqual(['punct:=', 'punct:>']);
    expect(texts('')).toEqual([]);
    expect(texts('  // only a comment')).toEqual([]);
  });

  it('a_use_name_is_one_token_where_javascript_would_not_lex_it', () => {
    expect(texts('use 2024_logs')).toEqual(['ident:use', 'ident:2024_logs']);
    expect(texts('db.a.find()\nuse 01-x; show dbs')).toEqual([
      'ident:db', 'punct:.', 'ident:a', 'punct:.', 'ident:find', 'punct:(', 'punct:)',
      'ident:use', 'ident:01-x', 'punct:;', 'ident:show', 'ident:dbs',
    ]);
    // Not at a statement start, or not after `use` and a blank: refused.
    expect(problem('x = 01')).toEqual({ offset: 4, message: 'Invalid number' });
    expect(problem('f(use 01)')?.message).toBe('Invalid number');
    expect(problem('use\t\t2024_x')).toBeNull();
    // Lexing goes on after the name, not knowing what came before: a leading `/` may be either.
    expect(problem('use 2024_logs\n/x/')).toEqual({ offset: 14, message: NOT_LIKE_JS });
  });
});
