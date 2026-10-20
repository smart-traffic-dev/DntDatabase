import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { CompletionObject, SchemaCache } from '../../bindings';
import { callsOf, mockBackend } from '../../test/backend';
import { forgetCompletionConnection, invalidateCompletion, useCompletion } from '../../store/completion';
import { analyze } from './context';
import { disposeCompletion, ensureCompletion, registerCompletion } from './monacoProvider';
import { splitIncremental, splitRanges } from './split';
import { currentSchemaOf, suggest, type Suggestion } from './provider';
import type { CompletionScope } from './provider';
import type { Engine } from '../engine';

/** `text` with a `|` marking the cursor. */
function at(marked: string) {
  const offset = marked.indexOf('|');
  return { text: marked.slice(0, offset) + marked.slice(offset + 1), offset };
}

const mysqlScope: CompletionScope = {
  database: 'shop',
  tables: [
    {
      schema: null,
      name: 'orders',
      kind: 'Table',
      columns: [
        { name: 'id', data_type: 'int' },
        { name: 'customer_id', data_type: 'int' },
        { name: 'total', data_type: 'decimal(12,2)' },
      ],
    },
    {
      schema: null,
      name: 'customers',
      kind: 'Table',
      columns: [
        { name: 'id', data_type: 'int' },
        { name: 'name', data_type: 'varchar(100)' },
      ],
    },
    { schema: null, name: 'order_totals', kind: 'View', columns: [{ name: 'total', data_type: 'decimal(34,2)' }] },
    { schema: null, name: 'order details', kind: 'Table', columns: [{ name: 'line no', data_type: 'int' }] },
  ],
};

const pgScope: CompletionScope = {
  database: 'analytics',
  tables: [
    {
      schema: 'public',
      name: 'Order Items',
      kind: 'Table',
      columns: [
        { name: 'order_id', data_type: 'bigint' },
        { name: 'qty', data_type: 'integer' },
      ],
    },
    { schema: 'public', name: 'orders', kind: 'Table', columns: [{ name: 'id', data_type: 'bigint' }] },
  ],
};

async function run(
  marked: string,
  engine: Engine = 'mysql',
  opts: { containers?: string[]; objects?: CompletionObject[]; ranges?: [number, number][] } = {},
) {
  const { text, offset } = at(marked);
  const loadObjects = vi.fn(async () => opts.objects ?? []);
  const items = await suggest({
    text,
    offset,
    ranges: opts.ranges ?? [],
    engine,
    scope: engine === 'mysql' ? mysqlScope : pgScope,
    containers: opts.containers ?? [],
    loadObjects,
  });
  return { items, loadObjects };
}

const labels = (items: Suggestion[]) => items.map((s) => s.label);
const ofKind = (items: Suggestion[], kind: Suggestion['kind']) => items.filter((s) => s.kind === kind);

