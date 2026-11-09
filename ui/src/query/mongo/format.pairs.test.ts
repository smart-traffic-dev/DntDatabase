import { expect, it } from 'vitest';
import { formatMongo, formatMongoRange } from './format';

// The formatter's [input, formatted] pairs, written to a fixture that the console's own
// test reads (`formatted_scripts_run_the_same_commands` in
// `crates/driver-mongo/src/shell/parse.rs`): splitting and parsing the formatted text
// there must give the commands of the input. This test fails when the fixture is stale.
// Regenerate both fixtures with: pnpm --dir ui test -u format.pairs
const FIXTURE = '../../../../crates/driver-mongo/tests/fixtures/format_pairs.json';

const INPUTS = [
  // Layout.
  'db.orders.find({status:"paid",total:{$gt:10}})',
  'db.orders.find({status:"paid",customer:"alice@example.com",total:{$gt:100,$lt:5000},createdAt:{$gte:ISODate("2024-01-01T00:00:00Z")}})',
  'db.orders.find({status:"paid"},{_id:0,customer:1,total:1}).sort({total:-1,createdAt:-1}).limit(10)',
  'db.c.find({a:1})\n  .sort({b:-1})\n  .limit(10)',
  'db.getSiblingDB("reporting").daily_totals.find({day:{$gte:"2024-01-01"}}).sort({day:1}).limit(100)',
  'db.users.updateOne({_id:ObjectId("64b7f0c2a1b2c3d4e5f60718")},{$set:{name:"Alice",email:"alice@example.com"}})',
  'db.orders.aggregate([{$match:{status:"paid"}},{$group:{_id:"$customer",total:{$sum:"$total"}}},{$sort:{total:-1}},{$limit:5}])',
  'db.orders.aggregate([{$match:{status:"paid"}},{$group:{_id:"$customer",total:{$sum:"$total"}}}]).explain()',
  `db.c.insertOne({matrix:[[${'1,'.repeat(20)}1],[${'2,'.repeat(20)}2]],e:[],o:{}})`,
  'db.c.insertMany([{a:1,},{b:[1,2,],}])',
  'db.c.find({a:\'x\',"b c":"y\\"z",r:/^a\\/b[/]/i,n:1.5e3,h:0x1F,f:.5,neg:-1,t:`a\n  b`})',
  'db.café.find({名前:"東京"})',
  // Statements.
  'use shop\ndb.a.find();db.b.find( ) ;  db.c.find()',
  'use   2024_logs\nshow    dbs\nuse 01-x',
  'use (x)\nuse a . b\nshow  ( x )',
  'db.c.find({a: [] // c7\n, // c3\nb: 1})\ndb.c // a\n.find() // b',
  'db.c.find({a: [1, 2]}) // x  db.c.drop()',
  'db.a.find()\n\n\n\ndb.b.find()\ndb.c.find()',
  '\n\n  db.a.find()  ;;\n\n',
  'db.a.find({x:1})\r\n\r\n\r\ndb.b.find()\r\n',
  'db.a.find()\n  .limit(1)\ndb.b.find({\na: 1\n})\nshow dbs',
  // Comments.
  '// the paid ones\ndb.orders.find({status:"paid"})   // recent\n\n/* block */ db.a.find()\n// last',
  'db.c.find({\n// only paid\nstatus:"paid", // trailing\n  total:{$gt:1} /* big */\n})',
  'db.c.find({a:1, // x\n})\ndb.c.find({ // y\n})\ndb.c.find({a: // z\n1})',
  'db.c.find({\n  a: 1\n})\n  .sort({a: -1})\n\n  // keep going\n  .limit(5)',
  'db.a.find(); // first\ndb.b.find() /* second */;',
  'db.c.find() /* x */ .limit(1)\ndb.c.find()\n/* y */ .limit(1)\ndb.c // w\n.find()',
  'db.c.find({a:1}\n// x\n, {b: 1})',
  'db.c.find({a: 1\n// one\n, // two\n})',
  'db.c.find(/* c */)\ndb.c.find([/* a */ 1, 2 /* b */])',
  'db.a.find()\n// old: db.c.find(); db.c.drop()\ndb.b.find()',
  'db.a.find() // keep; db.c.drop()\ndb.b.find({x :1})',
  'db.a.find( /* example: {a :1} */ )',
  // Line terminators the console ends a `//` comment at.
  'db.c // x\u2028 .find({a: 1})\n.limit(1)',
  'db.c.find({a: 1}) // x\rdb.c.drop()',
  'db.c.find({a: 1}) // x\u2029db.c.drop()',
  // Regex and division.
  'db.c.find().forEach(d => /(a,b)/.test(d.name))',
  'x = 1 - /a(db.c.deleteMany({ aaaaaaaaaaaaaaaaaaaaaa: 1, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: 2 }))/',
  'db.c.find({a: x/2, b: x / 2, c: [1]/2, d: /x y/, e: [/a,b/i], f: {$regex: /(a|b) c/}})',
  'db.c.find({a: 1 + /x(/, b: 2})',
  // What the formatter refuses (no formatted text).
  'db.c.find().limit(1) / 2',
  'db.c.find({a: "\\1"})',
  'db.c.find({a:1)',
  'db.c.find({a:1;})',
  'x = 1\ndb.c.find({a: 01})',
];

