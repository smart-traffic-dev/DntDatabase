import { describe, expect, it } from 'vitest';
import {
  ACCESS_LADDER,
  accessTone,
  classicOf,
  classicSummary,
  explainRow,
  extraItems,
  passRows,
  type ClassicTable,
} from './classicPlan';

const COLUMNS = ['id', 'select_type', 'table', 'partitions', 'type', 'possible_keys', 'key', 'key_len', 'ref', 'rows', 'filtered', 'Extra'];
const TABLE: ClassicTable = {
  columns: COLUMNS,
  rows: [
    ['1', 'SIMPLE', 'e', null, 'ALL', 'idx_student', null, null, null, '10302', '33.33', 'Using where; Using temporary; Using filesort'],
    ['1', 'SIMPLE', 's', null, 'eq_ref', 'PRIMARY', 'PRIMARY', '4', 'lms.e.student_id', '1', '100.00', null],
    ['1', 'SIMPLE', 'c', null, 'ref', 'PRIMARY,idx_status', 'PRIMARY', '4', 'lms.e.course_id', '1', '50.00', 'Using where'],
  ],
};

describe('classicOf', () => {
  it('takes the classic table of a MySQL 8 / MariaDB tree plan', () => {
    const plan = { kind: 'tree' as const, root: null as never, raw: '{}', classic: TABLE };
    expect(classicOf(plan)).toBe(TABLE);
  });

  it('reads a MySQL 5.7 table plan, its NULL text becoming null', () => {
    const plan = { kind: 'table' as const, columns: ['id', 'key'], rows: [['1', 'NULL']], raw: '' };
    expect(classicOf(plan)).toEqual({ columns: ['id', 'key'], rows: [['1', null]] });
  });

  it('none for a plan without one (PostgreSQL, or the classic explain failed)', () => {
    expect(classicOf({ kind: 'tree', root: null as never, raw: '[]' })).toBeNull();
    expect(classicOf({ kind: 'tree', root: null as never, raw: '[]', classic: null })).toBeNull();
    expect(classicOf(null)).toBeNull();
  });
});

describe('access types', () => {
  it('the ladder runs best to worst', () => {
    expect(ACCESS_LADDER).toEqual(['system', 'const', 'eq_ref', 'ref', 'range', 'index', 'ALL']);
  });

  it('tones: system / const / eq_ref are good, index and ALL are slow, the rest plain', () => {
    expect(accessTone('eq_ref')).toBe('good');
    expect(accessTone('const')).toBe('good');
    expect(accessTone('ALL')).toBe('slow');
    expect(accessTone('index')).toBe('slow');
    expect(accessTone('ref')).toBeNull();
    expect(accessTone(null)).toBeNull();
  });
});

describe('extraItems', () => {
  it('one item per phrase, explained, the slow ones marked', () => {
    expect(extraItems('Using where; Using temporary; Using filesort')).toEqual([
      { text: 'Using where', note: 'Rows are checked against the WHERE after they are read.', slow: false },
      { text: 'Using temporary', note: 'A temporary table holds intermediate rows.', slow: true },
      { text: 'Using filesort', note: 'The ORDER BY sorts rows without an index.', slow: true },
    ]);
  });

  it('an unknown phrase keeps its text with no note; empty or null gives nothing', () => {
    expect(extraItems('Something new')).toEqual([{ text: 'Something new', note: null, slow: false }]);
    expect(extraItems('Using join buffer (hash join)')[0]!.note).toMatch(/join buffer/i);
    expect(extraItems(null)).toEqual([]);
    expect(extraItems('')).toEqual([]);
  });
});

describe('passRows', () => {
  it('rows × filtered %, null when either is missing', () => {
    expect(passRows('10302', '33.33')).toBe(3434);
    expect(passRows('1', null)).toBe(1);
    expect(passRows(null, '50')).toBeNull();
  });
});

describe('explainRow', () => {
  it('a full scan with an unused index', () => {
    const r = explainRow(TABLE, 0);
    expect(r.table).toBe('e');
    expect(r.type).toBe('ALL');
    expect(r.access).toBe('Full table scan. MySQL reads every row of the table, about 10,302 rows.');
    expect(r.index).toBe('An index exists (idx_student) but the optimizer did not use it.');
    expect(r.examined).toBe(10302);
    expect(r.passing).toBe(3434);
    expect(r.extra.map((x) => x.text)).toEqual(['Using where', 'Using temporary', 'Using filesort']);
  });

  it('a key lookup', () => {
    const r = explainRow(TABLE, 1);
    expect(r.access).toBe('One row per row of the previous table, through a unique index.');
    expect(r.index).toBe('Uses PRIMARY (4 bytes of the key), matched on lms.e.student_id.');
  });

  it('columns are found by name, case-insensitive; missing columns read as null', () => {
    const t: ClassicTable = { columns: ['ID', 'TABLE', 'TYPE'], rows: [['1', 'x', 'const']] };
    const r = explainRow(t, 0);
    expect(r.table).toBe('x');
    expect(r.type).toBe('const');
    expect(r.index).toBe('No index can serve this access.');
    expect(r.examined).toBeNull();
  });
});

describe('classicSummary', () => {
  it('tables, rows read on the first table, full scans and the slow Extra phrases', () => {
    expect(classicSummary(TABLE)).toEqual({
      tables: 3,
      firstRows: 10302,
      fullScans: 1,
      slowExtra: ['Using temporary', 'Using filesort'],
    });
  });
});
