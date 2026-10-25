import { describe, expect, it } from 'vitest';
import type { ImportRequest, ImportTarget, Inspection, MappingReport, TargetColumn } from '../../bindings';
import {
  buildRequest,
  canAdvance,
  canOpen,
  formatFromPath,
  initialState,
  reduce,
  type WizardAction,
  type WizardState,
} from './importState';

const table = { database: 'shop', schema: null, name: 'orders', kind: 'Table' as const };

const column = (name: string, over: Partial<TargetColumn> = {}): TargetColumn => ({
  name,
  db_type: 'int',
  logical: 'Integer',
  nullable: true,
  has_default: false,
  auto_increment: false,
  identity: null,
  generated: false,
  enum_values: [],
  max_chars: null,
  collation: null,
  ...over,
});

const target: ImportTarget = {
  columns: [column('id', { nullable: false }), column('name', { db_type: 'varchar(20)', logical: 'Text' })],
  keys: [{ name: 'PRIMARY', columns: ['id'], primary: true }],
  atomicity: 'yes',
  env: null,
  read_only: false,
  server: { kind: 'mysql', variant: 'MySql8', version: [8, 0, 36], version_string: '8.0.36' },
};

const inspection: Inspection = {
  columns: ['id', 'name'],
  rows: [{ number: 2, cells: [{ t: 'Text', v: '1' }, { t: 'Text', v: 'Ann' }] }],
  sheets: [],
  delimiter: ',',
  total_bytes: 20,
};

const ok: MappingReport = { errors: [], warnings: [] };

const run = (state: WizardState, ...actions: WizardAction[]) => actions.reduce(reduce, state);

/** A state with a file inspected, the target loaded and a clean mapping report. */
function ready(): WizardState {
  return run(
    initialState({ connectionId: 'c1', table }),
    { type: 'file', path: 'C:\\data\\orders.csv' },
    { type: 'inspected', inspection },
    { type: 'targetLoaded', target },
    { type: 'mapping', mapping: [{ target: 'id', source: 0 }, { target: 'name', source: 1 }] },
    { type: 'report', report: ok },
  );
}

describe('formatFromPath', () => {
  it('guesses the format and the TSV delimiter from the extension', () => {
    expect(formatFromPath('a.CSV')).toEqual({ format: 'csv', delimiter: null });
    expect(formatFromPath('a.tsv')).toEqual({ format: 'csv', delimiter: '\t' });
    expect(formatFromPath('a.json')).toEqual({ format: 'json', delimiter: null });
    expect(formatFromPath('a.ndjson')).toEqual({ format: 'jsonl', delimiter: null });
    expect(formatFromPath('a.jsonl')).toEqual({ format: 'jsonl', delimiter: null });
    expect(formatFromPath('C:\\x\\book.xlsx')).toEqual({ format: 'xlsx', delimiter: null });
    expect(formatFromPath('notes.txt')).toEqual({ format: 'csv', delimiter: null });
  });
});

