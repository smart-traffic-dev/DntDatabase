import { beforeEach, describe, expect, it } from 'vitest';
import { callsOf, config, mockBackend, serverInfo } from '../../../test/backend';
import { useCompletion } from '../../../store/completion';
import { useConnections } from '../../../store/connections';
import { useQueryDocs } from '../../../store/queryTabs';
import { resetQuietOpens } from '../../../store/reopen';
import { analyzeMongo, type MongoContext } from './context';
import { suggestMongo, type MongoData } from './suggest';
import { mongoDocTarget, registerMongoCompletion } from './monacoProvider';

/** `text` with a `|` marking the cursor. */
function at(marked: string) {
  const offset = marked.indexOf('|');
  return { text: marked.slice(0, offset) + marked.slice(offset + 1), offset };
}

const ctx = (marked: string): MongoContext => {
  const { text, offset } = at(marked);
  return analyzeMongo(text, offset).context;
};

const data: MongoData = {
  objects: [
    { name: 'orders', kind: 'Collection' },
    { name: 'order items', kind: 'Collection' },
    { name: 'system.users', kind: 'Collection' },
    { name: 'paid_orders', kind: 'View' },
  ],
  fields: [
    { name: '_id', type: 'objectId' },
    { name: 'status', type: 'string' },
    { name: 'order date', type: 'date' },
  ],
  databases: ['admin', 'shop'],
};

const labels = (marked: string, d: MongoData = data) => {
  const { text, offset } = at(marked);
  return suggestMongo(analyzeMongo(text, offset), d).map((s) => s.label);
};

const item = (marked: string, label: string, d: MongoData = data) => {
  const { text, offset } = at(marked);
  return suggestMongo(analyzeMongo(text, offset), d).find((s) => s.label === label);
};

