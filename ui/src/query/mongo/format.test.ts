import { describe, expect, it } from 'vitest';
import { lexMongoScript } from '../completion/mongo/context';
import { formatMongo, mongoStatementRange } from './format';

/** The formatted text; fails the test when the formatter refuses. */
function fmt(text: string, width?: number): string {
  const r = formatMongo(text, width === undefined ? undefined : { printWidth: width });
  if (!r.ok) throw new Error(`refused: ${r.message} at ${r.offset}`);
  return r.text;
}

const lines = (...l: string[]) => l.join('\n');

describe('formatMongo: objects and arrays', () => {
  it('prints_a_short_document_on_one_line_with_spaces_inside_braces', () => {
    expect(fmt('db.orders.find({status:"paid",total:{$gt:10}})')).toBe('db.orders.find({ status: "paid", total: { $gt: 10 } })');
  });

  it('prints_arrays_without_inner_spaces', () => {
    expect(fmt('db.c.find({tags:{$in:[ "a" ,"b"  ]}})')).toBe('db.c.find({ tags: { $in: ["a", "b"] } })');
  });

  it('prints_empty_documents_arrays_and_calls_tight', () => {
    expect(fmt('db.c.find( { } ).sort({\n})')).toBe('db.c.find({}).sort({})');
    expect(fmt('db.c.insertMany([ ])')).toBe('db.c.insertMany([])');
    expect(fmt('db.c.find(  )')).toBe('db.c.find()');
  });

  it('breaks_a_long_document_one_entry_per_line_without_a_trailing_comma', () => {
    const text =
      'db.orders.find({status:"paid",customer:"alice@example.com",total:{$gt:100,$lt:5000},createdAt:{$gte:ISODate("2024-01-01T00:00:00Z")}})';
    expect(fmt(text)).toBe(
      lines(
        'db.orders.find({',
        '  status: "paid",',
        '  customer: "alice@example.com",',
        '  total: { $gt: 100, $lt: 5000 },',
        '  createdAt: { $gte: ISODate("2024-01-01T00:00:00Z") }',
        '})',
      ),
    );
  });

  it('drops_a_trailing_comma', () => {
    expect(fmt('db.c.find({a:1,})')).toBe('db.c.find({ a: 1 })');
    expect(fmt('db.c.find({a:[1,2,],})')).toBe('db.c.find({ a: [1, 2] })');
    // Not after a hole: `[1,,]` has two elements.
    expect(fmt('x = [1,,]')).toBe('x = [1, ,]');
  });

  it('nested_arrays_stay_flat_when_they_fit_and_break_inside_out_when_not', () => {
    expect(fmt('db.c.insertOne({m:[[1,2],[3,4]],e:[]})')).toBe('db.c.insertOne({ m: [[1, 2], [3, 4]], e: [] })');
    const long = `db.c.insertOne({matrix:[[${'1,'.repeat(20)}1],[${'2,'.repeat(20)}2]]})`;
    expect(fmt(long)).toBe(
      lines(
        'db.c.insertOne({',
        '  matrix: [',
        `    [${'1, '.repeat(20)}1],`,
        `    [${'2, '.repeat(20)}2]`,
        '  ]',
        '})',
      ),
    );
  });

  it('the_print_width_counts_the_indent', () => {
    // 80 columns exactly: one line; one more: broken.
    const fits = `db.c.find({ a: "${'x'.repeat(60)}" })`;
    expect(fits).toHaveLength(80);
    expect(fmt(fits)).toBe(fits);
    const over = `db.c.find({ a: "${'x'.repeat(61)}" })`;
    expect(fmt(over)).toBe(lines('db.c.find({', `  a: "${'x'.repeat(61)}"`, '})'));
    // An inner document of 77 columns fits 2 columns in (with its comma), not 4 columns in.
    const inner = `{ b: "${'y'.repeat(68)}" }`;
    expect(inner).toHaveLength(77);
    expect(fmt(`db.c.insertMany([${inner}, ${inner}])`)).toBe(lines('db.c.insertMany([', `  ${inner},`, `  ${inner}`, '])'));
    expect(fmt(`db.c.insertOne({ a: { a: ${inner} } })`)).toBe(
      lines('db.c.insertOne({', '  a: {', '    a: {', `      b: "${'y'.repeat(68)}"`, '    }', '  }', '})'),
    );
  });

  it('a_narrower_print_width_breaks_sooner', () => {
    expect(fmt('db.c.find({a:1,b:2})', 12)).toBe(lines('db.c.find({', '  a: 1,', '  b: 2', '})'));
  });
});

