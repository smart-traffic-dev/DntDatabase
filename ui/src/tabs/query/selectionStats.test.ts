import { describe, expect, it } from 'vitest';
import type { Value } from '../../bindings';
import { selectionStats, statsText } from './selectionStats';

const int = (v: number | string): Value => ({ t: 'Int', v });
const NULL: Value = { t: 'Null' };

describe('selectionStats', () => {
  it('shows_nothing_below_two_cells', () => {
    expect(selectionStats([int(1)], 1)).toBeNull();
  });

  it('sums_integers_exactly', () => {
    const s = selectionStats([int(1), int(2000), int(1449)], 3)!;
    expect(statsText(s)).toBe('Count 3 · Sum 3,450 · Avg 1,150 · Min 1 · Max 2,000');
  });

  it('matches_the_documented_example', () => {
    const values = [...Array.from({ length: 10 }, () => int(300)), int(1), int(999 + 0)];
    const s = selectionStats(values, 12)!;
    expect(s.count).toBe(12);
    expect(s.numeric).toMatchObject({ sum: '4,000', min: '1', max: '999', approx: false });
    expect(s.numeric?.avg).toBe('333.3333');
  });

  it('keeps_big_integers_exact', () => {
    const s = selectionStats([int('9223372036854775807'), int('1')], 2)!;
    expect(s.numeric?.sum).toBe('9,223,372,036,854,775,808');
    expect(s.numeric?.max).toBe('9,223,372,036,854,775,807');
    expect(s.numeric?.approx).toBe(false);
  });

  it('averages_to_a_trimmed_fraction_and_handles_negatives', () => {
    expect(selectionStats([int(1), int(2)], 2)!.numeric?.avg).toBe('1.5');
    expect(selectionStats([int(-1), int(-2)], 2)!.numeric?.avg).toBe('-1.5');
    expect(selectionStats([int(1), int(-1)], 2)!.numeric?.avg).toBe('0');
  });

  it('is_approximate_for_decimals_and_floats', () => {
    const s = selectionStats([{ t: 'Decimal', v: '1.50' }, { t: 'Float', v: 2.25 }, int(1)], 3)!;
    expect(s.numeric).toMatchObject({ sum: '≈4.75', avg: '≈1.5833', min: '≈1', max: '≈2.25', approx: true });
  });

  it('treats_integer_decimals_as_exact', () => {
    const s = selectionStats([{ t: 'Decimal', v: '12345678901234567890' }, int(1)], 2)!;
    expect(s.numeric?.sum).toBe('12,345,678,901,234,567,891');
    expect(s.numeric?.approx).toBe(false);
  });

  it('counts_only_for_non_numeric_cells_and_skips_null', () => {
    const s = selectionStats([{ t: 'Text', v: 'a' }, { t: 'Text', v: '1' }, NULL], 3)!;
    expect(s).toEqual({ count: 2, numeric: null });
    expect(statsText(s)).toBe('Count 2');
  });

  it('sums_only_the_numeric_cells_of_a_mixed_selection', () => {
    const s = selectionStats([int(2), { t: 'Text', v: 'x' }, int(4)], 3)!;
    expect(s.count).toBe(3);
    expect(s.numeric?.sum).toBe('6');
  });

  it('ignores_non_finite_floats', () => {
    const s = selectionStats([{ t: 'Float', v: Number.NaN }, int(3)], 2)!;
    expect(s.numeric?.sum).toBe('3');
  });

  it('handles_a_large_selection_without_overflowing_the_stack', () => {
    const values = Array.from({ length: 50_000 }, (_, i) => ({ t: 'Float', v: i + 0.5 }) as Value);
    expect(selectionStats(values, values.length)?.numeric?.approx).toBe(true);
  });
});
