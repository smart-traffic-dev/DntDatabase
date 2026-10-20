import { describe, expect, it } from 'vitest';
import { fuzzyMatch, rankItems } from './paletteMatch';

describe('fuzzyMatch', () => {
  it('matches a subsequence, case-insensitively, and reports the positions', () => {
    const m = fuzzyMatch('nq', 'File: New query');
    expect(m).not.toBeNull();
    expect(m?.indices.map((i) => 'File: New query'[i])).toEqual(['N', 'q']);
    expect(fuzzyMatch('NEW', 'new query')?.indices).toEqual([0, 1, 2]);
  });

  it('rejects text that lacks the characters or has them in the wrong order', () => {
    expect(fuzzyMatch('xyz', 'File: New query')).toBeNull();
    expect(fuzzyMatch('qn', 'new query')).toBeNull();
    expect(fuzzyMatch('longerthanthetext', 'short')).toBeNull();
  });

  it('matches everything for an empty query and ignores spaces in the query', () => {
    expect(fuzzyMatch('', 'anything')).toEqual({ score: 0, indices: [] });
    expect(fuzzyMatch('  ', 'anything')?.indices).toEqual([]);
    expect(fuzzyMatch('new q', 'New query')?.indices).toEqual([0, 1, 2, 4]);
  });

  it('scores consecutive characters and word starts above scattered ones', () => {
    const consecutive = fuzzyMatch('query', 'New query')!;
    const scattered = fuzzyMatch('query', 'quite unreasonably ready')!;
    expect(consecutive.score).toBeGreaterThan(scattered.score);
    const wordStart = fuzzyMatch('sq', 'Save query')!; // S, q at the start of words
    const inside = fuzzyMatch('sq', 'Xasqx')!;
    expect(wordStart.score).toBeGreaterThan(inside.score);
  });

  it('prefers the shorter text on equal matches', () => {
    expect(fuzzyMatch('save', 'Save')!.score).toBeGreaterThan(fuzzyMatch('save', 'Save as…')!.score);
  });

  it('finds the best alignment, not the first one', () => {
    // "ab" appears scattered first (a...b) and consecutive later: the consecutive one wins.
    const m = fuzzyMatch('ab', 'a xxxxxx ab')!;
    expect(m.indices).toEqual([9, 10]);
  });

  it('treats a camelCase hump as a word start', () => {
    const m = fuzzyMatch('oi', 'orderItems')!;
    expect(m.indices).toEqual([0, 5]);
  });
});

describe('rankItems', () => {
  const items = ['View: Zoom in', 'File: New query', 'File: Save', 'Query: Run all'];

  it('keeps every item in order for an empty query', () => {
    expect(rankItems(items, '', (s) => [s]).map((r) => r.item)).toEqual(items);
  });

  it('drops non-matches and puts the best first', () => {
    const ranked = rankItems(items, 'save', (s) => [s]);
    expect(ranked.map((r) => r.item)).toEqual(['File: Save']);
    const q = rankItems(items, 'query', (s) => [s]).map((r) => r.item);
    // A match at the very start of the text beats one at a word start later on.
    expect(q).toEqual(['Query: Run all', 'File: New query']);
  });

  it('keeps the original order between equal scores', () => {
    const same = ['aa', 'ab', 'ac'];
    expect(rankItems(same, 'a', (s) => [s]).map((r) => r.item)).toEqual(['aa', 'ab', 'ac']);
  });

  it('matches a secondary text with a lower score and no highlight', () => {
    const rows = [
      { name: 'orders', path: 'shop-prod › shop' },
      { name: 'customers', path: 'shop-prod › orders_db' },
    ];
    const ranked = rankItems(rows, 'orders', (r) => [r.name, `${r.path} › ${r.name}`]);
    expect(ranked.map((r) => r.item.name)).toEqual(['orders', 'customers']);
    expect(ranked[0]!.indices.length).toBe(6);
    expect(ranked[1]!.indices).toEqual([]);
  });
});