describe('formatMongo: calls and chains', () => {
  it('a_chain_that_fits_stays_on_one_line', () => {
    expect(fmt('db.c.find({a:1})\n  .sort({b:-1})\n  .limit(10)')).toBe('db.c.find({ a: 1 }).sort({ b: -1 }).limit(10)');
  });

  it('a_long_chain_puts_each_modifier_on_its_own_line', () => {
    const text = 'db.orders.find({status:"paid"},{_id:0,customer:1,total:1}).sort({total:-1,createdAt:-1}).limit(10)';
    expect(fmt(text)).toBe(
      lines(
        'db.orders.find({ status: "paid" }, { _id: 0, customer: 1, total: 1 })',
        '  .sort({ total: -1, createdAt: -1 })',
        '  .limit(10)',
      ),
    );
  });

  it('the_chain_starts_after_the_first_call', () => {
    const text = `db.getSiblingDB("reporting").daily_totals.find({day:{$gte:"2024-01-01"}}).sort({day:1}).limit(100)`;
    expect(fmt(text)).toBe(
      lines('db.getSiblingDB("reporting")', '  .daily_totals.find({ day: { $gte: "2024-01-01" } })', '  .sort({ day: 1 })', '  .limit(100)'),
    );
  });

  it('a_long_argument_list_puts_each_argument_on_its_own_line', () => {
    const text = 'db.users.updateOne({_id:ObjectId("64b7f0c2a1b2c3d4e5f60718")},{$set:{name:"Alice",email:"alice@example.com"}})';
    expect(fmt(text)).toBe(
      lines(
        'db.users.updateOne(',
        '  { _id: ObjectId("64b7f0c2a1b2c3d4e5f60718") },',
        '  { $set: { name: "Alice", email: "alice@example.com" } }',
        ')',
      ),
    );
  });

  it('helper_calls_and_plain_arguments_are_never_broken', () => {
    const id = 'a'.repeat(90);
    expect(fmt(`ObjectId("${id}")`)).toBe(`ObjectId("${id}")`);
    expect(fmt(`db.c.find().limit( 10 ).skip(5)`)).toBe('db.c.find().limit(10).skip(5)');
  });

  it('aggregate_puts_one_stage_per_line_when_it_does_not_fit', () => {
    const text = 'db.orders.aggregate([{$match:{status:"paid"}},{$group:{_id:"$customer",total:{$sum:"$total"}}},{$sort:{total:-1}},{$limit:5}])';
    expect(fmt(text)).toBe(
      lines(
        'db.orders.aggregate([',
        '  { $match: { status: "paid" } },',
        '  { $group: { _id: "$customer", total: { $sum: "$total" } } },',
        '  { $sort: { total: -1 } },',
        '  { $limit: 5 }',
        '])',
      ),
    );
    expect(fmt('db.c.aggregate([{$match:{a:1}},{$limit:5}])')).toBe('db.c.aggregate([{ $match: { a: 1 } }, { $limit: 5 }])');
  });

  it('a_broken_pipeline_with_a_modifier', () => {
    const text = 'db.orders.aggregate([{$match:{status:"paid"}},{$group:{_id:"$customer",total:{$sum:"$total"}}}]).toArray()';
    expect(fmt(text)).toBe(
      lines(
        'db.orders.aggregate([',
        '  { $match: { status: "paid" } },',
        '  { $group: { _id: "$customer", total: { $sum: "$total" } } }',
        '])',
        '  .toArray()',
      ),
    );
  });
});