describe('autocomplete', () => {
  it('tables_after_from', async () => {
    const { items } = await run('SELECT * FROM |');
    expect(analyze(at('SELECT * FROM |').text, 14, 'mysql').context).toEqual({ kind: 'table' });
    expect(labels(items)).toEqual(expect.arrayContaining(['orders', 'customers', 'order_totals']));
    expect(items.find((s) => s.label === 'order_totals')?.kind).toBe('view');
    expect(items.find((s) => s.label === 'orders')?.kind).toBe('table');
    expect(ofKind(items, 'keyword')).toEqual([]);
    // Names that need it are quoted with the dialect's quote.
    expect(items.find((s) => s.label === 'order details')?.insertText).toBe('`order details`');
    // Also after JOIN / UPDATE / INTO / TABLE, and after a comma in a FROM list.
    for (const sql of ['SELECT * FROM orders o JOIN |', 'UPDATE |', 'INSERT INTO |', 'TRUNCATE TABLE |', 'SELECT * FROM orders, |']) {
      expect(analyze(at(sql).text, at(sql).offset, 'mysql').context).toEqual({ kind: 'table' });
    }
  });

  it('columns_for_alias_dot', async () => {
    const { items } = await run('SELECT o.| FROM orders o');
    expect(labels(items)).toEqual(['id', 'customer_id', 'total']);
    expect(items.every((s) => s.kind === 'column')).toBe(true);
    expect(items.find((s) => s.label === 'total')?.detail).toBe('decimal(12,2)');
    // With a typed prefix the context stays the same.
    const typed = at('SELECT o.cu| FROM orders o');
    expect(analyze(typed.text, typed.offset, 'mysql')).toMatchObject({
      context: { kind: 'member', qualifier: 'o' },
      prefix: 'cu',
    });
  });

  it('columns_for_as_alias_quoted', async () => {
    const marked = 'SELECT oi.| FROM "Order Items" AS oi';
    const { text, offset } = at(marked);
    const { info } = analyze(text, offset, 'postgres');
    expect(info.aliases.get('oi')).toEqual({ schema: null, table: 'Order Items' });
    const { items } = await run(marked, 'postgres');
    expect(labels(items)).toEqual(['order_id', 'qty']);
    // MySQL backticks, and a schema-qualified PG reference.
    const my = at('SELECT d.| FROM `order details` d');
    expect(analyze(my.text, my.offset, 'mysql').info.aliases.get('d')).toEqual({ schema: null, table: 'order details' });
    const pg = at('SELECT x.| FROM public."Order Items" x');
    expect(analyze(pg.text, pg.offset, 'postgres').info.aliases.get('x')).toEqual({ schema: 'public', table: 'Order Items' });
    expect(labels((await run('SELECT x.| FROM public."Order Items" x', 'postgres')).items)).toEqual(['order_id', 'qty']);
    // Inserting a table whose name needs quoting on PG.
    const tables = (await run('SELECT * FROM |', 'postgres')).items;
    expect(tables.find((s) => s.label === 'Order Items')?.insertText).toBe('"Order Items"');
    expect(tables.find((s) => s.label === 'orders')?.insertText).toBe('orders');
  });

  it('table_name_dot_columns', async () => {
    const { items } = await run('SELECT orders.| FROM orders JOIN customers c ON c.id = orders.customer_id');
    expect(labels(items)).toEqual(['id', 'customer_id', 'total']);
    const joined = await run('SELECT * FROM orders o JOIN customers c ON c.| ');
    expect(labels(joined.items)).toEqual(['id', 'name']);
  });

  it('db_prefix_loads_objects', async () => {
    const objects: CompletionObject[] = [
      { schema: null, name: 'invoices', kind: 'Table' },
      { schema: null, name: 'v_sales', kind: 'View' },
    ];
    const { items, loadObjects } = await run('SELECT * FROM shop.|', 'mysql', { containers: ['shop', 'crm'], objects });
    expect(loadObjects).toHaveBeenCalledWith('shop');
    expect(items.map((s) => [s.label, s.kind])).toEqual([
      ['invoices', 'table'],
      ['v_sales', 'view'],
    ]);
    // Not a container, not an alias: nothing is loaded.
    const other = await run('SELECT * FROM nowhere.|', 'mysql', { containers: ['shop'] });
    expect(other.loadObjects).not.toHaveBeenCalled();
    expect(other.items).toEqual([]);
  });

  it('schema_prefix_pg', async () => {
    const objects: CompletionObject[] = [{ schema: 'sales', name: 'Big Deals', kind: 'Table' }];
    const { items, loadObjects } = await run('SELECT * FROM sales.|', 'postgres', { containers: ['public', 'sales'], objects });
    expect(loadObjects).toHaveBeenCalledWith('sales');
    expect(items).toEqual([expect.objectContaining({ label: 'Big Deals', kind: 'table', insertText: '"Big Deals"' })]);
  });

  it('columns_then_keywords_after_select', async () => {
    const { items } = await run('SELECT | FROM orders');
    const names = labels(items);
    expect(names.slice(0, 3)).toEqual(['id', 'customer_id', 'total']);
    const firstKeyword = items.findIndex((s) => s.kind === 'keyword');
    expect(firstKeyword).toBeGreaterThan(2);
    expect(names).toContain('DISTINCT');
    expect(ofKind(items, 'table')).toEqual([]);
    // Also after WHERE / ON / BY, and after an operator in a WHERE clause.
    for (const sql of [
      'SELECT * FROM orders WHERE |',
      'SELECT * FROM orders o JOIN customers c ON |',
      'SELECT * FROM orders ORDER BY |',
      'SELECT * FROM orders WHERE total > |',
      'SELECT id, | FROM orders',
    ]) {
      expect(analyze(at(sql).text, at(sql).offset, 'mysql').context).toEqual({ kind: 'column' });
    }
    // Columns of every table of the statement, each name once.
    const both = labels((await run('SELECT | FROM orders o JOIN customers c ON c.id = o.customer_id')).items);
    expect(both.slice(0, 4)).toEqual(['id', 'customer_id', 'total', 'name']);
  });

  it('default_has_keywords_functions_tables', async () => {
    const { items } = await run('|');
    expect(analyze('', 0, 'mysql').context).toEqual({ kind: 'default' });
    const kw = items.findIndex((s) => s.label === 'SELECT' && s.kind === 'keyword');
    const fn = items.findIndex((s) => s.label === 'COUNT' && s.kind === 'function');
    const table = items.findIndex((s) => s.label === 'orders' && s.kind === 'table');
    expect(kw).toBeGreaterThanOrEqual(0);
    expect(fn).toBeGreaterThan(kw);
    expect(table).toBeGreaterThan(fn);
    // Dialect lists differ.
    const pg = labels((await run('|', 'postgres')).items);
    expect(pg).toContain('RETURNING');
    expect(pg).toContain('STRING_AGG');
    expect(labels(items)).toContain('GROUP_CONCAT');
    expect(labels(items)).not.toContain('STRING_AGG');
  });

  it('only_current_statement_is_read', async () => {
    const marked = 'SELECT * FROM customers o;\nSELECT o.| FROM orders x';
    const { text } = at(marked);
    const ranges: [number, number][] = [
      [0, 25],
      [27, text.length],
    ];
    const withRanges = await run(marked, 'mysql', { ranges });
    expect(withRanges.items).toEqual([]);
    // Without ranges the `;` still ends the statement.
    expect((await run(marked)).items).toEqual([]);
    // The statement after the cursor is ignored too.
    const before = at('SELECT o.| FROM orders x; SELECT * FROM customers o');
    const { info } = analyze(before.text, before.offset, 'mysql');
    expect(info.aliases.has('o')).toBe(false);
    expect(info.tables).toEqual([{ schema: null, table: 'orders' }]);
    // The alias of the current statement is still found.
    const ok = await run('SELECT * FROM customers o;\nSELECT x.| FROM orders x', 'mysql', { ranges });
    expect(labels(ok.items)).toEqual(['id', 'customer_id', 'total']);
  });

  it('tables_after_from_when_ranges_end_before_the_cursor', async () => {
    // `statement_ranges` trims trailing blanks: `SELECT * FROM ` ends at 13, the cursor is at 14.
    for (const engine of ['mysql', 'postgres'] as const) {
      for (const sql of ['SELECT * FROM |', 'SELECT * FROM orders o JOIN |', 'UPDATE |', 'INSERT INTO  |']) {
        const { text } = at(sql);
        const end = text.trimEnd().length;
        const { items } = await run(sql, engine, { ranges: [[0, end]] });
        expect(ofKind(items, 'keyword')).toEqual([]);
        expect(items.length).toBeGreaterThan(0);
        expect(items.every((s) => s.kind === 'table' || s.kind === 'view')).toBe(true);
      }
    }
    // Multi-line gap after the statement, still no `;`: the same statement.
    const nl = at('SELECT *\nFROM\n  |');
    expect(labels((await run('SELECT *\nFROM\n  |', 'mysql', { ranges: [[0, nl.text.trimEnd().length]] })).items)).toContain(
      'orders',
    );
    // After a `;` the previous statement is over: default context.
    const after = at('SELECT * FROM orders;\n|');
    const def = await run('SELECT * FROM orders;\n|', 'mysql', { ranges: [[0, after.text.indexOf(';')]] });
    expect(ofKind(def.items, 'keyword').length).toBeGreaterThan(0);
  });

  it('no_suggestions_inside_strings_or_comments', async () => {
    expect((await run("SELECT 'FROM |'")).items).toEqual([]);
    expect((await run('SELECT 1 -- FROM |')).items).toEqual([]);
    expect((await run('SELECT /* o.| */ 1 FROM orders o')).items).toEqual([]);
  });
});

