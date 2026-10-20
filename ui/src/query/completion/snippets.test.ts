import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { mockBackend } from '../../test/backend';
import { useCompletion } from '../../store/completion';
import { registerCompletion } from './monacoProvider';
import { BUILTIN_SNIPPETS, allSnippets, setUserSnippets } from './snippets';

type Item = { label: string; kind: number; insertText: string; insertTextRules?: number; sortText: string; filterText?: string };
type Provider = { provideCompletionItems(model: unknown, pos: unknown, ctx: unknown, token: unknown): { suggestions: Item[] } };

function setup() {
  let provider: Provider | undefined;
  const monaco = {
    languages: {
      CompletionItemKind: { Keyword: 1, Function: 2, Struct: 3, Interface: 4, Field: 5, Module: 6, Snippet: 27 },
      CompletionItemInsertTextRule: { InsertAsSnippet: 4 },
      registerCompletionItemProvider: (_l: string, p: Provider) => {
        provider = p;
        return { dispose() {} };
      },
    },
    Range: class {},
  };
  registerCompletion(monaco as never, () => ({ tabId: 't', connId: 'c', db: 'shop', engine: 'mysql' }));
  const ask = (marked: string) => {
    const offset = marked.indexOf('|');
    const text = marked.slice(0, offset) + marked.slice(offset + 1);
    const model = {
      getValue: () => text,
      getVersionId: () => 1,
      getOffsetAt: () => offset,
      getWordUntilPosition: () => ({ startColumn: 1, endColumn: 1 }),
    };
    return provider!.provideCompletionItems(model, { lineNumber: 1 }, {}, { isCancellationRequested: false }).suggestions;
  };
  return ask;
}

beforeEach(() => {
  useCompletion.reset();
  mockBackend({
    completion_scope: () => ({
      database: 'shop',
      tables: [{ schema: null, name: 'orders', kind: 'Table', columns: [{ name: 'id', data_type: 'int' }] }],
    }),
    list_databases: () => [{ name: 'shop' }],
  });
});
afterEach(() => setUserSnippets([]));

describe('built-in snippets', () => {
  it('has_the_ten_prefixes', () => {
    expect(BUILTIN_SNIPPETS.map((s) => s.prefix)).toEqual(['sel', 'selw', 'ins', 'upd', 'del', 'join', 'lj', 'cte', 'cnt', 'grp']);
    for (const s of BUILTIN_SNIPPETS) {
      expect(s.label).not.toBe('');
      expect(s.description).not.toBe('');
      expect(s.body).toMatch(/\$\{1:/);
    }
  });

  it('sel_matches_the_spec', () => {
    expect(BUILTIN_SNIPPETS[0]!.body).toBe('SELECT ${1:*} FROM ${2:table} LIMIT ${3:100};');
  });

  it('are_offered_as_snippet_items_ranked_after_everything_else', async () => {
    await useCompletion.scope('c', 'shop');
    await useCompletion.containers('c', 'shop', 'mysql');
    const items = setup()('|');
    const snippets = items.filter((i) => i.kind === 27);
    expect(snippets.map((i) => i.filterText)).toEqual(BUILTIN_SNIPPETS.map((s) => s.prefix));
    expect(snippets.every((i) => i.insertTextRules === 4)).toBe(true);
    const others = items.filter((i) => i.kind !== 27);
    expect(others.length).toBeGreaterThan(0);
    const lastOther = others.map((i) => i.sortText).sort().at(-1)!;
    expect(snippets.every((i) => i.sortText > lastOther)).toBe(true);
  });

  it('are_not_offered_after_a_dot', async () => {
    await useCompletion.scope('c', 'shop');
    await useCompletion.containers('c', 'shop', 'mysql');
    const items = setup()('SELECT orders.| FROM orders');
    expect(items.some((i) => i.kind === 27)).toBe(false);
  });
});

describe('user snippets', () => {
  it('are_offered_after_the_built_ins', async () => {
    await useCompletion.scope('c', 'shop');
    setUserSnippets([{ prefix: 'mine', label: 'mine', body: 'SELECT ${1:1}', description: 'mine' }]);
    expect(allSnippets().at(-1)?.prefix).toBe('mine');
    const items = setup()('|').filter((i) => i.kind === 27);
    expect(items.at(-1)).toMatchObject({ filterText: 'mine', insertText: 'SELECT ${1:1}' });
  });

  it('replacing_the_list_drops_the_old_ones_and_blank_entries', () => {
    setUserSnippets([{ prefix: 'a', label: 'a', body: 'x', description: '' }]);
    setUserSnippets([
      { prefix: 'b', label: 'b', body: 'y', description: '' },
      { prefix: ' ', label: '', body: 'z', description: '' },
      { prefix: 'c', label: 'c', body: '', description: '' },
    ]);
    expect(allSnippets().slice(BUILTIN_SNIPPETS.length).map((s) => s.prefix)).toEqual(['b']);
  });
});
