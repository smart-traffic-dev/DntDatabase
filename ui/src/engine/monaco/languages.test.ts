import type * as Monaco from 'monaco-editor';
import { describe, expect, it, vi } from 'vitest';
import { registerCommandLanguages } from '.';
import { mongoLanguage } from './mongo';
import { redisLanguage } from './redis';

// Monaco's real tokenizer does not load in jsdom: this driver runs the subset of Monarch the two
// definitions use (rules `[regex, action, next?]`, group actions, `cases` over keyword lists,
// `include`, `@push` / `@pop` / `@state`, `^` rules only at the line start, `ignoreCase`) and
// merges adjacent tokens of one type, as Monaco does.
type Lang = Monaco.languages.IMonarchLanguage;
type Action = Monaco.languages.IMonarchLanguageAction;
type Rule = Monaco.languages.IMonarchLanguageRule;

function tokenize(lang: Lang, text: string): [string, string][] {
  const out: [string, string][] = [];
  const emit = (value: string, type: string) => {
    if (!value) return;
    const last = out.at(-1);
    if (last && last[1] === type) last[0] += value;
    else out.push([value, type]);
  };
  const flags = lang.ignoreCase ? 'i' : '';
  const inList = (name: string, word: string) => {
    const list = (lang as unknown as Record<string, string[]>)[name] ?? [];
    return lang.ignoreCase ? list.some((w) => w.toLowerCase() === word.toLowerCase()) : list.includes(word);
  };
  const resolve = (action: Action, matched: string): { token: string; next?: string } => {
    if (typeof action === 'string') return { token: action };
    const a = action as { token?: string; next?: string; cases?: Record<string, Action> };
    if (a.cases) {
      for (const [key, value] of Object.entries(a.cases)) {
        if (key === '@default' || (key.startsWith('@') && inList(key.slice(1), matched))) return resolve(value, matched);
      }
      throw new Error(`no case for ${matched}`);
    }
    return { token: a.token ?? '', next: a.next };
  };
  const rulesOf = (state: string): Rule[] =>
    (lang.tokenizer[state] ?? []).flatMap((r) =>
      'include' in (r as object) ? rulesOf((r as { include: string }).include.replace(/^@/, '')) : [r as Rule],
    );
  const stack = ['root'];
  for (const line of text.split('\n')) {
    let pos = 0;
    while (pos < line.length) {
      const state = stack.at(-1)!;
      let advanced = false;
      for (const rule of rulesOf(state)) {
        const [regex, action, nextArg] = rule as [RegExp, Action | Action[], string?];
        if (regex.source.startsWith('^') && pos > 0) continue;
        const m = new RegExp(`^(?:${regex.source})`, flags).exec(line.slice(pos));
        if (!m || m[0].length === 0) continue;
        let next = nextArg;
        if (Array.isArray(action)) {
          action.forEach((a, i) => emit(m[i + 1] ?? '', resolve(a, m[i + 1] ?? '').token));
        } else {
          const r = resolve(action, m[0]);
          emit(m[0], r.token);
          next ??= r.next;
        }
        if (next === '@pop') stack.pop();
        else if (next && next !== '@push') stack.push(next.replace(/^@/, ''));
        else if (next === '@push') stack.push(state);
        pos += m[0].length;
        advanced = true;
        break;
      }
      if (!advanced) {
        emit(line[pos]!, lang.defaultToken ?? '');
        pos++;
      }
    }
  }
  return out;
}

/** The tokens with a type (whitespace and plain text dropped). */
const typed = (lang: Lang, text: string) => tokenize(lang, text).filter(([, t]) => t !== '');

describe('Command family Monarch languages', () => {
  it('mongo_tokens', () => {
    expect(typed(mongoLanguage, 'db.orders.find({ $gt: 1 }) // c')).toEqual([
      ['db', 'keyword'],
      ['.', 'delimiter'],
      ['orders', 'identifier'],
      ['.', 'delimiter'],
      ['find', 'type.identifier'],
      ['(', 'delimiter.parenthesis'],
      ['{', 'delimiter.bracket'],
      ['$gt', 'keyword.operator'],
      [':', 'delimiter'],
      ['1', 'number'],
      ['}', 'delimiter.bracket'],
      [')', 'delimiter.parenthesis'],
      ['// c', 'comment'],
    ]);
  });

  it('mongo_strings_comments_and_predefined', () => {
    expect(typed(mongoLanguage, `db.c.insertOne({ _id: ObjectId('a\\'b'), at: ISODate("2024"), s: \`x\` })`)).toEqual([
      ['db', 'keyword'],
      ['.', 'delimiter'],
      ['c', 'identifier'],
      ['.', 'delimiter'],
      ['insertOne', 'type.identifier'],
      ['(', 'delimiter.parenthesis'],
      ['{', 'delimiter.bracket'],
      ['_id', 'identifier'],
      [':', 'delimiter'],
      ['ObjectId', 'predefined'],
      ['(', 'delimiter.parenthesis'],
      ["'a", 'string'],
      ["\\'", 'string.escape'],
      ["b'", 'string'],
      [')', 'delimiter.parenthesis'],
      [',', 'delimiter'],
      ['at', 'identifier'],
      [':', 'delimiter'],
      ['ISODate', 'predefined'],
      ['(', 'delimiter.parenthesis'],
      ['"2024"', 'string'],
      [')', 'delimiter.parenthesis'],
      [',', 'delimiter'],
      ['s', 'identifier'],
      [':', 'delimiter'],
      ['`x`', 'string'],
      ['}', 'delimiter.bracket'],
      [')', 'delimiter.parenthesis'],
    ]);
    // A block comment spans lines.
    expect(typed(mongoLanguage, '/* a\nb */ db')).toEqual([
      ['/* ab */', 'comment'],
      ['db', 'keyword'],
    ]);
  });

  it('redis_first_word_is_keyword', () => {
    expect(typed(redisLanguage, 'get user:1\nSET "a b" \'c\\\'d\' 10 # note\nHGetAll get')).toEqual([
      ['get', 'keyword'],
      ['user:1', 'identifier'],
      ['SET', 'keyword'],
      ['"a b"', 'string'],
      ["'c", 'string'],
      ["\\'", 'string.escape'],
      ["d'", 'string'],
      ['10', 'number'],
      ['# note', 'comment'],
      ['HGetAll', 'keyword'],
      // Only the first word of a line is the command.
      ['get', 'identifier'],
    ]);
    // A comment line has no command.
    expect(typed(redisLanguage, '# all of it')).toEqual([['# all of it', 'comment']]);
  });

  it('registers_both_languages_once_per_monaco', () => {
    const register = vi.fn();
    const setMonarchTokensProvider = vi.fn();
    const setLanguageConfiguration = vi.fn();
    const monaco = { languages: { register, setMonarchTokensProvider, setLanguageConfiguration } } as unknown as typeof Monaco;
    registerCommandLanguages(monaco);
    registerCommandLanguages(monaco);
    expect(register.mock.calls.map(([l]) => l.id)).toEqual(['mongo', 'redis']);
    expect(setMonarchTokensProvider.mock.calls).toEqual([
      ['mongo', mongoLanguage],
      ['redis', redisLanguage],
    ]);
  });
});
