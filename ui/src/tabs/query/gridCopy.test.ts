import { describe, expect, it } from 'vitest';
import type { ObjectRef, Value } from '../../bindings';
import {
  CELL_CAP_BYTES,
  csvField,
  maybeTruncated,
  plainText,
  qualifiedTable,
  singleSourceTable,
  sqlLiteral,
  toCsv,
  toInsertStatements,
  toTsv,
  tsvField,
} from './gridCopy';

const text = (v: string): Value => ({ t: 'Text', v });
const NULL: Value = { t: 'Null' };

describe('plainText', () => {
  it('writes_null_as_empty_and_json_verbatim', () => {
    expect(plainText(NULL)).toBe('');
    expect(plainText({ t: 'Json', v: '{ "a": 1 }' })).toBe('{ "a": 1 }');
    expect(plainText({ t: 'Int', v: '9223372036854775807' })).toBe('9223372036854775807');
    expect(plainText({ t: 'Bool', v: true })).toBe('true');
    expect(plainText(text('héllo ✓ 😀'))).toBe('héllo ✓ 😀');
  });
});

describe('toTsv', () => {
  it('joins_with_tabs_and_crlf_and_no_trailing_newline', () => {
    expect(toTsv([[text('a'), { t: 'Int', v: 1 }], [text('b'), NULL]])).toBe('a\t1\r\nb\t');
  });

  it('quotes_fields_with_tab_newline_or_quote_excel_style', () => {
    expect(tsvField('plain')).toBe('plain');
    expect(tsvField('a\tb')).toBe('"a\tb"');
    expect(tsvField('a\nb')).toBe('"a\nb"');
    expect(tsvField('a\r\nb')).toBe('"a\r\nb"');
    expect(tsvField('say "hi"')).toBe('"say ""hi"""');
    expect(toTsv([[text('x\ty'), text('q"')]])).toBe('"x\ty"\t"q"""');
  });

  it('keeps_commas_and_spaces_unquoted', () => {
    expect(toTsv([[text('a, b'), text(' padded ')]])).toBe('a, b\t padded ');
  });

  it('puts_the_header_first_and_quotes_it_too', () => {
    expect(toTsv([[text('1')]], ['id\tx'])).toBe('"id\tx"\r\n1');
    expect(toTsv([], ['id'])).toBe('id');
  });

  it('keeps_unicode_and_empty_strings', () => {
    expect(toTsv([[text('日本語'), text('')]])).toBe('日本語\t');
  });
});

describe('toCsv', () => {
  it('quotes_commas_quotes_and_line_breaks_and_adds_the_header', () => {
    expect(csvField('a,b')).toBe('"a,b"');
    expect(csvField('a\tb')).toBe('a\tb');
    expect(toCsv([[text('a,b'), NULL, text('x"y')]], ['c1', 'c,2', 'c3'])).toBe('c1,"c,2",c3\r\n"a,b",,"x""y"');
  });
});

describe('sqlLiteral', () => {
  const my = (v: Value, capped = false) => sqlLiteral(v, 'mysql', capped);
  const pg = (v: Value, capped = false) => sqlLiteral(v, 'postgres', capped);

  it('writes_numbers_bare_and_null_booleans', () => {
    expect(my({ t: 'Int', v: -5 })).toEqual({ ok: true, sql: '-5' });
    expect(my({ t: 'UInt', v: '18446744073709551615' })).toEqual({ ok: true, sql: '18446744073709551615' });
    expect(my({ t: 'Float', v: 1.5 })).toEqual({ ok: true, sql: '1.5' });
    expect(my({ t: 'Decimal', v: '12.340' })).toEqual({ ok: true, sql: '12.340' });
    expect(my(NULL)).toEqual({ ok: true, sql: 'NULL' });
    expect(my({ t: 'Bool', v: true })).toEqual({ ok: true, sql: 'TRUE' });
    expect(pg({ t: 'Bool', v: false })).toEqual({ ok: true, sql: 'FALSE' });
  });

  it('quotes_odd_decimals_and_refuses_non_finite_floats', () => {
    expect(pg({ t: 'Decimal', v: 'NaN' })).toEqual({ ok: true, sql: "'NaN'" });
    expect(my({ t: 'Float', v: Number.NaN }).ok).toBe(false);
    expect(my({ t: 'Float', v: Number.POSITIVE_INFINITY }).ok).toBe(false);
    expect(my({ t: 'Int', v: 'abc' }).ok).toBe(false);
  });

  it('quotes_strings_with_doubled_quotes', () => {
    expect(pg(text("O'Brien"))).toEqual({ ok: true, sql: "'O''Brien'" });
    expect(my(text("O'Brien"))).toEqual({ ok: true, sql: "'O''Brien'" });
    expect(my({ t: 'DateTime', v: '2024-01-02 03:04:05' })).toEqual({ ok: true, sql: "'2024-01-02 03:04:05'" });
    expect(my({ t: 'Uuid', v: 'abc' })).toEqual({ ok: true, sql: "'abc'" });
  });

  it('doubles_backslashes_for_mysql_only', () => {
    expect(my(text('C:\\dir\\f'))).toEqual({ ok: true, sql: "'C:\\\\dir\\\\f'" });
    expect(pg(text('C:\\dir\\f'))).toEqual({ ok: true, sql: "'C:\\dir\\f'" });
    expect(my(text("a\\'b"))).toEqual({ ok: true, sql: "'a\\\\''b'" });
  });

  it('keeps_unicode_and_newlines_inside_the_literal', () => {
    expect(my(text('日本\n😀'))).toEqual({ ok: true, sql: "'日本\n😀'" });
  });

  it('refuses_binary', () => {
    const r = my({ t: 'Bytes', v: { len: 3, preview: [1, 2, 3] } });
    expect(r.ok).toBe(false);
  });

  it('writes_postgres_arrays_and_refuses_them_for_mysql', () => {
    const arr: Value = { t: 'Array', v: [{ t: 'Int', v: 1 }, text("it's")] };
    expect(pg(arr)).toEqual({ ok: true, sql: "ARRAY[1, 'it''s']" });
    expect(pg({ t: 'Array', v: [] })).toEqual({ ok: true, sql: "'{}'" });
    expect(my(arr).ok).toBe(false);
    expect(pg({ t: 'Array', v: [{ t: 'Bytes', v: { len: 1, preview: [1] } }] }).ok).toBe(false);
  });

  it('writes_other_types_as_quoted_text', () => {
    expect(pg({ t: 'Other', v: { type_name: 'interval', text: '1 day' } })).toEqual({ ok: true, sql: "'1 day'" });
  });
});

