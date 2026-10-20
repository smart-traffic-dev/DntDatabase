import { describe, expect, it } from 'vitest';
import { formatCount, plural, pluralWord } from './plural';

describe('plural', () => {
  it('formats counts with thousands separators', () => {
    expect(formatCount(0)).toBe('0');
    expect(formatCount(999)).toBe('999');
    expect(formatCount(10000)).toBe('10,000');
    expect(formatCount(1234567)).toBe('1,234,567');
    expect(formatCount(-1500)).toBe('-1,500');
  });

  it('uses the singular for exactly one', () => {
    expect(plural(1, 'row')).toBe('1 row');
    expect(plural(0, 'row')).toBe('0 rows');
    expect(plural(2, 'row')).toBe('2 rows');
    expect(plural(10000, 'row')).toBe('10,000 rows');
    expect(plural(1, 'statement')).toBe('1 statement');
    expect(plural(6, 'statement')).toBe('6 statements');
  });

  it('takes an irregular plural', () => {
    expect(plural(1, 'query', 'queries')).toBe('1 query');
    expect(plural(3, 'query', 'queries')).toBe('3 queries');
    expect(pluralWord(1, 'entry', 'entries')).toBe('entry');
    expect(pluralWord(2, 'tab')).toBe('tabs');
  });
});
