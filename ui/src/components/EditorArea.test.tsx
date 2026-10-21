import { act, createEvent, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// The tabs themselves are tested on their own; here only which one a group shows matters.
vi.mock('../tabs/ObjectTab', () => ({ ObjectTab: ({ tab }: { tab: { id: string } }) => <div data-testid="object-tab">{tab.id}</div> }));
vi.mock('../tabs/query/QueryTab', () => ({ QueryTab: ({ tab }: { tab: { id: string } }) => <div data-testid="query-tab">{tab.id}</div> }));

import { groupTabs, useTabs, type ObjectTabRef } from '../store/tabs';
import { useTree } from '../store/tree';
import { resetStores } from '../test/backend';
import { dropZone, EditorArea } from './EditorArea';
import { setDraggedObject, setDraggedTab, useDrag } from './tabDrag';

const s = () => useTabs.getState();
const obj = (id: string): ObjectTabRef => ({ id, kind: 'object', connectionId: 'c1', title: id, objectRef: { database: 'd', schema: null, name: id, kind: 'Table' } });
const rect = { left: 0, top: 0, width: 300, height: 300 };

beforeEach(resetStores);
afterEach(() => {
  useDrag.setState({ item: null });
  vi.restoreAllMocks();
});

/** A drag event at (x, y) on `el` (jsdom has no DragEvent: the position goes on by hand). */
function dragAt(type: 'dragOver' | 'drop', el: Element, x: number, y: number) {
  const e = createEvent[type](el, { dataTransfer: { types: ['text/x-dnt-tab'], dropEffect: '' } });
  Object.defineProperties(e, { clientX: { value: x }, clientY: { value: y } });
  fireEvent(el, e);
}

const box = () =>
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({ ...rect, right: 300, bottom: 300, x: 0, y: 0, toJSON() {} } as DOMRect);

describe('dropZone', () => {
  it('the outer third of each side splits that side; the middle moves', () => {
    expect(dropZone(rect, 20, 150)).toBe('left');
    expect(dropZone(rect, 290, 150)).toBe('right');
    expect(dropZone(rect, 150, 10)).toBe('up');
    expect(dropZone(rect, 150, 280)).toBe('down');
    expect(dropZone(rect, 150, 150)).toBe('center');
    // A corner picks the nearer edge.
    expect(dropZone(rect, 10, 60)).toBe('left');
    expect(dropZone(rect, 60, 10)).toBe('up');
    expect(dropZone({ ...rect, width: 0 }, 0, 0)).toBe('center');
  });
});

describe('EditorArea', () => {
  it('renders the split tree: one pane per group, one splitter per split', () => {
    act(() => {
      for (const id of ['a', 'b', 'c']) s().open(obj(id));
      s().splitTab('c', 'right');
      s().splitGroup(s().groups[1]!.id, 'down');
    });
    render(<EditorArea />);
    expect(screen.getAllByTestId('editor-group')).toHaveLength(3);
    expect(screen.getAllByRole('separator', { name: 'Resize editor groups' })).toHaveLength(2);
    expect(screen.getByText('Drop a tab here or open one')).toBeInTheDocument();
  });

  it('double-clicking a splitter evens only its own split', () => {
    act(() => {
      for (const id of ['a', 'b', 'c']) s().open(obj(id));
      s().splitTab('c', 'right');
      s().splitGroup(s().groups[1]!.id, 'down');
      const t = s().tree as unknown as { id: string; b: { id: string } };
      s().setSplitRatio(t.id, 0.3);
      s().setSplitRatio(t.b.id, 0.7);
    });
    render(<EditorArea />);
    fireEvent.doubleClick(screen.getAllByRole('separator', { name: 'Resize editor groups' })[1]!);
    expect(s().tree).toMatchObject({ ratio: 0.3, b: { ratio: 0.5 } });
  });

  it('while a tab is dragged every group shows a drop layer above its content', () => {
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
    });
    render(<EditorArea />);
    expect(screen.queryAllByTestId('drop-layer')).toHaveLength(0);
    act(() => setDraggedTab('a'));
    expect(screen.getAllByTestId('drop-layer')).toHaveLength(2);
    act(() => void window.dispatchEvent(new Event('dragend')));
    expect(screen.queryAllByTestId('drop-layer')).toHaveLength(0);
  });

  it('dropping a tab on a pane edge splits that pane on that side; the middle moves it', () => {
    box();
    act(() => {
      for (const id of ['a', 'b', 'c']) s().open(obj(id));
      s().splitTab('c', 'right');
    });
    render(<EditorArea />);
    act(() => setDraggedTab('a'));
    const right = screen.getAllByTestId('drop-layer')[1]!;
    dragAt('dragOver', right, 20, 150);
    expect(right.querySelector('.drop-zone')).toHaveClass('zone-left');
    expect(right).toHaveTextContent('Split left');
    dragAt('drop', right, 20, 150);
    // c's pane split on its left; a sits in the new group between b and c.
    expect(s().groups.map((g) => groupTabs(s(), g.id).map((t) => t.id))).toEqual([['b'], ['a'], ['c']]);
    expect(s().activeId).toBe('a');

    act(() => setDraggedTab('b'));
    const middle = screen.getAllByTestId('drop-layer')[2]!;
    dragAt('drop', middle, 150, 150);
    expect(s().groups.map((g) => groupTabs(s(), g.id).map((t) => t.id))).toEqual([['a'], ['c', 'b']]);
  });

  it("a group's only tab over its own pane can only move (no split preview)", () => {
    box();
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
    });
    render(<EditorArea />);
    act(() => setDraggedTab('b'));
    const own = screen.getAllByTestId('drop-layer')[1]!;
    dragAt('dragOver', own, 290, 150);
    expect(own.querySelector('.drop-zone')).toHaveClass('zone-center');
  });

  it('an object dragged from the tree opens where it is dropped', () => {
    box();
    act(() => {
      s().open(obj('a'));
      useTree.setState({
        nodes: new Map([['conn:c1/db:d/folder:Table/obj:orders', { id: 'conn:c1/db:d/folder:Table/obj:orders', kind: 'object', label: 'orders', objectRef: { database: 'd', schema: null, name: 'orders', kind: 'Table' } }]]),
      });
    });
    render(<EditorArea />);
    act(() => setDraggedObject('conn:c1/db:d/folder:Table/obj:orders'));
    const layer = screen.getByTestId('drop-layer');
    dragAt('dragOver', layer, 290, 150);
    expect(layer).toHaveTextContent('Open to the right');
    dragAt('drop', layer, 290, 150);
    expect(s().groups).toHaveLength(2);
    expect(groupTabs(s(), s().groups[1]!.id).map((t) => t.title)).toEqual(['orders']);
  });

  it('a maximized group is shown alone', () => {
    act(() => {
      s().open(obj('a'));
      s().open(obj('b'));
      s().splitTab('b', 'right');
      s().toggleMaximize();
    });
    render(<EditorArea />);
    expect(screen.getAllByTestId('editor-group')).toHaveLength(1);
    expect(screen.getByTestId('object-tab')).toHaveTextContent('b');
  });
});