describe('mongo completion: context', () => {
  it('start_of_statement', () => {
    expect(ctx('|')).toEqual({ kind: 'statement' });
    expect(ctx('d|')).toEqual({ kind: 'statement' });
    expect(ctx('db.orders.find()\n|')).toEqual({ kind: 'statement' });
    expect(ctx('show dbs; |')).toEqual({ kind: 'statement' });
    expect(labels('|')).toEqual(['db', 'use', 'show']);
  });

  it('after_db_dot', () => {
    expect(ctx('db.|')).toEqual({ kind: 'db', db: null });
    expect(ctx('db.ord|')).toEqual({ kind: 'db', db: null });
    expect(ctx('db.getSiblingDB("logs").|')).toEqual({ kind: 'db', db: 'logs' });
    const l = labels('db.|');
    expect(l.slice(0, 4)).toEqual(['orders', 'order items', 'system.users', 'paid_orders']);
    expect(l).toEqual(expect.arrayContaining(['getCollectionNames','runCommand', 'getCollection', 'getSiblingDB']));
    // A name that is not an identifier is reached through getCollection().
    expect(item('db.|', 'order items')?.insertText).toBe('getCollection("order items")');
    expect(item('db.|', 'orders')?.insertText).toBe('orders');
    expect(item('db.|', 'paid_orders')?.kind).toBe('view');
    expect(item('db.|', 'runCommand')).toMatchObject({ insertText: 'runCommand({$0})', snippet: true });
  });

  it('after_a_collection_dot', () => {
    expect(ctx('db.orders.|')).toEqual({ kind: 'collection', db: null, coll: 'orders' });
    expect(ctx('db.getCollection("order items").|')).toEqual({ kind: 'collection', db: null, coll: 'order items' });
    expect(ctx("db['order items'].fi|")).toEqual({ kind: 'collection', db: null, coll: 'order items' });
    expect(ctx('db.getSiblingDB("logs").events.|')).toEqual({ kind: 'collection', db: 'logs', coll: 'events' });
    expect(ctx('db.getSiblingDB("logs")["a b"].|')).toEqual({ kind: 'collection', db: 'logs', coll: 'a b' });
    const l = labels('db.orders.|');
    expect(l).toEqual(expect.arrayContaining(['find', 'findOne', 'aggregate', 'insertOne', 'deleteMany', 'drop']));
    expect(item('db.orders.|', 'find')).toMatchObject({ insertText: 'find({$0})', snippet: true });
    expect(item('db.orders.|', 'aggregate')).toMatchObject({ insertText: 'aggregate([$0])', snippet: true });
    expect(item('db.orders.|', 'countDocuments')?.insertText).toBe('countDocuments({$0})');
    expect(item('db.orders.|', 'updateOne')?.insertText).toBe('updateOne({$1}, { \\$set: { $0 } })');
    // `db.system.` continues a dotted collection name.
    expect(ctx('db.system.|')).toEqual({ kind: 'collection', db: null, coll: 'system' });
    expect(item('db.system.|', 'users')).toMatchObject({ kind: 'collection', insertText: 'users' });
  });

  it('modifiers_after_find_or_aggregate', () => {
    expect(ctx('db.orders.find({}).|')).toEqual({ kind: 'modifier' });
    expect(ctx('db.orders.aggregate([]).|')).toEqual({ kind: 'modifier' });
    expect(ctx('db.orders.find({}).sort({a: 1}).|')).toEqual({ kind: 'modifier' });
    expect(ctx('db.orders.insertOne({}).|')).toEqual({ kind: 'none' });
    expect(ctx('db.orders.find({}) |')).toEqual({ kind: 'none' });
    expect(labels('db.orders.find().|')).toEqual(
      expect.arrayContaining(['sort', 'limit', 'skip', 'project', 'projection', 'maxTimeMS', 'count', 'toArray', 'pretty', 'explain']),
    );
    expect(item('db.orders.find().|', 'limit')).toMatchObject({ insertText: 'limit($0)', snippet: true });
  });

  it('chained_line_continues_the_statement', () => {
    expect(ctx('db.orders.find({})\n  .|')).toEqual({ kind: 'modifier' });
    expect(ctx('db.orders.find({})\n  // note\n  .so|')).toEqual({ kind: 'modifier' });
    expect(ctx('db.orders.find({})\n  .sort({|')).toEqual({ kind: 'key', role: 'fields', db: null, coll: 'orders' });
    // Without the dot the next line is a new statement.
    expect(ctx('db.orders.find({})\nso|')).toEqual({ kind: 'statement' });
  });

  it('filter_keys_are_fields_then_query_operators', () => {
    const filter = { kind: 'key', role: 'query', db: null, coll: 'orders' };
    expect(ctx('db.orders.find({|')).toEqual(filter);
    expect(ctx('db.orders.find({|})')).toEqual(filter);
    expect(ctx('db.orders.find({ status: "paid", |')).toEqual(filter);
    expect(ctx('db.orders.findOne({ st|')).toEqual(filter);
    expect(ctx('db.orders.countDocuments({|')).toEqual(filter);
    expect(ctx('db.orders.distinct("status", {|')).toEqual(filter);
    expect(ctx('db.orders.deleteMany({|')).toEqual(filter);
    expect(ctx('db.orders.updateOne({|')).toEqual(filter);
    expect(ctx('db.orders.findOneAndDelete({|')).toEqual(filter);
    expect(ctx('db.getCollection("order items").find({|')).toEqual({ ...filter, coll: 'order items' });
    expect(ctx('db.orders.find({ $or: [ { |')).toEqual(filter);
    expect(labels('db.orders.find({|')).toEqual(['_id', 'status', 'order date']);
    expect(item('db.orders.find({|', 'order date')?.insertText).toBe('"order date"');
    expect(item('db.orders.find({|', 'status')).toMatchObject({ kind: 'field', insertText: 'status' });
    // A key starting with `$`: the query operators, not the fields.
    const ops = labels('db.orders.find({ $|');
    expect(ops).toEqual(expect.arrayContaining(['$eq', '$gt', '$in', '$exists', '$regex', '$and', '$or', '$nor', '$not', '$elemMatch', '$expr']));
    expect(ops).not.toContain('status');
    expect(ops).not.toContain('$set');
    // The range replaces the `$` too.
    const { text, offset } = at('db.orders.find({ $g|');
    expect(analyzeMongo(text, offset)).toMatchObject({ prefix: '$g', from: offset - 2 });
  });

  it('operator_object_of_a_field', () => {
    expect(ctx('db.orders.find({ total: { |')).toEqual({ kind: 'key', role: 'operators', db: null, coll: 'orders' });
    expect(labels('db.orders.find({ total: { $|')).toContain('$gte');
    expect(labels('db.orders.find({ total: { |')).not.toContain('status');
  });

  it('update_document_and_projection', () => {
    expect(ctx('db.orders.updateOne({}, {|')).toEqual({ kind: 'key', role: 'update', db: null, coll: 'orders' });
    expect(ctx('db.orders.updateMany({}, { $|')).toEqual({ kind: 'key', role: 'update', db: null, coll: 'orders' });
    expect(ctx('db.orders.findOneAndUpdate({}, { $set: { |')).toEqual({ kind: 'key', role: 'fields', db: null, coll: 'orders' });
    expect(ctx('db.orders.find({}, {|')).toEqual({ kind: 'key', role: 'fields', db: null, coll: 'orders' });
    expect(ctx('db.orders.find({}).project({|')).toEqual({ kind: 'key', role: 'fields', db: null, coll: 'orders' });
    expect(ctx('db.orders.find({}).sort({ |')).toEqual({ kind: 'key', role: 'fields', db: null, coll: 'orders' });
    const update = labels('db.orders.updateOne({}, { $|');
    expect(update).toEqual(
      expect.arrayContaining(['$set', '$unset', '$inc', '$push', '$pull', '$addToSet', '$rename', '$min', '$max', '$mul', '$currentDate']),
    );
    expect(update).not.toContain('$gt');
    expect(labels('db.orders.updateOne({}, { $set: { |')).toEqual(['_id', 'status', 'order date']);
    // Only fields where a field name goes.
    expect(labels('db.orders.find({}).sort({ $|')).toEqual([]);
  });

  it('aggregate_stages', () => {
    expect(ctx('db.orders.aggregate([{|')).toEqual({ kind: 'key', role: 'stage', db: null, coll: 'orders' });
    expect(ctx('db.orders.aggregate([ { $match: {} }, { $|')).toEqual({ kind: 'key', role: 'stage', db: null, coll: 'orders' });
    expect(labels('db.orders.aggregate([{ |')).toEqual(
      expect.arrayContaining(['$match', '$group', '$project', '$sort', '$limit', '$skip', '$unwind', '$lookup', '$addFields', '$set', '$unset', '$count', '$facet', '$replaceRoot', '$sample', '$out', '$merge', '$unionWith']),
    );
    // A `$match` stage holds a filter.
    expect(ctx('db.orders.aggregate([{ $match: { |')).toEqual({ kind: 'key', role: 'query', db: null, coll: 'orders' });
    expect(ctx('db.orders.aggregate([{ $sort: { |')).toEqual({ kind: 'key', role: 'fields', db: null, coll: 'orders' });
  });

  it('value_position_offers_helpers', () => {
    expect(ctx('db.orders.find({ _id: |')).toEqual({ kind: 'value' });
    expect(ctx('db.orders.find({ _id: Obj|')).toEqual({ kind: 'value' });
    expect(ctx('db.orders.find({ status: { $in: [|')).toEqual({ kind: 'value' });
    const l = labels('db.orders.find({ _id: |');
    expect(l).toEqual(
      expect.arrayContaining(['ObjectId', 'ISODate', 'NumberLong', 'NumberInt', 'NumberDecimal', 'UUID', 'BinData', 'Timestamp', 'MinKey', 'MaxKey']),
    );
    expect(item('db.orders.find({ _id: |', 'ObjectId')).toMatchObject({ insertText: 'ObjectId("$0")', snippet: true });
    // After a complete value: nothing.
    expect(ctx('db.orders.find({ _id: 1 |')).toEqual({ kind: 'none' });
    expect(ctx('db.orders.find({ _id: ObjectId(|')).toEqual({ kind: 'none' });
  });

  it('use_and_show', () => {
    expect(ctx('use |')).toEqual({ kind: 'use' });
    expect(ctx('use sh|')).toEqual({ kind: 'use' });
    expect(ctx('use|')).toEqual({ kind: 'statement' });
    expect(ctx('show |')).toEqual({ kind: 'show' });
    expect(ctx('show dbs |')).toEqual({ kind: 'none' });
    expect(labels('use |')).toEqual(['admin', 'shop']);
    expect(labels('show |')).toEqual(['dbs', 'databases', 'collections', 'tables']);
  });

  it('nothing_inside_strings_or_comments', () => {
    expect(ctx('db.orders.find({ status: "pa|')).toEqual({ kind: 'none' });
    expect(ctx("db.orders.find({ '|')")).toEqual({ kind: 'none' });
    expect(ctx('db.orders.find({ "|" })')).toEqual({ kind: 'none' });
    expect(ctx('// db.|')).toEqual({ kind: 'none' });
    expect(ctx('/* db.orders.find({ | */')).toEqual({ kind: 'none' });
    expect(ctx('db.orders.find({ name: /ab|')).toEqual({ kind: 'none' });
    expect(ctx('db.orders.find({ s: `x|')).toEqual({ kind: 'none' });
    expect(labels('db.orders.find({ status: "pa|')).toEqual([]);
    // A closed string or comment is fine.
    expect(ctx('db.orders.find({ "status": "paid", |')).toEqual({ kind: 'key', role: 'query', db: null, coll: 'orders' });
    expect(ctx('/* x */ db.|')).toEqual({ kind: 'db', db: null });
  });

  it('unknown_collection_has_no_fields_but_operators_work', () => {
    const none: MongoData = { ...data, fields: [] };
    expect(labels('db.nope.find({|', none)).toEqual([]);
    expect(labels('db.nope.find({ $|', none)).toContain('$gt');
    expect(labels('db.nope.updateOne({}, { $|', none)).toContain('$set');
  });
});