describe('monaco provider', () => {
  it('registers_for_both_languages_and_maps_items', async () => {
    useCompletion.reset();
    const conn = '11111111-1111-4111-8111-111111111111';
    mockBackend({
      statement_ranges: () => [],
      completion_scope: () => ({ database: 'shop', tables: mysqlScope.tables }),
      list_databases: () => [{ name: 'shop' }],
    });
    type Provider = { provideCompletionItems(model: unknown, position: unknown): Promise<{ suggestions: { label: string; kind: number; sortText: string; detail?: string }[] }> };
    const providers = new Map<string, Provider>();
    const dispose = vi.fn();
    const fake = {
      languages: {
        CompletionItemKind: { Keyword: 1, Function: 2, Struct: 3, Interface: 4, Field: 5, Module: 6 },
        registerCompletionItemProvider: (lang: string, p: Provider) => {
          providers.set(lang, p);
          return { dispose };
        },
      },
      Range: class {
        constructor(...args: number[]) {
          Object.assign(this, { args });
        }
      },
    };
    const { text, offset } = at('SELECT o.| FROM orders o');
    const model = {
      getValue: () => text,
      getVersionId: () => 1,
      getOffsetAt: () => offset,
      getWordUntilPosition: () => ({ startColumn: offset + 1, endColumn: offset + 1 }),
    };
    const reg = registerCompletion(fake as never, () => ({ tabId: 't', connId: conn, db: 'shop', engine: 'mysql' }));
    expect([...providers.keys()]).toEqual(['mysql', 'pgsql']);
    await useCompletion.scope(conn, 'shop');
    const result = await providers.get('mysql')?.provideCompletionItems(model, { lineNumber: 1, column: offset + 1 });
    expect(result?.suggestions.map((s) => [s.label, s.kind, s.sortText])).toEqual([
      ['id', 5, '00000'],
      ['customer_id', 5, '00001'],
      ['total', 5, '00002'],
    ]);
    expect(callsOf('completion_scope')).toEqual([{ connId: conn, database: 'shop' }]);
    reg.dispose();
    expect(dispose).toHaveBeenCalledTimes(2);
  });
});