describe('formatMongo: tokens as written', () => {
  it('keeps_strings_regexes_numbers_and_helpers_exactly', () => {
    expect(fmt(`db.c.find({a:'x',"b c":"y\\"z",r:/^a\\/b[/]/i,n:1.5e3,h:0x1F,l:NumberLong("5"),d:ISODate( "2024-01-01" ),neg:-1})`, 200)).toBe(
      `db.c.find({ a: 'x', "b c": "y\\"z", r: /^a\\/b[/]/i, n: 1.5e3, h: 0x1F, l: NumberLong("5"), d: ISODate("2024-01-01"), neg: -1 })`,
    );
  });

  it('keeps_template_literals_and_unicode', () => {
    expect(fmt('db.café.find({名前:"東京",t:`a\n  b`})')).toBe('db.café.find({ 名前: "東京", t: `a\n  b` })');
  });

  it('counts_unicode_by_characters', () => {
    // 80 characters, more UTF-16 units: one line.
    const text = `db.c.find({ a: "${'😀'.repeat(60)}" })`;
    expect([...text]).toHaveLength(80);
    expect(fmt(text)).toBe(text);
  });

  it('use_and_show', () => {
    expect(fmt('use   2024_logs\nshow    dbs')).toBe('use 2024_logs\nshow dbs');
  });

  it('never_joins_tokens_that_would_read_as_one', () => {
    expect(fmt('x = 1 .toString()')).toBe('x = 1 .toString()');
    expect(fmt('db.c.find({ a: [1, -1] })')).toBe('db.c.find({ a: [1, -1] })');
  });
});

describe('formatMongo: statements', () => {
  it('one_statement_per_line_keeping_semicolons', () => {
    expect(fmt('use shop\ndb.a.find();db.b.find( ) ;  db.c.find()')).toBe(lines('use shop', 'db.a.find();', 'db.b.find();', 'db.c.find()'));
  });

  it('keeps_at_most_one_blank_line_between_statements', () => {
    expect(fmt('db.a.find()\n\n\n\ndb.b.find()\ndb.c.find()')).toBe(lines('db.a.find()', '', 'db.b.find()', 'db.c.find()'));
  });

  it('drops_empty_statements_and_surrounding_blank_space_but_keeps_a_final_newline', () => {
    expect(fmt('\n\n  db.a.find()  ;;\n\n')).toBe('db.a.find();\n');
    expect(fmt('db.a.find()')).toBe('db.a.find()');
    expect(fmt('   ')).toBe('   ');
    expect(fmt('')).toBe('');
  });

  it('blank_lines_inside_a_statement_go', () => {
    expect(fmt('db.c.insertOne({\n\n  a: 1,\n\n  b: 2\n\n})')).toBe('db.c.insertOne({ a: 1, b: 2 })');
  });

  it('keeps_crlf_line_endings', () => {
    expect(fmt('db.a.find({x:1})\r\n\r\n\r\ndb.b.find()\r\n')).toBe('db.a.find({ x: 1 })\r\n\r\ndb.b.find()\r\n');
    const long = 'db.orders.find({status:"paid"},{_id:0,customer:1,total:1}).sort({total:-1,createdAt:-1}).limit(10)\r\n';
    expect(fmt(long)).toBe(fmt(long.trim()).replaceAll('\n', '\r\n') + '\r\n');
  });

  it('keeps_the_statements_the_console_runs', () => {
    // The console splits at `;` and at a line end with its brackets closed, unless the next
    // line starts with `.`: formatting never changes which text is one command.
    const text = 'db.a.find()\n  .limit(1)\ndb.b.find({\na: 1\n})\nshow dbs';
    expect(fmt(text)).toBe(lines('db.a.find().limit(1)', 'db.b.find({ a: 1 })', 'show dbs'));
  });
});

