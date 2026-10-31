import { describe, expect, it } from 'vitest';
import { seedColumns, seedPreview, seedRequest, seedSample, seedTable, seedTarget } from '../../test/fixtures/seed';
import { generatorsFor, uniqueApplies } from './seedFormat';
import { buildRequest, defaultGenerator, initialState, reduce, type SeedAction, type SeedState } from './seedState';

const run = (state: SeedState, ...actions: SeedAction[]) => actions.reduce(reduce, state);
const fresh = () => initialState({ connectionId: 'c1', table: seedTable });
const loaded = () => run(fresh(), { type: 'targetLoaded', target: seedTarget() });
const byName = (name: string) => seedColumns.find((c) => c.column.name === name)!;

describe('seedState', () => {
  it('starts with the defaults and takes the inferred plans and seed when the target loads', () => {
    const s = fresh();
    expect(s).toMatchObject({ rows: 1000, batchRows: 1000, existing: 'keep', step: 'configure', target: null });
    expect(buildRequest(s)).toBeNull();
    const l = loaded();
    expect(l.plans).toEqual(seedColumns.map((c) => c.plan));
    expect(l.seed).toBe(48213);
    expect(buildRequest(l)).toEqual(seedRequest('c1'));
  });

  it('Run again keeps the stored request when the columns still match', () => {
    const params = seedRequest('c1', { rows: 50, seed: 7, existing: 'delete', columns: seedColumns.map((c) => ({ ...c.plan, null_percent: 0 })) });
    params.columns[2] = { ...params.columns[2]!, generator: { kind: 'pattern', pattern: '##' } };
    const s = run(initialState({ connectionId: 'c1', table: seedTable, params }), { type: 'targetLoaded', target: seedTarget() });
    expect(buildRequest(s)).toEqual(params);
  });

  it('Run again falls back to the inferred plans when the table changed', () => {
    const params = seedRequest('c1', { seed: 7, columns: [{ name: 'gone', generator: { kind: 'uuid' }, null_percent: 0, unique: false }] });
    const s = run(initialState({ connectionId: 'c1', table: seedTable, params }), { type: 'targetLoaded', target: seedTarget() });
    expect(s.plans).toEqual(seedColumns.map((c) => c.plan));
    expect(s.seed).toBe(7);
  });

  it('switching a generator takes its defaults; patching touches only that column', () => {
    const s = run(loaded(), { type: 'setGenerator', column: 'email', generator: defaultGenerator('pattern', byName('email')) });
    expect(s.plans[2]?.generator).toEqual({ kind: 'pattern', pattern: '???-#####' });
    const p = run(s, { type: 'setPlan', column: 'total', patch: { null_percent: 25 } });
    expect(p.plans[4]?.null_percent).toBe(25);
    expect(p.plans.filter((x) => x.null_percent !== 0)).toHaveLength(1);
  });

  it('defaults per generator kind', () => {
    expect(defaultGenerator('int_range', byName('customer_id'))).toEqual({ kind: 'int_range', min: 1, max: 1000 });
    expect(defaultGenerator('decimal_range', byName('customer_id'))).toEqual({ kind: 'decimal_range', min: 0, max: 1000, scale: 2 });
    expect(defaultGenerator('sequence', byName('total'))).toEqual({ kind: 'sequence', start: 1, step: 1 });
    expect(defaultGenerator('pick', byName('status'))).toEqual({ kind: 'pick', values: ['new', 'paid', 'shipped'] });
    expect(defaultGenerator('foreign_key', byName('customer_id'))).toEqual({ kind: 'foreign_key', constraint: 'fk_orders_customer' });
    expect(defaultGenerator('random_bytes', byName('note'))).toEqual({ kind: 'random_bytes', len: 16 });
    expect(defaultGenerator('constant', byName('note'))).toEqual({ kind: 'constant', value: '' });
    // The inferred generator is kept when its kind is chosen again.
    expect(defaultGenerator('fake', byName('email'))).toEqual({ kind: 'fake', fake: 'email' });
    const today = new Date(Date.UTC(2026, 9, 30));
    expect(defaultGenerator('date_range', byName('note'), today)).toEqual({ kind: 'date_range', from: '2024-10-30', to: '2026-10-30' });
    expect(defaultGenerator('date_time_range', byName('note'), today)).toEqual({
      kind: 'date_time_range',
      from: '2024-10-30 00:00:00',
      to: '2026-10-30 23:59:59',
    });
  });

  it('any edit after the review drops the plan and returns to Configure', () => {
    const r = run(loaded(), { type: 'sampled', sample: seedSample() }, { type: 'prepared', preview: seedPreview() });
    expect(r.step).toBe('review');
    for (const action of [
      { type: 'setRows', rows: 5 },
      { type: 'setBatch', batchRows: 10 },
      { type: 'setExisting', existing: 'truncate' },
      { type: 'setSeed', seed: 1 },
      { type: 'setPlan', column: 'note', patch: { null_percent: 5 } },
    ] as SeedAction[]) {
      const s = reduce(r, action);
      expect(s.step).toBe('configure');
      expect(s.preview).toBeNull();
    }
    const back = reduce(r, { type: 'back' });
    expect(back).toMatchObject({ step: 'configure', preview: null });
  });
});

describe('seedFormat', () => {
  it('generatorsFor follows the column', () => {
    expect(generatorsFor(byName('total_with_tax'))).toEqual(['skip']);
    const fk = generatorsFor(byName('customer_id'));
    expect(fk[0]).toBe('skip');
    expect(fk).toContain('foreign_key');
    expect(fk).not.toContain('fake');
    const text = generatorsFor(byName('note'));
    expect(text).toContain('fake');
    expect(text).toContain('pattern');
    expect(text).not.toContain('decimal_range');
    expect(generatorsFor(byName('status'))).toEqual(['skip', 'pick', 'constant']);
  });

  it('unique applies to generators that can repeat', () => {
    expect(uniqueApplies('bool')).toBe(false);
    expect(uniqueApplies('constant')).toBe(false);
    expect(uniqueApplies('skip')).toBe(false);
    expect(uniqueApplies('fake')).toBe(true);
    expect(uniqueApplies('int_range')).toBe(true);
  });
});
