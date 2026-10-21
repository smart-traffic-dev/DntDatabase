import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { ARRANGEMENT_KEY, persistArrangement, restoreArrangement } from './editorArrangement';
import { groupTabs, useTabs, type QueryTabRef } from './tabs';

const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });
const s = () => useTabs.getState();
let stop: () => void = () => {};

beforeEach(() => {
  localStorage.clear();
  s().reset();
});
afterEach(() => stop());

describe('editor arrangement across restarts', () => {
  it('saves changes once persistence is on, and restores them onto the reopened tabs', () => {
    for (const id of ['a', 'b']) s().open(q(id));
    stop = persistArrangement();
    s().splitTab('b', 'down');
    expect(JSON.parse(localStorage.getItem(ARRANGEMENT_KEY)!)).toMatchObject({ v: 2, tree: { kind: 'split', dir: 'col' } });
    stop();

    s().reset();
    for (const id of ['a', 'b']) s().open(q(id));
    restoreArrangement();
    expect(s().layout).toBe('rows');
    expect(groupTabs(s(), s().groups[1]!.id).map((t) => t.id)).toEqual(['b']);
  });

  it('restores the first saved shape (presets + shared ratios) as a tree', () => {
    for (const id of ['a', 'b']) s().open(q(id));
    localStorage.setItem(
      ARRANGEMENT_KEY,
      JSON.stringify({ layout: 'columns', ratios: { x: 0.3, y: 0.5 }, groups: [{ tabIds: ['a'] }, { tabIds: ['b'] }], focused: 1 }),
    );
    restoreArrangement();
    expect(s().layout).toBe('columns');
    expect(s().tree).toMatchObject({ kind: 'split', dir: 'row', ratio: 0.3 });
    expect(groupTabs(s(), s().focusedGroupId).map((t) => t.id)).toEqual(['b']);
  });

  it('ignores a missing or broken saved value', () => {
    s().open(q('a'));
    localStorage.setItem(ARRANGEMENT_KEY, '{not json');
    restoreArrangement();
    expect(s().groups).toHaveLength(1);
    localStorage.setItem(ARRANGEMENT_KEY, JSON.stringify({ layout: 'grid' }));
    restoreArrangement();
    expect(s().groups).toHaveLength(1);
  });

  it('does not save before it is turned on (startup must not overwrite the saved layout)', () => {
    localStorage.setItem(ARRANGEMENT_KEY, 'saved');
    s().open(q('a'));
    expect(localStorage.getItem(ARRANGEMENT_KEY)).toBe('saved');
  });
});
