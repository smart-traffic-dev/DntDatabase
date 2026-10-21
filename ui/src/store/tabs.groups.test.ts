import { beforeEach, describe, expect, it } from 'vitest';
import { groupTabs, useTabs, type QueryTabRef } from './tabs';

const q = (id: string): QueryTabRef => ({ id, kind: 'query', connectionId: null, title: id });
const s = () => useTabs.getState();
const ids = (groupId: string) => groupTabs(s(), groupId).map((t) => t.id);
const open = (...names: string[]) => names.forEach((n) => s().open(q(n)));
const groupIds = () => s().groups.map((g) => g.id);

beforeEach(() => s().reset());

describe('editor groups', () => {
  it('starts with one focused group; opening tabs fills it', () => {
    open('a', 'b');
    expect(s().groups).toHaveLength(1);
    expect(s().layout).toBe('single');
    expect(ids(s().focusedGroupId)).toEqual(['a', 'b']);
    expect(s().activeId).toBe('b');
  });

  it('split right moves the tab into a new group to the right and focuses it', () => {
    open('a', 'b');
    s().splitTab('b', 'right');
    expect(s().layout).toBe('columns');
    const [g1, g2] = groupIds();
    expect(ids(g1!)).toEqual(['a']);
    expect(ids(g2!)).toEqual(['b']);
    expect(s().focusedGroupId).toBe(g2);
    expect(s().activeId).toBe('b');
  });

  it('split down makes rows; a third group splits only its own pane; at most four groups', () => {
    open('a', 'b', 'c', 'd', 'e');
    s().splitTab('e', 'down');
    expect(s().layout).toBe('rows');
    s().activate('d');
    s().splitTab('d', 'right');
    expect(s().groups).toHaveLength(3);
    expect(s().layout).toBe('custom');
    // The top pane split into two columns; the bottom pane (e) is untouched.
    expect(s().tree).toMatchObject({ dir: 'col', a: { dir: 'row' }, b: { kind: 'group' } });
    expect(ids(s().groups[2]!.id)).toEqual(['e']);
    s().activate('c');
    s().splitTab('c', 'right');
    expect(s().groups).toHaveLength(4);
    s().activate('b');
    s().splitTab('b', 'right');
    expect(s().groups).toHaveLength(4);
    expect(s().canSplit()).toBe(false);
  });

  it('splitting the only tab of a group opens an empty group beside it, focused', () => {
    open('a');
    s().splitTab('a', 'right');
    expect(s().groups).toHaveLength(2);
    expect(ids(groupIds()[0]!)).toEqual(['a']);
    expect(ids(groupIds()[1]!)).toEqual([]);
    expect(s().focusedGroupId).toBe(groupIds()[1]);
    expect(s().activeId).toBeUndefined();
    // A new tab opens in the focused (empty) group.
    open('b');
    expect(ids(groupIds()[1]!)).toEqual(['b']);
  });

  it('activating a tab focuses its group; activeId is the focused group active tab', () => {
    open('a', 'b');
    s().splitTab('b', 'right');
    const [g1] = groupIds();
    s().activate('a');
    expect(s().focusedGroupId).toBe(g1);
    expect(s().activeId).toBe('a');
    s().focusGroup(groupIds()[1]!);
    expect(s().activeId).toBe('b');
  });

  it('opening a tab that is open in another group focuses it there', () => {
    open('a', 'b');
    s().splitTab('b', 'right');
    s().focusGroup(groupIds()[0]!);
    s().open(q('b'));
    expect(s().focusedGroupId).toBe(groupIds()[1]);
    expect(s().tabs.filter((t) => t.id === 'b')).toHaveLength(1);
  });

  it('closing the last tab of a group closes the group; the neighbour takes focus', () => {
    open('a', 'b');
    s().splitTab('b', 'right');
    s().close('b');
    expect(s().groups).toHaveLength(1);
    expect(s().layout).toBe('single');
    expect(s().activeId).toBe('a');
  });

  it('closing a tab activates its neighbour inside the same group', () => {
    open('a', 'b', 'c', 'd');
    s().splitTab('d', 'right');
    s().activate('b');
    s().close('b');
    expect(s().activeId).toBe('c');
    expect(ids(groupIds()[1]!)).toEqual(['d']);
  });

  it('moveTab moves a tab between groups and keeps its identity', () => {
    open('a', 'b', 'c');
    s().splitTab('c', 'right');
    const [g1, g2] = groupIds();
    s().moveTab('a', g2!);
    expect(ids(g1!)).toEqual(['b']);
    expect(ids(g2!)).toEqual(['c', 'a']);
    expect(s().activeId).toBe('a');
    // Moving the last tab out closes the source group.
    s().moveTab('b', g2!, 0);
    expect(s().groups).toHaveLength(1);
    expect(ids(g2!)).toEqual(['b', 'c', 'a']);
  });

  it('moveToNeighbour moves the active tab to the next / previous group', () => {
    open('a', 'b');
    s().splitTab('b', 'right');
    s().open(q('c'));
    s().moveToNeighbour(-1);
    expect(ids(groupIds()[0]!)).toEqual(['a', 'c']);
    expect(s().focusedGroupId).toBe(groupIds()[0]);
  });

  it('layout presets add empty groups or merge extra groups into the last kept one', () => {
    open('a', 'b', 'c');
    s().setLayout('grid');
    expect(s().groups).toHaveLength(4);
    expect(ids(groupIds()[0]!)).toEqual(['a', 'b', 'c']);
    s().activate('c');
    s().moveTab('c', groupIds()[3]!);
    s().setLayout('rows');
    expect(s().layout).toBe('rows');
    expect(s().groups).toHaveLength(2);
    expect([...ids(groupIds()[0]!), ...ids(groupIds()[1]!)].sort()).toEqual(['a', 'b', 'c']);
    s().setLayout('single');
    expect(s().groups).toHaveLength(1);
    expect(ids(groupIds()[0]!)).toEqual(['a', 'b', 'c']);
  });

  it('each split has its own ratio, clamped to keep groups usable', () => {
    open('a', 'b', 'c');
    s().splitTab('c', 'right');
    s().splitGroup(s().groups[1]!.id, 'down');
    const outer = s().tree as unknown as { id: string; b: { id: string } };
    s().setSplitRatio(outer.id, 0.02);
    s().setSplitRatio(outer.b.id, 0.99);
    expect(s().tree).toMatchObject({ ratio: 0.15, b: { ratio: 0.85 } });
  });

  it('splitGroup splits the target pane on any side, moving the dropped tab there', () => {
    open('a', 'b');
    const [g1] = groupIds();
    s().splitGroup(g1!, 'left', 'b');
    expect(s().tree).toMatchObject({ dir: 'row', a: { kind: 'group' }, b: { kind: 'group', id: g1 } });
    expect(ids(groupIds()[0]!)).toEqual(['b']);
    expect(s().activeId).toBe('b');
    s().splitGroup(g1!, 'up', 'b');
    // The left group lost its only tab and closed; b now sits above a.
    expect(s().tree).toMatchObject({ dir: 'col', a: { kind: 'group' }, b: { kind: 'group', id: g1 } });
    expect(s().groups).toHaveLength(2);
    // A group's only tab onto its own edge changes nothing.
    const before = s().tree;
    s().splitGroup(s().groupOf.b!, 'right', 'b');
    expect(s().tree).toBe(before);
  });

  it('maximize shows one group; focusing another group or splitting restores', () => {
    open('a', 'b');
    s().splitTab('b', 'right');
    const [g1, g2] = groupIds();
    s().toggleMaximize();
    expect(s().maximizedGroupId).toBe(g2);
    s().toggleMaximize();
    expect(s().maximizedGroupId).toBeUndefined();
    s().toggleMaximize(g1);
    expect(s().focusedGroupId).toBe(g1);
    s().focusGroup(g2!);
    expect(s().maximizedGroupId).toBeUndefined();
    // Nothing to maximize with one group.
    s().setLayout('single');
    s().toggleMaximize();
    expect(s().maximizedGroupId).toBeUndefined();
  });

  it('a preview tab is replaced by the next preview in its group; keepTab makes it stay', () => {
    const obj = (id: string) => ({ id, kind: 'object' as const, connectionId: 'c', title: id, objectRef: { database: 'd', schema: null, name: id, kind: 'Table' as const } });
    open('q');
    s().openPreview(obj('t1'));
    expect(s().tabs.map((t) => [t.id, !!t.preview])).toEqual([
      ['q', false],
      ['t1', true],
    ]);
    s().openPreview(obj('t2'));
    expect(s().tabs.map((t) => t.id)).toEqual(['q', 't2']);
    expect(s().activeId).toBe('t2');
    s().keepTab('t2');
    s().openPreview(obj('t3'));
    expect(s().tabs.map((t) => [t.id, !!t.preview])).toEqual([
      ['q', false],
      ['t2', false],
      ['t3', true],
    ]);
    // Opening an open tab as a preview only activates it.
    s().openPreview(obj('t2'));
    expect(s().tabs).toHaveLength(3);
    // Pinning a preview keeps it too.
    s().setPinned('t3', true);
    expect(s().tabs.find((t) => t.id === 't3')?.preview).toBe(false);
  });

  it('pinning reorders inside the group', () => {
    open('a', 'b', 'c');
    s().setPinned('c', true);
    expect(ids(s().focusedGroupId)).toEqual(['c', 'a', 'b']);
  });

  it('moving tabs keeps pinned tabs first in the target group', () => {
    open('a', 'b', 'p');
    s().setPinned('p', true);
    s().splitTab('b', 'right');
    const [g1, g2] = groupIds();
    open('c'); // into g2: [b, c]
    // A pinned tab dropped after the unpinned ones joins the pinned block.
    s().moveTab('p', g2!);
    expect(ids(g2!)).toEqual(['p', 'b', 'c']);
    // An unpinned tab dropped onto the pinned block lands right after it.
    s().moveTab('a', g2!, 0);
    expect(ids(g2!)).toEqual(['p', 'a', 'b', 'c']);
    expect(s().groups.map((g) => g.id)).toEqual([g2]);
    void g1;
  });

  it('closeConnection keeps every group consistent', () => {
    const obj = (id: string, connectionId: string) =>
      s().open({ id, kind: 'object', connectionId, title: id, objectRef: { database: 'd', schema: null, name: id, kind: 'Table' } });
    obj('x', 'c1');
    obj('y', 'c2');
    s().splitTab('y', 'right');
    s().closeConnection('c2');
    expect(s().groups).toHaveLength(1);
    expect(s().activeId).toBe('x');
  });

  it('arrangement round-trips (restore after start)', () => {
    open('a', 'b', 'c');
    s().splitTab('c', 'down');
    s().setSplitRatio((s().tree as { id: string }).id, 0.3);
    const saved = s().arrangement();
    s().reset();
    open('a', 'b', 'c', 'z');
    s().applyArrangement(JSON.parse(JSON.stringify(saved)));
    expect(s().layout).toBe('rows');
    expect(s().tree).toMatchObject({ dir: 'col', ratio: 0.3 });
    expect(ids(groupIds()[0]!)).toEqual(['a', 'b', 'z']);
    expect(ids(groupIds()[1]!)).toEqual(['c']);
  });

  it('an arrangement whose tabs are all gone falls back to one group', () => {
    open('a');
    s().splitTab('a', 'right');
    open('b');
    const saved = s().arrangement();
    s().reset();
    open('z');
    s().applyArrangement(saved);
    expect(s().groups).toHaveLength(1);
    expect(s().activeId).toBe('z');
  });
});