// Format with a selection: [document, from, to] (the first occurrences of two snippets). The
// fixture holds the document after Format, or the refusal; the console's test splits and
// parses the document before and after.
const SELECTIONS_FIXTURE = '../../../../crates/driver-mongo/tests/fixtures/format_selections.json';

const SELECTIONS: [document: string, from: string, to: string][] = [
  ['db.a.find()\ndb.b.find({\n  x: 1\n})\n  .limit(1); db.c.find( )\n', 'x: 1', 'x: 1'],
  ['db.a.find( )\ndb.b.find({\n  x: 1\n})\n  .limit(1); db.c.find( )\n', 'a.find', 'db.b'],
  // Comments: nothing, or the statements only.
  ['db.a.find()\n// old: db.c.find(); db.c.drop()\ndb.b.find()', 'db.c.find();', 'db.c.drop()'],
  ['db.a.find() // keep; db.c.drop()\ndb.b.find({x :1})', 'keep;', 'db.b.find({x :1})'],
  ['db.a.find( )\n/* example: {a :1} */\ndb.b.find( )', ':1}', 'db.b'],
  ['db.a.find( /* example: {a :1} */ )', 'example', 'example'],
  // A line the console cannot lex: refused when the selection is on it.
  ['db.orders.aggregate([{$match: {status: "paid", total: {$gt: 100}}}, {$out: "paid_orders_archive"}]).explain(); db.b.find() / 2', 'db.orders', '.explain();'],
  ['db.orders.find({status: "paid", customer: "alice@example.com", total: {$gt: 100}}).limit(1); f() / 2', 'db.orders', '.limit(1);'],
  ['db.orders.find({status: "paid", customer: "alice@example.com", total: {$gt: 100}}).limit(1); x = "\\1"', 'db.orders', '.limit(1);'],
  // A bad line after the selection, on another line: formatted.
  ['db.orders.aggregate([{$match: {status: "paid", total: {$gt: 100}}}, {$out: "paid_orders_archive"}]).explain()\ndb.b.find() / 2', 'db.orders', '.explain()'],
  ['db.a.find({x :1})\n\n\n"\\1"', 'x :1', 'x :1'],
  ['db.a.find({x :1})\ndb.b.find("a\\\nb") / 2', 'x :1', 'x :1'],
  ['db.a.find({x :1}); db.b.find("a\\\nb") / 2', 'x :1', 'x :1'],
  ['db.a.find({x :1})\ndb.b.find(`a\nb${x}`)', 'x :1', 'x :1'],
  // On its own the statement reads another way.
  ['a\n/x/', '/x/', '/x/'],
];

it('the_selection_fixture_holds_the_current_output_of_formatMongoRange', async () => {
  const entries = SELECTIONS.map(([document, a, b]) => {
    const from = document.indexOf(a);
    const to = document.indexOf(b, from) + b.length;
    const r = formatMongoRange(document, from, to);
    const result = r?.ok ? document.slice(0, r.from) + r.text + document.slice(r.to) : null;
    return { document, selection: [from, to], result, refused: r === null ? 'Nothing to format' : r.ok ? null : r.message };
  });
  await expect(JSON.stringify(entries, null, 2) + '\n').toMatchFileSnapshot(SELECTIONS_FIXTURE);
});

it('the_fixture_holds_the_current_output_of_formatMongo', async () => {
  const pairs = INPUTS.map((input) => {
    const r = formatMongo(input);
    return { input, formatted: r.ok ? r.text : null };
  });
  await expect(JSON.stringify(pairs, null, 2) + '\n').toMatchFileSnapshot(FIXTURE);
});
