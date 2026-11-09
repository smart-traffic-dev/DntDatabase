import { describe, expect, it } from 'vitest';
import { formatMongo, formatMongoRange } from './format';
import { lexConsole } from './lexer';

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
    expect(fmt('db.c.find([/* a */ 1, 2 /* b */])')).toBe('db.c.find([/* a */ 1, 2 /* b */])');
    expect(fmt('db.c.find([\n/* a */ 1])')).toBe('db.c.find([/* a */ 1])');
    // In empty brackets the comment stays where it is.
    expect(fmt('db.c.find(/* c */)')).toBe('db.c.find(/* c */)');
    expect(fmt('db.c.find([ /* c */ ])')).toBe('db.c.find([/* c */])');
    expect(fmt('db.c.find({/* c */})')).toBe('db.c.find({ /* c */ })');
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
    // A `,` after a comment of its own line goes back after the value.
    expect(fmt('db.c.find({a:1}\n// x\n, {b: 1})')).toBe(lines('db.c.find(', '  { a: 1 },', '  // x', '  { b: 1 }', ')'));
    expect(fmt('db.c.find({a:1}\n// x\n, /* y */ {b: 1})')).toBe(lines('db.c.find(', '  { a: 1 },', '  // x', '  /* y */ { b: 1 }', ')'));
  });

  it('a_dropped_trailing_comma_keeps_its_comments_in_order', () => {
    expect(fmt('db.c.find({a: 1\n// one\n, // two\n})')).toBe(lines('db.c.find({', '  a: 1', '  // one', '  // two', '})'));
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
        const { tokens, comments } = lexConsole(s);
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
  const cm = () => pick(['', '', '', '', ' /* c */ ', ' // lc\n', '\n// own\n', '/*m\nl*/', ' // ls\u2028', ' // cr\r']);
  const scalar = () =>
    pick(['1', '"str value here"', "'q'", '/re/i', '/(a,b) c/', 'ObjectId("64b7f0c2a1b2c3d4e5f60718")', '-1', '- /x(y)/', 'x/2', 'x / 2', '.5', 'ISODate("2024-01-01")', '`t\nx`']);
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
    const { tokens } = lexConsole(src);
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
      if (depth === 0 && (!next || (src.slice(t.end, next.start).includes('\n') && !next.text.startsWith('.')))) {
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

describe('formatMongoRange', () => {
  const text = 'db.a.find()\ndb.b.find({\n  x: 1\n})\n  .limit(1); db.c.find()\n';
  const sel = (s: string, from: string, to: string = from) => formatMongoRange(s, s.indexOf(from), s.indexOf(to) + to.length);

  it('formats_exactly_the_statements_the_selection_touches', () => {
    // Inside `x: 1` only: the whole db.b statement with its `;`.
    const b = text.indexOf('db.b');
    const end = text.indexOf('; db.c') + 1;
    expect(sel(text, 'x: 1')).toEqual({ ok: true, from: b, to: end, text: 'db.b.find({ x: 1 }).limit(1);' });
    // From inside db.a to the blank after db.b's `;`: both, and not the blank.
    expect(formatMongoRange(text, 3, end + 1)).toEqual({ ok: true, from: 0, to: end, text: 'db.a.find()\ndb.b.find({ x: 1 }).limit(1);' });
  });

  it('a_selection_inside_a_line_comment_formats_nothing', () => {
    // The commented-out code stays a comment.
    const s = 'db.a.find()\n// old: db.c.find(); db.c.drop()\ndb.b.find()';
    expect(sel(s, 'db.c.find();', 'db.c.drop()')).toBeNull();
    expect(sel('  // only\n', '// only')).toBeNull();
  });

  it('a_selection_from_a_line_comment_formats_only_the_statements_after_it', () => {
    const s = 'db.a.find() // keep; db.c.drop()\ndb.b.find({x :1})';
    const r = formatMongoRange(s, s.indexOf('keep;'), s.length);
    expect(r).toEqual({ ok: true, from: s.indexOf('db.b'), to: s.length, text: 'db.b.find({ x: 1 })' });
    expect(s.slice(0, s.indexOf('db.b')) + (r as { text: string }).text).toBe('db.a.find() // keep; db.c.drop()\ndb.b.find({ x: 1 })');
  });

  it('a_selection_edge_inside_a_block_comment_never_rewrites_it', () => {
    // Between statements: nothing to format.
    const s = 'db.a.find( )\n/* example: {a :1} */\ndb.b.find( )';
    expect(sel(s, 'example', '{a :1}')).toBeNull();
    // From inside the comment into db.b: db.b only, the comment as it is.
    const r = sel(s, ':1}', 'db.b');
    expect(r).toEqual({ ok: true, from: s.indexOf('db.b'), to: s.length, text: 'db.b.find()' });
    // A comment inside a statement is formatted with it, its text unchanged.
    const t = 'db.a.find( /* example: {a :1} */ )';
    expect(sel(t, 'example')).toEqual({ ok: true, from: 0, to: t.length, text: 'db.a.find(/* example: {a :1} */)' });
  });

  it('a_statement_where_the_script_stops_lexing_is_refused', () => {
    const s = 'db.a.find( )\ndb.b.find("x\ndb.c.find()';
    expect(sel(s, 'a.find')).toEqual({ ok: true, from: 0, to: 12, text: 'db.a.find()' });
    expect(sel(s, '"x')).toEqual({ ok: false, message: 'Unterminated string', offset: s.indexOf('"x') });
  });

  it('a_selection_the_console_would_read_another_way_on_its_own_is_refused', () => {
    // `/x/` after `a` on the line before is a division; on its own, a regex.
    const s = 'a\n/x/';
    expect(sel(s, '/x/')).toEqual({ ok: false, message: 'This selection reads differently without the text before it', offset: 2 });
  });

  it('a_statement_on_a_line_the_console_cannot_lex_is_refused', () => {
    // The console reads that whole line as one refused command: laying out its first
    // statement over lines would let them run on their own (the `$out` without `.explain()`).
    for (const [doc, selected, bad] of [
      ['db.orders.aggregate([{$match: {status: "paid", total: {$gt: 100}}}, {$out: "paid_orders_archive"}]).explain(); db.b.find() / 2', '.explain();', '/ 2'],
      ['db.orders.find({status: "paid", customer: "alice@example.com", total: {$gt: 100}}).limit(1); f() / 2', '.limit(1);', '/ 2'],
      ['db.orders.find({status: "paid", customer: "alice@example.com", total: {$gt: 100}}).limit(1); x = "\\1"', '.limit(1);', '\\1'],
    ] as const) {
      const message = formatMongo(doc);
      expect(message.ok).toBe(false);
      expect(formatMongoRange(doc, 0, doc.indexOf(selected) + selected.length), doc).toEqual(message);
      expect((message as { offset: number }).offset).toBe(doc.indexOf(bad));
    }
  });

  it('a_bad_line_after_the_selection_leaves_it_formattable', () => {
    const doc = 'db.a.find({x :1})\n\ndb.b.find() / 2';
    expect(sel(doc, 'x :1')).toEqual({ ok: true, from: 0, to: 17, text: 'db.a.find({ x: 1 })' });
    // A bad construct starts the line, after blank lines.
    expect(sel('db.a.find({x :1})\n\n\n"\\1"', 'x :1')).toEqual({ ok: true, from: 0, to: 17, text: 'db.a.find({ x: 1 })' });
  });

  it('the_console_backs_off_to_where_a_string_over_lines_began', () => {
    // The bad line is the third, but its string began on the second, after a line
    // continuation: the console backs off to the second line. The first still formats.
    const doc = 'db.a.find({x :1})\ndb.b.find("a\\\nb") / 2';
    expect(sel(doc, 'x :1')).toEqual({ ok: true, from: 0, to: 17, text: 'db.a.find({ x: 1 })' });
    // On one line with that string: refused.
    const same = 'db.a.find({x :1}); db.b.find("a\\\nb") / 2';
    expect(sel(same, 'x :1')).toEqual(formatMongo(same));
  });
});

describe('formatMongo: the console reading', () => {
  const refused = (text: string) => {
    const r = formatMongo(text);
    return r.ok ? null : { message: r.message, offset: r.offset };
  };

  it('a_regex_after_any_punctuation_is_kept_as_written', () => {
    expect(fmt('db.c.find().forEach(d => /(a,b)/.test(d.name))')).toBe('db.c.find().forEach(d => /(a,b)/.test(d.name))');
    // A regex holding what looks like code is one token: nothing in it is laid out.
    const long = 'x = 1 - /a(db.c.deleteMany({ aaaaaaaaaaaaaaaaaaaaaa: 1, bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: 2 }))/';
    expect(fmt(long)).toBe(long);
  });

  it('a_division_keeps_its_spacing', () => {
    expect(fmt('db.c.find({a: x/2})')).toBe('db.c.find({ a: x/2 })');
    expect(fmt('db.c.find({a: x / 2,b:[1]/2})')).toBe('db.c.find({ a: x / 2, b: [1]/2 })');
  });

  it('a_line_comment_ends_where_the_console_ends_it', () => {
    // U+2028 ends a `//` comment but not a command: the chain goes on.
    // Its comment stays before the code, with its U+2028 (a `\n` there would split the command).
    expect(fmt('db.c // x\u2028 .find({a: 1})\n.limit(1)')).toBe('db.c // x\u2028 .find({ a: 1 }).limit(1)');
    expect(fmt('db.c.find({a: [1, 2]}) // x\u2028 db.c.drop()')).toBe('db.c.find({ a: [1, 2] }) // x\u2028 db.c.drop()');
    expect(fmt('db.c.find() // x\rdb.d.find()')).toBe('db.c.find() // x\ndb.d.find()');
  });

  it('refuses_what_the_console_reads_a_line_at_a_time', () => {
    expect(refused('db.c.find().limit(1) / 2')).toEqual({ message: 'JavaScript may read this differently', offset: 21 });
    expect(refused('db.c.find({a: "\\1"})')).toEqual({ message: 'Invalid escape sequence', offset: 15 });
    expect(refused('db.c.find({a: `${x}`})')).toEqual({ message: 'Template literal substitutions (${…}) are not supported', offset: 15 });
    expect(refused('db.c.find({a: 01})')).toEqual({ message: 'Invalid number', offset: 14 });
  });

  it('use_names_the_console_reads_raw', () => {
    expect(fmt('use   2024-logs\nuse\t01_x')).toBe('use 2024-logs\nuse 01_x');
    // After `use` / `show` the console reads raw text: it stays as written.
    expect(fmt('use (x)\nuse a . b\nshow  ( x )')).toBe('use (x)\nuse a . b\nshow ( x )');
  });

  it('a_second_line_comment_starts_its_own_line', () => {
    expect(fmt('db.c.find({a: [] // c7\n, // c3\nb: 1})')).toBe(lines('db.c.find({', '  a: [], // c7', '  // c3', '  b: 1', '})'));
    expect(fmt('db.c // a\n.find() // b')).toBe(lines('db.c.find() // a', '// b'));
  });
});