describe('step gating', () => {
  it('Next is disabled until a file is inspected', () => {
    let s = initialState({ connectionId: 'c1', table });
    expect(canAdvance(s)).toBe(false);
    s = reduce(s, { type: 'file', path: 'orders.csv' });
    expect(s.source.format).toBe('csv');
    expect(canAdvance(s)).toBe(false);
    s = reduce(s, { type: 'inspected', inspection });
    expect(canAdvance(s)).toBe(true);
  });

  it('Target needs the loaded target; Mapping needs a report without errors', () => {
    let s = run(initialState({ connectionId: 'c1', table }), { type: 'file', path: 'a.csv' }, { type: 'inspected', inspection }, { type: 'goto', step: 'target' });
    expect(s.step).toBe('target');
    expect(canAdvance(s)).toBe(false);
    s = reduce(s, { type: 'targetLoaded', target });
    expect(canAdvance(s)).toBe(true);
    s = run(s, { type: 'goto', step: 'mapping' }, { type: 'mapping', mapping: [{ target: 'id', source: null }] });
    // Not checked yet.
    expect(canAdvance(s)).toBe(false);
    s = reduce(s, { type: 'report', report: { errors: [{ column: 'id', message: 'Required' }], warnings: [] } });
    expect(canAdvance(s)).toBe(false);
    s = run(s, { type: 'mapOne', target: 'id', source: 0 });
    expect(s.report).toBeNull();
    expect(s.mapping).toEqual([{ target: 'id', source: 0 }]);
    s = reduce(s, { type: 'report', report: ok });
    expect(canAdvance(s)).toBe(true);
  });

  it('Upsert requires a key', () => {
    let s = run(ready(), { type: 'goto', step: 'options' });
    expect(canAdvance(s)).toBe(true);
    s = run(s, { type: 'mode', mode: { kind: 'upsert', key: [] } }, { type: 'report', report: ok });
    expect(canAdvance(s)).toBe(false);
    s = run(s, { type: 'mode', mode: { kind: 'upsert', key: ['id'] } }, { type: 'report', report: ok });
    expect(canAdvance(s)).toBe(true);
  });

  it('a step opens only when every step before it can be left', () => {
    const s = initialState({ connectionId: 'c1', table });
    expect(canOpen(s, 'source')).toBe(true);
    expect(canOpen(s, 'target')).toBe(false);
    const r = ready();
    expect(canOpen(r, 'review')).toBe(true);
    // A refused goto keeps the step.
    expect(reduce(s, { type: 'goto', step: 'review' }).step).toBe('source');
  });
});

describe('invalidation', () => {
  it('a new file or source option drops the inspection, mapping, dry run and plan', () => {
    let s = run(ready(), { type: 'dryRun', jobId: 'j1', server: false }, { type: 'prepared', preview: {} as never });
    s = reduce(s, { type: 'source', patch: { header: false } });
    expect(s.inspection).toBeNull();
    expect(s.mapping).toEqual([]);
    expect(s.report).toBeNull();
    expect(s.dryRun).toBeNull();
    expect(s.preview).toBeNull();
  });

  it('a new table drops the target and the mapping; options keep the mapping but drop the plan', () => {
    let s = reduce(ready(), { type: 'table', table: { ...table, name: 'items' } });
    expect(s.target).toBeNull();
    expect(s.mapping).toEqual([]);
    s = run(ready(), { type: 'prepared', preview: {} as never }, { type: 'convert', patch: { trim: true } });
    expect(s.preview).toBeNull();
    expect(s.mapping).toHaveLength(2);
    expect(s.report).toEqual(ok);
  });

  it('a new error policy drops the dry run (its result was collected under the old one)', () => {
    const s = run(ready(), { type: 'dryRun', jobId: 'j1', server: true }, { type: 'errors', errors: { kind: 'skip', max_errors: 5 } });
    expect(s.dryRun).toBeNull();
    expect(s.report).toEqual(ok);
  });
});

describe('buildRequest and params', () => {
  it('builds the ImportRequest from the state', () => {
    const s = run(ready(), { type: 'errors', errors: { kind: 'skip', max_errors: 50 } }, { type: 'batch', rows: 500, bytes: 2048 });
    expect(buildRequest(s)).toEqual<ImportRequest>({
      connection_id: 'c1',
      table,
      path: 'C:\\data\\orders.csv',
      source: { format: 'csv', delimiter: null, header: true, sheet: null },
      mapping: [{ target: 'id', source: 0 }, { target: 'name', source: 1 }],
      mode: { kind: 'append' },
      errors: { kind: 'skip', max_errors: 50 },
      convert: { empty: 'null', null_text: null, trim: false, binary: 'hex' },
      batch_rows: 500,
      batch_bytes: 2048,
      override_identity: false,
    });
    expect(buildRequest(initialState({ connectionId: 'c1' }))).toBeNull();
  });

  it('stored parameters pre-fill every choice (Run again)', () => {
    const params = buildRequest(run(ready(), { type: 'mode', mode: { kind: 'replace', truncate: true } }))!;
    const s = initialState({ connectionId: 'c1', table, params });
    expect(s.path).toBe(params.path);
    expect(s.mapping).toEqual(params.mapping);
    expect(s.mode).toEqual({ kind: 'replace', truncate: true });
    expect(s.step).toBe('source');
    expect(s.inspection).toBeNull();
  });
});