type FakeProvider = {
  provideCompletionItems(
    model: unknown,
    position: unknown,
    context?: unknown,
    token?: { isCancellationRequested: boolean },
  ): Promise<{ suggestions: { label: string; insertText: string }[] }>;
};

function fakeMonaco() {
  const providers = new Map<string, FakeProvider>();
  const registered: string[] = [];
  const disposed: string[] = [];
  const monaco = {
    languages: {
      CompletionItemKind: { Keyword: 1, Function: 2, Struct: 3, Interface: 4, Field: 5, Module: 6 },
      registerCompletionItemProvider: (lang: string, p: FakeProvider) => {
        providers.set(lang, p);
        registered.push(lang);
        return { dispose: () => disposed.push(lang) };
      },
    },
    Range: class {},
  };
  return { monaco, providers, registered, disposed };
}

function fakeModel(marked: string, version = 1) {
  const { text, offset } = at(marked);
  const model = {
    version,
    getValue: () => text,
    getVersionId: () => model.version,
    getOffsetAt: () => offset,
    getWordUntilPosition: () => ({ startColumn: 1, endColumn: 1 }),
  };
  return model;
}

describe('monaco provider: fix round 1', () => {
  const conn = '11111111-1111-4111-8111-111111111111';
  const target = () => ({ tabId: 't', connId: conn, db: 'shop', engine: 'mysql' as const });
  const live = { isCancellationRequested: false };

  /** The scope and qualifier lists, loaded (the provider itself never waits for them). */
  const loaded = async () => {
    await useCompletion.scope(conn, 'shop');
    await useCompletion.containers(conn, 'shop', 'mysql');
  };

  beforeEach(() => {
    useCompletion.reset();
    mockBackend({
      statement_ranges: () => [],
      completion_scope: () => ({ database: 'shop', tables: mysqlScope.tables }),
      list_databases: () => [{ name: 'shop' }],
    });
  });

  it('no_statement_ranges_ipc_per_keystroke', async () => {
    await loaded();
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const p = providers.get('mysql') as FakeProvider;
    const model = fakeModel('SELECT o.| FROM orders o');
    const first = p.provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    // Answered synchronously: no promise, no IPC round trip.
    expect(Array.isArray((first as unknown as { suggestions: unknown[] }).suggestions)).toBe(true);
    model.version = 2;
    await p.provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    expect(callsOf('statement_ranges')).toEqual([]);
  });

  it('scope_loading_returns_keywords_at_once_then_tables', async () => {
    let resolveScope: (v: unknown) => void = () => {};
    mockBackend({
      completion_scope: () => new Promise((r) => (resolveScope = r)),
      list_databases: () => [{ name: 'shop' }],
    });
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const p = providers.get('mysql') as FakeProvider;
    const before = p.provideCompletionItems(fakeModel('sel|'), { lineNumber: 1 }, {}, live) as unknown as {
      suggestions: { label: string }[];
      incomplete?: boolean;
    };
    // Not a promise: keywords at once, marked incomplete while the tables load.
    expect(before.suggestions.map((s) => s.label)).toContain('SELECT');
    expect(before.incomplete).toBe(true);
    resolveScope({ database: 'shop', tables: mysqlScope.tables });
    await loaded();
    const after = (await p.provideCompletionItems(fakeModel('sel|'), { lineNumber: 1 }, {}, live)) as {
      suggestions: { label: string }[];
      incomplete?: boolean;
    };
    expect(after.suggestions.map((s) => s.label)).toEqual(expect.arrayContaining(['SELECT', 'orders']));
    expect(after.incomplete).toBeFalsy();
    expect(callsOf('completion_scope')).toHaveLength(1);
  });

  it('open_suggest_widget_is_refreshed_once_the_scope_loads', async () => {
    let resolveScope: (v: unknown) => void = () => {};
    mockBackend({
      completion_scope: () => new Promise((r) => (resolveScope = r)),
      list_databases: () => [{ name: 'shop' }],
    });
    const { monaco, providers } = fakeMonaco();
    const model = fakeModel('SELECT * FROM |');
    const widget = document.createElement('div');
    const dom = document.createElement('div');
    dom.appendChild(widget);
    const trigger = vi.fn();
    const editors = [{ getModel: () => model, getDomNode: () => dom, trigger }];
    const withEditors = { ...monaco, editor: { getEditors: () => editors } };
    registerCompletion(withEditors as never, target);
    const p = providers.get('mysql') as FakeProvider;
    p.provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    p.provideCompletionItems(model, { lineNumber: 1 }, {}, live); // still loading: one refresh, not two
    widget.className = 'suggest-widget visible';
    resolveScope({ database: 'shop', tables: mysqlScope.tables });
    await loaded();
    await Promise.resolve();
    expect(trigger).toHaveBeenCalledTimes(1);
    expect(trigger).toHaveBeenCalledWith('dnt', 'editor.action.triggerSuggest', {});
  });

  it('closed_suggest_widget_is_not_reopened_when_the_scope_loads', async () => {
    let resolveScope: (v: unknown) => void = () => {};
    mockBackend({
      completion_scope: () => new Promise((r) => (resolveScope = r)),
      list_databases: () => [{ name: 'shop' }],
    });
    const { monaco, providers } = fakeMonaco();
    const model = fakeModel('SELECT * FROM |');
    const trigger = vi.fn();
    const editors = [{ getModel: () => model, getDomNode: () => document.createElement('div'), trigger }];
    registerCompletion({ ...monaco, editor: { getEditors: () => editors } } as never, target);
    (providers.get('mysql') as FakeProvider).provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    resolveScope({ database: 'shop', tables: mysqlScope.tables });
    await loaded();
    await Promise.resolve();
    expect(trigger).not.toHaveBeenCalled();
  });

  it('large_script_completion_is_fast', async () => {
    await loaded();
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const p = providers.get('mysql') as FakeProvider;
    // 10k lines with strings and comments holding `;`, the cursor in the last statement.
    const lines = Array.from({ length: 10_000 }, (_, i) =>
      i % 3 === 0 ? `SELECT 'a;b', id FROM customers c WHERE c.id = ${i}; -- x;y` : `/* ; */ UPDATE orders SET total = ${i};`,
    );
    const head = `${lines.join('\n')}\nSELECT o.`;
    const tail = ' FROM orders o';
    // A model that types at the cursor and reports its edits like Monaco does.
    let typed = '';
    let version = 1;
    const listeners: ((e: { changes: { rangeOffset: number }[] }) => void)[] = [];
    const model = {
      getValue: () => head + typed + tail,
      getVersionId: () => version,
      getOffsetAt: () => head.length + typed.length,
      getWordUntilPosition: () => ({ startColumn: 1, endColumn: 1 }),
      onDidChangeContent: (l: (typeof listeners)[number]) => {
        listeners.push(l);
        return { dispose: () => {} };
      },
    };
    const type = (s: string) => {
      const at = head.length + typed.length;
      typed += s;
      version += 1;
      listeners.forEach((l) => l({ changes: [{ rangeOffset: at }] }));
    };
    p.provideCompletionItems(model, { lineNumber: 1 }, {}, live); // first split: the whole script
    const runs = 20;
    const t0 = performance.now();
    let result: { suggestions: { label: string }[] } | undefined;
    for (const ch of 'customer_id_and_more'.slice(0, runs)) {
      type(ch); // a keystroke: a new model version every time
      result = p.provideCompletionItems(model, { lineNumber: 1 }, {}, live) as unknown as typeof result;
    }
    const perKeystroke = (performance.now() - t0) / runs;
    expect(result?.suggestions.map((s) => s.label)).toEqual(['id', 'customer_id', 'total']);
    expect(callsOf('statement_ranges')).toEqual([]);
    expect(perKeystroke).toBeLessThan(50);
    // Without content events every keystroke splits the whole script: still within budget.
    const plain = fakeModel(`${head}|${tail}`);
    const t1 = performance.now();
    for (let k = 0; k < 5; k++) {
      plain.version += 1;
      p.provideCompletionItems(plain, { lineNumber: 1 }, {}, live);
    }
    expect((performance.now() - t1) / 5).toBeLessThan(50);
  });

  it('split_whitespace_matches_rust', () => {
    // Statement trimming is Rust's `str::trim` (Unicode White_Space): NEL and NBSP are
    // trimmed, a BOM is not (JS `\s` would get both wrong).
    const nel = 'select 1\u0085;  select 2 ';
    expect(splitRanges(nel, 'mysql').map(([s, e]) => nel.slice(s, e))).toEqual(['select 1', 'select 2']);
    const bom = '﻿select 1';
    expect(splitRanges(bom, 'mysql').map(([s, e]) => bom.slice(s, e))).toEqual(['﻿select 1']);
    // Blank-fragment detection is ASCII only: an NBSP between comments is significant.
    const nbsp = 'select 1; /* a */ /* b */';
    expect(splitRanges(nbsp, 'mysql')).toHaveLength(2);
  });

  it('incremental_split_without_a_known_edit_offset_is_a_full_split', () => {
    const before = 'SELECT 1; SELECT 2; SELECT 3';
    const prev = splitIncremental(null, before, 'mysql', 0);
    // The edit offset is unknown (Infinity): reusing the old boundaries would be wrong here.
    const after = "SELECT ';"; // the first statement now opens a string that swallows the rest
    expect(splitIncremental(prev, after, 'mysql', Infinity).ranges).toEqual(splitRanges(after, 'mysql'));
    expect(splitIncremental(prev, after, 'mysql', Number.NaN).ranges).toEqual(splitRanges(after, 'mysql'));
  });

  it('incremental_split_matches_a_full_split', () => {
    const base = "SELECT 'a;b' FROM t; -- c;\nDELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nSELECT 2; /* x; */ SELECT 3";
    let state = splitIncremental(null, base, 'mysql', 0);
    let text = base;
    // Edits anywhere: inserts and deletes, including ones that open a string or remove a `;`.
    const edits: [number, number, string][] = [
      [text.length, 0, ' WHERE x = 1'],
      [9, 0, "'"],
      [9, 1, ''],
      [19, 1, ''],
      [40, 0, ';'],
      [0, 0, 'USE db;\n'],
      [70, 5, ''],
    ];
    for (const [at, del, ins] of edits) {
      text = text.slice(0, at) + ins + text.slice(at + del);
      state = splitIncremental(state, text, 'mysql', at);
      expect(state.ranges).toEqual(splitRanges(text, 'mysql'));
    }
  });

  it('ctrl_space_after_from_lists_tables_first', async () => {
    await loaded();
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const result = (await providers
      .get('mysql')
      ?.provideCompletionItems(fakeModel('SELECT * FROM |'), { lineNumber: 1 }, {}, live)) as {
      suggestions: { label: string }[];
      incomplete?: boolean;
    };
    expect(result.suggestions.slice(0, 3).map((s) => s.label)).toEqual(['orders', 'customers', 'order_totals']);
    expect(result.suggestions.map((s) => s.label)).not.toContain('SELECT');
    expect(result.incomplete).toBeFalsy();
  });

  it('failed_scope_marks_the_result_incomplete', async () => {
    mockBackend({
      statement_ranges: () => [],
      completion_scope: () => {
        throw { kind: 'not_open', message: 'x' };
      },
      list_databases: () => [{ name: 'shop' }],
    });
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const result = (await providers
      .get('mysql')
      ?.provideCompletionItems(fakeModel('SELECT * FROM |'), { lineNumber: 1 }, {}, live)) as { incomplete?: boolean };
    // Monaco asks again on the next keystroke instead of filtering a table-less list.
    expect(result.incomplete).toBe(true);
  });

  it('cancelled_request_returns_nothing', async () => {
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const result = await providers
      .get('mysql')
      ?.provideCompletionItems(fakeModel('SELECT o.| FROM orders o'), { lineNumber: 1 }, {}, { isCancellationRequested: true });
    expect(result?.suggestions).toEqual([]);
  });

  it('large_text_skips_statement_ranges', async () => {
    await loaded();
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    // > 1 MB of earlier statements, one with a `;` inside a string, then the current one.
    const filler = "SELECT 'a;b' FROM customers o;\n".repeat(40_000);
    const result = await providers
      .get('mysql')
      ?.provideCompletionItems(fakeModel(`${filler}SELECT o.| FROM orders o; SELECT 2`), { lineNumber: 1 }, {}, live);
    expect(callsOf('statement_ranges')).toEqual([]);
    expect(result?.suggestions.map((s) => s.label)).toEqual(['id', 'customer_id', 'total']);
  });

  it('split_ranges_match_the_backend_splitter', () => {
    // The cases of `crates/core/src/sql/split.rs` (statement texts, trimmed, no delimiter).
    const cases: [string, Engine, string[]][] = [
      ['select 1; select 2', 'mysql', ['select 1', 'select 2']],
      ['select \';\' ; select "a;b"; select `c;d`', 'mysql', ["select ';'", 'select "a;b"', 'select `c;d`']],
      ['select 1 -- x;y\n; select 2 # z;\n;', 'mysql', ['select 1 -- x;y', 'select 2 # z;']],
      ['/* a;b */ select 1;', 'mysql', ['/* a;b */ select 1']],
      [
        'DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;\nCALL p();',
        'mysql',
        ['CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END', 'CALL p()'],
      ],
      ["select 'it''s;'; select 2", 'mysql', ["select 'it''s;'", 'select 2']],
      [
        'create function f() returns int as $$ begin return 1; end $$ language plpgsql; select 1',
        'postgres',
        ['create function f() returns int as $$ begin return 1; end $$ language plpgsql', 'select 1'],
      ],
      ['select $tag$ ; $$ ; $tag$; select 2', 'postgres', ['select $tag$ ; $$ ; $tag$', 'select 2']],
      ["select E'a\\';b'; select 2", 'postgres', ["select E'a\\';b'", 'select 2']],
      ['select 1 # not a comment in pg; select 2', 'postgres', ['select 1 # not a comment in pg', 'select 2']],
      ['  ;; \n', 'mysql', []],
      ["select 'chưa đóng", 'mysql', ["select 'chưa đóng"]],
      ["select 'a\\';b'; select 2", 'mysql', ["select 'a\\';b'", 'select 2']],
      ["select 'C:\\'; select 2", 'postgres', ["select 'C:\\'", 'select 2']],
      ['/* a /* b; */ c; */ select 1; select 2', 'postgres', ['/* a /* b; */ c; */ select 1', 'select 2']],
      ['select $1; select a$b$; select 3', 'postgres', ['select $1', 'select a$b$', 'select 3']],
      ['select 1--1; select 2', 'mysql', ['select 1--1', 'select 2']],
      ['select 1; -- done\n', 'mysql', ['select 1']],
      ['select 1; /* end */', 'postgres', ['select 1']],
      [
        'delimiter //\r\nselect 1; select 2//\r\n  Delimiter ;\r\nselect 3;',
        'mysql',
        ['select 1; select 2', 'select 3'],
      ],
      ['DELIMITER $$\nselect 1', 'postgres', ['DELIMITER $$\nselect 1']],
      ['select $x$ ; ', 'postgres', ['select $x$ ;']],
      ['select 1 /* ; ', 'mysql', ['select 1 /* ;']],
      [
        '/*!40014 SET FOREIGN_KEY_CHECKS=0 */;\nselect 1;',
        'mysql',
        ['/*!40014 SET FOREIGN_KEY_CHECKS=0 */', 'select 1'],
      ],
      ['/*M!100100 SET a=1; */;\n/*!40101 SET NAMES utf8mb4 */', 'mysql', ['/*M!100100 SET a=1; */', '/*!40101 SET NAMES utf8mb4 */']],
      ['select 1; /*! x */', 'postgres', ['select 1']],
      ["select 'ư;đ'; select 2", 'mysql', ["select 'ư;đ'", 'select 2']],
      ['select $thẻ$ ; $thẻ$; select 2', 'postgres', ['select $thẻ$ ; $thẻ$', 'select 2']],
    ];
    for (const [input, engine, expected] of cases) {
      const texts = splitRanges(input, engine).map(([s, e]) => input.slice(s, e));
      expect(texts, `${input} (${engine})`).toEqual(expected);
    }
  });

  it('registers_once_per_monaco_instance', () => {
    const { monaco, registered, disposed } = fakeMonaco();
    ensureCompletion(monaco as never);
    ensureCompletion(monaco as never);
    expect(registered).toEqual(['mysql', 'pgsql']);
    disposeCompletion(monaco as never);
    expect(disposed).toEqual(['mysql', 'pgsql']);
    ensureCompletion(monaco as never);
    expect(registered).toHaveLength(4);
    disposeCompletion(monaco as never);
  });

  it('registers_on_a_frozen_namespace_like_monaco', () => {
    // The real monaco is an ES module namespace object: not extensible.
    const { monaco, registered, disposed } = fakeMonaco();
    const frozen = Object.freeze({ ...monaco });
    expect(() => ensureCompletion(frozen as never)).not.toThrow();
    ensureCompletion(frozen as never);
    expect(registered).toEqual(['mysql', 'pgsql']);
    disposeCompletion(frozen as never);
    expect(disposed).toEqual(['mysql', 'pgsql']);
  });

  it('dollar_quoted_body_is_a_string', () => {
    const { text, offset } = at('SELECT $fn$ FROM o.| $fn$, 1');
    expect(analyze(text, offset, 'postgres').context).toEqual({ kind: 'none' });
    const after = at('SELECT $$x$$ FROM |');
    expect(analyze(after.text, after.offset, 'postgres').context).toEqual({ kind: 'table' });
  });

  it('pg_tables_outside_current_schema_are_qualified', async () => {
    const scope: CompletionScope = {
      database: 'analytics',
      tables: [
        ...pgScope.tables,
        { schema: 'audit', name: 'Log Entries', kind: 'Table', columns: [] },
        { schema: 'audit', name: 'events', kind: 'View', columns: [] },
      ],
    };
    const items = await suggest({
      text: 'SELECT * FROM ',
      offset: 14,
      ranges: [],
      engine: 'postgres',
      scope,
      containers: [],
      currentSchema: 'public',
      loadObjects: async () => [],
    });
    const insert = (label: string) => items.find((s) => s.label === label)?.insertText;
    expect(insert('orders')).toBe('orders');
    expect(insert('Order Items')).toBe('"Order Items"');
    expect(insert('Log Entries')).toBe('audit."Log Entries"');
    expect(insert('events')).toBe('audit.events');
    expect(currentSchemaOf(scope)).toBe('public');
    expect(currentSchemaOf({ database: 'x', tables: [{ schema: 'app', name: 't', kind: 'Table', columns: [] }] })).toBe('app');
  });
});