// ---- Monaco provider ----

type FakeProvider = {
  triggerCharacters: string[];
  provideCompletionItems: (...a: unknown[]) => unknown;
};

function fakeMonaco() {
  const providers = new Map<string, FakeProvider>();
  const monaco = {
    languages: {
      CompletionItemKind: { Keyword: 1, Function: 2, Struct: 3, Interface: 4, Field: 5, Module: 6, Method: 7, Operator: 8, Snippet: 27 },
      CompletionItemInsertTextRule: { InsertAsSnippet: 4 },
      registerCompletionItemProvider: (lang: string, p: FakeProvider) => {
        providers.set(lang, p);
        return { dispose: () => providers.delete(lang) };
      },
    },
    Range: class {
      constructor(
        public startLineNumber: number,
        public startColumn: number,
        public endLineNumber: number,
        public endColumn: number,
      ) {}
    },
  };
  return { monaco, providers };
}

function fakeModel(marked: string) {
  const { text, offset } = at(marked);
  return {
    getValue: () => text,
    getOffsetAt: () => offset,
    getPositionAt: (o: number) => ({ lineNumber: 1, column: o + 1 }),
  };
}

type List = { suggestions: { label: string; insertText: string; insertTextRules?: number; range: { startColumn: number } }[]; incomplete?: boolean };