describe('formatMongo: comments', () => {
  it('keeps_comments_between_statements_and_at_line_ends', () => {
    const text = '// the paid ones\ndb.orders.find({status:"paid"})   // recent\n\n/* block */ db.a.find()\n// last';
    expect(fmt(text)).toBe(lines('// the paid ones', 'db.orders.find({ status: "paid" }) // recent', '', '/* block */ db.a.find()', '// last'));
  });

  it('keeps_comments_inside_documents', () => {
    const text = 'db.c.find({\n// only paid\nstatus:"paid", // trailing\n  total:{$gt:1} /* big */\n})';
    expect(fmt(text)).toBe(lines('db.c.find({', '  // only paid', '  status: "paid", // trailing', '  total: { $gt: 1 } /* big */', '})'));
  });

  it('a_line_comment_after_the_last_entry_keeps_the_document_open', () => {
    expect(fmt('db.c.find({a:1, // x\n})')).toBe(lines('db.c.find({', '  a: 1 // x', '})'));
    expect(fmt('db.c.find({ // x\n})')).toBe(lines('db.c.find({ // x', '})'));
  });

  it('inline_block_comments_stay_inline', () => {
    expect(fmt('db.c.find({a:/* one */1})')).toBe('db.c.find({ a: /* one */ 1 })');
    expect(fmt('db.c.find([/* a */ 1, 2 /* b */])')).toBe('db.c.find([ /* a */ 1, 2 /* b */])');
  });

  it('keeps_comments_in_a_chain', () => {
    const text = 'db.c.find({\n  a: 1\n})\n  .sort({a: -1})\n\n  // keep going\n  .limit(5)';
    expect(fmt(text)).toBe(lines('db.c.find({ a: 1 })', '  .sort({ a: -1 })', '  // keep going', '  .limit(5)'));
  });

  it('a_comment_after_a_semicolon', () => {
    expect(fmt('db.a.find(); // first\ndb.b.find() /* second */;')).toBe(lines('db.a.find(); // first', 'db.b.find() /* second */;'));
  });

  it('a_line_comment_at_the_end_without_a_newline', () => {
    expect(fmt('db.a.find() // end')).toBe('db.a.find() // end');
  });

  it('comments_around_commas_and_hugged_arguments_stay', () => {
    expect(fmt('db.c.find({a:1} /* x */)')).toBe('db.c.find({ a: 1 } /* x */)');
    expect(fmt('db.c.find({a:1}\n// x\n, {b: 1})')).toBe(lines('db.c.find(', '  { a: 1 }', '  // x', '  ,', '  { b: 1 }', ')'));
  });
});

describe('formatMongo: refusals', () => {
  const refused = (text: string) => {
    const r = formatMongo(text);
    return r.ok ? null : { message: r.message, offset: r.offset };
  };

  it('refuses_unbalanced_brackets_with_the_position', () => {
    expect(refused('db.c.find({a:1)')).toEqual({ message: "Unexpected ')'", offset: 14 });
    expect(refused('db.c.find({a:1}')).toEqual({ message: "Unclosed '('", offset: 9 });
    expect(refused('db.a.find()\ndb.c.find({a:1})}')).toEqual({ message: "Unexpected '}'", offset: 28 });
    // The console ends a command at `;` even inside brackets.
    expect(refused('db.c.find({a:1;})')).toEqual({ message: "Unclosed '{'", offset: 10 });
  });

  it('refuses_unterminated_strings_comments_and_regexes', () => {
    expect(refused('db.c.find({a:"x})')).toEqual({ message: 'Unterminated string', offset: 13 });
    expect(refused("db.c.find({a:'x})\ndb.d.find()")).toEqual({ message: 'Unterminated string', offset: 13 });
    expect(refused('db.c.find({a:`x})')).toEqual({ message: 'Unterminated template literal', offset: 13 });
    expect(refused('db.a.find()\n/* x')).toEqual({ message: 'Unterminated comment', offset: 12 });
    expect(refused('db.c.find({a:/x})')).toEqual({ message: 'Unterminated regular expression', offset: 13 });
  });

  it('positions_count_the_original_line_breaks', () => {
    expect(refused('db.a.find()\r\ndb.b.find(1')).toEqual({ message: "Unclosed '('", offset: 22 });
    expect(refused('db.a.find()\r\n\r\ndb.b.find({a:"x')).toEqual({ message: 'Unterminated string', offset: 28 });
  });
});