describe('completion cache', () => {
  const conn = '11111111-1111-4111-8111-111111111111';
  const cache = (database: string): SchemaCache => ({ database, tables: mysqlScope.tables });

  beforeEach(() => {
    useCompletion.reset();
  });

  it('invalidate_after_ddl', async () => {
    mockBackend({ completion_scope: ({ database }) => cache(database as string) });
    // Cached per (connection, database): no new fetch per keystroke.
    expect((await useCompletion.scope(conn, 'shop'))?.tables).toHaveLength(4);
    await useCompletion.scope(conn, 'shop');
    expect(callsOf('completion_scope')).toEqual([{ connId: conn, database: 'shop' }]);
    // Another database is its own entry.
    await useCompletion.scope(conn, 'crm');
    expect(callsOf('completion_scope')).toHaveLength(2);
    // A DDL run invalidates that database: the next completion fetches again.
    useCompletion.invalidate(conn, 'shop');
    await useCompletion.scope(conn, 'shop');
    await useCompletion.scope(conn, 'crm');
    expect(callsOf('completion_scope')).toHaveLength(3);
    // Without a database, every database of the connection goes.
    invalidateCompletion(conn);
    await useCompletion.scope(conn, 'shop');
    await useCompletion.scope(conn, 'crm');
    expect(callsOf('completion_scope')).toHaveLength(5);
  });

  it('connection_close_forgets_only_that_connection', async () => {
    const other = '22222222-2222-4222-8222-222222222222';
    mockBackend({
      completion_scope: ({ database }) => cache(database as string),
      list_databases: () => [{ name: 'shop' }],
    });
    await useCompletion.scope(conn, 'shop');
    await useCompletion.scope(conn, 'crm');
    await useCompletion.containers(conn, 'shop', 'mysql');
    await useCompletion.scope(other, 'shop');
    forgetCompletionConnection(conn);
    await useCompletion.scope(conn, 'shop');
    await useCompletion.scope(conn, 'crm');
    await useCompletion.containers(conn, 'shop', 'mysql');
    await useCompletion.scope(other, 'shop');
    expect(callsOf('completion_scope')).toHaveLength(5);
    expect(callsOf('list_databases')).toHaveLength(2);
  });

  it('failed_scope_is_null_and_retried_later', async () => {
    vi.useFakeTimers();
    try {
      mockBackend({
        completion_scope: () => {
          throw { kind: 'internal', message: 'boom' };
        },
      });
      expect(await useCompletion.scope(conn, 'shop')).toBeNull();
      expect(await useCompletion.scope(conn, 'shop')).toBeNull();
      expect(callsOf('completion_scope')).toHaveLength(1);
      vi.advanceTimersByTime(10_001);
      expect(await useCompletion.scope(conn, 'shop')).toBeNull();
      expect(callsOf('completion_scope')).toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('containers_and_objects_follow_the_engine', async () => {
    mockBackend({
      list_databases: () => [{ name: 'shop' }, { name: 'crm' }],
      list_schemas: () => [{ name: 'public' }, { name: 'sales' }],
      completion_objects: () => [{ schema: null, name: 'invoices', kind: 'Table' }],
    });
    expect(await useCompletion.containers(conn, 'shop', 'mysql')).toEqual(['shop', 'crm']);
    expect(await useCompletion.containers(conn, 'analytics', 'postgres')).toEqual(['public', 'sales']);
    await useCompletion.containers(conn, 'shop', 'mysql');
    expect(callsOf('list_databases')).toHaveLength(1);
    await useCompletion.objects(conn, 'shop', 'mysql', 'crm');
    await useCompletion.objects(conn, 'analytics', 'postgres', 'sales');
    expect(callsOf('completion_objects')).toEqual([
      { connId: conn, database: 'crm', schema: null },
      { connId: conn, database: 'analytics', schema: 'sales' },
    ]);
  });
});