describe('truncated cells', () => {
  const cut = (n: number) => 'x'.repeat(n);

  it('flags_text_cut_at_the_cap_only_when_the_page_reports_cut_cells', () => {
    expect(maybeTruncated(text(cut(CELL_CAP_BYTES)), true)).toBe(true);
    expect(maybeTruncated(text(cut(CELL_CAP_BYTES)), false)).toBe(false);
    expect(maybeTruncated(text(cut(100)), true)).toBe(false);
    expect(maybeTruncated(text(cut(CELL_CAP_BYTES - 1)), true)).toBe(true);
    expect(maybeTruncated(text(cut(CELL_CAP_BYTES - 10)), true)).toBe(false);
  });

  it('counts_utf8_bytes_not_characters', () => {
    // 3-byte characters cut on a character boundary: 21845 * 3 = 65535 bytes.
    expect(maybeTruncated(text('€'.repeat(21845)), true)).toBe(true);
    expect(maybeTruncated(text('€'.repeat(1000)), true)).toBe(false);
  });

  it('looks_inside_json_other_and_arrays', () => {
    expect(maybeTruncated({ t: 'Json', v: cut(CELL_CAP_BYTES) }, true)).toBe(true);
    expect(maybeTruncated({ t: 'Other', v: { type_name: 't', text: cut(CELL_CAP_BYTES) } }, true)).toBe(true);
    expect(maybeTruncated({ t: 'Array', v: [text(cut(CELL_CAP_BYTES))] }, true)).toBe(true);
    expect(maybeTruncated({ t: 'Int', v: 1 }, true)).toBe(false);
  });

  it('refuses_a_literal_for_a_truncated_cell', () => {
    expect(sqlLiteral(text(cut(CELL_CAP_BYTES)), 'mysql', true).ok).toBe(false);
    expect(sqlLiteral(text(cut(CELL_CAP_BYTES)), 'mysql', false).ok).toBe(true);
  });
});

describe('toInsertStatements', () => {
  const columns = ['id', 'name', 'note'];
  const rows: Value[][] = [
    [{ t: 'Int', v: 1 }, text("O'Neil"), NULL],
    [{ t: 'Int', v: 2 }, text('b'), { t: 'Bool', v: true }],
  ];

  it('writes_one_statement_per_row_with_mysql_quoting', () => {
    const r = toInsertStatements({ table: '`shop`.`users`', engine: 'mysql', columns, rows });
    expect(r).toEqual({
      ok: true,
      text:
        "INSERT INTO `shop`.`users` (`id`, `name`, `note`) VALUES (1, 'O''Neil', NULL);\n" +
        "INSERT INTO `shop`.`users` (`id`, `name`, `note`) VALUES (2, 'b', TRUE);",
    });
  });

  it('quotes_postgres_identifiers_and_escapes_them', () => {
    const r = toInsertStatements({ table: '"public"."t"', engine: 'postgres', columns: ['a"b'], rows: [[text('x')]] });
    expect(r).toEqual({ ok: true, text: `INSERT INTO "public"."t" ("a""b") VALUES ('x');` });
  });

  it('names_the_column_and_row_when_it_refuses', () => {
    const r = toInsertStatements({
      table: 't',
      engine: 'mysql',
      columns: ['id', 'blob'],
      rows: [[{ t: 'Int', v: 1 }, { t: 'Bytes', v: { len: 1, preview: [1] } }]],
      rowNumbers: [41],
    });
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.message).toMatch(/column blob, row 41/);
  });

  it('refuses_truncated_text', () => {
    const r = toInsertStatements({
      table: 't',
      engine: 'mysql',
      columns: ['c'],
      rows: [[text('x'.repeat(CELL_CAP_BYTES))]],
      capped: true,
    });
    expect(r.ok).toBe(false);
  });
});

describe('table names', () => {
  const ref = (over: Partial<ObjectRef> = {}): ObjectRef => ({ database: 'shop', schema: 'public', name: 'users', kind: 'Table', ...over }) as ObjectRef;

  it('qualifies_per_engine', () => {
    expect(qualifiedTable(ref(), 'mysql')).toBe('`shop`.`users`');
    expect(qualifiedTable(ref(), 'postgres')).toBe('"public"."users"');
    expect(qualifiedTable(ref({ schema: null as never }), 'postgres')).toBe('"users"');
    expect(qualifiedTable(ref({ name: 'we`ird' }), 'mysql')).toBe('`shop`.`we``ird`');
  });

  it('finds_the_single_source_table_of_a_result', () => {
    const a = ref();
    expect(singleSourceTable([{ origin_table: a }, { origin_table: null }, { origin_table: ref() }])).toEqual(a);
    expect(singleSourceTable([{ origin_table: a }, { origin_table: ref({ name: 'other' }) }])).toBeNull();
    expect(singleSourceTable([{ origin_table: null }])).toBeNull();
  });
});