describe('mongo completion: monaco provider', () => {
  const conn = '33333333-3333-4333-8333-333333333333';
  const live = { isCancellationRequested: false };
  const target = () => ({ tabId: 't', connId: conn, db: 'shop' });
  const position = { lineNumber: 1, column: 1 };

  beforeEach(() => {
    useCompletion.reset();
    useConnections.getState().reset();
    resetQuietOpens();
    useConnections.getState().setInfo(conn, serverInfo('Mongo', [7, 0, 0]));
    mockBackend({
      list_objects: ({ kind }) => (kind === 'Collection' ? [{ name: 'orders', kind: 'Collection', comment: null, row_estimate: 3 }] : []),
      table_detail: ({ obj }) => ({
        object: obj,
        columns: [
          { name: '_id', db_type: 'objectId' },
          { name: 'status', db_type: 'string' },
        ],
      }),
      list_databases: () => [{ name: 'shop', key_count: null }],
    });
  });

  it('registers_for_the_mongo_language_only', () => {
    const { monaco, providers } = fakeMonaco();
    const h = registerMongoCompletion(monaco as never, target);
    expect([...providers.keys()]).toEqual(['mongo']);
    expect(providers.get('mongo')?.triggerCharacters).toEqual(expect.arrayContaining(['.', '$', '{']));
    h.dispose();
    expect(providers.size).toBe(0);
  });

  it('db_dot_waits_for_the_collections_then_lists_them', async () => {
    const { monaco, providers } = fakeMonaco();
    registerMongoCompletion(monaco as never, target);
    const p = providers.get('mongo')!;
    const list = (await p.provideCompletionItems(fakeModel('db.|'), position, {}, live)) as List;
    expect(list.suggestions.map((s) => s.label)).toContain('orders');
    expect(list.suggestions.find((s) => s.label === 'find')).toBeUndefined();
    expect(list.suggestions.find((s) => s.label === 'getCollection')?.insertTextRules).toBe(4);
    expect(callsOf('list_objects')).toEqual([
      { id: conn, scope: { database: 'shop', schema: null }, kind: 'Collection', showSystem: false },
      { id: conn, scope: { database: 'shop', schema: null }, kind: 'View', showSystem: false },
    ]);
    // Cached: answered at once the next time, no second load.
    const again = p.provideCompletionItems(fakeModel('db.|'), position, {}, live) as List;
    expect(Array.isArray(again.suggestions)).toBe(true);
    expect(callsOf('list_objects')).toHaveLength(2);
  });

  it('filter_keys_load_the_sampled_fields', async () => {
    const { monaco, providers } = fakeMonaco();
    registerMongoCompletion(monaco as never, target);
    const p = providers.get('mongo')!;
    const list = (await p.provideCompletionItems(fakeModel('db.orders.find({|'), position, {}, live)) as List;
    expect(list.suggestions.map((s) => s.label)).toEqual(['_id', 'status']);
    expect(callsOf('table_detail')).toEqual([{ id: conn, obj: { database: 'shop', schema: null, name: 'orders', kind: 'Collection' } }]);
    // An unknown collection: no detail load, operators still offered.
    const ops = (await p.provideCompletionItems(fakeModel('db.nope.find({ $|'), position, {}, live)) as List;
    expect(ops.suggestions.map((s) => s.label)).toContain('$gt');
    expect(ops.suggestions[0]!.range.startColumn).toBe('db.nope.find({ '.length + 1);
    const none = (await p.provideCompletionItems(fakeModel('db.nope.find({|'), position, {}, live)) as List;
    expect(none.suggestions).toEqual([]);
    expect(callsOf('table_detail')).toHaveLength(1);
  });

  it('use_lists_the_databases_and_strings_get_nothing', async () => {
    const { monaco, providers } = fakeMonaco();
    registerMongoCompletion(monaco as never, target);
    const p = providers.get('mongo')!;
    const list = (await p.provideCompletionItems(fakeModel('use |'), position, {}, live)) as List;
    expect(list.suggestions.map((s) => s.label)).toEqual(['shop']);
    const str = (await p.provideCompletionItems(fakeModel('db.orders.find({ a: "|'), position, {}, live)) as List;
    expect(str.suggestions).toEqual([]);
  });

  it('target_is_a_mongo_tab_only', () => {
    mockBackend({ tabs_save: () => null });
    useConnections.getState().setStored(
      [],
      [config(conn, 'docs', { kind: 'mongo', default_db: 'shop' }), config('11111111-1111-4111-8111-111111111111', 'sql'), config('22222222-2222-4222-8222-222222222222', 'cache', { kind: 'redis' })],
    );
    const mongoDoc = useQueryDocs.getState().newQuery(conn, 'logs');
    const sqlDoc = useQueryDocs.getState().newQuery('11111111-1111-4111-8111-111111111111', 'shop');
    const redisDoc = useQueryDocs.getState().newQuery('22222222-2222-4222-8222-222222222222', null);
    const model = (id: string) => ({ uri: { scheme: 'query', authority: id } });
    expect(mongoDocTarget(model(mongoDoc))).toEqual({ tabId: mongoDoc, connId: conn, db: 'logs' });
    // A tab without a database of its own: the connection's default database, as the toolbar shows it.
    const defaultDoc = useQueryDocs.getState().newQuery(conn, null);
    expect(mongoDocTarget(model(defaultDoc))).toEqual({ tabId: defaultDoc, connId: conn, db: 'shop' });
    expect(mongoDocTarget(model(sqlDoc))).toBeNull();
    expect(mongoDocTarget(model(redisDoc))).toBeNull();
    expect(mongoDocTarget({ uri: { scheme: 'file', authority: 'x' } })).toBeNull();
  });
});