describe('formatMongo: idempotent', () => {
  const samples = [
    'db.orders.find({status:"paid",customer:"alice@example.com",total:{$gt:100,$lt:5000},createdAt:{$gte:ISODate("2024-01-01T00:00:00Z")}})',
    'db.orders.find({status:"paid"},{_id:0,customer:1,total:1}).sort({total:-1,createdAt:-1}).limit(10)',
    'db.orders.aggregate([{$match:{status:"paid"}},{$group:{_id:"$customer",total:{$sum:"$total"}}}]).toArray()',
    'db.c.find({\n// only paid\nstatus:"paid", // trailing\n  total:{$gt:1} /* big */\n})',
    'db.c.find({\n  a: 1\n})\n  .sort({a: -1})\n\n  // keep going\n  .limit(5)',
    '// a\n\n\n/* b */ use x\nshow dbs; db.a.find() // c\n',
    'db.c.find([/* a */ 1, 2 /* b */])\ndb.c.find({a:/* one */1}).sort({b /* x */: 1})',
    'db.c.find({a:1, // x\n})\ndb.c.find({ // y\n})\ndb.c.find({a: // z\n1})',
    'db.c.find() /* x */ .limit(1)\ndb.c.find()\n/* y */ .limit(1)\ndb.c // w\n.find()',
    `db.c.insertOne({matrix:[[${'1,'.repeat(20)}1],[${'2,'.repeat(20)}2]],t:\`a\n  b\`})\r\n`,
    'db.users.updateOne({_id:ObjectId("64b7f0c2a1b2c3d4e5f60718")},{$set:{name:"Alice",email:"alice@example.com"}})',
  ];

  it.each(samples)('format_of_formatted_is_unchanged %#', (text) => {
    const once = fmt(text);
    expect(fmt(once)).toBe(once);
  });

  it('formatting_changes_only_whitespace_and_dropped_commas', () => {
    for (const text of samples) {
      const kept = (s: string) => {
        const { tokens, comments } = lexMongoScript(s);
        return [...tokens.map((t) => t.text), ...comments.map((c) => c.text.trimEnd())].filter((t) => t !== ',' && t !== ';');
      };
      expect(kept(fmt(text)).sort()).toEqual(kept(text).sort());
    }
  });
});

