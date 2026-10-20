import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { CompletionObject, SchemaCache } from '../../bindings';
import { callsOf, mockBackend } from '../../test/backend';
import { forgetCompletionConnection, invalidateCompletion, useCompletion } from '../../store/completion';
import { analyze, localStatementRange } from './context';
import { disposeCompletion, ensureCompletion, registerCompletion } from './monacoProvider';
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

  beforeEach(() => {
    useCompletion.reset();
    mockBackend({
      statement_ranges: () => [],
      completion_scope: () => ({ database: 'shop', tables: mysqlScope.tables }),
      list_databases: () => [{ name: 'shop' }],
    });
  });

  it('ranges_are_cached_per_model_version', async () => {
    const { monaco, providers } = fakeMonaco();
    registerCompletion(monaco as never, target);
    const p = providers.get('mysql') as FakeProvider;
    const model = fakeModel('SELECT o.| FROM orders o');
    await p.provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    await p.provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    expect(callsOf('statement_ranges')).toHaveLength(1);
    model.version = 2;
    await p.provideCompletionItems(model, { lineNumber: 1 }, {}, live);
    expect(callsOf('statement_ranges')).toHaveLength(2);
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

  it('local_statement_range_cuts_at_semicolons_outside_strings', () => {
    const { text, offset } = at("SELECT ';' x; SELECT 'a;b', | FROM t; SELECT 3");
    const [lo, hi] = localStatementRange(text, offset, 'mysql');
    expect(text.slice(lo, hi).trim()).toBe("SELECT 'a;b',  FROM t");
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