describe('formatMongo: generated scripts', () => {
  // A small seeded generator of console scripts with odd spacing, comments, chains, trailing
  // commas and CRLF: formatting is idempotent and keeps every command the console would run.
  let seed = 12345;
  const rnd = (n: number) => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed % n;
  };
  const pick = <T,>(a: T[]) => a[rnd(a.length)]!;
  const ws = () => pick(['', ' ', '  ', '\n', '\n  ', ' \n\n ', '\r\n']);
  const cm = () => pick(['', '', '', '', ' /* c */ ', ' // lc\n', '\n// own\n', '/*m\nl*/']);
  const scalar = () => pick(['1', '"str value here"', "'q'", '/re/i', 'ObjectId("64b7f0c2a1b2c3d4e5f60718")', '-1', 'ISODate("2024-01-01")', '`t\nx`']);
  const value = (d: number): string => {
    if (d > 3) return scalar();
    const r = rnd(6);
    if (r === 0) return doc(d + 1);
    if (r === 1) return `[${ws()}${Array.from({ length: rnd(4) }, () => value(d + 1)).join(`${ws()},${cm()}${ws()}`)}${pick(['', ','])}${ws()}]`;
    return scalar();
  };
  const doc = (d: number): string => {
    const n = rnd(5);
    const entry = () => `${cm()}${pick(['a', 'status', '$gt', '"q k"', '名前'])}${pick(['', ' '])}:${cm()}${ws()}${value(d)}${cm()}`;
    return `{${ws()}${Array.from({ length: n }, entry).join(`${ws()},${cm()}${ws()}`)}${n ? pick(['', ',']) : ''}${ws()}}`;
  };
  const statement = () => {
    const call = pick(['find', 'aggregate', 'updateOne', 'insertMany']);
    const args = Array.from({ length: rnd(3) }, () => (call === 'aggregate' ? `[${doc(1)},${doc(1)}]` : doc(0))).join(`,${ws()}`);
    let s = `db.${pick(['orders', 'c'])}.${call}(${args})`;
    for (let k = rnd(4); k > 0; k--) s += `${pick(['', '\n  ', ' '])}${cm()}.${pick(['sort', 'limit'])}(${pick(['10', doc(2)])})`;
    return pick([s, 'show dbs', 'use x', s + ';']);
  };
  /** The commands the console runs, as token texts (trailing commas aside). */
  const commands = (text: string): string[] => {
    const src = text.replace(/\r\n?/g, '\n');
    const { tokens } = lexMongoScript(src);
    const out: string[] = [];
    let cur: string[] = [];
    let depth = 0;
    tokens.forEach((t, i) => {
      const next = tokens[i + 1];
      if (t.kind === 'punct' && t.text === ';') {
        if (cur.length) out.push(cur.join(' '));
        cur = [];
        depth = 0;
        return;
      }
      if (t.kind === 'punct' && '([{'.includes(t.text)) depth++;
      if (t.kind === 'punct' && ')]}'.includes(t.text)) depth--;
      if (!(t.text === ',' && next && ')]}'.includes(next.text))) cur.push(t.text.replace(/\r\n?/g, '\n'));
      if (depth === 0 && (!next || (src.slice(t.end, next.start).includes('\n') && next.text !== '.'))) {
        out.push(cur.join(' '));
        cur = [];
      }
    });
    return out;
  };

  it('idempotent_and_the_same_commands', () => {
    for (let n = 0; n < 400; n++) {
      const text = Array.from({ length: 1 + rnd(4) }, statement).join(pick(['\n', '\n\n', '; ', '\n\n\n']));
      const once = fmt(text);
      expect(fmt(once), JSON.stringify(text)).toBe(once);
      expect(commands(once), JSON.stringify(text)).toEqual(commands(text));
    }
  });
});

describe('mongoStatementRange', () => {
  const text = 'db.a.find()\ndb.b.find({\n  x: 1\n})\n  .limit(1); db.c.find()\n';
  it('expands_a_selection_to_the_statements_it_touches', () => {
    // Inside `x: 1` only: the whole db.b statement with its `;`.
    const at = text.indexOf('x: 1');
    const b = text.indexOf('db.b');
    expect(mongoStatementRange(text, at, at + 1)).toEqual({ from: b, to: text.indexOf('; db.c') + 1 });
    // From db.a into db.b: both.
    expect(mongoStatementRange(text, 3, b + 2)).toEqual({ from: 0, to: text.indexOf('; db.c') + 1 });
  });

  it('a_selection_without_statements_stays', () => {
    expect(mongoStatementRange('  // only\n', 0, 5)).toEqual({ from: 0, to: 5 });
  });

  it('where_the_script_stops_lexing_the_statement_runs_to_the_end', () => {
    const text = 'db.a.find()\ndb.b.find("x\ndb.c.find()';
    expect(mongoStatementRange(text, 2, 4)).toEqual({ from: 0, to: 11 });
    expect(mongoStatementRange(text, 15, 16)).toEqual({ from: 12, to: text.length });
    expect(formatMongo(text.slice(12))).toEqual({ ok: false, message: 'Unterminated string', offset: 10 });
  });
});
